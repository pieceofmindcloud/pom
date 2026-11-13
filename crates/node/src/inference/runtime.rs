//! Mutable operating state of this node.
//!
//! Everything that used to be fixed by the command line at startup — which GGUF
//! to serve, which layers, the sampling knobs — lives here so the web UI can
//! change it while the process keeps running. The runtime owns the in-process
//! Rust inference server and restarts it when the configuration changes.

use crate::model::{LayerAssignment, ModelProfile};
use crate::network::Network;
use crate::thermal::{ThermalAction, ThermalMachine, ThermalPolicy};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{watch, Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

/// Give up restarting a crash-looping inference server after this many tries.
/// The same budget is persisted by `lastslice` so a process crash cannot reset it.
const MAX_RESTARTS: u32 = crate::lastslice::MAX_AUTOMATIC_ATTEMPTS;
/// Pause between a failed load and the next attempt. A failed load is most
/// often memory: the dead process' VRAM, the driver's cleanup and the page
/// cache take a while to come back, and the old five seconds retried before
/// they had, failing again for the same reason.
const RESTART_INTERVAL: Duration = Duration::from_secs(30);
/// An old supervisor cannot block the first request indefinitely
/// after an unload. The limit covers the transport's graceful shutdown and
/// gives `spawn_blocking` a short window for native cleanup;
/// if it overruns, the task is kept joinable and the new generation does not start.
/// Five seconds cover the 3 s TCP drain and the 2 s QUIC close without
/// turning a transport failure into a wait of tens of seconds.
const SUPERVISOR_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// The announcement is only a discovery update: a failure cannot
/// keep the local server from serving again. The deadline covers DNS resolution
/// and the UDP sends to the known addresses without leaving the reload stuck. One
/// second is short relative to the 300 ms QUIC fallback path, but leaves
/// margin for a local lookup without affecting a healthy announcement.
const ANNOUNCE_DEADLINE: Duration = Duration::from_secs(1);

/// A single /health only proves that a response was assembled. The Windows
/// crash happened at the same instant that response became
/// model_loaded, so the persistent annotation can only be promoted after
/// two consecutive probes. With the watcher's 3 s interval, this
/// lets the first return pass plus a short stability window without
/// inventing a new product timeout.
const HEALTH_CONFIRMATION_POLLS: u32 = 2;
/// The context ceiling is the model's own: `<arch>.context_length` from the
/// GGUF header (`splitter::read_context_length`). A header that does not
/// declare it leaves the limit to the engine; POM sets no fixed cap.
fn model_context_limit(model_path: &str) -> Option<u32> {
    crate::splitter::read_context_length(std::path::Path::new(model_path))
        .ok()
        .flatten()
}

/// Total KV-cache capacity needed to give every configured session the full
/// context requested by the operator.
pub fn total_context_tokens(n_ctx: u32, max_sessions: u32) -> Result<u32> {
    n_ctx
        .checked_mul(max_sessions.max(1))
        .ok_or_else(|| anyhow::anyhow!("n_ctx * max_sessions exceeds the numeric limit"))
}

/// The same `NodeConfig` -> `EngineConfig` conversion that `infer_server::run`
/// builds before loading the model for real.
///
/// Extracted to be reused by the capacity preview
/// (`crate::deploy::capacity_preview_endpoint`), which needs the same normalized
/// parameters without calling the FFI. Both pass `cfg.needs_residual_export()`
/// as `need_embeddings`.
pub(crate) fn engine_config_from_node(
    cfg: &NodeConfig,
    need_embeddings: bool,
) -> Result<crate::llm::EngineConfig> {
    let rs_seq_window = cfg.speculative_k.max(cfg.mtp_draft_tokens + 1);
    Ok(crate::llm::EngineConfig {
        mmproj_path: cfg.mmproj_path.clone(),
        mtp_draft_path: cfg.mtp_draft_path.clone(),
        n_ctx: total_context_tokens(cfg.n_ctx, cfg.max_sessions)?,
        offload_kqv: cfg.offload_kqv,
        cache_type_k: cfg.cache_type_k,
        cache_type_v: cfg.cache_type_v,
        fit_memory: cfg.fit_memory && cfg.tp.is_none(),
        fit_memory_margin_mb: cfg.fit_memory_margin_mb,
        n_batch: cfg.n_batch,
        n_ubatch: cfg.n_ubatch,
        n_gpu_layers: cfg.n_gpu_layers as i32,
        n_threads: 0,
        max_sessions: cfg.max_sessions.max(1),
        need_embeddings,
        mtp: cfg.mtp_effective(),
        mtp_cpu_fallback_enabled: cfg.mtp_cpu_fallback_enabled,
        mtp_placement: cfg.mtp_placement,
        moe_experts_in_ram: cfg.moe_experts_in_ram,
        moe_cache_mib: cfg.moe_cache_mib,
        rs_seq_window,
    })
}

/// Converts the server's readiness proof into a publishable capacity.
/// `n_ctx_per_session` is the window llama.cpp actually managed to reserve;
/// `max_new_tokens` remains only the size of one leg. The published
/// capacity is derived only from these effective runtime values.
fn runtime_capacity_from_health(
    body: &serde_json::Value,
    config: &NodeConfig,
) -> crate::network::RuntimeCapacity {
    let n_ctx = body
        .get("n_ctx_per_session")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(config.n_ctx);
    crate::network::RuntimeCapacity {
        n_ctx,
        max_new_tokens: config.max_new_tokens,
    }
}

// ── Configuration ────────────────────────────────────────────────────────────

/// Conservative headroom for the CUDA allocation projection: 256 MiB keeps the
/// capacity gain observed on the hybrid model without going back to consuming 1 GiB.
/// Defined in `llama-engine` (the `llm` uses it); re-exported so that
/// `crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB` remains valid.
pub use llama_engine::DEFAULT_FIT_MEMORY_MARGIN_MB;

/// What this node is currently serving. Serialisable so the UI can round-trip it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    /// Path of the GGUF this peer loads — a full model or one slice of one.
    pub model_path: String,
    /// Multimodal projector loaded together with the GGUF, equivalent to `--mmproj`.
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// External MTP draft GGUF ("shared MTP module"), present only on the
    /// machine that serves the last layer of the model (where the `nextn` block
    /// would run). MTP uses its own `mtp-` convention and quantization; the
    /// `mmproj_path` uses association verified by GGUF identity and dimension.
    /// See `models::catalog::find_mtp_draft_sibling` and `crate::llm::mtp`.
    #[serde(default)]
    pub mtp_draft_path: Option<String>,
    /// Label shared by every peer of the same model. The pipeline planner only
    /// chains peers whose label matches exactly, so this must agree across the
    /// cluster (see `ModelProfile::guess_from_name`).
    pub model_label: String,
    /// The deployment this node is part of. See `models/deployment.rs`.
    ///
    /// It is what allows the same model to be up twice, on different machines or
    /// configurations: whoever assembles the pipeline groups by this id, and
    /// no longer by equality of `model_label`, which did not tell the two apart.
    ///
    /// Empty on a node configured by the command line, which went through no
    /// deployment - there the grouping falls back to the label. It is also
    /// what a peer of an earlier version announces.
    #[serde(default)]
    pub deploy_id: String,
    /// Operational name of this deployment, separate from the model's technical
    /// label used by routing.
    #[serde(default)]
    pub deployment_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    /// How many peers the model is split across.
    pub peer_count: u32,
    /// 0-based position of this peer in that split.
    pub peer_index: u32,
    /// This peer is the entry point clients talk to.
    pub primary: bool,
    /// Tensor parallelism identity of this node (path C), when it is
    /// primary/worker of a group. `None` = node without TP (today's case).
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// The plugin whose inference service runs this whole model instead of
    /// llama.cpp (`RuntimeParams::inference_service`); `None` is llama.cpp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_service: Option<String>,
    /// That engine's settings (`RuntimeParams::engine_settings`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engine_settings: std::collections::BTreeMap<String, serde_json::Value>,
    /// The machine's part in a prefill/decode split (`RuntimeParams::inference_role`).
    #[serde(default, skip_serializing_if = "crate::webui::InferenceRole::is_both")]
    pub inference_role: crate::webui::InferenceRole,
    /// The prefill/decode split's threshold (`RuntimeParams::prefill_min_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_min_tokens: Option<u32>,

    pub inference_port: u16,
    pub n_gpu_layers: u32,
    pub n_ctx: u32,
    /// Keep the llama.cpp KV cache on the GPU. Disable to trade latency for
    /// host RAM capacity on large-context hardware.
    #[serde(default = "default_offload_kqv")]
    pub offload_kqv: bool,
    /// Precision used for hidden state crossing between inference peers.
    #[serde(default)]
    pub hidden_state_dtype: crate::wire::WireDtype,
    /// Numeric type of the K cache, defined by the whole installation.
    #[serde(default)]
    pub cache_type_k: crate::config::KvCacheType,
    /// Numeric type of the V cache, defined by the whole installation.
    #[serde(default)]
    pub cache_type_v: crate::config::KvCacheType,
    /// Directory for KV cache snapshots (`Config::slot_save_path`), injected
    /// from the installation in `apply_locked` as `cache_type_*`. `None` turns off the
    /// snapshot routes on this peer. It is not part of `weights_are_compatible`:
    /// changing the directory does not invalidate the loaded weights.
    #[serde(default)]
    pub slot_save_path: Option<String>,
    /// Host-memory KV cache setting in MiB (`Config::cache_ram_mib`, the
    /// `--cache-ram` of llama-server), injected from the installation like
    /// `slot_save_path`: `0` is off, a positive value is a ceiling, absent or
    /// negative lets the cache use what the user reserve allows. See
    /// `infer_server/ram_cache.rs`.
    #[serde(default)]
    pub cache_ram_mib: Option<i64>,
    /// Lets llama.cpp adjust layers/context to the free memory.
    #[serde(default = "default_fit_memory")]
    pub fit_memory: bool,
    /// Safety reserve that `fit` leaves free on each device.
    #[serde(default = "default_fit_memory_margin_mb")]
    pub fit_memory_margin_mb: u32,
    pub n_batch: u32,
    /// Physical micro-batch (`n_ubatch` of llama.cpp) while the memory fit
    /// is on; never larger than `n_batch`.
    #[serde(default = "default_n_ubatch")]
    pub n_ubatch: u32,
    /// Concurrent generations; each one gets the full configured context.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// Enable the in-process multi-sequence scheduler.
    #[serde(default = "default_batching_enabled")]
    pub batching_enabled: bool,
    /// Maximum number of requests waiting before admission returns overload.
    #[serde(default = "default_max_queue_requests")]
    pub max_queue_requests: u32,
    /// Time allowed for collecting a local microbatch.
    #[serde(default = "default_batch_window_ms")]
    pub batch_window_ms: u32,
    /// Maximum active sequences in one llama batch. Clamped to max_sessions.
    #[serde(default = "default_max_batch_sequences")]
    pub max_batch_sequences: u32,
    /// Maximum tokens in one llama batch. Clamped to n_batch.
    #[serde(default = "default_max_batch_tokens")]
    pub max_batch_tokens: u32,
    /// Prompt tokens pushed through the pipeline per round trip.
    #[serde(default = "default_prefill_chunk")]
    pub prefill_chunk_tokens: u32,
    /// Depth of the prefill pipeline: chunks kept in flight by the chain
    /// at the same time. The runtime caps the effective value at eight and at a byte
    /// budget of hidden state; eight is the default, which fills the pipeline in a
    /// cluster of few hops. One turns the overlap off.
    #[serde(default = "default_prefill_max_inflight_chunks")]
    pub prefill_max_inflight_chunks: u32,
    pub max_new_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
    pub repeat_last_n: u32,
    /// llama.cpp sampler penalties, subtracted from the logit of the tokens
    /// seen in the window: once per token (`presence`) and per occurrence
    /// (`frequency`). 0 turns them off.
    #[serde(default)]
    pub presence_penalty: f32,
    #[serde(default)]
    pub frequency_penalty: f32,
    /// Cuts candidates below this fraction of the most probable. 0 turns it off.
    #[serde(default)]
    pub min_p: f32,
    /// Sampler generator seed, applied when the deployment loads.
    /// The default is the fixed value POM has always used.
    #[serde(
        with = "llama_engine::llm::seed_text",
        default = "llama_engine::llm::seed_text::default_seed"
    )]
    pub seed: u64,
    /// llama.cpp DRY sampler: penalizes the token that would extend a sequence
    /// already seen. `dry_multiplier` 0 turns it off.
    #[serde(default)]
    pub dry_multiplier: f32,
    #[serde(default = "default_dry_base")]
    pub dry_base: f32,
    #[serde(default = "default_dry_allowed_length")]
    pub dry_allowed_length: u32,
    #[serde(default = "default_dry_penalty_last_n")]
    pub dry_penalty_last_n: u32,
    /// Token limit of a reasoning block (`--reasoning-budget` of
    /// llama.cpp): upon reaching it, the end tag is forced. -1 means no limit.
    #[serde(default = "default_reasoning_budget")]
    pub reasoning_budget: i32,
    pub no_thinking: bool,
    pub single_peer: bool,
    /// Disable embedding extraction in llama.cpp. Set on the last (sampling)
    /// peer of a multi-peer pipeline when `embeddings=True` breaks logits
    /// (observed with phi-4 where the model outputs EOS as first token).
    #[serde(default)]
    pub no_embeddings: bool,
    /// Operator kill-switch, independent of whether every peer in a plan
    /// negotiated `speculate/v1-lookup` — off by default so turning the
    /// feature on is opt-in even on an all-capable cluster.
    #[serde(default)]
    pub speculative_decoding_enabled: bool,
    /// Draft width: the confirmed seed token plus up to `speculative_k - 1`
    /// guessed continuation tokens, batched through one decode.
    #[serde(default = "default_speculative_k")]
    pub speculative_k: u32,
    /// How many trailing tokens of history define the pattern
    /// `find_draft` searches for a prior repeat of.
    #[serde(default = "default_speculative_lookup_window")]
    pub speculative_lookup_window: u32,
    /// Multi-token prediction by the model's own `nextn` block.
    ///
    /// **A switch separate from `speculative_decoding_enabled` on
    /// purpose**: they are two different mechanisms, and the one above
    /// (prompt-lookup) uses no model weight at all. See `crate::mtp` for
    /// the difference and `mtp_effective` for when it really takes effect.
    #[serde(default)]
    pub mtp_enabled: bool,
    /// How many tokens the `nextn` head drafts per round.
    #[serde(default = "default_mtp_draft_tokens")]
    pub mtp_draft_tokens: u32,
    /// Minimum confidence for the draft to continue. Zero accepts any
    /// guess (the llama.cpp default); above that the draft stops at the
    /// first doubtful token, instead of widening every hop's batch
    /// for a bet the verification will refuse.
    #[serde(default = "default_mtp_min_probability")]
    pub mtp_min_probability: f32,
    /// Allows loading the `nextn` weights and context into RAM when VRAM
    /// cannot hold the requested MTP. On by default to honor the deployment;
    /// the operator may turn the fallback off if the local measurement does not pay off.
    #[serde(default = "default_mtp_cpu_fallback_enabled")]
    pub mtp_cpu_fallback_enabled: bool,
    /// Where the MTP block lives: automatic, always in VRAM or always in RAM.
    #[serde(default)]
    pub mtp_placement: llama_engine::MtpPlacement,
    /// All the routed experts of a MoE stay in RAM and the CPU computes them
    /// (llama.cpp's `--cpu-moe`); VRAM keeps the rest of the model. It skips the
    /// memory fit, which refuses overrides set before it.
    #[serde(default)]
    pub moe_experts_in_ram: bool,
    /// GPU cache, in MiB, of the experts that stayed in RAM: the recently used ones
    /// are served from VRAM, only the misses go up over PCIe (batches of up to
    /// 32 tokens). `0` turns it off. With the memory fit on, the fit counts
    /// the cache and leaves fewer whole expert layers on the GPU.
    #[serde(default)]
    pub moe_cache_mib: u32,
    /// The gateway picks each request's prefill chunk (between 32 tokens and
    /// `prefill_chunk_tokens`) from a model of the route it learns. Off, every
    /// prompt goes in chunks of exactly `prefill_chunk_tokens`.
    #[serde(default = "default_adaptive_prefill_chunks")]
    pub adaptive_prefill_chunks: bool,
    /// Unloads the weights when this peer is idle.
    #[serde(default)]
    pub unload_if_idle: bool,
    /// The GPU arbiter may unload this model while no request is in flight,
    /// for an image component on the same GPU (`gpu_arbiter.rs`).
    #[serde(default)]
    pub yield_gpu_to_images: bool,
    /// Minutes without a request before entering `idle`.
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u32,
}

impl NodeConfig {
    /// A configuration with the **parameters** filled in and nothing else.
    ///
    /// The identity fields (path, label, range, position in the pipeline)
    /// come out blank on purpose: callers fill them in on top, with the
    /// struct update syntax. It exists so that the list of "what is a
    /// parameter" lives in **one place only** - before, it was written by hand in
    /// `run_apply`, and any new path that assembled a `NodeConfig` would have
    /// had to repeat the thirty lines without forgetting any.
    ///
    /// `allow_fit_memory` is false on a tensor parallelism node: llama.cpp's native
    /// `fit` does not know how to handle `split-mode tensor`, so there it stays
    /// off regardless of what the operator asked for.
    pub fn from_params(p: &crate::webui::RuntimeParams, allow_fit_memory: bool) -> Self {
        Self {
            deploy_id: String::new(),
            deployment_name: String::new(),
            model_path: String::new(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            peer_count: 1,
            peer_index: 0,
            primary: false,
            tp: None,
            inference_service: p.inference_service.clone(),
            engine_settings: p.engine_settings.clone(),
            inference_role: p.inference_role,
            prefill_min_tokens: p.prefill_min_tokens,
            inference_port: 0,
            single_peer: false,
            no_embeddings: p.no_embeddings,

            n_gpu_layers: p.n_gpu_layers,
            n_ctx: p.n_ctx,
            offload_kqv: p.offload_kqv,
            hidden_state_dtype: p.hidden_state_dtype,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            cache_ram_mib: None,
            fit_memory: p.fit_memory && allow_fit_memory,
            fit_memory_margin_mb: p.fit_memory_margin_mb,
            n_batch: p.n_batch,
            n_ubatch: p.n_ubatch,
            max_new_tokens: p.max_new_tokens,
            max_sessions: p.max_sessions,
            batching_enabled: p.batching_enabled,
            max_queue_requests: p.max_queue_requests,
            batch_window_ms: p.batch_window_ms,
            max_batch_sequences: p.max_batch_sequences,
            max_batch_tokens: p.max_batch_tokens,
            prefill_chunk_tokens: p.prefill_chunk_tokens,
            prefill_max_inflight_chunks: p.prefill_max_inflight_chunks,
            temperature: p.temperature,
            top_p: p.top_p,
            top_k: p.top_k,
            repeat_penalty: p.repeat_penalty,
            repeat_last_n: p.repeat_last_n,
            presence_penalty: p.presence_penalty,
            frequency_penalty: p.frequency_penalty,
            min_p: p.min_p,
            seed: p.seed,
            dry_multiplier: p.dry_multiplier,
            dry_base: p.dry_base,
            dry_allowed_length: p.dry_allowed_length,
            dry_penalty_last_n: p.dry_penalty_last_n,
            reasoning_budget: p.reasoning_budget,
            no_thinking: p.no_thinking,
            speculative_decoding_enabled: p.speculative_decoding_enabled,
            speculative_k: p.speculative_k,
            speculative_lookup_window: p.speculative_lookup_window,
            mtp_enabled: p.mtp_enabled,
            mtp_draft_tokens: p.mtp_draft_tokens,
            mtp_min_probability: p.mtp_min_probability,
            mtp_cpu_fallback_enabled: p.mtp_cpu_fallback_enabled,
            mtp_placement: p.mtp_placement,
            moe_experts_in_ram: p.moe_experts_in_ram,
            moe_cache_mib: p.moe_cache_mib,
            adaptive_prefill_chunks: p.adaptive_prefill_chunks,
            unload_if_idle: p.unload_if_idle,
            yield_gpu_to_images: p.yield_gpu_to_images,
            idle_minutes: p.idle_minutes,
        }
    }

    /// This same slice, with other parameters.
    ///
    /// **Nothing that identifies the slice changes**: path, label, layer
    /// range, position in the pipeline, TP group and port remain those of this
    /// node. This guarantee is what makes it safe to reconfigure a live
    /// deployment - the node reloads the same file with another `n_ctx`, and does not
    /// become another node of the pipeline midway.
    ///
    /// `no_embeddings` is an exception only in appearance: it is decided by the position
    /// in the pipeline (the tail does not export the residual), so it comes from what was already
    /// here, and not from what the operator typed.
    pub fn with_params(&self, p: &crate::webui::RuntimeParams) -> Self {
        let mut next = Self {
            deploy_id: self.deploy_id.clone(),
            deployment_name: self.deployment_name.clone(),
            model_path: self.model_path.clone(),
            mmproj_path: self.mmproj_path.clone(),
            mtp_draft_path: self.mtp_draft_path.clone(),
            model_label: self.model_label.clone(),
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            total_layers: self.total_layers,
            peer_count: self.peer_count,
            peer_index: self.peer_index,
            primary: self.primary,
            tp: self.tp.clone(),
            inference_port: self.inference_port,
            single_peer: self.single_peer,
            no_embeddings: self.no_embeddings,
            ..Self::from_params(p, self.tp.is_none())
        };
        next.cache_type_k = self.cache_type_k;
        next.cache_type_v = self.cache_type_v;
        next
    }
}

fn default_max_sessions() -> u32 {
    1
}
fn default_batching_enabled() -> bool {
    true
}
fn default_max_queue_requests() -> u32 {
    128
}
fn default_batch_window_ms() -> u32 {
    0
}
fn default_max_batch_sequences() -> u32 {
    u32::MAX
}
fn default_max_batch_tokens() -> u32 {
    u32::MAX
}
fn default_prefill_chunk() -> u32 {
    crate::prefill::DEFAULT_CHUNK_TOKENS
}
fn default_prefill_max_inflight_chunks() -> u32 {
    crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32
}
fn default_offload_kqv() -> bool {
    true
}

fn default_fit_memory() -> bool {
    true
}
fn default_fit_memory_margin_mb() -> u32 {
    DEFAULT_FIT_MEMORY_MARGIN_MB
}
fn default_speculative_k() -> u32 {
    4
}
fn default_speculative_lookup_window() -> u32 {
    3
}
/// The same default as `llama.cpp` (`common_params_speculative_draft::n_max`).
fn default_mtp_draft_tokens() -> u32 {
    3
}
/// Zero, like llama.cpp's `p_min`: no confidence cutoff until a
/// measurement says that cutting pays more than drafting.
fn default_mtp_min_probability() -> f32 {
    0.0
}
fn default_dry_base() -> f32 {
    1.75
}
fn default_dry_allowed_length() -> u32 {
    2
}
fn default_dry_penalty_last_n() -> u32 {
    1024
}
fn default_n_ubatch() -> u32 {
    llama_engine::DEFAULT_N_UBATCH
}
fn default_reasoning_budget() -> i32 {
    -1
}
fn default_adaptive_prefill_chunks() -> bool {
    true
}

fn default_mtp_cpu_fallback_enabled() -> bool {
    true
}
fn default_idle_minutes() -> u32 {
    60
}

impl NodeConfig {
    pub fn layers_count(&self) -> u32 {
        self.end_layer.saturating_sub(self.start_layer) + 1
    }

    /// Does this node really need to hand the hidden state to its successor?
    ///
    /// It is the **only** condition that justifies installing the
    /// `ggml_backend_sched_eval_callback` capture in `llm.rs`: the callback
    /// changes how the ggml scheduler groups the graph nodes and cost 7.8-8.2 ms
    /// per token in the 2026-08-12 measurement, against 0.14-0.67 ms on the hop that does not
    /// install it - same number of layers, and the one that does not install it still carries
    /// the LM head. The cost follows the position, not the machine.
    ///
    /// Erring on the low side does not corrupt silently: `LlmEngine::hidden_state`
    /// refuses with "residual capture did not fire" instead of returning the
    /// post-norm.
    pub fn needs_residual_export(&self) -> bool {
        if self.single_peer || self.no_embeddings {
            return false;
        }
        let serves_last_layer = self.end_layer + 1 >= self.total_layers;
        let last_in_split = self.peer_index + 1 >= self.peer_count;
        !serves_last_layer && !last_in_split
    }

    /// Is MTP worth it on **this** node?
    ///
    /// Two cases, and they are not to be confused:
    ///
    /// * **Whole model:** a single machine serves all the layers, including the
    ///   last one with the `nextn` block. The draft and the batch come out of the same head.
    /// * **Tail of a sliced pipeline:** the `nextn` block lives here but whoever
    ///   assembles the batch is the head, two different machines. Here only the draft
    ///   *engine* is turned on - draft production via `mtp_draft` and the
    ///   observation (`observe`) after `decode_hidden`. The gateway does the rest.
    ///
    /// Whether the model has a `nextn` block or not is **not** decided here: whoever
    /// knows that is the loaded GGUF, and `MtpDrafter::attach` returns `None`
    /// when it has none.
    pub fn mtp_effective(&self) -> bool {
        if !self.mtp_enabled {
            return false;
        }
        let serves_last_layer = self.end_layer + 1 >= self.total_layers;
        if !serves_last_layer {
            return false;
        }
        // Whole model.
        if self.start_layer == 0 && self.peer_count <= 1 {
            return true;
        }
        // Tail of a sliced pipeline - only the last layer has the block.
        if self.start_layer > 0 && self.peer_count > 1 {
            return true;
        }
        false
    }

    /// Weights may survive this configuration change when the slice and the
    /// way of loading remain the same. Context, KV, sessions, batch and
    /// sampling are outside this identity and are rebuilt on the server.
    fn weights_are_compatible(&self, other: &Self) -> bool {
        self.model_path == other.model_path
            && self.mmproj_path == other.mmproj_path
            && self.mtp_draft_path == other.mtp_draft_path
            && self.start_layer == other.start_layer
            && self.end_layer == other.end_layer
            && self.total_layers == other.total_layers
            && self.n_gpu_layers == other.n_gpu_layers
            && self.fit_memory == other.fit_memory
            && (!self.fit_memory || self.fit_memory_margin_mb == other.fit_memory_margin_mb)
            && self.mtp_effective() == other.mtp_effective()
            && self.tp == other.tp
    }

    fn validate(&self) -> Result<()> {
        if self.model_path.trim().is_empty() {
            bail!("model_path is required");
        }
        if !PathBuf::from(&self.model_path).exists() {
            bail!("file not found: {}", self.model_path);
        }
        if let Some(mmproj_path) = self.mmproj_path.as_deref() {
            if !PathBuf::from(mmproj_path).is_file() {
                bail!("mmproj file not found: {mmproj_path}");
            }
            crate::catalog::validate_mmproj_pair(
                std::path::Path::new(&self.model_path),
                std::path::Path::new(mmproj_path),
            )?;
        }
        if self.start_layer > self.end_layer {
            bail!(
                "invalid layer range: {}-{}",
                self.start_layer,
                self.end_layer
            );
        }
        if self.total_layers == 0 {
            bail!("total_layers cannot be zero");
        }
        if self.n_ctx == 0 {
            bail!("n_ctx must be at least 1");
        }
        if let Some(limit) = model_context_limit(&self.model_path) {
            if self.n_ctx > limit {
                bail!("n_ctx {} exceeds the {limit} tokens the model declares (context_length)", self.n_ctx);
            }
        }
        total_context_tokens(self.n_ctx, self.max_sessions)?;
        if self.end_layer >= self.total_layers {
            bail!(
                "final layer {} outside the model of {} layers",
                self.end_layer,
                self.total_layers
            );
        }
        if self.peer_count == 0 {
            bail!("peer_count must be >= 1");
        }
        if self.peer_index >= self.peer_count {
            bail!(
                "peer_index {} outside a split of {} peers",
                self.peer_index,
                self.peer_count
            );
        }
        Ok(())
    }

    /// The assignment this configuration implies, for gossip and the TUI.
    pub fn to_assignment(&self, profile: &ModelProfile) -> LayerAssignment {
        let count = self.layers_count();
        LayerAssignment {
            model: self.model_label.clone(),
            deploy_id: self.deploy_id.clone(),
            total_layers: self.total_layers,
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            layers_count: count,
            memory_used_mb: count as f64 * profile.memory_per_layer_mb + profile.overhead_mb,
            memory_allocated_mb: 0.0,
        }
    }
}

// ── Status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    /// No inference server configured on this node.
    Disabled,
    /// Process spawned, model still loading.
    Starting,
    /// `/health` answered and the model is loaded.
    Ready,
    /// Model unloaded for inactivity; the deployment remains announced.
    Idle,
    /// Crashed past the restart budget.
    Failed,
    /// Model was unloaded because a GPU exceeded the thermal limit.
    Thermal,
}

/// What `NodeRuntime::wake_from_idle` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The model is loaded.
    Ready,
    /// A load was already running.
    Loading,
    /// This call started the load.
    Started,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub state: ServerState,
    pub detail: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub log_path: Option<String>,
    /// Layer count read back from the shard's GGUF metadata via `/health`.
    pub reported_total_layers: Option<u32>,
    /// How many layers llama.cpp **actually** put on the GPU, read from the same
    /// `/health`, at the same instant and for the same reason as the line above: it is the
    /// engine reporting what happened, not what was requested.
    ///
    /// The distinction is the whole value of the field. `n_gpu_layers` of the config is the
    /// request, and the request is usually the sentinel 99 ("offload everything"); with
    /// `fit_memory`, llama.cpp reduces this number to fit in VRAM and this is where
    /// the chosen number shows up. Reading the request to say where the layers
    /// are answers "all on the GPU" precisely when some are not.
    ///
    /// Negative means "all" in the llama.cpp convention; whoever reads it must
    /// treat it as the whole range, not as zero.
    pub reported_gpu_layers: Option<i32>,
    pub changed_at: String,
}

impl RuntimeStatus {
    fn new(state: ServerState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            pid: None,
            restarts: 0,
            log_path: None,
            reported_total_layers: None,
            reported_gpu_layers: None,
            changed_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// ── Runtime ──────────────────────────────────────────────────────────────────

pub struct NodeRuntime {
    config: RwLock<Option<NodeConfig>>,
    /// Behind its own Arc so supervisor tasks, which outlive any `&self`
    /// borrow, can keep writing to it.
    status: Arc<RwLock<RuntimeStatus>>,
    assignment: Arc<RwLock<Option<LayerAssignment>>>,
    net: Arc<Network>,
    /// Bumped on every `apply`; the supervisor of an older generation exits.
    generation: watch::Sender<u64>,
    /// Certificate the inference server presents, when there is one.
    /// `None` in tests, which come up on `127.0.0.1` and speak in clear text.
    tls: Option<crate::serve::TlsServer>,
    /// HTTP/3 endpoint of the inference. It does not depend on the TLS policy over TCP.
    quic: Option<crate::serve::QuicServerConfig>,
    /// Dedicated UDP port announced for the inference HTTP/3 endpoint.
    quic_port: Option<u16>,
    /// Live configuration used to authorize internal inference calls.
    auth: Option<crate::auth::AuthState>,
    /// Owner of the last set of loaded weights. Each generation's server
    /// keeps its own reference until it drains the in-flight requests.
    model_cache: Arc<crate::llm::ModelCache>,
    /// Counter shared by the gateway and the local inference server.
    /// Without the same instance, the panel would only see the zero of the gateway's tracker
    /// while the requests ran on the inference port.
    loads: Arc<crate::pipeline::LoadTracker>,
    request_activity: Arc<crate::reqlog::RequestActivity>,
    status_changed: Arc<Notify>,
    /// Serializes apply, manual unload and thermal unload so an operator action
    /// cannot race a temperature transition.
    operation: Mutex<()>,
    /// Keeps the previous supervisor joinable until its inference server has
    /// released the listener and native model resources. A reload must not
    /// start a second generation while the first one is still unwinding.
    supervisor: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    thermal: Arc<Mutex<ThermalRuntime>>,
    /// Where this node keeps `<config>.last-slice.json`, fixed when the runtime
    /// is built. Read from the process environment on every write, a runtime
    /// followed a later change of `POM_CONFIG`: in the suite, one test's stop
    /// erased the annotation another test was watching.
    last_slice_path: std::path::PathBuf,
}

struct ThermalRuntime {
    blocked: bool,
    recovery: Option<NodeConfig>,
}

impl NodeRuntime {
    pub fn new(net: Arc<Network>, assignment: Arc<RwLock<Option<LayerAssignment>>>) -> Arc<Self> {
        Self::new_with_tls(net, assignment, None)
    }

    /// Like `new`, but saying which certificate the inference server
    /// presents. It is the path `main.rs` uses; `new` is left for the tests.
    pub fn new_with_tls(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        tls: Option<crate::serve::TlsServer>,
    ) -> Arc<Self> {
        Self::new_with_tls_and_quic_and_auth(net, assignment, tls, None, None)
    }

    pub fn new_with_tls_and_auth(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        tls: Option<crate::serve::TlsServer>,
        auth: Option<crate::auth::AuthState>,
    ) -> Arc<Self> {
        Self::new_with_tls_and_quic_and_auth(net, assignment, tls, None, auth)
    }

    pub fn new_with_tls_and_quic_and_auth(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        tls: Option<crate::serve::TlsServer>,
        quic: Option<crate::serve::QuicServerConfig>,
        auth: Option<crate::auth::AuthState>,
    ) -> Arc<Self> {
        Self::build(net, assignment, tls, quic, auth, crate::lastslice::path())
    }

    /// A runtime whose annotation lives at `last_slice_path`, so a test bench
    /// never touches the annotation beside the real configuration.
    #[cfg(test)]
    pub(crate) fn new_with_last_slice_path(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        last_slice_path: std::path::PathBuf,
    ) -> Arc<Self> {
        Self::build(net, assignment, None, None, None, last_slice_path)
    }

    fn build(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        tls: Option<crate::serve::TlsServer>,
        quic: Option<crate::serve::QuicServerConfig>,
        auth: Option<crate::auth::AuthState>,
        last_slice_path: std::path::PathBuf,
    ) -> Arc<Self> {
        let (generation, _) = watch::channel(0u64);
        let quic_port = net.inference_quic_port;
        Arc::new(Self {
            config: RwLock::new(None),
            status: Arc::new(RwLock::new(RuntimeStatus::new(
                ServerState::Disabled,
                "no model loaded",
            ))),
            assignment,
            net,
            generation,
            tls,
            quic,
            quic_port,
            auth,
            model_cache: Arc::new(crate::llm::ModelCache::new()),
            loads: Arc::new(crate::pipeline::LoadTracker::new()),
            request_activity: crate::reqlog::RequestActivity::new(),
            status_changed: Arc::new(Notify::new()),
            operation: Mutex::new(()),
            supervisor: std::sync::Mutex::new(None),
            thermal: Arc::new(Mutex::new(ThermalRuntime {
                blocked: false,
                recovery: None,
            })),
            last_slice_path,
        })
    }

    /// Tracker shared with this machine's gateway and with the current generation's
    /// inference server.
    pub fn load_tracker(&self) -> Arc<crate::pipeline::LoadTracker> {
        self.loads.clone()
    }

    fn take_supervisor(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.supervisor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    async fn wait_for_supervisor(
        supervisor: Option<tokio::task::JoinHandle<()>>,
    ) -> (Option<tokio::task::JoinHandle<()>>, Result<()>) {
        Self::wait_for_supervisor_with_timeout(supervisor, SUPERVISOR_SHUTDOWN_DEADLINE).await
    }

    async fn wait_for_supervisor_with_timeout(
        supervisor: Option<tokio::task::JoinHandle<()>>,
        deadline: Duration,
    ) -> (Option<tokio::task::JoinHandle<()>>, Result<()>) {
        let Some(mut supervisor) = supervisor else {
            return (None, Ok(()));
        };

        match tokio::time::timeout(deadline, &mut supervisor).await {
            Ok(Ok(())) => (None, Ok(())),
            Ok(Err(error)) => {
                warn!("previous supervisor ended unexpectedly: {error}");
                (None, Ok(()))
            }
            Err(_) => (
                Some(supervisor),
                Err(anyhow::anyhow!(
                    "previous supervisor did not stop in {deadline:?}"
                )),
            ),
        }
    }

    /// Atomic activity marker used by the out-of-band idle evaluator.
    pub fn note_request(&self) {
        self.request_activity.mark_now();
    }

    /// Wait for the health watcher to prove that a reload is ready. Notify is
    /// used instead of polling so the first request after idle never adds a
    /// timer or a sleep to inference.
    pub async fn wait_until_ready(&self) -> Result<()> {
        loop {
            let notified = self.status_changed.notified();
            match self.status.read().await.state {
                ServerState::Ready => return Ok(()),
                ServerState::Failed => bail!("the model could not reload"),
                ServerState::Thermal => bail!("the model is waiting for cooldown"),
                ServerState::Disabled => bail!("the deployment is not configured"),
                ServerState::Starting | ServerState::Idle => notified.await,
            }
        }
    }

    /// Start loading an idle deployment and return at once, without waiting
    /// for the model. Idempotent: a peer already loading or ready is left
    /// alone, and the operation lock keeps concurrent wake-ups from starting
    /// two loads.
    ///
    /// This is what a remote gateway calls. Holding one HTTP call open for the
    /// whole load made the wake depend on a connection surviving minutes of
    /// silence, on every transport; the gateway asks again instead until the
    /// answer is `Ready`.
    pub async fn wake_from_idle(&self) -> Result<Wake> {
        let _operation = self.operation.lock().await;
        let state = self.status.read().await.state;
        match state {
            ServerState::Idle => {
                let Some(cfg) = self.config.read().await.clone() else {
                    bail!("idle deployment without configuration");
                };
                self.note_request();
                info!(
                    "⏰ waking the idle deployment: {} layers {}-{}",
                    cfg.model_label, cfg.start_layer, cfg.end_layer
                );
                // An image component on the same GPU may hold its VRAM: ask
                // it to free the card before the model loads again.
                crate::gpu_arbiter::before_llm_reload().await;
                // Keep the operation lock while transitioning Idle ->
                // Starting so concurrent requests cannot duplicate a load.
                self.apply_locked(cfg, true, true).await?;
                Ok(Wake::Started)
            }
            ServerState::Ready => Ok(Wake::Ready),
            ServerState::Starting => {
                // A request is waiting for this load: it must not look idle
                // again to the evaluator the moment it becomes ready.
                self.note_request();
                Ok(Wake::Loading)
            }
            ServerState::Failed => bail!(
                "the model could not reload: {}",
                self.status.read().await.detail
            ),
            ServerState::Thermal => bail!("the model is waiting for cooldown"),
            ServerState::Disabled => bail!("the deployment is not configured"),
        }
    }

    /// Ensure this peer is ready for a planned request. Idle peers are loaded;
    /// peers already starting are awaited; ready peers return immediately.
    /// Returns whether this call started the load. Used for the local node,
    /// where waiting costs no connection (`wait_until_ready` is a `Notify`).
    pub async fn reload_from_idle(&self) -> Result<bool> {
        let wake = self.wake_from_idle().await?;
        self.wait_until_ready().await?;
        Ok(wake == Wake::Started)
    }

    /// Evaluate the per-peer policy from the existing monitor. This method is
    /// intentionally not called by a request handler.
    async fn evaluate_idle(&self) {
        let Some(cfg) = self.config.read().await.clone() else {
            return;
        };
        if !cfg.unload_if_idle
            || cfg.idle_minutes == 0
            || self.status.read().await.state != ServerState::Ready
            || !self.request_activity.is_idle(cfg.idle_minutes)
        {
            return;
        }
        self.unload_for_idle().await;
    }

    async fn unload_for_idle(&self) {
        let _operation = self.operation.lock().await;
        let Some(cfg) = self.config.read().await.clone() else {
            return;
        };
        if !cfg.unload_if_idle
            || cfg.idle_minutes == 0
            || !self.request_activity.is_idle(cfg.idle_minutes)
            || self.loads.load(&self.net.node_id).await != 0
            || self.status.read().await.state != ServerState::Ready
        {
            return;
        }
        let Some(assignment) = self.assignment.read().await.clone() else {
            return;
        };
        let detail = format!("model unloaded after {} min without requests", cfg.idle_minutes);
        self.unload_locked(&cfg, &assignment, detail).await;
    }

    /// Lend the GPU to an image component on the same card (`gpu_arbiter.rs`):
    /// the idle unload, without waiting for the idle timer, only when the
    /// deployment allows it (`yield_gpu_to_images`) and no request is in
    /// flight. The next request reloads it as after an idle unload.
    pub async fn yield_for_images(&self) -> bool {
        let _operation = self.operation.lock().await;
        let Some(cfg) = self.config.read().await.clone() else {
            return false;
        };
        if !cfg.yield_gpu_to_images
            || self.loads.load(&self.net.node_id).await != 0
            || self.status.read().await.state != ServerState::Ready
        {
            return false;
        }
        let Some(assignment) = self.assignment.read().await.clone() else {
            return false;
        };
        self.unload_locked(&cfg, &assignment, "model lent its GPU to an image".to_owned())
            .await;
        true
    }

    /// The unload itself, under the operation lock: the model leaves memory,
    /// the peer keeps announcing and enters `idle`.
    async fn unload_locked(&self, cfg: &NodeConfig, assignment: &LayerAssignment, detail: String) {
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);
        self.model_cache.clear();
        self.net.set_batching_ready(false);
        self.net.set_slot_persist_ready(false);
        self.net.set_runtime_generation(generation);
        self.net.set_idle(true);
        *self.status.write().await = RuntimeStatus::new(
            ServerState::Idle,
            detail.clone(),
        );
        self.status_changed.notify_waiters();
        info!("💤 {detail}; the next request reloads it");
        if let Err(error) = crate::lastslice::save_at(
            &self.last_slice_path,
            cfg,
            crate::lastslice::SliceState::Idle,
        ) {
            warn!("could not record the idle slice: {error}");
        }
        if let Err(error) = self.net.announce(Some(assignment)).await {
            warn!("announce after the idle unload failed: {error}");
        }
    }

    /// Restore an idle deployment without loading its model during process
    /// startup. The first planned request explicitly wakes it.
    pub async fn restore_idle(&self, cfg: NodeConfig) {
        let profile = ModelProfile::guess_from_name(&cfg.model_path);
        let assignment = cfg.to_assignment(&profile);
        *self.config.write().await = Some(cfg.clone());
        *self.assignment.write().await = Some(assignment.clone());
        self.net.set_idle(true);
        self.net.set_batching_ready(false);
        self.net.set_slot_persist_ready(false);
        *self.status.write().await = RuntimeStatus::new(
            ServerState::Idle,
            "deployment restored idle; waiting for a request",
        );
        self.status_changed.notify_waiters();
        let _ = self.net.announce(Some(&assignment)).await;
    }

    pub async fn config(&self) -> Option<NodeConfig> {
        self.config.read().await.clone()
    }

    /// Puts a configuration in place without bringing up any inference server.
    ///
    /// `apply` is the real path and loads a model; a test that only
    /// needs the gateway to read `speculative_decoding_enabled` has no GGUF
    /// to give it.
    #[cfg(test)]
    pub(crate) async fn seed_config_for_test(&self, cfg: NodeConfig) {
        *self.config.write().await = Some(cfg);
    }

    pub async fn status(&self) -> RuntimeStatus {
        self.status.read().await.clone()
    }

    /// Label the pipeline planner matches on; falls back to the CLI default.
    pub async fn model_label(&self, fallback: &str) -> String {
        self.config
            .read()
            .await
            .as_ref()
            .map(|c| c.model_label.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Start the local thermal monitor before restoring or applying a model.
    /// The initial sample closes the race where a hot GPU would otherwise load
    /// a model before the first periodic tick notices it.
    pub async fn start_thermal_monitor(self: &Arc<Self>, auth: crate::auth::AuthState) {
        let policy = auth.read().await.thermal;
        let mut machine = ThermalMachine::new(policy);
        let initial = crate::sysmon::sample().await;
        if let ThermalAction::Suspend { observed_c } = machine.observe(
            &initial
                .gpus
                .iter()
                .map(|gpu| gpu.temperature_c)
                .collect::<Vec<_>>(),
            std::time::Instant::now(),
        ) {
            warn!(
                temperature_c = observed_c,
                limit_c = policy.limit_c,
                "GPU above the limit before loading; new deployments wait for cooldown"
            );
            self.thermal.lock().await.blocked = true;
        }

        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                runtime.evaluate_idle().await;
                let policy = auth.read().await.thermal;
                machine.set_policy(policy);
                let system = crate::sysmon::sample().await;
                let readings: Vec<_> = system.gpus.iter().map(|gpu| gpu.temperature_c).collect();
                match machine.observe(&readings, std::time::Instant::now()) {
                    ThermalAction::None => {}
                    ThermalAction::Suspend { observed_c } => {
                        runtime.suspend_for_thermal(observed_c, policy).await;
                    }
                    ThermalAction::Resume { observed_c } => {
                        if let Err(error) = runtime.resume_from_thermal(observed_c, policy).await {
                            warn!("could not redeploy after cooldown: {error:#}");
                        }
                    }
                }
            }
        });
    }

    /// Stop the current inference server and announce to peers that this
    /// node no longer serves any layers. Called before a cluster reconfiguration
    /// so no peer sends inference requests to a stale endpoint.
    pub async fn stop(&self) {
        let _operation = self.operation.lock().await;
        info!("stopping the inference server on this node");

        let previous_supervisor = self.take_supervisor();
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        let (pending_supervisor, wait_result) =
            Self::wait_for_supervisor(previous_supervisor).await;
        // A native load (llama.cpp reading the weights) does not look at the generation while it runs and can
        // take minutes: the supervisor cannot end in the deadline. The stop still happens - giving up here left
        // the configuration, the announcement and the pending journal in place, so an undeploy during a load did
        // nothing, and a load that then crashed the process came back on the next start as a "pending
        // deployment". The supervisor is kept so the next apply waits for it, and it releases the model the
        // moment the load returns (`infer_server::run` checks the generation right after loading).
        let still_loading = pending_supervisor.is_some();
        if let Some(supervisor) = pending_supervisor {
            *self
                .supervisor
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(supervisor);
        }
        if let Err(error) = wait_result {
            warn!(
                "the inference server is still loading ({error}); this node leaves the deployment now and \
                 releases the model when the load returns"
            );
        }

        *self.config.write().await = None;
        *self.assignment.write().await = None;
        self.model_cache.clear();
        *self.status.write().await = RuntimeStatus::new(
            ServerState::Disabled,
            if still_loading {
                "stopped; the model being loaded is released when its load returns"
            } else {
                "stopped for reconfiguration"
            },
        );
        self.status_changed.notify_waiters();
        self.net.set_idle(false);
        self.net.set_batching_ready(false);
        self.net.set_slot_persist_ready(false);
        self.net.set_external_engine(None);
        self.net.set_external_engine_role(None);
        self.net.set_runtime_generation(generation);

        // Stopping is the decision of whoever operated. Forgetting the annotation is what keeps the
        // node from coming back serving on the next start and undoing that decision.
        crate::lastslice::clear_at(&self.last_slice_path);

        self.thermal.lock().await.recovery = None;

        let _ = self.net.announce(None).await;
    }

    /// Switch this node to a new configuration: stop the current inference
    /// server, re-announce the new layer range, start the new server.
    pub async fn apply(&self, cfg: NodeConfig) -> Result<()> {
        let _operation = self.operation.lock().await;
        self.apply_locked(cfg, true, false).await
    }

    /// Resumes a slice that the journal marked as pending after a crash.
    /// The caller has already incremented the attempt on disk; this path cannot
    /// reset it to 1 before starting the native server.
    pub async fn apply_after_restart(&self, cfg: NodeConfig) -> Result<()> {
        let _operation = self.operation.lock().await;
        self.apply_locked(cfg, false, false).await
    }

    /// Stops the current generation and starts the server for `cfg`.
    ///
    /// `waking` is the reload of an idle deployment: the node keeps announcing
    /// `idle` while the model loads, so a gateway that plans in the meantime
    /// waits for it (`reload_model`) instead of probing a server that is not
    /// up yet and dropping the peer from the plan. The health watcher clears
    /// the flag when the model answers.
    async fn apply_locked(
        &self,
        mut cfg: NodeConfig,
        persist_attempt: bool,
        waking: bool,
    ) -> Result<()> {
        if let Some(auth) = &self.auth {
            let installation = auth.read().await;
            cfg.cache_type_k = installation.cache_type_k;
            cfg.cache_type_v = installation.cache_type_v;
            // Precedence: what already came in `cfg` wins; otherwise
            // `POM_SLOT_SAVE_PATH`, then the `pom.json` field, then the
            // `~/.pom/kv-slots` default (`slot_save_path_effective`). A
            // deployment from the UI arrives here with `None` and adopts the
            // installation's value.
            cfg.slot_save_path = cfg
                .slot_save_path
                .take()
                .or_else(|| installation.slot_save_path_effective());
            cfg.cache_ram_mib = cfg
                .cache_ram_mib
                .take()
                .or_else(|| installation.cache_ram_mib_effective());
        }
        cfg.validate()?;

        let blocked = self.thermal.lock().await.blocked;
        if blocked {
            self.queue_thermal_recovery(cfg).await;
            return Ok(());
        }
        self.thermal.lock().await.recovery = None;

        if persist_attempt {
            let attempt = crate::lastslice::begin_new_attempt_at(&self.last_slice_path, &cfg)?;
            info!(attempt, "deployment persisted before starting the server");
        }

        let profile = ModelProfile::guess_from_name(&cfg.model_path);
        let assignment = cfg.to_assignment(&profile);

        info!(
            "⚙️  Reconfiguring node: {} layers {}-{}/{} (peer {}/{})",
            cfg.model_label,
            cfg.start_layer,
            cfg.end_layer,
            cfg.total_layers,
            cfg.peer_index + 1,
            cfg.peer_count
        );

        let reuse_weights = self
            .config
            .read()
            .await
            .as_ref()
            .is_some_and(|current| current.weights_are_compatible(&cfg));
        if !reuse_weights {
            // A path can be reused for a different slice, or its loader
            // parameters can change. Drop the cache entry before publishing
            // the new generation so a failed load cannot leave stale weights
            // eligible for a later retry.
            self.model_cache.clear();
        }

        let previous_supervisor = self.take_supervisor();

        // Bumping the generation stops the previous supervisor, which kills its
        // child on the way out. Join it before starting another generation so
        // the old server has completed native cleanup and released its port.
        //
        // `send_replace`, not `send`: with no live receivers — the case on the
        // very first apply, before any supervisor exists — `send` reports an
        // error and leaves the stored value untouched, so the supervisor we are
        // about to spawn would read a stale generation and exit immediately.
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        let (pending_supervisor, wait_result) =
            Self::wait_for_supervisor(previous_supervisor).await;
        if let Some(supervisor) = pending_supervisor {
            *self
                .supervisor
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(supervisor);
        }
        wait_result?;
        info!("previous supervisor stopped; preparing a new generation");

        *self.config.write().await = Some(cfg.clone());
        *self.assignment.write().await = Some(assignment.clone());
        *self.status.write().await =
            RuntimeStatus::new(ServerState::Starting, "starting the inference server");
        self.status_changed.notify_waiters();
        self.request_activity.mark_now();
        self.net.set_idle(waking);
        self.net.set_external_engine(cfg.inference_service.clone());
        // a prefill/decode role is a whole-model machine's: a stage of a split model always does both
        self.net.set_external_engine_role(
            (cfg.inference_service.is_some() && !crate::plugin_stage_link::is_stage(&cfg))
                .then_some(cfg.inference_role)
                .filter(|role| !role.is_both()),
        );
        // The new configuration only becomes effective capacity after the
        // new process's `/health` answers `model_loaded`.
        self.net.set_runtime_generation(generation);

        // Let peers know immediately instead of waiting for the next gossip
        // tick. Discovery is best effort, so a dead hostname or a stalled
        // socket must not keep the local reload from starting the server.
        match tokio::time::timeout(ANNOUNCE_DEADLINE, self.net.announce(Some(&assignment))).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!("announce after the reconfiguration failed: {error}"),
            Err(_) => warn!(
                "announce after the reconfiguration exceeded {ANNOUNCE_DEADLINE:?}; continuing locally"
            ),
        }

        info!("announce of the new generation completed; starting the supervisor");
        self.spawn_supervisor(cfg, generation, reuse_weights);
        Ok(())
    }

    async fn queue_thermal_recovery(&self, cfg: NodeConfig) {
        let policy = if let Some(auth) = &self.auth {
            auth.read().await.thermal
        } else {
            ThermalPolicy::default()
        };
        let current = self.config.read().await.clone();
        let port = current
            .as_ref()
            .map(|current| current.inference_port)
            .unwrap_or(cfg.inference_port);
        {
            let mut thermal = self.thermal.lock().await;
            thermal.recovery = Some(cfg.clone());
        }
        if current.is_some() {
            self.clear_active_for_thermal(&cfg, None, policy).await;
        } else {
            self.set_thermal_status(&cfg, policy).await;
        }
        append_log(
            port,
            &format!(
                "model waiting for cooldown: limit {:.1} °C, resume below {:.1} °C",
                policy.limit_c,
                policy.resume_limit_c()
            ),
        );
    }

    async fn suspend_for_thermal(&self, observed_c: f64, policy: ThermalPolicy) {
        let _operation = self.operation.lock().await;
        let current = self.config.read().await.clone();
        self.thermal.lock().await.blocked = true;
        let Some(current) = current else {
            return;
        };
        {
            let mut thermal = self.thermal.lock().await;
            if thermal.recovery.is_none() {
                thermal.recovery = Some(current.clone());
            }
        }
        warn!(
            temperature_c = observed_c,
            limit_c = policy.limit_c,
            resume_below_c = policy.resume_limit_c(),
            cooldown_secs = policy.cooldown_secs,
            "undeploying the model because of the GPU temperature"
        );
        append_log(
            current.inference_port,
            &format!(
                "undeployed because of temperature: {:.1} °C (limit {:.1} °C; resume below {:.1} °C for {}s)",
                observed_c,
                policy.limit_c,
                policy.resume_limit_c(),
                policy.cooldown_secs
            ),
        );
        self.clear_active_for_thermal(&current, Some(observed_c), policy)
            .await;
    }

    async fn resume_from_thermal(&self, observed_c: f64, policy: ThermalPolicy) -> Result<()> {
        let _operation = self.operation.lock().await;
        let recovery = {
            let mut thermal = self.thermal.lock().await;
            thermal.blocked = false;
            thermal.recovery.take()
        };
        let Some(cfg) = recovery else {
            return Ok(());
        };
        info!(
            temperature_c = observed_c,
            limit_c = policy.limit_c,
            resume_below_c = policy.resume_limit_c(),
            cooldown_secs = policy.cooldown_secs,
            "redeploying the model after the GPU cooldown"
        );
        append_log(
            cfg.inference_port,
            &format!(
                "redeploying after cooldown: {:.1} °C (limit {:.1} °C; resume below {:.1} °C for {}s)",
                observed_c,
                policy.limit_c,
                policy.resume_limit_c(),
                policy.cooldown_secs
            ),
        );
        self.apply_locked(cfg, true, false).await
    }

    async fn clear_active_for_thermal(
        &self,
        cfg: &NodeConfig,
        observed_c: Option<f64>,
        policy: ThermalPolicy,
    ) {
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);
        *self.config.write().await = None;
        *self.assignment.write().await = None;
        self.model_cache.clear();
        let reason = observed_c.map_or_else(
            || {
                format!(
                    "model waiting to cool below {:.1} °C for {}s (limit {:.1} °C)",
                    policy.resume_limit_c(),
                    policy.cooldown_secs,
                    policy.limit_c
                )
            },
            |temperature| {
                format!(
                    "model undeployed because of the GPU temperature ({temperature:.1} °C; limit {:.1} °C); waiting to cool below {:.1} °C for {}s",
                    policy.limit_c,
                    policy.resume_limit_c(),
                    policy.cooldown_secs
                )
            },
        );
        *self.status.write().await = RuntimeStatus::new(ServerState::Thermal, reason);
        self.net.set_batching_ready(false);
        self.net.set_slot_persist_ready(false);
        self.net.set_runtime_generation(generation);
        if let Err(error) = crate::lastslice::save_at(
            &self.last_slice_path,
            cfg,
            crate::lastslice::SliceState::Serving,
        ) {
            warn!("could not preserve the slice for thermal recovery: {error}");
        }
        let _ = self.net.announce(None).await;
    }

    async fn set_thermal_status(&self, cfg: &NodeConfig, policy: ThermalPolicy) {
        *self.status.write().await = RuntimeStatus::new(
            ServerState::Thermal,
            format!(
                "model waiting to cool below {:.1} °C for {}s (limit {:.1} °C)",
                policy.resume_limit_c(),
                policy.cooldown_secs,
                policy.limit_c
            ),
        );
        append_log(
            cfg.inference_port,
            &format!(
                "model waiting for cooldown: limit {:.1} °C, resume below {:.1} °C",
                policy.limit_c,
                policy.resume_limit_c()
            ),
        );
        self.net.set_runtime_generation(*self.generation.borrow());
        let _ = self.net.announce(None).await;
    }

    /// Records that the annotated slice did not come back because it never managed to load.
    ///
    /// The start that refuses to resume it has to say so: the panel of an
    /// empty and silent node is indistinguishable from that of a machine that never received
    /// any slice, and this is the machine whose last attempt brought the
    /// process down. It is what the operator needs to read to know they have to give
    /// it fewer layers.
    pub async fn note_unloadable_slice(&self, cfg: &NodeConfig, attempts: u32) {
        let detail = format!(
            "slice {} {}-{} never loaded: it failed {} consecutive attempts before /health and was blocked; undeploy or apply again manually",
            cfg.model_label, cfg.start_layer, cfg.end_layer, attempts
        );
        warn!("⚠️  {detail}");
        *self.status.write().await = RuntimeStatus::new(ServerState::Disabled, detail);
    }

    /// Start (or restart) the Rust inference server for `generation`.
    fn spawn_supervisor(&self, cfg: NodeConfig, generation: u64, reuse_weights: bool) {
        let gen_rx = self.generation.subscribe();
        let status = StatusWriter {
            status: self.status_handle(),
            changed: self.status_changed.clone(),
        };
        let port = cfg.inference_port;
        let cfg_for_health = cfg.clone();
        // Only the engine knows whether the GGUF carries the `nextn` block; the
        // announcement reads from here.
        // See `Network::mtp_ready`.
        let mtp_ready = self.net.mtp_ready_handle();
        let batching_ready = self.net.batching_ready_handle();
        // `slot_save_path` is static machine config: it is already known here whether
        // this peer serves `POST /v1/session/slot`. See `Network::slot_persist_ready`.
        self.net.set_slot_persist_ready(
            cfg.slot_save_path
                .as_deref()
                .map(str::trim)
                .is_some_and(|dir| !dir.is_empty()),
        );
        let capacity_net = self.net.clone();
        let auth = self.auth.clone();
        let model_cache = self.model_cache.clone();
        let loads = self.loads.clone();
        let request_activity = self.request_activity.clone();
        let load_node_id = self.net.node_id.clone();
        // The inference port only receives peers: no redirecting
        // to https whoever arrives in clear text, because there is no browser here.
        let tls = self.tls.as_ref().map(|t| t.peers_only());
        let quic = self.quic.clone();
        let quic_port = self.quic_port;
        let last_slice_path = self.last_slice_path.clone();

        let supervisor = tokio::spawn(async move {
            status.set_log(&log_file_path(port).to_string_lossy()).await;
            append_log(port, "supervisor started");
            let mut restarts = 0u32;
            loop {
                if *gen_rx.borrow() != generation {
                    return; // superseded by a newer configuration
                }
                if crate::lastslice::auto_resume_disabled_at(&last_slice_path) {
                    let detail =
                        "automatic resume blocked by undeploy; waiting for a manual deployment";
                    append_log(port, detail);
                    status
                        .set(ServerState::Disabled, detail.to_string(), restarts)
                        .await;
                    return;
                }
                // On each attempt, not only before the loop: whoever holds the port
                // may have appeared later, or may not have died on the first
                // strike. Clearing only once made the other nine attempts
                // hit the same occupied port.
                let held_port = free_port(port).await.err();
                if restarts > 0 {
                    warn!(
                        "restarting the inference server (attempt {}/{})",
                        restarts, MAX_RESTARTS
                    );
                    // A new deployment does not wait out the pause.
                    let mut superseded = gen_rx.clone();
                    tokio::select! {
                        _ = sleep(RESTART_INTERVAL) => {}
                        _ = superseded.changed() => {}
                    }
                    if *gen_rx.borrow() != generation {
                        return;
                    }
                    let Ok(attempt) = crate::lastslice::resume_attempt_at(&last_slice_path, &cfg)
                    else {
                        let detail = "could not persist the deployment resume; server blocked";
                        append_log(port, detail);
                        status
                            .set(ServerState::Failed, detail.to_string(), restarts)
                            .await;
                        return;
                    };
                    if !crate::lastslice::should_resume(attempt) {
                        let detail = format!(
                            "gave up after {attempt} persistent attempts; undeploy or apply again manually"
                        );
                        append_log(port, &detail);
                        status.set(ServerState::Failed, detail, restarts).await;
                        return;
                    }
                }

                // A stuck port cannot pass in silence: the server is going to
                // die with "Address already in use" and, while the attempt budget
                // is not exhausted, the operator saw only "loading" and the
                // TUI kept announcing health. The visible state gives the reason
                // from the first attempt (same pattern as
                // `note_unloadable_slice`).
                match &held_port {
                    Some(reason) => {
                        append_log(port, reason);
                        status
                            .set(
                                ServerState::Failed,
                                format!("{reason}; attempt {}/{MAX_RESTARTS}", restarts + 1),
                                restarts,
                            )
                            .await;
                    }
                    None => {
                        append_log(
                            port,
                            &format!(
                                "loading {} (attempt {}/{MAX_RESTARTS})",
                                short_path(&cfg.model_path),
                                restarts + 1
                            ),
                        );
                        status
                            .set(
                                ServerState::Starting,
                                format!("loading {}", short_path(&cfg.model_path)),
                                restarts,
                            )
                            .await;
                    }
                }

                let result = if cfg.inference_service.is_some() {
                    crate::plugin_engine::run(
                        cfg.clone(),
                        gen_rx.clone(),
                        generation,
                        tls.clone(),
                        quic.clone(),
                        quic_port,
                    )
                    .await
                } else {
                    crate::infer_server::run(
                        cfg.clone(),
                        gen_rx.clone(),
                        generation,
                        mtp_ready.clone(),
                        batching_ready.clone(),
                        tls.clone(),
                        quic.clone(),
                        quic_port,
                        auth.clone(),
                        model_cache.clone(),
                        reuse_weights,
                        loads.clone(),
                        load_node_id.clone(),
                        request_activity.clone(),
                    )
                    .await
                };
                if *gen_rx.borrow() != generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        info!("inference server stopped gracefully");
                        append_log(port, "stopped gracefully");
                        mtp_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        capacity_net.set_runtime_capacity_for_generation(generation, None);
                        return;
                    }
                    Err(e) => {
                        warn!("inference server stopped with an error: {e}");
                        mtp_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        capacity_net.set_runtime_capacity_for_generation(generation, None);
                        let _ = capacity_net.announce(None).await;
                        append_log(port, &format!("stopped with an error: {e}"));
                        restarts += 1;
                        if restarts >= MAX_RESTARTS {
                            // A slice that did get to serve (and for that reason is
                            // annotated as `Serving`) and then broke.
                            // The pending journal remains on disk. If the
                            // main process goes down now, the next
                            // start will see the same limit and will not try
                            // again until the operator intervenes.
                            let detail = match &held_port {
                                Some(reason) => {
                                    format!("gave up after {MAX_RESTARTS} persistent attempts: {reason}")
                                }
                                None => {
                                    format!("stopped {MAX_RESTARTS}x: {e}; deployment blocked")
                                }
                            };
                            append_log(port, &detail);
                            status.set(ServerState::Failed, detail, restarts).await;
                            return;
                        }
                    }
                }
            }
        });

        *self
            .supervisor
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(supervisor);

        self.spawn_health_watch(cfg_for_health, generation);
    }

    /// Poll the inference server until it answers, then mark the node ready and
    /// correct `total_layers` from the shard's GGUF metadata.
    fn spawn_health_watch(&self, cfg: NodeConfig, generation: u64) {
        let port = cfg.inference_port;
        // The runtime's own annotation path (`last_slice_path`), never the
        // environment's: two POM instances may use distinct POM_CONFIG values
        // in the same host process.
        let last_slice_path = self.last_slice_path.clone();
        let status = StatusWriter {
            status: self.status_handle(),
            changed: self.status_changed.clone(),
        };
        let assignment = self.assignment.clone();
        let net = self.net.clone();
        let gen_rx = self.generation.subscribe();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        tokio::spawn(async move {
            let url = format!("http://127.0.0.1:{port}/health");
            let mut healthy_polls = 0;
            loop {
                sleep(Duration::from_secs(3)).await;
                if *gen_rx.borrow() != generation {
                    return;
                }

                let Ok(resp) = http.get(&url).send().await else {
                    healthy_polls = 0;
                    continue;
                };
                if !resp.status().is_success() {
                    healthy_polls = 0;
                    continue;
                }
                let Ok(body) = resp.json::<serde_json::Value>().await else {
                    healthy_polls = 0;
                    continue;
                };
                if *gen_rx.borrow() != generation {
                    return;
                }
                if !body
                    .get("model_loaded")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    healthy_polls = 0;
                    continue;
                }

                healthy_polls += 1;
                if healthy_polls < HEALTH_CONFIRMATION_POLLS {
                    append_log(
                        port,
                        &format!(
                            "model answered model_loaded; waiting for stability ({healthy_polls}/{HEALTH_CONFIRMATION_POLLS})"
                        ),
                    );
                    continue;
                }

                net.set_runtime_capacity_for_generation(
                    generation,
                    Some(runtime_capacity_from_health(&body, &cfg)),
                );

                let reported = body
                    .get("original_block_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                // The same `/health`, already fetched: how many layers ended up on the
                // GPU after the fit. No new call.
                let reported_gpu = body
                    .get("n_gpu_layers_effective")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);

                // Shards written by split_model.py carry the real layer count;
                // trust it over the name-based guess.
                if let Some(total) = reported {
                    let mut w = assignment.write().await;
                    if let Some(ref mut a) = *w {
                        if a.total_layers != total {
                            info!(
                                "total_layers corrected: {} → {total} (GGUF metadata)",
                                a.total_layers
                            );
                            a.total_layers = total;
                        }
                    }
                }

                // The only point in the code that proves the slice is up:
                // `/health` answered `model_loaded` in consecutive
                // probes. A first return is not enough: the loading confirmation
                // may be precisely the path that brings down
                // the native process.
                if let Err(e) = crate::lastslice::save_at(
                    &last_slice_path,
                    &cfg,
                    crate::lastslice::SliceState::Serving,
                ) {
                    warn!("could not record the served slice: {e}");
                }

                if *gen_rx.borrow() != generation {
                    return;
                }
                append_log(port, "model loaded (/health answered model_loaded)");
                status.set_ready(reported, reported_gpu).await;
                net.set_idle(false);
                if *gen_rx.borrow() != generation {
                    return;
                }
                let current_assignment = assignment.read().await.clone();
                if let Err(e) = net.announce(current_assignment.as_ref()).await {
                    warn!("batch capability announce after readiness failed: {e}");
                }
                return;
            }
        });
    }

    fn status_handle(&self) -> Arc<RwLock<RuntimeStatus>> {
        self.status.clone()
    }
}

/// Where the inference server logs for this port live.
///
/// One file per port, under the project root, because two instances on the same
/// machine (different `POM_HOME`) cannot write to the same file.
pub fn log_file_path(port: u16) -> PathBuf {
    crate::config::project_root().join(format!("infer_server_{port}.log"))
}

/// Appends a line to the inference server log.
///
/// The process stdout mixes everything and scrolls, so it is a bad place for the
/// history of the server's startup and crash: by the time the operator goes to look, the
/// line has already passed. The file is what remains, and it is what
/// `RuntimeStatus::log_path` points to.
fn append_log(port: u16, line: &str) {
    use std::io::Write;
    let path = log_file_path(port);
    let stamped = format!("{} {line}\n", chrono::Utc::now().to_rfc3339());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match opened {
        Ok(mut f) => {
            if let Err(e) = f.write_all(stamped.as_bytes()) {
                warn!("could not write to {}: {e}", path.display());
            }
        }
        Err(e) => warn!("could not open {}: {e}", path.display()),
    }
}

fn short_path(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Can the inference server still take this port?
///
/// It binds the dual-stack wildcard `::`, so that is what has to be probed: a
/// `0.0.0.0` probe succeeds while a `::` listener already owns the port on some
/// stacks, and the supervisor would then start a child doomed to `address
/// already in use`. Falls back to IPv4 on a host with IPv6 turned off.
fn port_is_free(port: u16) -> bool {
    use std::net::TcpListener;
    match TcpListener::bind(("::", port)) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => false,
        Err(_) => TcpListener::bind(("0.0.0.0", port)).is_ok(),
    }
}

/// The PIDs holding `port`, read from the output of Windows `netstat -ano`.
///
/// Separated from the process call precisely so it can be tested on
/// any system: it is the part that errs silently, and a PID read wrong here
/// becomes a `taskkill` on an innocent process.
///
/// Format of the lines (the state is absent in the UDP ones, but the PID is always the last
/// field):
///
/// ```text
///   TCP    0.0.0.0:8181     0.0.0.0:0    LISTENING    1234
///   TCP    [::]:8181        [::]:0       LISTENING    1234
///   UDP    0.0.0.0:8181     *:*                       5678
/// ```
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat_pids(output: &str, port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        // The local address is the second field. Comparing the port as a number, and
        // not the textual suffix, avoids matching 18181 with 8181.
        let local = fields[1];
        let Some((_, port_str)) = local.rsplit_once(':') else {
            continue;
        };
        if port_str.parse::<u16>() != Ok(port) {
            continue;
        }
        if let Ok(pid) = fields[fields.len() - 1].parse::<u32>() {
            // PID 0 is the system idle process: it owns nothing
            // we could kill, and sending a taskkill to it only generates an error.
            if pid != 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Who is holding `port`, asked the way each system does it.
async fn pids_holding_port(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    {
        let out = crate::console::hidden_async(&mut tokio::process::Command::new("netstat"))
            .args(["-ano"])
            .output()
            .await;
        let Ok(out) = out else {
            warn!("could not run netstat to check port {port}");
            return Vec::new();
        };
        return parse_netstat_pids(&String::from_utf8_lossy(&out.stdout), port);
    }

    #[cfg(not(windows))]
    {
        let out = tokio::process::Command::new("lsof")
            .args(["-ti", &format!(":{port}")])
            .output()
            .await;
        let Ok(out) = out else {
            warn!("could not run lsof to check port {port}");
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect()
    }
}

/// Kills `pid` by force.
async fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = crate::console::hidden_async(&mut Command::new("taskkill"))
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .await;

    #[cfg(not(windows))]
    let _ = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output()
        .await;
}

/// Kill any process holding `port` so the inference server can bind.
///
/// Unix: `lsof -ti :PORT` finds the PID and it gets SIGKILL. Windows: `netstat -ano` and
/// `taskkill /F /PID`, which are the equivalents that ship with the system. Afterwards,
/// on both, the port is probed for up to 5 seconds until it is free.
async fn free_port(port: u16) -> Result<(), String> {
    // Fast path: if we can bind, the port is already free.
    if port_is_free(port) {
        return Ok(());
    }

    info!("port {port} in use — looking for the process to kill");

    let pids = pids_holding_port(port).await;
    if pids.is_empty() {
        let reason = format!("port {port} busy and the process holding it could not be identified");
        warn!("{reason}");
        return Err(reason);
    }

    for pid in pids {
        // Skip our own process - never kill ourselves.
        if pid == std::process::id() {
            continue;
        }
        info!("killing process {pid} that holds port {port}");
        kill_pid(pid).await;
    }

    // Poll for up to 5 seconds until the port is free.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if port_is_free(port) {
            info!("port {port} released");
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            let reason = format!("port {port} still busy 5s after killing the occupant");
            warn!("{reason}");
            return Err(reason);
        }
        sleep(Duration::from_millis(200)).await;
    }
}

// ── Status plumbing ──────────────────────────────────────────────────────────
//
// Supervisor tasks outlive the borrow of `&self`, so they need an owned handle
// to the status cell. The runtime keeps that cell behind its own Arc.

struct StatusWriter {
    status: Arc<RwLock<RuntimeStatus>>,
    changed: Arc<Notify>,
}

impl StatusWriter {
    async fn set(&self, state: ServerState, detail: String, restarts: u32) {
        let mut s = self.status.write().await;
        s.state = state;
        s.detail = detail;
        s.restarts = restarts;
        s.changed_at = chrono::Utc::now().to_rfc3339();
        self.changed.notify_waiters();
    }

    async fn set_log(&self, path: &str) {
        self.status.write().await.log_path = Some(path.to_string());
        self.changed.notify_waiters();
    }

    async fn set_ready(
        &self,
        reported_total_layers: Option<u32>,
        reported_gpu_layers: Option<i32>,
    ) {
        let mut s = self.status.write().await;
        s.state = ServerState::Ready;
        s.detail = "model loaded".to_string();
        s.reported_total_layers = reported_total_layers;
        s.reported_gpu_layers = reported_gpu_layers;
        s.changed_at = chrono::Utc::now().to_rfc3339();
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stuck port is a visible failure, not a silent wait.
    ///
    /// With the occupant unidentifiable (or not killable, as here: whoever holds
    /// the port is the test process itself, and the supervisor never kills itself), the
    /// server dies with "Address already in use" on every attempt. Before, the
    /// operator saw only "loading" until `MAX_RESTARTS` ran out; the peer only
    /// broke at inference time. The reason has to be in the status from the
    /// first attempt - and the log file has to exist, because stdout
    /// does not keep the history.
    #[tokio::test]
    async fn the_held_port_shows_up_in_the_status_and_in_the_log() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("pom-porta-presa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("POM_HOME", &dir);

        // Someone already has the port, and it is this process: `free_port` never
        // kills itself, so the port stays stuck as in the real defect.
        // On the dual-stack wildcard, which is what `port_is_free` probes and what the
        // inference server binds.
        let occupant = match tokio::net::TcpListener::bind("[::]:0").await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap(),
        };
        let port = occupant.local_addr().unwrap().port();

        let net = Arc::new(
            Network::testkit("no".into(), "pk".into(), vec![], None, None)
                .await
                .expect("UDP socket on an ephemeral port"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));
        let mut c = cfg();
        c.inference_port = port;
        rt.spawn_supervisor(c, 0, false);

        let mut status = rt.status().await;
        for _ in 0..80 {
            sleep(Duration::from_millis(250)).await;
            status = rt.status().await;
            if status.state == ServerState::Failed {
                break;
            }
        }

        // The supervisor remains a live task while it walks through the
        // restart budget. Signal the generation and wait for its exit before
        // returning the global environment to the rest of the suite, otherwise a
        // late attempt may write the journal using the next test's
        // POM_HOME.
        let generation = *rt.generation.borrow() + 1;
        rt.generation.send_replace(generation);
        let (pending_supervisor, wait_result) = NodeRuntime::wait_for_supervisor_with_timeout(
            rt.take_supervisor(),
            Duration::from_secs(10),
        )
        .await;
        assert!(
            pending_supervisor.is_none(),
            "supervisor of the held port ended"
        );
        wait_result.unwrap();

        let log = log_file_path(port);
        let content = std::fs::read_to_string(&log).unwrap_or_default();
        std::env::remove_var("POM_HOME");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            status.state,
            ServerState::Failed,
            "a held port cannot stay in Starting: {}",
            status.detail
        );
        assert!(
            status.detail.contains(&port.to_string()),
            "the status must name the port, and it says: {}",
            status.detail
        );
        assert_eq!(
            status.log_path.as_deref(),
            Some(log.to_string_lossy().as_ref()),
            "the status must point to the log file"
        );
        assert!(
            content.contains("supervisor started"),
            "the log file must exist and have the supervisor lines: {content:?}"
        );
    }

    #[tokio::test]
    async fn idle_reload_waits_for_previous_supervisor_to_finish() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_in_supervisor = finished.clone();
        let supervisor = tokio::spawn(async move {
            sleep(Duration::from_millis(40)).await;
            finished_in_supervisor.store(true, std::sync::atomic::Ordering::Release);
        });

        let (pending_supervisor, wait_result) = NodeRuntime::wait_for_supervisor_with_timeout(
            Some(supervisor),
            Duration::from_millis(100),
        )
        .await;
        assert!(pending_supervisor.is_none());
        wait_result.unwrap();

        assert!(
            finished.load(std::sync::atomic::Ordering::Acquire),
            "the next generation can only start after the cleanup of the previous one"
        );
    }

    #[tokio::test]
    async fn a_pending_supervisor_is_bounded_and_stays_joinable() {
        let supervisor = tokio::spawn(async {
            std::future::pending::<()>().await;
        });

        let (pending_supervisor, wait_result) = NodeRuntime::wait_for_supervisor_with_timeout(
            Some(supervisor),
            Duration::from_millis(100),
        )
        .await;
        let pending_supervisor = pending_supervisor.expect("the supervisor must be preserved");
        assert!(wait_result.is_err());

        pending_supervisor.abort();
        assert!(pending_supervisor.await.is_err());
    }

    /// The guarantee that makes it safe to reconfigure a live deployment: touching
    /// the parameters **must not** touch anything that identifies the slice.
    ///
    /// If `with_params` let a `start_layer` or a `model_path` escape,
    /// an "increase n_ctx" would silently turn into a machine serving another
    /// range - and the hole would only show up on the next request, as
    /// "Gap at layer N".
    #[test]
    fn reconfiguring_changes_parameters_and_nothing_else() {
        let mut before = cfg();
        before.deploy_id = "d-4f2a91c3".into();
        before.tp = None;
        before.cache_type_k = crate::config::KvCacheType::Q8_0;
        before.cache_type_v = crate::config::KvCacheType::Bf16;

        let mut p = crate::webui::RuntimeParams::default();
        p.n_ctx = 100_000;
        p.max_new_tokens = 32000;
        p.n_gpu_layers = 7;
        p.temperature = 1.25;
        p.max_sessions = 4;

        let after = before.with_params(&p);

        // What had to change, changed.
        assert_eq!(after.n_ctx, 100_000);
        assert_eq!(after.max_new_tokens, 32000);
        assert_eq!(after.n_gpu_layers, 7);
        assert_eq!(after.temperature, 1.25);
        assert_eq!(after.max_sessions, 4);

        // And the slice identity is byte for byte the same.
        assert_eq!(after.deploy_id, before.deploy_id);
        assert_eq!(after.model_path, before.model_path);
        assert_eq!(after.model_label, before.model_label);
        assert_eq!(after.start_layer, before.start_layer);
        assert_eq!(after.end_layer, before.end_layer);
        assert_eq!(after.total_layers, before.total_layers);
        assert_eq!(after.peer_count, before.peer_count);
        assert_eq!(after.peer_index, before.peer_index);
        assert_eq!(after.primary, before.primary);
        assert_eq!(after.inference_port, before.inference_port);
        assert_eq!(after.single_peer, before.single_peer);
        assert_eq!(after.no_embeddings, before.no_embeddings);
        assert_eq!(after.cache_type_k, before.cache_type_k);
        assert_eq!(after.cache_type_v, before.cache_type_v);
    }

    #[test]
    fn health_capacity_uses_effective_context_and_keeps_limits_separate() {
        let mut config = cfg();
        config.n_ctx = 8_192;
        config.max_new_tokens = 32_000;

        let capacity = runtime_capacity_from_health(
            &serde_json::json!({
                "model_loaded": true,
                "n_ctx_per_session": 100_000,
            }),
            &config,
        );

        assert_eq!(capacity.n_ctx, 100_000);
        assert_eq!(capacity.max_new_tokens, 32_000);
    }

    #[test]
    fn reconfiguring_the_context_reuses_weights_but_a_new_slice_does_not() {
        let before = cfg();
        let mut params = crate::webui::RuntimeParams::default();
        params.n_ctx = 65536;
        params.max_sessions = 4;
        params.n_batch = 1024;
        let mut after = before.with_params(&params);
        after.cache_type_k = crate::config::KvCacheType::Q8_0;
        after.cache_type_v = crate::config::KvCacheType::Bf16;

        assert!(before.weights_are_compatible(&after));

        after.fit_memory_margin_mb += 1;
        assert!(!before.weights_are_compatible(&after));

        after.start_layer += 1;
        assert!(!before.weights_are_compatible(&after));
    }

    /// In a tensor parallelism group llama.cpp's native `fit` does not serve, and
    /// this cannot depend on whoever filled in the form.
    #[test]
    fn the_memory_fit_stays_off_on_a_tp_node() {
        let mut before = cfg();
        before.tp = Some(crate::network::TpIdentity {
            role: "worker".into(),
            group_id: "g".into(),
            size: 2,
            rank: 1,
            rank0_url: None,
            rank0_node_id: None,
            members: Vec::new(),
        });

        let mut p = crate::webui::RuntimeParams::default();
        p.fit_memory = true;

        assert!(
            !before.with_params(&p).fit_memory,
            "the native fit cannot handle split-mode tensor"
        );
    }

    fn cfg() -> NodeConfig {
        NodeConfig {
            deploy_id: String::new(),
            model_path: "/nonexistent/model.gguf".into(),
            mmproj_path: None,
            mtp_draft_path: None,
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_service: None,
            engine_settings: Default::default(),
            inference_role: Default::default(),
            prefill_min_tokens: None,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            cache_ram_mib: None,
            fit_memory: true,
            fit_memory_margin_mb: DEFAULT_FIT_MEMORY_MARGIN_MB,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            prefill_max_inflight_chunks:
                crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32,
            n_batch: 512,
            n_ubatch: llama_engine::DEFAULT_N_UBATCH,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            min_p: 0.0,
            seed: llama_engine::llm::DEFAULT_SAMPLER_SEED,
            dry_multiplier: 0.0,
            dry_base: 1.75,
            dry_allowed_length: 2,
            dry_penalty_last_n: 1024,
            reasoning_budget: -1,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            mtp_placement: Default::default(),
            moe_experts_in_ram: false,
            moe_cache_mib: 0,
            adaptive_prefill_chunks: true,
            unload_if_idle: false,
            yield_gpu_to_images: false,
            idle_minutes: 60,
            tp: None,
        }
    }

    /// The other half of the law: whoever promotes the annotation to `Serving` is
    /// `/health` answering `model_loaded`, and no one else.
    ///
    /// Without this promotion `apply` would annotate `Pending` forever and the guarantee
    /// of `c7b70d8` would die silently - a node that was serving healthy and went down
    /// would come back empty, opening a layer hole in the cluster. The test brings up a fake
    /// `/health` because there is no GGUF that loads on this machine; what
    /// it proves is the contract between the watch and the disk, not llama.cpp.
    #[tokio::test]
    async fn health_is_what_promotes_the_annotation_to_serving() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct RestoreConfigEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreConfigEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("POM_CONFIG", value),
                    None => std::env::remove_var("POM_CONFIG"),
                }
            }
        }
        let _restore_config = RestoreConfigEnv(std::env::var_os("POM_CONFIG"));
        let dir = std::env::temp_dir().join(format!("pom-health-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("pom.json");
        std::env::set_var("POM_CONFIG", &cfg_path);
        let annotation = crate::lastslice::path();
        std::fs::remove_file(&annotation).ok();

        let health_polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let health_release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let health_polls_server = health_polls.clone();
        let health_release_server = health_release.clone();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let poll = health_polls_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let model_loaded =
                    poll == 0 || health_release_server.load(std::sync::atomic::Ordering::SeqCst);
                async move { axum::Json(serde_json::json!({ "model_loaded": model_loaded })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let net = Arc::new(
            Network::testkit("no".into(), "pk".into(), vec![], None, None)
                .await
                .expect("UDP socket on an ephemeral port"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));

        let mut c = cfg();
        c.inference_port = port;
        // As `apply` leaves it: applied, not yet loaded.
        crate::lastslice::save(&c, crate::lastslice::SliceState::Pending).unwrap();
        rt.spawn_health_watch(c, 0);

        // Wait for the first real probe, instead of depending on the scheduler
        // delivering exactly the 3 s interval. The first return still
        // cannot clear the attempt: it is the window of the observed crash.
        for _ in 0..60 {
            if health_polls.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        assert!(
            health_polls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the watcher must have done the first probe"
        );
        assert!(
            matches!(
                crate::lastslice::inspect(),
                crate::lastslice::LastSlice::Pending { .. }
            ),
            "a single model_loaded does not confirm the deployment"
        );
        let alternate_config = dir.join("alternate-pom.json");
        std::env::set_var("POM_CONFIG", &alternate_config);
        std::fs::remove_file(crate::lastslice::path()).ok();
        health_release.store(true, std::sync::atomic::Ordering::SeqCst);

        for _ in 0..40 {
            sleep(Duration::from_millis(250)).await;
            if rt.status().await.state == ServerState::Ready {
                break;
            }
        }

        assert_eq!(
            rt.status().await.state,
            ServerState::Ready,
            "/health came up"
        );
        assert!(
            matches!(
                crate::lastslice::inspect(),
                crate::lastslice::LastSlice::Nothing
            ),
            "the watcher cannot change configuration along with the process environment"
        );
        std::env::set_var("POM_CONFIG", &cfg_path);
        assert!(
            matches!(
                crate::lastslice::inspect(),
                crate::lastslice::LastSlice::Serving(_)
            ),
            "the annotation goes back to the captured configuration when /health confirmed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The start that refuses to resume a slice has to say so to the operator.
    ///
    /// Without this line the node comes up empty and silent: the panel would say only "no
    /// model loaded", indistinguishable from a machine that never received
    /// any slice - and the machine that just brought the process down trying to
    /// load is precisely the one that needs to be explained.
    #[tokio::test]
    async fn a_slice_that_did_not_load_shows_up_in_the_status_with_the_reason() {
        let net = Arc::new(
            Network::testkit("no".into(), "pk".into(), vec![], None, None)
                .await
                .expect("UDP socket on an ephemeral port"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));

        rt.note_unloadable_slice(&cfg(), crate::lastslice::MAX_AUTOMATIC_ATTEMPTS)
            .await;

        let s = rt.status().await;
        assert_eq!(s.state, ServerState::Disabled, "the node serves nothing");
        assert!(
            s.detail.contains("qwen35") && s.detail.contains("0-15"),
            "the status must name the refused slice, and it says: {}",
            s.detail
        );
        assert!(
            s.detail.contains("never loaded"),
            "and say why it did not come back, and it says: {}",
            s.detail
        );
    }

    /// The undeploy during a native load: the load does not look at the generation, so the supervisor outlives
    /// the deadline. The stop must still take effect - the configuration gone, the status saying so and the
    /// pending journal cleared (it brought the deployment back after the load crashed the process) - and keep the
    /// supervisor, which releases the model when the load returns.
    #[tokio::test]
    async fn a_stop_during_a_load_leaves_the_deployment_and_keeps_the_supervisor_to_finish() {
        let net = Arc::new(
            Network::testkit("no".into(), "pk".into(), vec![], None, None)
                .await
                .expect("UDP socket on an ephemeral port"),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = dir.path().join("pom.json.last-slice.json");
        let rt = NodeRuntime::new_with_last_slice_path(net, Arc::new(RwLock::new(None)), journal.clone());
        rt.seed_config_for_test(cfg()).await;
        crate::lastslice::begin_new_attempt_at(&journal, &cfg()).expect("pending journal");
        assert!(journal.exists());
        // a load that ignores the generation until it returns
        let (release, loading) = tokio::sync::oneshot::channel::<()>();
        *rt.supervisor.lock().unwrap() = Some(tokio::spawn(async move {
            let _ = loading.await;
        }));

        rt.stop().await;

        assert!(rt.config().await.is_none(), "the node no longer has the deployment");
        let status = rt.status().await;
        assert_eq!(status.state, ServerState::Disabled);
        assert!(status.detail.contains("released when its load returns"), "{}", status.detail);
        assert!(!journal.exists(), "a crash of the load must not bring the deployment back");
        let supervisor = rt.take_supervisor().expect("the loading supervisor is kept for the next apply to wait on");
        release.send(()).unwrap();
        supervisor.await.unwrap();
    }

    #[tokio::test]
    async fn a_manual_stop_cancels_the_pending_thermal_resume() {
        let net = Arc::new(
            Network::testkit("no".into(), "pk".into(), vec![], None, None)
                .await
                .expect("UDP socket on an ephemeral port"),
        );
        // `stop` forgets the annotation: its own, never the real configuration's.
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = NodeRuntime::new_with_last_slice_path(
            net,
            Arc::new(RwLock::new(None)),
            dir.path().join("pom.json.last-slice.json"),
        );
        rt.thermal.lock().await.recovery = Some(cfg());

        rt.stop().await;

        assert!(
            rt.thermal.lock().await.recovery.is_none(),
            "a manual undeploy cannot be undone when the temperature drops"
        );
        assert_eq!(rt.status().await.state, ServerState::Disabled);
    }

    /// The residual capture callback costs ~8 ms per token (measured on
    /// 2026-08-12: 7.8 and 8.2 ms on the hop that installs it, against 0.14 and 0.67 on
    /// the one that does not). Whoever serves the last layer samples tokens and has no one to
    /// export to - installing it there is paying 11% of the budget for nothing. The
    /// derivation has to hold also for the node started by command line,
    /// which does not go through `ApplyRequest` and always sent `no_embeddings: false`.
    #[test]
    fn a_slice_that_serves_the_last_layer_never_exports_the_residual() {
        let mut c = cfg();
        assert!(c.needs_residual_export(), "0-15 of 32 has a successor");

        c.start_layer = 16;
        c.end_layer = 31;
        assert!(
            !c.needs_residual_export(),
            "16-31 of 32 samples tokens, does not export state"
        );
    }

    #[test]
    fn single_peer_and_the_operator_kill_switch_still_win() {
        let mut c = cfg();
        c.single_peer = true;
        assert!(!c.needs_residual_export());

        let mut c = cfg();
        c.no_embeddings = true;
        assert!(!c.needs_residual_export());
    }

    /// The last of the division may not be the last in layers when the plan comes
    /// from the interface with `peer_index`/`peer_count`; both paths hold.
    #[test]
    fn the_last_peer_of_the_split_also_skips_it() {
        let mut c = cfg();
        c.peer_index = 1;
        c.peer_count = 2;
        c.end_layer = 15;
        assert!(!c.needs_residual_export());
    }

    /// MTP only makes sense on whoever serves the whole model. The `nextn` block
    /// lives in the last layer and whoever assembles the batch is the head: in a slice, the
    /// two ends are different machines.
    #[test]
    fn mtp_only_where_one_machine_serves_the_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = true;
        c.start_layer = 0;
        c.end_layer = 31;
        c.total_layers = 32;
        c.peer_count = 1;
        c.peer_index = 0;
        assert!(
            c.mtp_effective(),
            "0-31 of 32, alone: it is the whole model"
        );
    }

    #[test]
    fn mtp_only_on_the_tail_of_a_split_and_on_a_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = true;
        c.total_layers = 32;
        c.peer_count = 2;

        c.start_layer = 0;
        c.end_layer = 15;
        c.peer_index = 0;
        assert!(!c.mtp_effective(), "the head does not have the nextn block");

        c.start_layer = 16;
        c.end_layer = 31;
        c.peer_index = 1;
        assert!(
            c.mtp_effective(),
            "the tail of a sliced pipeline has the nextn block — this is where the draft is born"
        );
    }

    /// The switch turned off wins over everything: turning MTP on changes the graph of every
    /// decode (`llama_set_embeddings_nextn` on the target context), so
    /// it cannot be inferred from "the model has the block".
    #[test]
    fn mtp_off_stays_off_on_a_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = false;
        c.start_layer = 0;
        c.end_layer = 31;
        c.total_layers = 32;
        c.peer_count = 1;
        assert!(!c.mtp_effective());
    }

    #[test]
    fn rejects_missing_file() {
        assert!(cfg().validate().is_err());
    }

    #[test]
    fn rejects_a_persisted_projector_with_incompatible_output_dimension() {
        let root = std::env::temp_dir().join(format!(
            "pom-runtime-mmproj-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let model = root.join("model.gguf");
        let projector = root.join("mmproj-model.gguf");
        std::fs::write(
            &model,
            crate::catalog::tests::text_model_fixture("Shared Identity", "llama", 3072),
        )
        .unwrap();
        std::fs::write(
            &projector,
            crate::catalog::tests::projector_fixture("Shared Identity", 2048),
        )
        .unwrap();
        let mut config = cfg();
        config.model_path = model.to_string_lossy().into_owned();
        config.mmproj_path = Some(projector.to_string_lossy().into_owned());

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("incompatible mmproj"), "{error}");
        assert!(error.contains("3072") && error.contains("2048"), "{error}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_inverted_range() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.start_layer = 20;
        c.end_layer = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_range_past_end_of_model() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.end_layer = 40;
        assert!(c.validate().is_err());
    }

    #[test]
    fn the_context_ceiling_is_the_model_s_declared_context_length() {
        let dir = std::env::temp_dir().join(format!("pom-ctx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.gguf");
        std::fs::write(&model, crate::catalog::tests::gguf_header_fixture(
            &[("llama.context_length", 262_144), ("llama.block_count", 32)],
            &[("general.architecture", "llama")],
        ))
        .unwrap();
        let mut c = cfg();
        c.model_path = model.to_string_lossy().into_owned();
        c.n_ctx = 262_144;
        assert!(c.validate().is_ok(), "{:?}", c.validate());
        c.n_ctx = 262_145;
        let error = c.validate().unwrap_err().to_string();
        assert!(error.contains("262144"), "{error}");
        c.n_ctx = 0;
        assert!(c.validate().is_err());

        // A header without context_length sets no POM ceiling.
        c.model_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/inference/runtime.rs")
            .to_string_lossy()
            .into_owned();
        c.n_ctx = 1_000_000;
        assert!(c.validate().is_ok(), "{:?}", c.validate());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_session_gets_the_configured_context_capacity() {
        assert_eq!(total_context_tokens(32_768, 2).unwrap(), 65_536);
        assert_eq!(total_context_tokens(32_768, 0).unwrap(), 32_768);
    }

    #[test]
    fn context_capacity_rejects_numeric_overflow() {
        assert!(total_context_tokens(u32::MAX, 2).is_err());
    }

    #[test]
    fn rejects_peer_index_outside_split() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.peer_index = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn layers_count_is_inclusive() {
        assert_eq!(cfg().layers_count(), 16);
    }

    /// The output of `netstat -ano` is the only clue to who holds the port on
    /// Windows, and a PID read wrong there becomes a `taskkill` on an
    /// innocent process. The test runs on any system on purpose: the parser is
    /// common code, only the process call changes per platform.
    #[test]
    fn netstat_gives_the_pids_of_the_requested_port() {
        let output = "\
Conexoes ativas

  Proto  Endereco local         Endereco externo       Estado           PID
  TCP    0.0.0.0:8181           0.0.0.0:0              LISTENING       1234
  TCP    [::]:8181              [::]:0                 LISTENING       1234
  TCP    127.0.0.1:52777        0.0.0.0:0              LISTENING       9999
  UDP    0.0.0.0:8181          *:*                                     5678
";
        // 1234 appears twice (IPv4 and IPv6) and comes back only once.
        assert_eq!(parse_netstat_pids(output, 8181), vec![1234, 5678]);
        assert_eq!(parse_netstat_pids(output, 52777), vec![9999]);
        assert!(parse_netstat_pids(output, 8080).is_empty());
    }

    /// Comparing the port as text would match 18181 with 8181 and kill the
    /// neighboring node on the same machine.
    #[test]
    fn netstat_does_not_mistake_a_suffix_for_the_port() {
        let output = "  TCP    0.0.0.0:18181    0.0.0.0:0    LISTENING    4242\n";
        assert!(parse_netstat_pids(output, 8181).is_empty());
        assert_eq!(parse_netstat_pids(output, 18181), vec![4242]);
    }

    /// PID 0 is the system idle process: sending taskkill to it only generates an error.
    #[test]
    fn netstat_ignores_pid_zero() {
        let output = "  TCP    0.0.0.0:8181    0.0.0.0:0    LISTENING    0\n";
        assert!(parse_netstat_pids(output, 8181).is_empty());
    }
}
