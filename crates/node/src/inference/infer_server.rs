//! HTTP inference server — the Rust replacement for `inference_server.py`.
//!
//! Exposes:
//!
//!   POST /v1/prefill          — one chunk of the prompt
//!   POST /v1/step             — one autoregressive token step (optionally
//!                                speculative — see `step_blocking`)
//!   POST /v1/step-stream      - remote decode stream with opportunistic batches
//!   POST /v1/session/release  — free a session's KV slot
//!   POST /v1/session/rewind   - keep the prompt and discard generated suffix
//!   POST /v1/session/rollback — discard an unconfirmed speculative batch
//!   POST /v1/session/slot     - save/restore/erase a KV cache snapshot on disk
//!                                (`--slot-save-path`, like llama-server)
//!   GET  /health              — readiness probe
//!
//! Wire format is defined in [`crate::wire`] (binary framing, f16 on the wire).
//! Decode calls run in `spawn_blocking` so they don't starve the tokio runtime.

use crate::llm::{sample_token_profiled, LlmEngine, ModelCache, SamplingConfig};
use crate::pipeline::PipelineStep;
use crate::runtime::NodeConfig;
use crate::wire::{self, TensorShape, WireDtype};
use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, warn};

const SESSION_IDLE_TIMEOUT_SECS: u64 = 900;
/// Upper bound for one binary f16 inference frame received from a peer. Keep
/// it in lockstep with the wire validator; the relay splits coalesced payloads
/// before this limit and the handler applies it before decoding the body.
const INFERENCE_FRAME_BODY_LIMIT: usize = wire::MAX_FRAME_PAYLOAD_BYTES;
const BATCH_CHILD_HEADER: &str = "x-pom-batch-child";

pub(crate) const SPECULATION_REASON_TAIL_WITHOUT_NEXTN: &str = "tail_without_nextn";
pub(crate) const SPECULATION_REASON_MODEL_WITHOUT_MTP: &str = "model_without_mtp";
pub(crate) const SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK: &str = "hybrid_tail_no_rollback";
pub(crate) const SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE: &str =
    "peer_without_sliced_mtp_wire";
const SPECULATION_REASON_TAIL_DRAFT_EMPTY: &str = "tail_mtp_draft_empty";

/// Resolve a preferência persistida pelo deploy para o seletor operacional.
///
/// Uma escolha explícita na UI não pode ser sobrescrita por uma variável do
/// processo. `Auto` continua aceitando `POM_INFERENCE_MODE` para preservar o
/// override operacional existente; sem ele, `executor::select` decide pela
/// metadata GGUF real.
fn configured_inference_mode(
    mode: crate::webui::InferenceMode,
) -> Result<crate::executor::InferenceMode> {
    match mode {
        crate::webui::InferenceMode::Auto => crate::executor::InferenceMode::from_env(),
        crate::webui::InferenceMode::Dense => Ok(crate::executor::InferenceMode::Dense),
        crate::webui::InferenceMode::Moe => Ok(crate::executor::InferenceMode::Moe),
    }
}

/// Timings measured on the inference node itself. The names deliberately name
/// the boundary being measured instead of guessing whether a CUDA call is
/// asynchronous. In particular, `residual_ms` contains the GPU -> host copy
/// and the CUDA stream synchronization performed by `take_residual()`.
#[derive(Default)]
struct MoeTimings {
    forward_us: u64,
    executors: Vec<crate::moe::runtime::MoeExecutorKind>,
    dtypes: Vec<u32>,
    reasons: Vec<String>,
    diagnostics: Option<crate::moe::runtime::MoeExecutorDiagnostics>,
}

impl MoeTimings {
    fn record_executor(&mut self, executor: crate::moe::runtime::MoeExecutorKind) {
        if !self.executors.contains(&executor) {
            self.executors.push(executor);
        }
    }

    fn record_telemetry(&mut self, telemetry: &crate::moe::runtime::MoeExecutorTelemetry) {
        self.record_executor(telemetry.executor);
        for dtype in &telemetry.dtypes {
            if !self.dtypes.contains(dtype) {
                self.dtypes.push(*dtype);
            }
        }
        if let Some(reason) = &telemetry.reason {
            if !self.reasons.contains(reason) {
                self.reasons.push(reason.clone());
            }
        }
        if telemetry.diagnostics.is_some() {
            self.diagnostics = telemetry.diagnostics.clone();
        }
    }

    fn record_forward(&mut self, elapsed_us: u64) {
        self.forward_us = self.forward_us.saturating_add(elapsed_us);
    }

    fn record_reason(&mut self, reason: &str) {
        if !self.reasons.iter().any(|existing| existing == reason) {
            self.reasons.push(reason.to_string());
        }
    }

    fn merge(&mut self, other: &Self) {
        self.forward_us = self.forward_us.saturating_add(other.forward_us);
        for executor in &other.executors {
            self.record_executor(*executor);
        }
        for dtype in &other.dtypes {
            if !self.dtypes.contains(dtype) {
                self.dtypes.push(*dtype);
            }
        }
        for reason in &other.reasons {
            if !self.reasons.contains(reason) {
                self.reasons.push(reason.clone());
            }
        }
        if other.diagnostics.is_some() {
            self.diagnostics = other.diagnostics.clone();
        }
    }
}

#[derive(Default)]
struct NodeTimings {
    frame_decode_us: u64,
    input_prepare_us: u64,
    queue_us: u64,
    /// Espera pelo mutex do motor, antes de qualquer trabalho.
    ///
    /// O `llama_context` não é seguro para uso concorrente, então todo decode
    /// deste nó serializa aqui. Sem este contador a espera caía no vão entre a
    /// entrada do handler e o primeiro medidor, e aparecia na tela só como
    /// "não atribuído" - 194 ms por token numa medição de 2026-08-14, 63% do
    /// tempo da requisição, invisível.
    lock_wait_us: u64,
    setup_us: u64,
    decode_us: u64,
    backend_enqueue_us: u64,
    backend_completion_us: u64,
    residual_capture_d2h_sync_us: u64,
    residual_us: u64,
    logits_access_us: u64,
    logits_clone_us: u64,
    repetition_penalty_us: u64,
    softmax_us: u64,
    top_k_us: u64,
    top_p_us: u64,
    selection_us: u64,
    token_text_us: u64,
    sampling_us: u64,
    encode_us: u64,
    selected_path: crate::executor::InferencePath,
    executed_engines: Vec<crate::executor::ExecutionEngine>,
    moe: MoeTimings,
    tp: Option<crate::tp::TpDecodeTelemetry>,
}

impl NodeTimings {
    fn record_engine(&mut self, engine: crate::executor::ExecutionEngine) {
        if engine != crate::executor::ExecutionEngine::Unknown
            && !self.executed_engines.contains(&engine)
        {
            self.executed_engines.push(engine);
        }
    }

    fn executed_engine(&self) -> crate::executor::ExecutionEngine {
        match self.executed_engines.as_slice() {
            [engine] => *engine,
            _ => crate::executor::ExecutionEngine::Unknown,
        }
    }

    fn mixed_execution(&self) -> bool {
        self.executed_engines.len() > 1
    }

    fn engine_mismatch(&self) -> bool {
        let expected = self.selected_path.execution_engine();
        if self.executed_engines.is_empty() {
            return false;
        }
        if self.mixed_execution() {
            return !self.executed_engines.contains(&expected);
        }
        self.executed_engine() != expected
    }

    fn record_moe_executor(&mut self, executor: crate::moe::runtime::MoeExecutorKind) {
        self.moe.record_executor(executor);
    }

    fn record_moe_telemetry(&mut self, telemetry: crate::moe::runtime::MoeExecutorTelemetry) {
        self.moe.record_telemetry(&telemetry);
    }

    fn record_moe_forward(&mut self, elapsed_us: u64) {
        self.moe.record_forward(elapsed_us);
    }

    fn record_moe_reason(&mut self, reason: &str) {
        self.moe.record_reason(reason);
    }

    fn merge_moe(&mut self, other: &MoeTimings) {
        self.moe.merge(other);
    }

    fn merge_engine_moe(&mut self, other: &crate::llm::DecodeTimings) {
        if other.moe_forward_us != 0 {
            self.record_moe_forward(other.moe_forward_us);
            for executor in &other.moe_executors {
                self.record_moe_executor(*executor);
            }
            if let Some(telemetry) = &other.moe_executor {
                self.record_moe_telemetry(telemetry.clone());
            }
            self.record_engine(crate::executor::ExecutionEngine::Moe);
        }
    }

    fn moe_executor(&self) -> &'static str {
        match self.moe.executors.as_slice() {
            [executor] => executor.as_str(),
            [] => "not-run",
            _ => "mixed",
        }
    }

    fn json(&self) -> Value {
        // Counters are kept in microseconds so a stage costing less than a
        // millisecond is not truncated to zero on every call and lost in the
        // sum. The wire stays in milliseconds, with a fraction preserved for
        // the diagnostic fields the screen reads.
        let ms = |us: u64| us as f64 / 1_000.0;
        json!({
            "frame_decode_ms": ms(self.frame_decode_us),
            "input_prepare_ms": ms(self.input_prepare_us),
            "queue_ms": ms(self.queue_us),
            "lock_wait_ms": ms(self.lock_wait_us),
            "setup_ms": ms(self.setup_us),
            "decode_ms": ms(self.decode_us),
            "backend_enqueue_ms": ms(self.backend_enqueue_us),
            "backend_completion_ms": ms(self.backend_completion_us),
            "residual_capture_d2h_sync_ms": ms(self.residual_capture_d2h_sync_us),
            "residual_ms": ms(self.residual_us),
            "logits_access_ms": ms(self.logits_access_us),
            "logits_clone_ms": ms(self.logits_clone_us),
            "repetition_penalty_ms": ms(self.repetition_penalty_us),
            "softmax_ms": ms(self.softmax_us),
            "top_k_ms": ms(self.top_k_us),
            "top_p_ms": ms(self.top_p_us),
            "selection_ms": ms(self.selection_us),
            "token_text_ms": ms(self.token_text_us),
            "sampling_ms": ms(self.sampling_us),
            "encode_ms": ms(self.encode_us),
            "moe_forward_ms": ms(self.moe.forward_us),
            "moe_executor": self.moe_executor(),
            "moe_executors": self
                .moe
                .executors
                .iter()
                .map(|executor| executor.as_str())
                .collect::<Vec<_>>(),
            "moe_executor_dtypes": self.moe.dtypes,
            "moe_executor_reason": self.moe.reasons.first(),
            "moe_executor_reasons": self.moe.reasons,
            "moe_executor_diagnostics": self.moe.diagnostics,
            "selected_path": self.selected_path.as_str(),
            "executed_engine": self.executed_engine().as_str(),
            "executed_engines": self
                .executed_engines
                .iter()
                .map(|engine| engine.as_str())
                .collect::<Vec<_>>(),
            "engine_mismatch": self.engine_mismatch(),
            "mixed_execution": self.mixed_execution(),
            "tp": self
                .tp
                .as_ref()
                .map(|tp| serde_json::to_value(tp).unwrap_or(Value::Null)),
        })
    }
}

fn moe_health_summary(
    inference_path: crate::executor::InferencePath,
    telemetry: Option<&crate::moe::runtime::MoeExecutorTelemetry>,
    fallback_reason: Option<&str>,
) -> (bool, &'static str, Vec<u32>, Option<String>) {
    let requested = inference_path.is_moe();
    let executor = telemetry
        .map(|value| value.executor.as_str())
        .unwrap_or(if requested {
            "not-installed"
        } else {
            "not-requested"
        });
    let dtypes = telemetry
        .map(|value| value.dtypes.clone())
        .unwrap_or_default();
    let reason = telemetry
        .and_then(|value| value.reason.clone())
        .or_else(|| fallback_reason.map(str::to_owned));
    (requested, executor, dtypes, reason)
}

struct TimedReply {
    bytes: Vec<u8>,
    timings: NodeTimings,
}

struct PrefillJob {
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    reuse_prefix: bool,
    /// A decisão da cabeça que os hops intermediários só repassam. Um hop que
    /// não tokeniza não pode substituir `true` por seu `PrefixDecision::default`.
    cached_tokens: u32,
    /// Candidato de prefixo comum, inclusive quando a decisão foi fria.
    prefix_match_tokens: u32,
    start_pos: Option<u32>,
    /// Fronteira entre o prompt reutilizável e o rabicho deste turno, decidida
    /// por quem tokenizou - ver `Session::stable_len`.
    stable_len: Option<u32>,
    /// Bloco local onde parar, quando o remetente pediu menos que a fatia
    /// inteira. `None` é a fatia toda. Ver `resolve_exec_range`.
    capture_block: Option<u32>,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    chat_template: Option<bool>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    chunk_tokens: Option<Vec<i32>>,
    queued_at: Instant,
}

struct StepJob {
    sid_key: String,
    is_last: bool,
    remote_stream: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    /// Bloco local onde parar, quando o remetente pediu menos que a fatia
    /// inteira. `None` é a fatia toda. Ver `resolve_exec_range`.
    capture_block: Option<u32>,
    speculate: Option<Speculate>,
    incoming_draft: Option<Vec<i32>>,
    batch_tokens: Option<Vec<i32>>,
    mtp_k: Option<u32>,
    queued_at: Instant,
}

enum InferenceCommand {
    Prefill {
        job: PrefillJob,
        reply: oneshot::Sender<Result<TimedReply>>,
        _permit: OwnedSemaphorePermit,
    },
    Step {
        job: StepJob,
        reply: oneshot::Sender<Result<TimedReply>>,
        _permit: OwnedSemaphorePermit,
    },
}

impl InferenceCommand {
    fn into_queue_item(self) -> crate::scheduler::QueueItem<Self> {
        let (session_id, class, tokens, queued_at) = match &self {
            Self::Prefill { job, .. } => (
                job.sid_key.clone(),
                if job.tensor.is_some() {
                    crate::scheduler::WorkClass::PrefillHidden
                } else {
                    crate::scheduler::WorkClass::PrefillTokens
                },
                job.tensor
                    .as_ref()
                    .map(|(_, shape)| shape[0])
                    .unwrap_or(job.chunk_size as usize)
                    .max(1),
                job.queued_at,
            ),
            Self::Step { job, .. } => (
                job.sid_key.clone(),
                if job.tensor.is_some() {
                    crate::scheduler::WorkClass::DecodeHidden
                } else {
                    crate::scheduler::WorkClass::DecodeTokens
                },
                job.tensor
                    .as_ref()
                    .map(|(_, shape)| shape[0])
                    .unwrap_or(1)
                    .max(1),
                job.queued_at,
            ),
        };
        crate::scheduler::QueueItem {
            session_id,
            class,
            tokens,
            enqueued_at: queued_at,
            payload: self,
        }
    }
}

#[derive(Debug)]
enum SchedulerSubmitError {
    Full { limit: usize },
    Closed,
}

#[derive(Clone)]
struct SchedulerHandle {
    tx: mpsc::Sender<InferenceCommand>,
    limit: usize,
    admission: Arc<Semaphore>,
    stats: Arc<SchedulerStats>,
    config: crate::scheduler::SchedulerConfig,
}

#[derive(Default)]
struct SchedulerStats {
    queue_depth: AtomicUsize,
    last_batch_size: AtomicUsize,
    last_batch_tokens: AtomicUsize,
    total_batches: AtomicU64,
    total_items: AtomicU64,
}

impl SchedulerStats {
    fn json(&self, config: &crate::scheduler::SchedulerConfig) -> Value {
        json!({
            "enabled": config.enabled,
            "queue_depth": self.queue_depth.load(Ordering::Relaxed),
            "queue_limit": config.max_queue_requests,
            "batch_window_ms": config.batch_window.as_millis(),
            "max_batch_sequences": config.max_batch_sequences,
            "max_batch_tokens": config.max_batch_tokens,
            "last_batch_size": self.last_batch_size.load(Ordering::Relaxed),
            "last_batch_tokens": self.last_batch_tokens.load(Ordering::Relaxed),
            "total_batches": self.total_batches.load(Ordering::Relaxed),
            "total_items": self.total_items.load(Ordering::Relaxed),
        })
    }
}

impl SchedulerHandle {
    fn submit_prefill(
        &self,
        job: PrefillJob,
    ) -> std::result::Result<oneshot::Receiver<Result<TimedReply>>, SchedulerSubmitError> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| SchedulerSubmitError::Full { limit: self.limit })?;
        let (reply, receiver) = oneshot::channel();
        self.tx
            .try_send(InferenceCommand::Prefill {
                job,
                reply,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    SchedulerSubmitError::Full { limit: self.limit }
                }
                mpsc::error::TrySendError::Closed(_) => SchedulerSubmitError::Closed,
            })?;
        Ok(receiver)
    }

    fn submit_step(
        &self,
        job: StepJob,
    ) -> std::result::Result<oneshot::Receiver<Result<TimedReply>>, SchedulerSubmitError> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| SchedulerSubmitError::Full { limit: self.limit })?;
        let (reply, receiver) = oneshot::channel();
        self.tx
            .try_send(InferenceCommand::Step {
                job,
                reply,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    SchedulerSubmitError::Full { limit: self.limit }
                }
                mpsc::error::TrySendError::Closed(_) => SchedulerSubmitError::Closed,
            })?;
        Ok(receiver)
    }
}

#[derive(Clone)]
struct InferenceAppState {
    state: SharedState,
    scheduler: SchedulerHandle,
    /// TP exposes one global decode/barrier identity per process. Serialize
    /// complete inbound batches so a second request cannot replace that id
    /// while the first batch is waiting for its workers.
    tp_batch_gate: Arc<tokio::sync::Mutex<()>>,
    /// Keeps direct prefill forwarding in chunk order while the next chunk is
    /// already being computed by the preceding hop.
    direct_forward_order: DirectForwardOrder,
}

#[derive(Clone, Default)]
struct DirectForwardOrder {
    sequences: Arc<Mutex<HashMap<String, Arc<DirectForwardSequence>>>>,
}

struct DirectForwardSequence {
    next_chunk: Mutex<u32>,
    failed: Mutex<Option<String>>,
    notify: tokio::sync::Notify,
}

struct DirectForwardTurn {
    owner: DirectForwardOrder,
    key: String,
    sequence: Arc<DirectForwardSequence>,
    chunk_index: u32,
}

impl DirectForwardOrder {
    fn ensure_sequence(&self, key: &str) -> Result<()> {
        let mut sequences = self
            .sequences
            .lock()
            .map_err(|_| anyhow::anyhow!("ordem do repasse direto corrompida"))?;
        sequences.entry(key.to_owned()).or_insert_with(|| {
            Arc::new(DirectForwardSequence {
                next_chunk: Mutex::new(0),
                failed: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
            })
        });
        Ok(())
    }

    async fn wait_turn(
        &self,
        key: impl Into<String>,
        chunk_index: u32,
    ) -> Result<DirectForwardTurn> {
        let key = key.into();
        let sequence = {
            let mut sequences = self
                .sequences
                .lock()
                .map_err(|_| anyhow::anyhow!("ordem do repasse direto corrompida"))?;
            if let Some(sequence) = sequences.get(&key) {
                sequence.clone()
            } else if chunk_index == 0 {
                let sequence = Arc::new(DirectForwardSequence {
                    next_chunk: Mutex::new(0),
                    failed: Mutex::new(None),
                    notify: tokio::sync::Notify::new(),
                });
                sequences.insert(key.clone(), sequence.clone());
                sequence
            } else {
                return Err(anyhow::anyhow!(
                    "chunk direto {chunk_index} chegou sem o chunk predecessor"
                ));
            }
        };

        loop {
            let notified = sequence.notify.notified();
            if let Some(error) = sequence
                .failed
                .lock()
                .map_err(|_| anyhow::anyhow!("estado de ordem do repasse corrompido"))?
                .clone()
            {
                return Err(anyhow::anyhow!(error));
            }
            let next_chunk = *sequence
                .next_chunk
                .lock()
                .map_err(|_| anyhow::anyhow!("estado de ordem do repasse corrompido"))?;
            if next_chunk == chunk_index {
                drop(notified);
                return Ok(DirectForwardTurn {
                    owner: self.clone(),
                    key,
                    sequence,
                    chunk_index,
                });
            }
            if chunk_index < next_chunk {
                return Err(anyhow::anyhow!(
                    "chunk direto {chunk_index} chegou depois de {next_chunk}"
                ));
            }
            notified.await;
        }
    }
}

impl DirectForwardTurn {
    fn finish(self, success: bool, terminal: bool) {
        if success {
            if let Ok(mut next_chunk) = self.sequence.next_chunk.lock() {
                if *next_chunk == self.chunk_index {
                    *next_chunk += 1;
                }
            }
        } else if let Ok(mut failed) = self.sequence.failed.lock() {
            *failed = Some(format!(
                "repasse direto falhou no chunk {}",
                self.chunk_index
            ));
        }
        self.sequence.notify.notify_waiters();

        if terminal || !success {
            if let Ok(mut sequences) = self.owner.sequences.lock() {
                if sequences
                    .get(&self.key)
                    .is_some_and(|current| Arc::ptr_eq(current, &self.sequence))
                {
                    sequences.remove(&self.key);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::relay::InProcessInference for InferenceAppState {
    async fn dispatch(&self, path: &str, body: Bytes) -> Result<Response> {
        let headers = axum::http::HeaderMap::new();
        Ok(match path {
            "/v1/prefill" => handle_prefill(State(self.clone()), headers, body).await,
            "/v1/step" => handle_step(State(self.clone()), headers, body).await,
            "/v1/step-stream" => handle_step_stream(State(self.clone()), headers, body).await,
            "/v1/session/rewind" => {
                let request = serde_json::from_slice(&body)
                    .map_err(|e| anyhow::anyhow!("rewind inválido: {e}"))?;
                handle_rewind(
                    State(self.clone()),
                    axum::http::HeaderMap::new(),
                    Json(request),
                )
                .await
                .into_response()
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "rota de inferência em processo desconhecida: {path}"
                ))
            }
        })
    }
}

fn spawn_scheduler(state: SharedState, cfg: crate::scheduler::SchedulerConfig) -> SchedulerHandle {
    let limit = cfg.max_queue_requests;
    let (tx, rx) = mpsc::channel(limit);
    let admission = Arc::new(Semaphore::new(limit));
    let stats = Arc::new(SchedulerStats::default());
    let handle = SchedulerHandle {
        tx,
        limit,
        admission,
        stats: stats.clone(),
        config: cfg.clone(),
    };
    tokio::spawn(run_scheduler(rx, state, cfg, stats));
    handle
}

async fn run_scheduler(
    mut rx: mpsc::Receiver<InferenceCommand>,
    state: SharedState,
    cfg: crate::scheduler::SchedulerConfig,
    stats: Arc<SchedulerStats>,
) {
    let mut queue = crate::scheduler::SchedulerQueue::new(cfg.clone());
    let slot_freed = {
        let guard = state.lock().unwrap_or_else(|e| e.into_inner());
        guard.slot_freed.clone()
    };
    loop {
        if queue.is_empty() {
            let Some(command) = rx.recv().await else {
                return;
            };
            let _ = queue.try_push(command.into_queue_item());
        }

        while queue.len() < cfg.max_queue_requests {
            let Ok(command) = rx.try_recv() else {
                break;
            };
            if queue.try_push(command.into_queue_item()).is_err() {
                unreachable!("a fila foi verificada antes de inserir o comando");
            }
        }
        stats.queue_depth.store(queue.len(), Ordering::Relaxed);

        if cfg.batch_window > std::time::Duration::ZERO && queue.len() < cfg.max_batch_sequences {
            tokio::time::sleep(cfg.batch_window).await;
            while queue.len() < cfg.max_queue_requests {
                let Ok(command) = rx.try_recv() else {
                    break;
                };
                if queue.try_push(command.into_queue_item()).is_err() {
                    unreachable!("a fila foi verificada antes de inserir o comando");
                }
            }
            stats.queue_depth.store(queue.len(), Ordering::Relaxed);
        }

        let Some(front_class) = queue.front_class() else {
            continue;
        };

        // Admissão de sessão: um prefill de sessão nova só entra se houver slot
        // - livre, ou uma sessão retida despejável (`evict_retained`). Sem
        // slot, o item fica na fila FIFO em vez de ir ao worker e voltar com
        // "seq pool esgotado".
        let (present, new_slots) = {
            let guard = state.lock().unwrap_or_else(|e| e.into_inner());
            let present: std::collections::HashSet<String> =
                guard.sessions.keys().cloned().collect();
            let retained = guard.sessions.values().filter(|s| s.retained).count();
            (present, guard.seq_pool.len() + retained)
        };
        let is_existing = |sid: &str| present.contains(sid);

        // A classe da frente primeiro (FIFO por chegada); depois as demais, para
        // que a decodificação de uma sessão viva não fique presa atrás de um
        // prefill de sessão nova que está esperando um slot.
        let mut selection = crate::scheduler::BatchSelection {
            items: Vec::new(),
            total_tokens: 0,
        };
        for class in [
            front_class,
            crate::scheduler::WorkClass::PrefillTokens,
            crate::scheduler::WorkClass::PrefillHidden,
            crate::scheduler::WorkClass::DecodeTokens,
            crate::scheduler::WorkClass::DecodeHidden,
        ] {
            let sel = queue.take_batch(Instant::now(), class, is_existing, new_slots);
            if !sel.items.is_empty() {
                selection = sel;
                break;
            }
        }

        if selection.items.is_empty() {
            // Só há prefill de sessão nova esperando slot. Aguarda trabalho
            // novo OU um slot liberado, sem busy-loop.
            stats.queue_depth.store(queue.len(), Ordering::Relaxed);
            tokio::select! {
                command = rx.recv() => match command {
                    Some(command) => {
                        let _ = queue.try_push(command.into_queue_item());
                    }
                    None => return,
                },
                _ = slot_freed.notified() => {}
            }
            continue;
        }
        stats.queue_depth.store(queue.len(), Ordering::Relaxed);
        stats
            .last_batch_size
            .store(selection.items.len(), Ordering::Relaxed);
        stats
            .last_batch_tokens
            .store(selection.total_tokens, Ordering::Relaxed);
        stats.total_batches.fetch_add(1, Ordering::Relaxed);
        stats
            .total_items
            .fetch_add(selection.items.len() as u64, Ordering::Relaxed);
        let state = state.clone();
        let enabled = cfg.enabled;
        let _ = tokio::task::spawn_blocking(move || {
            execute_scheduled_batch(state, selection.items, enabled);
        })
        .await;
    }
}

fn execute_scheduled_batch(
    state: SharedState,
    items: Vec<crate::scheduler::QueueItem<InferenceCommand>>,
    enabled: bool,
) {
    if !enabled || items.len() == 1 {
        for item in items {
            execute_one_command(state.clone(), item.payload);
        }
        return;
    }
    let class = items[0].class;
    match class {
        crate::scheduler::WorkClass::PrefillTokens | crate::scheduler::WorkClass::PrefillHidden => {
            let mut jobs = Vec::with_capacity(items.len());
            for item in items {
                match item.payload {
                    InferenceCommand::Prefill { job, reply, .. } => jobs.push((job, reply)),
                    InferenceCommand::Step { reply, .. } => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "scheduler recebeu fase de inferência incompatível"
                        )));
                    }
                }
            }
            for (reply, result) in prefill_batch_blocking(state, jobs) {
                let _ = reply.send(result);
            }
        }
        crate::scheduler::WorkClass::DecodeTokens | crate::scheduler::WorkClass::DecodeHidden => {
            let mut jobs = Vec::with_capacity(items.len());
            for item in items {
                match item.payload {
                    InferenceCommand::Step { job, reply, .. } => jobs.push((job, reply)),
                    InferenceCommand::Prefill { reply, .. } => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "scheduler recebeu fase de inferência incompatível"
                        )));
                    }
                }
            }
            for (reply, result) in step_batch_blocking(state, jobs) {
                let _ = reply.send(result);
            }
        }
    }
}

fn execute_one_command(state: SharedState, command: InferenceCommand) {
    match command {
        InferenceCommand::Prefill { job, reply, .. } => {
            let result = prefill_blocking(
                state,
                job.sid_key,
                job.chunk_index,
                job.chunk_size,
                job.reuse_prefix,
                job.cached_tokens,
                job.prefix_match_tokens,
                job.start_pos,
                job.stable_len,
                job.is_last,
                job.is_final_chunk,
                job.wire_dtype,
                job.prompt,
                job.chat_template,
                job.tensor,
                job.capture_block,
                job.chunk_tokens,
                job.queued_at,
            );
            let _ = reply.send(result);
        }
        InferenceCommand::Step { job, reply, .. } => {
            let result = step_blocking(
                state,
                job.sid_key,
                job.is_last,
                job.remote_stream,
                job.wire_dtype,
                job.token_id,
                job.tensor,
                job.capture_block,
                job.speculate,
                job.incoming_draft,
                job.batch_tokens,
                job.mtp_k,
                job.queued_at,
            );
            let _ = reply.send(result);
        }
    }
}

fn inference_frame_body_limit() -> usize {
    INFERENCE_FRAME_BODY_LIMIT
}

fn lock_inference_state<T>(state: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex do estado de inferência envenenado"))
}

fn finish_terminal_stream(
    sessions: &mut HashMap<String, Session>,
    sid_key: &str,
    text: &mut String,
) -> Result<()> {
    let session = sessions
        .get_mut(sid_key)
        .ok_or_else(|| anyhow::anyhow!("sessão '{sid_key}' desconhecida"))?;
    text.push_str(&session.finish_token_stream());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::DefaultBodyLimit,
        http::Request,
    };
    use tower::ServiceExt;

    async fn accept_frame(body: Bytes) -> StatusCode {
        assert_eq!(body.len(), 4 * 1024 * 1024);
        StatusCode::NO_CONTENT
    }

    #[test]
    fn health_reports_the_context_window_per_session() {
        assert_eq!(context_per_session(400_000, 4), 100_000);
        assert_eq!(context_per_session(100_000, 0), 100_000);
    }

    #[test]
    fn resposta_de_item_unico_preserva_identidade_do_batch() {
        let request = json!({
            "item_id": "request-item",
            "batch_id": "request-batch",
        });
        let mut response = json!({"done": true});

        preserve_batch_identity(&request, &mut response);

        assert_eq!(response["item_id"], "request-item");
        assert_eq!(response["batch_id"], "request-batch");
    }

    #[test]
    fn serializacao_de_timing_preserva_submilissegundos() {
        let timings = NodeTimings {
            frame_decode_us: 999,
            backend_completion_us: 999,
            residual_us: 1_234,
            sampling_us: 1,
            ..Default::default()
        };

        let json = timings.json();

        assert_eq!(json["frame_decode_ms"].as_f64(), Some(0.999));
        assert_eq!(json["backend_completion_ms"].as_f64(), Some(0.999));
        assert_eq!(json["residual_ms"].as_f64(), Some(1.234));
        assert_eq!(json["sampling_ms"].as_f64(), Some(0.001));
        assert_ne!(json["backend_completion_ms"], serde_json::json!(0));
    }

    #[test]
    fn telemetria_separa_caminho_selecionado_do_executor_real() {
        let mut timings = NodeTimings {
            selected_path: crate::executor::InferencePath::MoeQ4_0,
            ..Default::default()
        };
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);

        let value = timings.json();

        assert_eq!(value["selected_path"], "moe/q4_0");
        assert_eq!(value["executed_engine"], "llama.cpp");
        assert_eq!(value["executed_engines"], serde_json::json!(["llama.cpp"]));
        assert!(value["engine_mismatch"].as_bool().unwrap());
        assert!(!value["mixed_execution"].as_bool().unwrap());
    }

    #[test]
    fn telemetria_sem_executor_observado_nao_inventa_divergencia() {
        let timings = NodeTimings {
            selected_path: crate::executor::InferencePath::DenseLlamaCpp,
            ..Default::default()
        };

        let value = timings.json();

        assert_eq!(value["executed_engine"], "unknown");
        assert_eq!(value["executed_engines"], serde_json::json!([]));
        assert!(!value["engine_mismatch"].as_bool().unwrap());
    }

    #[test]
    fn health_moe_expõe_pedido_sem_executor_e_motivo() {
        let (requested, executor, dtypes, reason) = moe_health_summary(
            crate::executor::InferencePath::MoeQ4_0,
            None,
            Some("executor MoE indisponível: formato sem decoder"),
        );
        assert!(requested);
        assert_eq!(executor, "not-installed");
        assert!(dtypes.is_empty());
        assert_eq!(
            reason.as_deref(),
            Some("executor MoE indisponível: formato sem decoder")
        );
    }

    #[test]
    fn telemetria_detecta_execucao_mista_no_mesmo_hop() {
        let mut timings = NodeTimings {
            selected_path: crate::executor::InferencePath::MoeQ4_0,
            ..Default::default()
        };
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
        timings.record_engine(crate::executor::ExecutionEngine::Moe);

        let value = timings.json();

        assert_eq!(value["executed_engine"], "unknown");
        assert_eq!(
            value["executed_engines"],
            serde_json::json!(["llama.cpp", "moe"])
        );
        assert!(!value["engine_mismatch"].as_bool().unwrap());
        assert!(value["mixed_execution"].as_bool().unwrap());
    }

    #[test]
    fn telemetria_expoe_backend_real_dos_experts_apos_forward() {
        let mut timings = NodeTimings {
            selected_path: crate::executor::InferencePath::MoeQ4_0,
            ..Default::default()
        };
        timings.record_moe_executor(crate::moe::runtime::MoeExecutorKind::Cuda);
        timings.record_moe_forward(12_345);

        let value = timings.json();

        assert_eq!(value["moe_executor"], "cuda");
        assert_eq!(value["moe_executors"], serde_json::json!(["cuda"]));
        assert_eq!(value["moe_forward_ms"].as_f64(), Some(12.345));
    }

    #[test]
    fn telemetria_nomeia_dtype_e_motivo_do_executor_cpu() {
        let mut timings = NodeTimings {
            selected_path: crate::executor::InferencePath::MoeQ4_0,
            ..Default::default()
        };
        timings.record_moe_telemetry(crate::moe::runtime::MoeExecutorTelemetry {
            executor: crate::moe::runtime::MoeExecutorKind::CpuReference,
            dtypes: vec![21, 22, 23],
            reason: Some(
                "dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado".into(),
            ),
            diagnostics: None,
        });

        let value = timings.json();

        assert_eq!(value["moe_executor"], "cpu-reference");
        assert_eq!(
            value["moe_executor_dtypes"],
            serde_json::json!([21, 22, 23])
        );
        assert_eq!(
            value["moe_executor_reason"],
            "dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado"
        );
    }

    #[test]
    fn modo_explicito_do_deploy_chega_ao_seletor_sem_ser_sobrescrito_pelo_ambiente() {
        assert_eq!(
            configured_inference_mode(crate::webui::InferenceMode::Dense).unwrap(),
            crate::executor::InferenceMode::Dense
        );
        assert_eq!(
            configured_inference_mode(crate::webui::InferenceMode::Moe).unwrap(),
            crate::executor::InferenceMode::Moe
        );
    }

    #[test]
    fn hop_terminal_moe_publica_moe_apos_forward_real() {
        let mut timings = NodeTimings {
            selected_path: crate::executor::InferencePath::MoeQ4_0,
            ..Default::default()
        };

        record_terminal_engine(&mut timings, crate::executor::InferencePath::MoeQ4_0, true);

        let value = timings.json();
        assert_eq!(value["executed_engine"], "moe");
        assert_eq!(value["executed_engines"], serde_json::json!(["moe"]));
        assert!(!value["engine_mismatch"].as_bool().unwrap());
        assert!(!value["mixed_execution"].as_bool().unwrap());
    }

    #[test]
    fn resposta_terminal_carrega_a_contagem_de_tokens_do_prompt() {
        let value = terminal_prefill_meta(33, 7, 8, false);

        assert_eq!(value["n_prompt_tokens"], 33);
        assert_eq!(value["start_pos"], 7);
        assert_eq!(value["next_pos"], 8);
    }

    #[test]
    fn terminal_stream_cleanup_keeps_session_for_rewind() {
        let mut sessions = HashMap::new();
        let mut session = Session::new(0);
        assert_eq!(session.feed_token_piece(b"ok"), "ok");
        sessions.insert("conversation".to_string(), session);
        let mut text = String::new();

        finish_terminal_stream(&mut sessions, "conversation", &mut text).unwrap();

        assert_eq!(text, "");
        assert!(sessions.contains_key("conversation"));
    }

    #[test]
    fn terminal_stream_cleanup_reports_unknown_session_without_panicking() {
        let mut sessions = HashMap::new();
        let mut text = String::new();

        let error = finish_terminal_stream(&mut sessions, "gone", &mut text).unwrap_err();

        assert!(error.to_string().contains("sessão 'gone' desconhecida"));
    }

    #[test]
    fn poisoned_inference_lock_returns_an_error_instead_of_panicking() {
        let state = Mutex::new(());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.lock().unwrap();
            panic!("simulate a local inference panic");
        }));

        let error = lock_inference_state(&state).unwrap_err();

        assert!(error.to_string().contains("mutex do estado de inferência"));
    }

    #[tokio::test]
    async fn prefill_route_accepts_a_default_f16_frame_larger_than_axums_two_mib_default() {
        let app = Router::new()
            .route("/v1/prefill", post(accept_frame))
            .layer(DefaultBodyLimit::max(inference_frame_body_limit()));
        let request = Request::post("/v1/prefill")
            .body(Body::from(vec![0_u8; 4 * 1024 * 1024]))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }

    /// O socket tem que atender IPv4 mesmo estando ligado em `[::]`, e é isso
    /// que `IPV6_V6ONLY=0` compra. O teste conecta por 127.0.0.1 de verdade,
    /// porque o defeito que ele guarda não aparece na leitura do código: o
    /// caminho antigo desligava a opção *depois* do bind, o que não tem efeito
    /// nenhum. No Linux passava por acidente (o padrão do sistema já é 0) e no
    /// Windows deixaria o servidor mudo para todo peer IPv4 da LAN.
    #[test]
    fn listener_em_ipv6_tambem_atende_ipv4() {
        let l = match listener_dual_stack(0) {
            Ok(l) => l,
            // Máquina sem IPv6 cai no IPv4 puro, e aí não há o que provar.
            Err(_) => return,
        };
        let port = l.local_addr().unwrap().port();
        if l.local_addr().unwrap().is_ipv4() {
            return;
        }

        let aceito = std::thread::spawn(move || l.accept().is_ok());
        let conectou = std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();

        assert!(conectou, "um cliente IPv4 não conseguiu conectar em [::]");
        assert!(aceito.join().unwrap(), "a conexão IPv4 não foi aceita");
    }

    #[test]
    fn listener_em_ipv6_tambem_atende_ipv6() {
        let l = match listener_dual_stack(0) {
            Ok(l) => l,
            Err(_) => return,
        };
        if l.local_addr().unwrap().is_ipv4() {
            return;
        }
        let port = l.local_addr().unwrap().port();
        let aceito = std::thread::spawn(move || l.accept().is_ok());
        let conectou = std::net::TcpStream::connect(("::1", port)).is_ok();

        assert!(conectou, "um cliente IPv6 não conseguiu conectar em [::]");
        assert!(aceito.join().unwrap(), "a conexão IPv6 não foi aceita");
    }

    /// O socket tem que pedir `SO_REUSEADDR`, como o `TcpListener::bind` da std
    /// pede sozinho.
    ///
    /// Sem isso, toda conexao que ficou em TIME_WAIT naquela porta - e sempre
    /// ha, porque o gateway sonda `/health` e manda inferencia o tempo todo -
    /// impede o proximo servidor de ligar por ate um minuto. O sintoma e
    /// "Address already in use" com `lsof` sem achar dono nenhum: TIME_WAIT nao
    /// tem processo. Foi o que travou toda implantacao de versao nova, dez
    /// tentativas seguidas.
    ///
    /// A flag se perdeu quando o socket passou a ser montado a mao com
    /// `socket2` para o dual-stack; um socket cru nao herda o padrao da std.
    #[test]
    fn o_listener_pede_reuse_address() {
        let l = match listener_dual_stack(0) {
            Ok(l) => l,
            Err(_) => return,
        };
        let sock = socket2::Socket::from(l);
        assert!(
            sock.reuse_address().unwrap(),
            "sem SO_REUSEADDR, TIME_WAIT segura a porta por ~60s a cada troca de versao"
        );
    }

    /// Porta ocupada tem que falhar *antes* de o modelo ser carregado.
    ///
    /// O `run` ligava o socket depois do `LlmEngine::load_with_cache`, então cada uma das
    /// 10 tentativas do supervisor carregava um modelo inteiro na VRAM para
    /// descobrir, no fim, que a porta estava tomada - minutos por tentativa, e
    /// memória de GPU presa em cargas descartadas. O sintoma no log era
    /// "encerrou com erro: Address already in use" logo depois de
    /// "llama context ready".
    ///
    /// Aqui isso vira uma garantia estrutural: `serve_port` é o primeiro passo
    /// do `run`, e devolve `Err` sem que nada caro tenha acontecido.
    #[tokio::test]
    async fn porta_ocupada_falha_de_imediato() {
        let ocupante = match listener_dual_stack(0) {
            Ok(l) => l,
            Err(_) => return,
        };
        let port = ocupante.local_addr().unwrap().port();

        // O caminho de um modelo que nao existe: se o `run` carregasse antes de
        // ligar, o erro falaria do arquivo (ou o processo morreria dentro do
        // llama.cpp). Falar de porta ocupada e a prova de que a ordem esta certa.
        let mut cfg = crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: "/nao/existe/modelo.gguf".into(),
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_mode: Default::default(),
            inference_port: port,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            slot_save_path: None,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
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
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            cycle_detection_enabled: true,
            cycle_detection_min_window_tokens: crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS,
            cycle_detection_max_window_tokens: crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS,
            cycle_detection_repetitions: crate::cycle::DEFAULT_CYCLE_REPETITIONS,
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
            tp: None,
        };
        cfg.inference_port = port;
        let (_tx, rx) = tokio::sync::watch::channel(0u64);

        let comecou = std::time::Instant::now();
        let erro = run(
            cfg,
            rx,
            0,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
            None,
            None,
            Arc::new(ModelCache::new()),
            false,
        )
        .await
        .expect_err("porta ocupada devia falhar");

        assert!(
            erro.to_string().to_lowercase().contains("address") || erro.to_string().contains("use"),
            "o erro tinha que ser o da porta, e foi: {erro}"
        );
        assert!(
            comecou.elapsed() < std::time::Duration::from_secs(1),
            "falhou depois de carregar alguma coisa: levou {:?}",
            comecou.elapsed()
        );
    }
}

/// `relay_and_bubble` against a real (in-process, on a loopback port) fake
/// next hop — a real `reqwest` round trip, not an in-process `oneshot` call,
/// since that's what a relaying hop actually does. No `LlmEngine` involved:
/// this is exactly the "new glue code" the plan flagged as the concentrated
/// risk (the wire meta assembled by hand here, `more_chunks`-style, is what
/// a bug like the prefill overlay issue would hide in).
#[cfg(test)]
mod relay_tests {
    use super::*;
    use axum::{body::to_bytes, routing::post as axum_post};

    fn step(node_id: &str, url: String) -> PipelineStep {
        PipelineStep {
            node_id: node_id.into(),
            inference_url: url,
            inference_candidates: Vec::new(),
            start_layer: 16,
            end_layer: 31,
            exec_start: 16,
            exec_end: 31,
            inference_frame_capabilities: vec!["relay/v1-chain".into()],
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        }
    }

    async fn spawn_fake_hop(
        handler: impl Fn() -> Response + Clone + Send + Sync + 'static,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/v1/step",
            axum_post(move |_body: Bytes| {
                let handler = handler.clone();
                async move { handler() }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn prepends_the_called_hops_own_entry_onto_the_bubbled_response() {
        let url = spawn_fake_hop(|| {
            let meta = json!({
                "done": true,
                "token_id": 42,
                "text": "!",
                "processing_ms": 5,
                "node_timings": {"decode_ms": 3},
            });
            let bytes = wire::encode(&meta, None, WireDtype::F16).unwrap();
            (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response()
        })
        .await;

        let hops = vec![step("tail", url)];
        let residual = vec![0.0f32; 8];
        let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));

        let meta = relay_and_bubble(
            &http,
            &hops,
            "/v1/step",
            json!({"session_id": "sess-1"}),
            (&residual, vec![1, 8]),
            WireDtype::F16,
            1,
            json!(7),
            json!({"decode_ms": 2}),
        )
        .await
        .expect("relay should succeed");

        assert_eq!(meta["done"], true);
        assert_eq!(meta["token_id"], 42);
        assert_eq!(meta["text"], "!");
        // Ours, not the tail's — the tail reported processing_ms=5.
        assert_eq!(meta["processing_ms"], 7);
        assert_eq!(meta["node_timings"]["decode_ms"], 2);

        let hop_timings = meta["hop_timings"].as_array().expect("hop_timings array");
        assert_eq!(hop_timings.len(), 1);
        assert_eq!(
            hop_timings[0]["processing_ms"], 5,
            "the tail's self-reported processing_ms"
        );
        assert_eq!(hop_timings[0]["node_timings"]["decode_ms"], 3);
    }

    #[tokio::test]
    async fn a_downstream_relay_failure_is_propagated_unchanged() {
        let url = spawn_fake_hop(|| {
            let failure = crate::relay::RelayFailure {
                failed_node_id: "deep-hop".into(),
                kind: "timeout".into(),
                message: "no response".into(),
            };
            (StatusCode::BAD_GATEWAY, Json(failure)).into_response()
        })
        .await;

        let hops = vec![step("directly-called-hop", url)];
        let residual = vec![0.0f32; 8];
        let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));

        let response = relay_and_bubble(
            &http,
            &hops,
            "/v1/step",
            json!({"session_id": "sess-1"}),
            (&residual, vec![1, 8]),
            WireDtype::F16,
            1,
            json!(7),
            json!({}),
        )
        .await
        .expect_err("the fake hop always fails");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let failure: crate::relay::RelayFailure = serde_json::from_slice(&body).unwrap();
        // Not "directly-called-hop": the failure must still name the peer
        // that actually died, several hops further down the chain.
        assert_eq!(failure.failed_node_id, "deep-hop");
        assert_eq!(failure.kind, "timeout");
    }

    /// The verified bug this ring implementation exists to not repeat: a
    /// bubbled ring response's `more_chunks`/`n_prompt_tokens` come from the
    /// *tail* (always `false`/absent, since only the head owns the prompt)
    /// unless the head overwrites them. Without `overlay_head_authoritative_fields`,
    /// a multi-chunk prompt on a ring pipeline would silently stop after one
    /// chunk — this proves the head's values win.
    #[test]
    fn the_heads_more_chunks_overwrites_the_tails_bubbled_false() {
        let local_meta = json!({
            "owns_tokens": true,
            "more_chunks": true,
            "n_prompt_tokens": 900,
            "start_pos": 640,
            "next_pos": 1152,
        });
        // What actually bubbled back from a tail that isn't the head.
        let mut bubbled = json!({"done": false, "more_chunks": false, "hop_timings": []});

        overlay_head_authoritative_fields(&local_meta, &mut bubbled);

        assert_eq!(bubbled["more_chunks"], true);
        assert_eq!(bubbled["n_prompt_tokens"], 900);
        assert_eq!(bubbled["start_pos"], 640);
        assert_eq!(bubbled["next_pos"], 1152);
    }

    /// A middle hop (never the head) must leave the bubbled response alone —
    /// its own `more_chunks`/`n_prompt_tokens` are meaningless placeholders,
    /// not a second, competing source of truth.
    #[test]
    fn a_non_head_hop_never_overlays_its_own_placeholder_fields() {
        let local_meta = json!({"owns_tokens": false, "more_chunks": false, "n_prompt_tokens": 0});
        let mut bubbled = json!({"done": true, "token_id": 7, "more_chunks": false});
        let before = bubbled.clone();

        overlay_head_authoritative_fields(&local_meta, &mut bubbled);

        assert_eq!(bubbled, before);
    }

    #[test]
    fn is_final_chunk_for_forward_uses_the_heads_own_determination() {
        let head = json!({"owns_tokens": true, "more_chunks": true});
        assert!(!is_final_chunk_for_forward(&head, false));

        let head_last_chunk = json!({"owns_tokens": true, "more_chunks": false});
        assert!(is_final_chunk_for_forward(&head_last_chunk, false));
    }

    #[test]
    fn is_final_chunk_for_forward_passes_through_for_a_non_head_hop() {
        let middle = json!({"owns_tokens": false, "more_chunks": false});
        assert!(is_final_chunk_for_forward(&middle, true));
        assert!(!is_final_chunk_for_forward(&middle, false));
    }

    #[test]
    fn direct_prompt_progress_preserves_the_continuation_position() {
        let inbound = json!({
            "prompt_progress": {
                "more_chunks": true,
                "n_prompt_tokens": 1200,
                "start_pos": 640,
                "next_pos": 1152,
            }
        });
        let mut result = json!({"done": false});

        apply_prompt_progress(&inbound, &mut result);

        assert_eq!(result["start_pos"], 640);
        assert_eq!(result["next_pos"], 1152);
        assert_eq!(result["n_prompt_tokens"], 1200);
    }

    #[test]
    fn step_forward_meta_carries_the_next_hops_execution_range() {
        let mut next = step("tail", "http://127.0.0.1:8182".into());
        next.exec_start = 20;
        next.exec_end = 27;
        let local_meta = json!({
            "draft": [11, 12],
            "tokens": [10, 11, 12],
            "mtp_k": 3,
        });

        let forwarded = step_forward_meta(
            "sess-1",
            &json!({"item_id": "item-1", "remote_stream": true}),
            &local_meta,
            &next,
        );

        assert_eq!(forwarded["session_id"], "sess-1");
        assert_eq!(forwarded["item_id"], "item-1");
        assert_eq!(forwarded["exec_start"], 20);
        assert_eq!(forwarded["exec_end"], 27);
        assert_eq!(forwarded["draft"], json!([11, 12]));
        assert_eq!(forwarded["tokens"], json!([10, 11, 12]));
        assert_eq!(forwarded["mtp_k"], 3);
        assert_eq!(forwarded["remote_stream"], true);
    }

    #[test]
    fn a_step_subrange_maps_to_the_local_capture_block() {
        let capture = resolve_exec_range(&json!({"exec_start": 16, "exec_end": 23}), 16, 31)
            .expect("a subrange at the slice start is executable");

        assert_eq!(capture, Some(7));
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

/// O que a cabeça **executou** com o prefixo retido no pedaço 0.
///
/// Viaja de volta ao gateway e adiante para os hops seguintes. A distinção
/// importa: o gateway escolhe *tentar* reusar, a cabeça é quem decide se deu, e
/// era exatamente essa diferença que a telemetria antiga não mostrava.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixDecision {
    pub(crate) reused: bool,
    pub(crate) cached_tokens: u32,
    /// Quantos tokens abriram o prefixo retido, mesmo quando a divergência
    /// obrigou o prefill a recomeçar a frio.
    pub(crate) matched_tokens: u32,
}

/// A regra do reuso, sem sessão nem motor por perto.
///
/// `retidos` são exatamente os tokens do prompt do turno anterior que estão no
/// KV, e portanto seu tamanho tem que ser `n_past`. O prefixo serve quando
/// esses tokens ainda abrem o prompt novo e sobra pelo menos um token para
/// decodificar:
/// aí o prefill continua da posição `n_past` e **nenhuma posição sai do KV**,
/// que é a diferença entre funcionar em todo modelo e depender de o motor
/// aceitar `llama_memory_seq_rm` parcial.
///
/// Note que a comparação é contra `n_past`, não contra o prompt retido inteiro:
/// o que o turno anterior guardou no KV termina antes do rabicho volátil do
/// template de conversa (ver `stable_prefix_len`). Divergir depois de `n_past`
/// é o caso **normal** - é onde a conversa cresceu.
///
/// Recusa quando os `n_past` tokens não abrem mais o prompt novo (a conversa
/// foi editada, trocou de template, mudou a lista de ferramentas), quando não
/// sobraria token para decodificar, e quando a sessão não tem prompt no KV.
/// Recusar custa o reuso daquele turno e nunca a requisição.
pub(crate) fn plan_prefix_reuse(retidos: &[i32], n_past: u32, novos: &[i32]) -> PrefixDecision {
    let comum = retidos
        .iter()
        .zip(novos)
        .take_while(|(old, new)| old == new)
        .count() as u32;
    if retidos.len() as u32 != n_past {
        return PrefixDecision {
            matched_tokens: comum.min(n_past),
            ..PrefixDecision::default()
        };
    }
    // O último token do novo prompt tem de ser decodificado: é dele que saem os
    // logits do primeiro token gerado.
    let cabe_decodificar = (n_past as usize) < novos.len();
    if n_past > 0 && comum >= n_past && cabe_decodificar {
        PrefixDecision {
            reused: true,
            cached_tokens: n_past,
            matched_tokens: n_past,
        }
    } else {
        PrefixDecision {
            matched_tokens: comum.min(n_past),
            ..PrefixDecision::default()
        }
    }
}

/// Quantos tokens do começo deste prompt continuam valendo quando a conversa
/// crescer - a fronteira entre o que vale a pena reter e o que é deste turno.
///
/// **Medido, não presumido.** O template de conversa fecha o prompt com o
/// rabicho que abre o turno do assistente (`<|im_end|>\n<|im_start|>assistant\n`
/// no chatml, outra coisa noutro modelo). Esse rabicho fica no *fim* do prompt,
/// então o prompt de um turno nunca é prefixo do prompt do turno seguinte: o
/// texto novo entra antes dele. Medido no Ornith-1.0-35B: prompt de 50 tokens,
/// 45 estáveis, 5 voláteis - e era exatamente por 5 tokens que todo reuso era
/// recusado, com a retenção e o cache funcionando perfeitamente.
///
/// A sonda renderiza o mesmo prompt com a continuação que `render_prompt`
/// acrescentaria e compara os tokens: o prefixo comum é, por construção, a
/// parte que sobrevive. Vale para qualquer template e qualquer tokenizador,
/// porque é medição e não suposição sobre o formato. Sonda conservadora custa
/// alguns tokens de reuso; nunca corrompe, porque `plan_prefix_reuse` só
/// aproveita o que está de fato no KV.
fn stable_prefix_len(
    engine: &crate::llm::LlmEngine,
    texto: &str,
    use_tmpl: bool,
    no_think: bool,
    tokens: &[i32],
) -> usize {
    // A forma exata do que o turno seguinte acrescenta - ver `render_prompt`.
    const CONTINUACAO: &str = "\n\nAssistant: x";
    let sondado = if use_tmpl {
        match engine.apply_chat_template(
            &[("user", &format!("{texto}{CONTINUACAO}"))],
            true,
            no_think,
        ) {
            Ok(rendered) => rendered,
            // Sem sonda não há fronteira medida: não retenha nada que possa
            // depender de um template que não conseguimos comparar.
            Err(_) => return 0,
        }
    } else {
        format!("{texto}{CONTINUACAO}")
    };
    let sondados = engine.tokenize(&sondado, !use_tmpl, true);
    let comum = tokens
        .iter()
        .zip(&sondados)
        .take_while(|(a, b)| a == b)
        .count();
    // Reserve the final prompt token for `gen_seq`. Without this cap, a raw
    // prompt whose continuation tokenizes independently could leave the whole
    // prompt in `prompt_seq`, putting the first generated token back into the
    // retained sequence.
    retainable_prefix_len(comum, tokens.len())
}

fn retainable_prefix_len(common: usize, prompt_len: usize) -> usize {
    common.min(prompt_len.saturating_sub(1))
}

/// A posição absoluta do KV em que este pedaço de prompt deve começar.
///
/// `start_pos` vem do peer que tokenizou. Ele só pode faltar no pedaço 0, onde
/// a posição da própria sessão é a referência: zero numa sessão recomeçada,
/// ou o fim do prefixo retido quando houve reuso. Para qualquer pedaço
/// posterior, inferir a posição com `chunk_index * chunk_size` é incorreto:
/// um pedaço anterior pode ter terminado no token 42, não no múltiplo 512.
/// Recusar o frame sem posição nomeia a divergência e deixa o gateway fazer o
/// fallback frio, em vez de misturar KV de sessões diferentes.
fn expected_prefill_pos(
    start_pos: Option<u32>,
    chunk_index: u32,
    session_n_past: u32,
) -> Result<u32> {
    match start_pos {
        Some(position) => Ok(position),
        None if chunk_index == 0 => Ok(session_n_past),
        None => bail!(
            "chunk {chunk_index} sem start_pos absoluto; não é seguro derivar a posição do índice"
        ),
    }
}

#[cfg(test)]
mod prefix_rule_tests {
    use super::*;

    /// O caso do relatório: o cache acerta, o pedaço 0 é julgado por zero, e o
    /// reuso é desfeito antes de decodificar.
    #[test]
    fn chunk_zero_of_a_hit_starts_where_the_retained_prefix_ends() {
        assert_eq!(expected_prefill_pos(None, 0, 1523).unwrap(), 1523);
    }

    #[test]
    fn chunk_zero_of_a_cold_session_starts_at_zero() {
        assert_eq!(expected_prefill_pos(None, 0, 0).unwrap(), 0);
    }

    /// Do pedaço 1 em diante quem manda é a posição que o peer devolveu.
    #[test]
    fn later_chunks_follow_the_position_the_head_reported() {
        assert_eq!(expected_prefill_pos(Some(1600), 1, 1600).unwrap(), 1600);
        assert!(expected_prefill_pos(None, 2, 1024).is_err());
    }

    #[test]
    fn partial_head_prefill_publishes_absolute_progress() {
        let meta = partial_prefill_meta(true, 600, 42, 512);

        assert_eq!(meta["done"], false);
        assert_eq!(meta["more_chunks"], true);
        assert_eq!(meta["n_prompt_tokens"], 600);
        assert_eq!(meta["start_pos"], 42);
        assert_eq!(meta["next_pos"], 512);
    }

    #[test]
    fn a_conversation_that_only_grows_reuses_the_whole_previous_prompt() {
        let turno1 = vec![10, 11, 12, 13];
        let turno2 = vec![10, 11, 12, 13, 20, 21];
        let d = plan_prefix_reuse(&turno1, 4, &turno2);
        assert!(d.reused);
        assert_eq!(d.cached_tokens, 4);
    }

    /// Divergência no meio exigiria encolher o KV. Recomeçar é a resposta, e
    /// ela não custa nada além do reuso daquele turno.
    #[test]
    fn a_prompt_that_diverges_restarts_instead_of_shrinking_the_cache() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 4, &[10, 11, 99, 13, 20]);
        assert!(!d.reused);
        assert_eq!(d.cached_tokens, 0);
        assert_eq!(d.matched_tokens, 2);
    }

    /// Prompt idêntico ao retido: não sobraria token para decodificar, e recuar
    /// um só continuaria sendo remoção parcial. Recomeça.
    #[test]
    fn an_identical_prompt_restarts_because_nothing_would_be_left_to_decode() {
        let tokens = vec![10, 11, 12, 13];
        let d = plan_prefix_reuse(&tokens, 4, &tokens);
        assert!(!d.reused);
    }

    #[test]
    fn stable_frontier_always_leaves_a_prompt_token_for_generation() {
        assert_eq!(retainable_prefix_len(9, 10), 9);
        assert_eq!(retainable_prefix_len(10, 10), 9);
        assert_eq!(retainable_prefix_len(1, 1), 0);
        assert_eq!(retainable_prefix_len(0, 0), 0);
    }

    /// O KV pode ter menos posições que os tokens guardados (turno anterior
    /// interrompido no meio do prefill). Reusar até `n_past` seria mentira
    /// sobre o que está no cache.
    #[test]
    fn a_partially_ingested_prompt_is_not_offered_as_a_prefix() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 2, &[10, 11, 12, 13, 20]);
        assert!(!d.reused, "o retido tem 4 tokens mas só 2 estão no KV");
    }

    #[test]
    fn a_session_with_no_prompt_in_the_cache_is_not_a_hit() {
        assert!(!plan_prefix_reuse(&[], 0, &[10, 11]).reused);
    }

    /// Cada sessão tem uma sequência de prompt e a de geração emparelhada com
    /// ela - ver `llm::kv_seq_budget`. Os dois conjuntos não podem se cruzar.
    #[test]
    fn prompt_and_generation_sequences_never_collide() {
        let max_sessions = 4u32;
        let orcamento = crate::llm::kv_seq_budget(max_sessions);
        assert_eq!(orcamento, 8);
        let prompts: Vec<u32> = (0..max_sessions).collect();
        let geracoes: Vec<u32> = prompts.iter().map(|s| s + max_sessions).collect();
        for g in &geracoes {
            assert!(
                !prompts.contains(g),
                "sequência {g} servindo aos dois papéis"
            );
            assert!(
                *g < orcamento,
                "sequência {g} fora do orçamento do contexto"
            );
        }
    }
}

/// Uma conversa viva neste peer, com o KV do prompt separado do KV da geração.
///
/// **O prompt nunca é estendido.** `prompt_seq` recebe os pedaços do prompt e
/// para aí; assim que o turno vai gerar, `open_generation` copia esse KV para
/// `gen_seq` e todo token gerado cai na cópia. No fim do turno a cópia é
/// apagada inteira - `kv_remove(gen, 0, -1)`, remoção total, que todo motor
/// aceita - e `prompt_seq` continua exatamente com o prompt, pronto para o
/// turno seguinte. É por isso que o reuso de prefixo não depende de o modelo
/// aceitar remoção parcial de KV: nada precisa ser removido do meio.
struct Session {
    /// Sequência do prompt. Só o prefill escreve nela.
    prompt_seq: u32,
    /// Sequência da geração deste turno, aberta na primeira decodificação de
    /// token gerado. `None` entre turnos.
    gen_seq: Option<u32>,
    /// Quantos tokens de prompt estão no KV de `prompt_seq`. É a posição para
    /// onde a sessão volta no fim do turno, e o que o gateway confirma no
    /// `/v1/session/rewind`.
    prompt_len: u32,
    /// Onde acaba a parte do prompt que sobrevive ao turno seguinte, e portanto
    /// onde a sequência de geração começa - ver `stable_prefix_len`. Tudo daqui
    /// para a frente (o rabicho do template e os tokens gerados) vive na cópia
    /// e some no fim do turno. `u32::MAX` é "ainda não sei", que mantém tudo na
    /// sequência do prompt: é o estado dos hops antes de a cabeça informar.
    stable_len: u32,
    n_past: u32,
    recent_tokens: Vec<i32>,
    /// Stored by the first peer so subsequent chunks don't re-tokenize.
    prompt_tokens: Vec<i32>,
    /// UTF-8 tail for this request's token stream. It must never be shared
    /// between sessions because a byte prefix from one generation can only be
    /// completed by a later token in that same generation.
    utf8: crate::llm::Utf8Assembler,
    /// `true` entre turnos: o KV guarda só o prompt e ninguém está gerando.
    /// Uma sessão retida é cache reclamável - `alloc_seq` pode despejá-la
    /// quando falta slot, e o turno seguinte apenas perde o reuso.
    retained: bool,
    last_used: Instant,
}

impl Session {
    fn new(seq_id: u32) -> Self {
        Self {
            prompt_seq: seq_id,
            gen_seq: None,
            prompt_len: 0,
            stable_len: u32::MAX,
            n_past: 0,
            recent_tokens: Vec::new(),
            prompt_tokens: Vec::new(),
            utf8: crate::llm::Utf8Assembler::new(),
            retained: false,
            last_used: Instant::now(),
        }
    }

    /// Onde a próxima decodificação escreve: a cópia de geração quando o turno
    /// já está gerando, a sequência do prompt enquanto ele ainda entra.
    fn seq_id(&self) -> u32 {
        self.gen_seq.unwrap_or(self.prompt_seq)
    }

    fn feed_token_piece(&mut self, piece: &[u8]) -> String {
        self.utf8.feed(piece)
    }

    fn finish_token_stream(&mut self) -> String {
        self.utf8.flush()
    }
}

#[cfg(test)]
mod session_text_tests {
    use super::*;

    /// Reproduz o caminho de emissão que alimenta o delta do chat: duas peças
    /// de token chegam em chamadas distintas, mas o consumidor precisa ver o
    /// caractere inteiro, não dois U+FFFD criados por conversões independentes.
    #[test]
    fn streaming_token_pieces_reach_the_consumer_as_one_utf8_character() {
        let mut session = Session::new(0);

        let mut emitted = String::new();
        emitted.push_str(&session.feed_token_piece(b"caf\xc3"));
        emitted.push_str(&session.feed_token_piece(b"\xa9"));

        assert_eq!(emitted, "café");
    }

    /// O fim da sessão precisa drenar um prefixo truncado uma única vez.
    #[test]
    fn ending_stream_flushes_pending_bytes_without_leaking_them() {
        let mut session = Session::new(0);

        assert_eq!(session.feed_token_piece(b"ok\xc3"), "ok");
        assert_eq!(session.finish_token_stream(), "\u{fffd}");
        assert_eq!(session.finish_token_stream(), "");
    }

    /// Duas requisições intercaladas não podem compartilhar o rabicho de UTF-8.
    #[test]
    fn concurrent_sessions_keep_pending_utf8_isolated() {
        let mut first = Session::new(0);
        let mut second = Session::new(1);

        assert_eq!(first.feed_token_piece(b"Jo\xc3"), "Jo");
        assert_eq!(second.feed_token_piece(b"\xf0\x9f"), "");
        assert_eq!(first.feed_token_piece(b"\xa3o"), "ão");
        assert_eq!(second.feed_token_piece(b"\x98\x80"), "😀");
    }
}

// ── Snapshots de KV cache (`--slot-save-path`) ───────────────────────────────

/// Versão do arquivo sidecar. Sobe quando o formato do `.json` muda de forma
/// incompatível; o `.bin` é o formato do próprio llama.cpp (`LLAMA_STATE_SEQ_*`).
const SLOT_SIDECAR_VERSION: u32 = 1;

/// Metadados que acompanham cada `<name>.bin`, gravados ao lado como
/// `<name>.json`. O `.bin` do llama.cpp valida a geometria de KV (contagem de
/// camadas, tipo e tamanho de linha) ao restaurar, mas **não** valida o modelo
/// nem a faixa de camadas; é o que este sidecar cobre antes de tocar no motor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SlotSidecar {
    format_version: u32,
    /// Rótulo técnico do modelo. Igual em todos os peers da implantação.
    model_label: String,
    /// Caminho do GGUF neste peer (uma fatia, ou o modelo inteiro).
    model_path: String,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    /// Largura do hidden state do motor carregado.
    n_embd: u32,
    /// Quantos tokens de prompt o snapshot cobre (a fronteira estável retida,
    /// `Session::prompt_len`). É a posição em que a sessão restaurada fica.
    retained_tokens: u32,
    /// Tokens reais do prompt quando este peer os conhece (a cabeça); vazio nos
    /// hops que só viram hidden state. O `.bin` carrega um cabeçalho de tokens
    /// próprio (consultivo), então este campo serve só ao reuso de prefixo.
    #[serde(default)]
    prompt_tokens: Vec<i32>,
    /// RFC3339 de quando o snapshot foi gravado.
    saved_at: String,
    /// Tamanho do `.bin` em bytes, como o llama.cpp reportou.
    bin_bytes: u64,
}

/// Recusa um nome de snapshot que não seja um único componente de caminho
/// seguro. Mesma disciplina de `transfer::safe_model_path`: sem separador, sem
/// `..`, sem nome vazio ou oculto. O gateway compõe `"<nome>__<node_id>"`, e o
/// `node_id` é hex de hash, então o conjunto `[A-Za-z0-9._-]` basta.
fn sanitize_slot_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("nome de snapshot vazio");
    }
    if name.len() > 200 {
        bail!("nome de snapshot longo demais (máx 200)");
    }
    if name.starts_with('.') {
        bail!("nome de snapshot não pode começar com '.'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("nome de snapshot só aceita letras, dígitos, '.', '_' e '-'");
    }
    Ok(name)
}

fn slot_bin_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.bin"))
}

fn slot_json_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.json"))
}

/// Apaga o `.bin` e o `.json` de um snapshot. Devolve `true` quando algo foi
/// removido; um snapshot ausente não é erro (mesma ética de
/// `/v1/session/release`).
fn erase_slot(dir: &std::path::Path, name: &str) -> Result<bool> {
    let name = sanitize_slot_name(name)?;
    let mut removed = false;
    for path in [slot_bin_path(dir, name), slot_json_path(dir, name)] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(anyhow::anyhow!("apagar {}: {error}", path.display())),
        }
    }
    Ok(removed)
}

/// Lê os sidecars de todos os snapshots num diretório. Um `.json` corrompido é
/// registrado e pulado, não derruba a listagem.
fn list_slots(dir: &std::path::Path) -> Vec<(String, SlotSidecar)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // O índice do cache de prefixo do gateway mora no mesmo diretório e
        // não é um sidecar de snapshot.
        if name == "prefix-auto-index" {
            continue;
        }
        match std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| serde_json::from_slice::<SlotSidecar>(&raw).map_err(anyhow::Error::from))
        {
            Ok(sidecar) => out.push((name.to_string(), sidecar)),
            Err(error) => warn!("sidecar de snapshot {} ilegível: {error}", path.display()),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Resultado de `InferState::restore_slot`, ecoado na resposta da rota para o
/// gateway conferir que todos os hops restauraram a mesma fronteira.
struct SlotRestoreOutcome {
    restored_tokens: u32,
}

#[cfg(test)]
mod slot_name_tests {
    use super::sanitize_slot_name;

    #[test]
    fn accepts_a_plain_component_and_the_gateway_node_namespacing() {
        assert_eq!(sanitize_slot_name("demo").unwrap(), "demo");
        assert_eq!(
            sanitize_slot_name("demo__a1b2c3d4e5f6").unwrap(),
            "demo__a1b2c3d4e5f6"
        );
        assert_eq!(sanitize_slot_name("  trimmed  ").unwrap(), "trimmed");
    }

    #[test]
    fn refuses_path_traversal_and_separators() {
        for bad in ["", ".", "..", "a/b", "a\\b", "../escape", ".hidden", "a b", "sÿm"] {
            assert!(
                sanitize_slot_name(bad).is_err(),
                "esperava recusar {bad:?}"
            );
        }
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

struct InferState {
    engine: LlmEngine,
    moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
    moe_fallback_reason: Option<String>,
    sessions: HashMap<String, Session>,
    seq_pool: Vec<u32>,
    max_sessions: u32,
    sampling: SamplingConfig,
    no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    chat_template: bool,
    start_layer: u32,
    end_layer: u32,
    /// Total de camadas da implantação (`NodeConfig::total_layers`). Guardado
    /// para validar um snapshot de KV antes de restaurá-lo.
    total_layers: u32,
    /// Rótulo técnico do modelo servido, compartilhado por todos os peers da
    /// mesma implantação. Identidade do snapshot de KV.
    model_label: String,
    /// Caminho do GGUF carregado neste peer. Identidade do snapshot de KV.
    model_path: String,
    /// Diretório de snapshots de KV cache (`Config::slot_save_path`), ou `None`
    /// quando o recurso está desligado nesta máquina. Ver `save_slot`.
    slot_dir: Option<std::path::PathBuf>,
    /// Acordado sempre que um slot de sequência pode ter ficado livre (fim de
    /// sessão, retenção, coleta de ociosidade, despejo). O `run_scheduler`
    /// espera nele quando o item da frente é um prefill de sessão nova e não
    /// há slot: a requisição fica na fila FIFO em vez de ser recusada com
    /// "seq pool esgotado".
    slot_freed: Arc<tokio::sync::Notify>,
    original_block_count: Option<u32>,
    inference_path: crate::executor::InferencePath,
    /// Corte de confiança do rascunho MTP — ver `NodeConfig::mtp_min_probability`.
    mtp_min_probability: f32,
    /// Duração do último handshake TLS aceito por este servidor, em ms.
    ///
    /// Medida no laço de accept (`serve.rs`), que é onde o `TlsAcceptor` roda e
    /// o tempo é exato. Sai daqui para o `meta` da resposta, e de lá para a
    /// telemetria por salto do gateway. `None` quando o nó não tem TLS.
    tls_handshake_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Identidade TP deste nó. O rank 0 precisa da lista de membros para pôr
    /// os workers no mesmo decode; sem isso a barreira nunca fecha.
    tp: Option<crate::network::TpIdentity>,
}

type SharedState = Arc<Mutex<InferState>>;

fn use_chat_template(requested: Option<bool>, configured_default: bool) -> bool {
    requested.unwrap_or(configured_default)
}

fn record_terminal_engine(
    timings: &mut NodeTimings,
    path: crate::executor::InferencePath,
    moe_forwarded: bool,
) {
    if path.is_moe() && moe_forwarded {
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
}

/// Anota no meta o que este peer **executou** com o prefixo retido.
///
/// Só o pedaço 0 carrega a decisão, porque só ele pode reusar. Os hops
/// seguintes obedecem a este campo em vez do pedido original do gateway: quem
/// tokeniza é quem sabe se o prefixo servia, e um hop reusando enquanto a
/// cabeça recomeçou (ou o contrário) desalinha as posições de todo o pipeline.
fn apply_prefix_decision(
    meta: &mut Value,
    chunk_index: u32,
    prefix: PrefixDecision,
    requested_reuse: bool,
    requested_cached_tokens: u32,
    requested_prefix_match_tokens: u32,
    owns_tokens: bool,
) {
    if chunk_index != 0 {
        return;
    }
    if owns_tokens {
        meta["reuse_prefix"] = json!(prefix.reused);
        meta["cached_tokens"] = json!(prefix.cached_tokens);
        meta["prefix_match_tokens"] = json!(prefix.matched_tokens);
    } else {
        // Só a cabeça pode decidir se os tokens batem. Todos os hops
        // intermediários devem transportar a decisão autoritativa recebida;
        // recalculá-la como `false` faria o hop seguinte resetar a sessão e
        // depois receber um hidden state com posição positiva.
        meta["reuse_prefix"] = json!(requested_reuse);
        meta["cached_tokens"] = json!(requested_cached_tokens);
        meta["prefix_match_tokens"] = json!(requested_prefix_match_tokens);
    }
}

#[cfg(test)]
mod prefix_decision_tests {
    use super::*;

    #[tokio::test]
    async fn direct_forward_waits_for_the_previous_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-1", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move {
            waiter_order.wait_turn("prefill-1", 1).await.map(|turn| {
                turn.finish(true, true);
                1
            })
        });

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!second.is_finished());
        first.finish(true, false);
        assert_eq!(second.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn direct_forward_failure_reaches_the_next_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-2", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move { waiter_order.wait_turn("prefill-2", 1).await });

        tokio::task::yield_now().await;
        first.finish(false, false);
        let error = match second.await.unwrap() {
            Ok(_) => panic!("o chunk seguinte não pode ultrapassar uma falha"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("repasse direto falhou"));
    }

    #[test]
    fn an_intermediate_hop_echoes_the_head_decision() {
        let mut meta = json!({
            "reuse_prefix": false,
            "cached_tokens": 0,
        });

        apply_prefix_decision(&mut meta, 0, PrefixDecision::default(), true, 42, 17, false);

        assert_eq!(meta["reuse_prefix"], true);
        assert_eq!(meta["cached_tokens"], 42);
        assert_eq!(meta["prefix_match_tokens"], 17);
    }

    #[test]
    fn the_token_owner_reports_what_it_executed() {
        let mut meta = json!({
            "reuse_prefix": true,
            "cached_tokens": 99,
        });

        apply_prefix_decision(
            &mut meta,
            0,
            PrefixDecision {
                reused: false,
                cached_tokens: 0,
                matched_tokens: 0,
            },
            true,
            42,
            0,
            true,
        );

        assert_eq!(meta["reuse_prefix"], false);
        assert_eq!(meta["cached_tokens"], 0);
    }
}

/// Anota no meta a fronteira medida por quem tokenizou, para os hops seguintes
/// partirem o prompt na mesma posição. Ver `Session::stable_len`.
fn apply_stable_len(meta: &mut Value, stable_len: u32) {
    if stable_len != u32::MAX {
        meta["stable_len"] = json!(stable_len);
    }
}

fn partial_prefill_meta(
    more_chunks: bool,
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
) -> Value {
    json!({
        "done": false,
        "more_chunks": more_chunks,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

fn terminal_prefill_meta(
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
    done: bool,
) -> Value {
    json!({
        "done": done,
        "more_chunks": false,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

/// A porta de inferência aceita a credencial compartilhada do cluster ou uma
/// API key ativa. O gateway usa a connection_key nas chamadas internas; a
/// alternativa de API key mantém o contrato dos clientes OpenAI/Anthropic.
async fn require_inference_auth(
    State((config, database)): State<(crate::auth::AuthState, Option<Arc<crate::db::PomDb>>)>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.strip_prefix("Bearer ").unwrap_or(value))
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        });

    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing inference credential").into_response();
    };

    if !inference_credential_is_valid(&config, database.as_ref(), token).await {
        return (StatusCode::FORBIDDEN, "invalid inference credential").into_response();
    }

    next.run(request).await
}

/// A regra de credencial da porta de inferência, isolada do middleware porque
/// o canal persistente (`/v1/link`) precisa **da mesma regra por quadro**.
///
/// Uma conexão longa que autenticasse só na abertura manteria válida por horas
/// uma API key revogada por outro processo - exatamente o que a revalidação por
/// requisição existe para impedir. O custo é o mesmo de hoje: tráfego entre
/// peers casa na `connection_key` e nem chega ao banco.
pub(crate) async fn inference_credential_is_valid(
    config: &crate::auth::AuthState,
    database: Option<&Arc<crate::db::PomDb>>,
    token: &str,
) -> bool {
    let cfg = config.read().await;
    if !cfg.connection_key.is_empty() && token == cfg.connection_key {
        return true;
    }
    match database {
        Some(database) => database.api_key_for_token(token).ok().flatten().is_some(),
        None => crate::auth::is_valid_inference_token(&cfg, token),
    }
}

/// A credencial que veio nos cabeçalhos, na mesma ordem que o middleware usa.
fn inference_credential_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.strip_prefix("Bearer ").unwrap_or(value))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
        .map(ToOwned::to_owned)
}

impl InferState {
    fn new(
        engine: LlmEngine,
        cfg: &NodeConfig,
        inference_path: crate::executor::InferencePath,
        moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
        moe_fallback_reason: Option<String>,
    ) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        Self {
            tls_handshake_ms: None,
            engine,
            moe,
            moe_fallback_reason,
            sessions: HashMap::new(),
            seq_pool,
            max_sessions,
            sampling: SamplingConfig {
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                top_k: cfg.top_k as i32,
                repeat_penalty: cfg.repeat_penalty,
                repeat_last_n: cfg.repeat_last_n as usize,
            },
            no_thinking: cfg.no_thinking,
            // Mantém o comportamento dos chamadores antigos que não enviam a
            // flag. O gateway atual sempre a explicita por endpoint.
            chat_template: cfg.no_thinking,
            start_layer: cfg.start_layer,
            end_layer: cfg.end_layer,
            total_layers: cfg.total_layers,
            model_label: cfg.model_label.clone(),
            model_path: cfg.model_path.clone(),
            slot_dir: cfg
                .slot_save_path
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(std::path::PathBuf::from),
            slot_freed: Arc::new(tokio::sync::Notify::new()),
            original_block_count: None, // filled in after engine load
            inference_path,
            mtp_min_probability: cfg.mtp_min_probability,
            tp: cfg.tp.clone(),
        }
    }

    fn apply_moe_hidden(
        &self,
        hidden: Vec<f32>,
        n_tokens: usize,
        capture_block: Option<u32>,
        timings: &mut NodeTimings,
    ) -> Result<Vec<f32>> {
        // A runtime object is not ownership. Only the explicitly selected MoE
        // path may claim the boundary block; a dense model must never execute
        // the sidecar merely because one was accidentally constructed.
        if !self.inference_path.is_moe() {
            return Ok(hidden);
        }
        if self.engine.moe_hook_enabled() {
            return Ok(hidden);
        }
        let Some(moe) = self.moe.as_ref() else {
            if let Some(reason) = self.moe_fallback_reason.as_deref() {
                timings.record_moe_reason(reason);
            }
            return Ok(hidden);
        };
        let layer = capture_block.unwrap_or_else(|| moe.layers().saturating_sub(1));
        let forward_started = Instant::now();
        let output = moe
            .forward_batch(layer, &hidden, n_tokens)
            .map_err(|error| anyhow::anyhow!("forward MoE falhou: {error}"))?;
        timings.record_moe_telemetry(moe.executor_telemetry());
        timings.record_moe_forward(forward_started.elapsed().as_micros() as u64);
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
        Ok(output)
    }

    fn alloc_seq(&mut self) -> Option<u32> {
        if self.seq_pool.is_empty() {
            self.reap_idle();
        }
        if self.seq_pool.is_empty() {
            // Uma sessão retida é só cache de prompt entre turnos: devolvê-la
            // custa o reuso de um turno, e não devolvê-la custa a requisição
            // inteira ("seq pool esgotado"). O cache cede primeiro.
            self.evict_retained();
        }
        if self.seq_pool.is_empty() {
            return None;
        }
        Some(self.seq_pool.remove(0))
    }

    fn free_seq(&mut self, seq_id: u32) {
        if !self.seq_pool.contains(&seq_id) {
            self.seq_pool.push(seq_id);
        }
    }

    /// A sequência de geração que acompanha uma sequência de prompt.
    ///
    /// Par fixo, não sorteado de um pool: quem tem slot de prompt tem slot de
    /// geração por construção, então abrir a geração nunca falha por falta de
    /// slot. Ver `llm::kv_seq_budget`, que reserva as duas metades no contexto.
    fn gen_seq_of(&self, prompt_seq: u32) -> u32 {
        prompt_seq + self.max_sessions.max(1)
    }

    /// Despeja a sessão retida menos usada, devolvendo o slot dela.
    fn evict_retained(&mut self) -> bool {
        let Some(sid) = self
            .sessions
            .iter()
            .filter(|(_, session)| session.retained)
            .min_by_key(|(_, session)| session.last_used)
            .map(|(sid, _)| sid.clone())
        else {
            return false;
        };
        info!(
            "despejando prefixo retido da sessão {}… para liberar slot",
            &sid[..8.min(sid.len())]
        );
        self.finish(&sid);
        true
    }

    /// Abre a sequência de geração deste turno copiando o KV do prompt.
    ///
    /// Idempotente: chamado antes de cada decodificação de token gerado, faz
    /// trabalho só na primeira. Depois disto nada mais escreve em
    /// `prompt_seq`, que é o que torna o prefixo reutilizável sem remoção
    /// parcial de KV - ver o doc de `Session`.
    fn open_generation(&mut self, sid: &str) -> Result<()> {
        let Some(session) = self.sessions.get(sid) else {
            bail!("sessão '{sid}' desconhecida — faça o prefill antes");
        };
        if session.gen_seq.is_some() {
            return Ok(());
        }
        let prompt_seq = session.prompt_seq;
        let gen = self.gen_seq_of(prompt_seq);
        // A cópia herda o que estiver na sequência de destino se ela não
        // estiver limpa; limpar é remoção total, sempre aceita.
        let _ = self.engine.kv_remove(gen, 0, -1);
        self.engine.kv_copy_seq(prompt_seq, gen);
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.gen_seq = Some(gen);
        session.retained = false;
        Ok(())
    }

    /// Decide, no pedaço 0, se o prefixo retido serve para este prompt - e
    /// deixa a sessão coerente com a decisão.
    ///
    /// A regra é **só acrescentar**: o prefixo serve quando os tokens retidos
    /// são exatamente um prefixo dos novos, e nem um token a mais. Assim o
    /// prefill continua de onde parou e **nenhuma posição precisa sair do KV** -
    /// que é a diferença entre funcionar em todo modelo e depender de o motor
    /// aceitar `llama_memory_seq_rm` parcial. Qualquer outro caso (divergência
    /// no meio, prompt idêntico ao retido, sessão sem prompt) recomeça a sessão
    /// do zero, o que é uma perda de reuso naquele turno, nunca um erro.
    ///
    /// O último token do prompt nunca entra no prefixo aproveitado: é dele que
    /// saem os logits do primeiro token gerado, então sempre sobra pelo menos
    /// um token para decodificar.
    fn adopt_prompt_prefix(
        &mut self,
        sid: &str,
        tokens: Vec<i32>,
        stable_len: u32,
        requested_reuse: bool,
    ) -> Result<PrefixDecision> {
        let mut decision = PrefixDecision::default();
        if requested_reuse {
            let session = self.sessions.get(sid).ok_or_else(|| {
                anyhow::anyhow!("sessão '{sid}' desconhecida no reuso de prefixo")
            })?;
            let retained = session
                .prompt_tokens
                .get(..session.n_past as usize)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "sessão '{sid}' retém {} tokens, mas só conhece {}",
                        session.n_past,
                        session.prompt_tokens.len()
                    )
                })?;
            decision = plan_prefix_reuse(retained, session.n_past, &tokens);
            if !decision.reused {
                let comum = retained
                    .iter()
                    .zip(&tokens)
                    .take_while(|(old, new)| old == new)
                    .count();
                debug!(
                    "prefixo da sessão {}… não serve: comum={comum} retido={} retidos={} novos={}",
                    &sid[..8.min(sid.len())],
                    session.n_past,
                    session.prompt_tokens.len(),
                    tokens.len()
                );
                self.get_or_create(sid, true)?;
            }
        }
        let session = self
            .sessions
            .get_mut(sid)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid}' desapareceu no prefill"))?;
        session.prompt_tokens = tokens;
        session.stable_len = stable_len;
        session.retained = false;
        Ok(decision)
    }

    /// Fecha o turno mantendo o prompt: apaga a sequência de geração inteira e
    /// devolve a sessão à posição do fim do prompt.
    ///
    /// Só usa remoção **total** de sequência, que todo motor aceita - inclusive
    /// os recorrentes/híbridos, onde `llama_memory_seq_rm` recusa qualquer
    /// recuo parcial maior que a janela de snapshots (`n_rs_seq`). É a
    /// substituição do rewind que era recusado em 75 das 176 requisições.
    ///
    /// Devolve quantos tokens de prompt ficaram retidos, ou `None` quando não
    /// havia prompt para reter (aí a sessão é encerrada de vez).
    fn retain_prompt(&mut self, sid: &str) -> Option<u32> {
        let session = self.sessions.get(sid)?;
        if session.prompt_len == 0 {
            self.finish(sid);
            return None;
        }
        let (gen_seq, prompt_len) = (session.gen_seq, session.prompt_len);
        if let Some(gen) = gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.gen_seq = None;
        session.n_past = prompt_len;
        session.recent_tokens.clear();
        let _ = session.finish_token_stream();
        session.retained = true;
        session.last_used = Instant::now();
        // Uma sessão retida vira slot reclamável (`evict_retained`): um prefill
        // de sessão nova que esperava pode agora entrar.
        self.signal_slot_freed();
        Some(prompt_len)
    }

    /// Apaga o KV das duas sequências de uma sessão.
    fn wipe_kv(&mut self, session: &Session) {
        if let Some(gen) = session.gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        // A sequência de geração pode ter sobrado de um turno anterior que não
        // passou por aqui; limpar as duas é barato e não deixa célula presa.
        let _ = self
            .engine
            .kv_remove(self.gen_seq_of(session.prompt_seq), 0, -1);
        let _ = self.engine.kv_remove(session.prompt_seq, 0, -1);
    }

    /// Grava o prefixo retido de uma sessão como um snapshot de KV em disco,
    /// como o `--slot-save-path` do llama-server.
    ///
    /// **Sempre salva `prompt_seq`, nunca `gen_seq`.** A cauda volátil da
    /// geração some no fim do turno e não faz parte de um snapshot; o que fica
    /// é o prompt estável que o próximo turno reaproveita. O `.bin` do
    /// llama.cpp serializa as células da sequência com suas posições, e o
    /// sidecar `.json` grava a identidade do modelo e da fatia para a
    /// restauração recusar um estado incompatível.
    fn save_slot(&self, sid: &str, dir: &std::path::Path, name: &str) -> Result<SlotSidecar> {
        let name = sanitize_slot_name(name)?;
        let session = self
            .sessions
            .get(sid)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid}' desconhecida"))?;
        if session.prompt_len == 0 {
            bail!("sessão '{sid}' não tem prompt retido para snapshot");
        }
        let seq = session.prompt_seq;
        let retained = session.prompt_len;
        // Os tokens reais do prompt retido, quando este peer os conhece (a
        // cabeça). Vazio nos hops que só receberam hidden state.
        let real_tokens: Vec<i32> = session
            .prompt_tokens
            .get(..retained as usize)
            .map(<[i32]>::to_vec)
            .unwrap_or_default();
        // Cabeçalho de tokens do `.bin`: os reais quando existem, senão um vetor
        // do tamanho certo - o llama.cpp nunca confere o cabeçalho contra as
        // células, ele é consultivo.
        let header_tokens: Vec<i32> = if real_tokens.is_empty() {
            vec![0i32; retained as usize]
        } else {
            real_tokens.clone()
        };
        std::fs::create_dir_all(dir)
            .with_context(|| format!("criar diretório de snapshots {}", dir.display()))?;
        let bin = slot_bin_path(dir, name);
        let bin_bytes = self.engine.state_seq_save_file(&bin, seq, &header_tokens)? as u64;
        let sidecar = SlotSidecar {
            format_version: SLOT_SIDECAR_VERSION,
            model_label: self.model_label.clone(),
            model_path: self.model_path.clone(),
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            total_layers: self.total_layers,
            n_embd: self.engine.n_embd as u32,
            retained_tokens: retained,
            prompt_tokens: real_tokens,
            saved_at: chrono::Utc::now().to_rfc3339(),
            bin_bytes,
        };
        let json = serde_json::to_vec_pretty(&sidecar)?;
        std::fs::write(slot_json_path(dir, name), json)
            .with_context(|| format!("gravar sidecar de {name}"))?;
        Ok(sidecar)
    }

    /// Lê um snapshot de KV do disco para uma **nova** sessão. Recusa um
    /// snapshot de outro modelo, faixa de camadas ou largura de hidden state
    /// antes de tocar no motor; o `.bin` recusa por si só uma geometria de KV
    /// incompatível (tipo/tamanho de linha, contagem de camadas).
    fn restore_slot(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        new_sid: &str,
    ) -> Result<SlotRestoreOutcome> {
        let name = sanitize_slot_name(name)?;
        if self.sessions.contains_key(new_sid) {
            bail!("sessão '{new_sid}' já existe; escolha outro id");
        }
        let raw = std::fs::read(slot_json_path(dir, name))
            .with_context(|| format!("ler sidecar de {name}"))?;
        let sidecar: SlotSidecar = serde_json::from_slice(&raw)
            .with_context(|| format!("sidecar de {name} ilegível"))?;
        self.check_slot_compatible(&sidecar)?;

        let seq = self.alloc_seq().ok_or_else(|| {
            anyhow::anyhow!("seq pool esgotado; libere uma sessão antes de restaurar")
        })?;
        let cap = sidecar
            .retained_tokens
            .max(sidecar.prompt_tokens.len() as u32)
            .saturating_add(8) as usize;
        let bin = slot_bin_path(dir, name);
        let (_bytes, header_tokens) = match self.engine.state_seq_load_file(&bin, seq, cap) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.free_seq(seq);
                return Err(error);
            }
        };

        let mut session = Session::new(seq);
        session.prompt_len = sidecar.retained_tokens;
        session.n_past = sidecar.retained_tokens;
        session.stable_len = sidecar.retained_tokens;
        session.prompt_tokens = if sidecar.prompt_tokens.is_empty() {
            header_tokens
        } else {
            sidecar.prompt_tokens.clone()
        };
        session.retained = true;
        session.last_used = Instant::now();
        self.sessions.insert(new_sid.to_string(), session);
        Ok(SlotRestoreOutcome {
            restored_tokens: sidecar.retained_tokens,
        })
    }

    /// Confere que um sidecar descreve o mesmo modelo e a mesma fatia que este
    /// peer serve. Não entra `cache_type_*`: o `.bin` já recusa uma geometria
    /// de KV divergente com o tipo e o tamanho de linha por camada.
    fn check_slot_compatible(&self, sidecar: &SlotSidecar) -> Result<()> {
        if sidecar.format_version != SLOT_SIDECAR_VERSION {
            bail!(
                "sidecar de versão {} incompatível (esperado {SLOT_SIDECAR_VERSION})",
                sidecar.format_version
            );
        }
        let mismatch = |campo: &str, esperado: String, achou: String| {
            anyhow::anyhow!("snapshot de KV incompatível: {campo} {achou} != {esperado} deste peer")
        };
        if sidecar.model_label != self.model_label {
            return Err(mismatch(
                "model_label",
                self.model_label.clone(),
                sidecar.model_label.clone(),
            ));
        }
        if (sidecar.start_layer, sidecar.end_layer, sidecar.total_layers)
            != (self.start_layer, self.end_layer, self.total_layers)
        {
            return Err(mismatch(
                "faixa de camadas",
                format!(
                    "{}-{}/{}",
                    self.start_layer, self.end_layer, self.total_layers
                ),
                format!(
                    "{}-{}/{}",
                    sidecar.start_layer, sidecar.end_layer, sidecar.total_layers
                ),
            ));
        }
        if sidecar.n_embd != self.engine.n_embd as u32 {
            return Err(mismatch(
                "n_embd",
                (self.engine.n_embd as u32).to_string(),
                sidecar.n_embd.to_string(),
            ));
        }
        Ok(())
    }

    fn reap_idle(&mut self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(SESSION_IDLE_TIMEOUT_SECS))
            .unwrap_or_else(Instant::now);
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.last_used < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for sid in stale {
            if let Some(mut s) = self.sessions.remove(&sid) {
                // A timeout is an aborted stream. Clear its byte tail before
                // returning the sequence slot so it cannot affect a later
                // request that reuses the slot.
                let _ = s.finish_token_stream();
                info!(
                    "reclaiming idle session {}… (seq={})",
                    &sid[..8.min(sid.len())],
                    s.prompt_seq
                );
                // Limpeza total das sequências, que todo modelo aceita.
                self.wipe_kv(&s);
                self.free_seq(s.prompt_seq);
            }
        }
        if count > 0 {
            self.signal_slot_freed();
        }
        count
    }

    fn get_or_create(&mut self, sid: &str, reset: bool) -> Result<&mut Session> {
        if reset {
            if let Some(mut old) = self.sessions.remove(sid) {
                let _ = old.finish_token_stream();
                self.wipe_kv(&old);
                self.free_seq(old.prompt_seq);
            }
            let seq = self.alloc_seq().ok_or_else(|| {
                anyhow::anyhow!(
                    "seq pool esgotado (max {} sessões simultâneas)",
                    self.max_sessions
                )
            })?;
            self.sessions.insert(sid.to_string(), Session::new(seq));
        }
        self.sessions.get_mut(sid).ok_or_else(|| {
            anyhow::anyhow!("sessão '{}' desconhecida — envie reset=true antes", sid)
        })
    }

    /// Encerra a sessão de vez: as duas sequências são apagadas e o slot volta
    /// ao pool. O fim normal de um turno **não** passa por aqui - passa por
    /// `retain_prompt`, que guarda o prompt. Aqui chegam a liberação explícita
    /// do gateway, o cancelamento e a limpeza de erro.
    fn finish(&mut self, sid: &str) {
        if let Some(mut s) = self.sessions.remove(sid) {
            // The output path flushes before the caller reaches this method;
            // the cleanup cases deliberately discard the result after clearing
            // it so no tail can leak into the next session.
            let _ = s.finish_token_stream();
            self.wipe_kv(&s);
            self.free_seq(s.prompt_seq);
            self.signal_slot_freed();
        }
    }

    /// Acorda o `run_scheduler` para ele reavaliar um prefill de sessão nova
    /// que ficou esperando slot.
    fn signal_slot_freed(&self) {
        self.slot_freed.notify_one();
    }
}

// ── Start ─────────────────────────────────────────────────────────────────────

/// Load the model and serve the inference HTTP server.
///
/// Blocks until the server exits or `stop_rx` fires.
pub async fn run(
    cfg: NodeConfig,
    mut stop_rx: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    mtp_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    batching_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tls: Option<crate::serve::TlsServer>,
    auth: Option<crate::auth::AuthState>,
    database: Option<Arc<crate::db::PomDb>>,
    model_cache: Arc<ModelCache>,
    reuse_weights: bool,
) -> Result<()> {
    // Derivado, não copiado do pedido: o caminho de linha de comando nunca
    // preenche `no_embeddings`, então sem isto o nó da última fatia instalava o
    // callback de captura e pagava ~8 ms por token para capturar um residual
    // que ninguém lê. Ver `NodeConfig::needs_residual_export`.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    let port = cfg.inference_port;
    let listener = bind_dual_stack(port).await?;
    info!("porta {port} ligada; carregando o modelo");

    let model_info = match crate::gguf::read_info(std::path::Path::new(&cfg.model_path)) {
        Ok(info) => Some(info),
        Err(error) => {
            warn!("não foi possível ler metadados GGUF antes da carga: {error}");
            None
        }
    };
    let inference_mode = configured_inference_mode(cfg.inference_mode)?;
    let inference_path = crate::executor::select(inference_mode, model_info.as_ref())?;
    info!(
        "caminho de inferência selecionado: {} (modo {:?})",
        inference_path.as_str(),
        inference_mode
    );

    // The terminal MoE hop also needs the captured boundary residual so its
    // own executor can run before the response is sampled. Dense single-peer
    // contexts keep the old fast path without installing the callback.
    let need_embeddings = cfg.needs_residual_export() || inference_path.is_moe();
    // `rs_seq_window` (maior largura de lote especulativo, dos dois mecanismos
    // lookup e MTP) é computado dentro de `engine_config_from_node` agora -
    // ver o doc de `EngineConfig::rs_seq_window`.
    let moe_plan = if inference_path.is_moe() {
        Some(
            crate::moe::runtime::MoeRuntime::external_executor_plan(&cfg.model_path).map_err(
                |error| anyhow::anyhow!("planejamento de ownership MoE falhou: {error}"),
            )?,
        )
    } else {
        None
    };
    let moe_external_executor = moe_plan.as_ref().is_some_and(|plan| plan.enabled());
    if let Some(plan) = moe_plan.as_ref().filter(|plan| !plan.enabled()) {
        let reason = plan
            .reason
            .as_deref()
            .unwrap_or("nenhuma camada tem executor externo disponível");
        return Err(anyhow::anyhow!(
            "combinação MoE não suportada antes da carga do modelo: dtypes GGML {:?}; {reason}; o caminho MoE exige um executor externo e não usará o llama.cpp nativo",
            plan.dtypes
        ));
    }
    let engine_cfg = crate::runtime::engine_config_from_node(
        &cfg,
        need_embeddings,
        moe_external_executor,
        moe_plan
            .as_ref()
            .map(|plan| plan.layers.clone())
            .unwrap_or_default(),
    )?;

    let model_path = cfg.model_path.clone();
    let mut engine = tokio::task::spawn_blocking(move || {
        LlmEngine::load_with_cache(&model_path, &engine_cfg, &model_cache, reuse_weights)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    // Só agora se sabe se a cabeça `nextn` existe: depende do GGUF, não da
    // configuração. Ver `Network::mtp_ready`.
    mtp_ready.store(engine.mtp_ready(), std::sync::atomic::Ordering::Relaxed);

    let moe = if inference_path.is_moe() && moe_external_executor {
        let info = model_info.as_ref().ok_or_else(|| {
            anyhow::anyhow!("caminho MoE exige metadados GGUF para preparar o router")
        })?;
        let experts = info
            .expert_count
            .ok_or_else(|| anyhow::anyhow!("caminho MoE exige expert_count no GGUF"))?;
        let top_k = info.expert_used_count.unwrap_or(1).max(1) as usize;
        let weights_bytes = std::fs::metadata(&cfg.model_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let cuda_compute_bytes = engine.cuda_compute_bytes();
        Some(Arc::new(
            crate::moe::runtime::MoeRuntime::open(
                &cfg.model_path,
                cfg.model_label.clone(),
                experts,
                top_k,
                weights_bytes,
                cuda_compute_bytes,
            )
            .map_err(|error| anyhow::anyhow!("runtime MoE indisponível: {error}"))?,
        ))
    } else {
        None
    };

    if let Some(runtime) = moe.as_ref() {
        runtime.start_device_warmup();
        engine.set_moe_runtime(Arc::clone(runtime))?;
    }
    let moe_fallback_reason = moe_plan
        .as_ref()
        .filter(|plan| !plan.enabled())
        .and_then(|plan| plan.reason.clone());
    let mut state_inner = InferState::new(engine, &cfg, inference_path, moe, moe_fallback_reason);
    state_inner.tls_handshake_ms = tls.as_ref().map(|t| t.last_handshake_ms.clone());
    state_inner.original_block_count = model_info
        .as_ref()
        .and_then(|info| info.original_block_count);

    // Fora do mutex do estado: o decode segura aquele lock e é de dentro dele
    // que o reduce espera a soma. Ver `handle_tp_reduce`.
    let tp_barrier = crate::collective::ReduceBarrier::new();
    let tp_size = cfg.tp.as_ref().map(|t| t.size).unwrap_or(1);
    let local_barrier_handle = crate::tp::set_local_barrier(tp_barrier.clone(), tp_size);

    let state: SharedState = Arc::new(Mutex::new(state_inner));
    let scheduler_cfg = crate::scheduler::SchedulerConfig::from_node_config(&cfg);
    // TP participates in the same canonical batch frame. The handler opens
    // one shared decode id for the whole frame, broadcasts that frame to the
    // workers and then submits all items to this local scheduler together.
    batching_ready.store(scheduler_cfg.enabled, std::sync::atomic::Ordering::Relaxed);
    let scheduler = spawn_scheduler(state.clone(), scheduler_cfg);
    let app_state = InferenceAppState {
        state,
        scheduler,
        tp_batch_gate: Arc::new(tokio::sync::Mutex::new(())),
        direct_forward_order: DirectForwardOrder::default(),
    };
    crate::relay::install_local_inference(generation, Arc::new(app_state.clone())).await;

    if let Some(auth) = auth.clone() {
        http_client().set_auth_state(auth);
    }

    let inference_routes = Router::new()
        .route(
            "/v1/prefill",
            post(handle_prefill).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step",
            post(handle_step).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step-stream",
            post(handle_step_stream).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route("/v1/session/release", post(handle_release))
        .route("/v1/session/rewind", post(handle_rewind))
        .route("/v1/session/rollback", post(handle_rollback))
        .route("/v1/session/slot", post(handle_slot));
    let inference_routes = if let Some(auth) = auth.clone() {
        inference_routes.layer(middleware::from_fn_with_state(
            (auth, database.clone()),
            require_inference_auth,
        ))
    } else {
        inference_routes
    };
    let inference_routes = inference_routes.with_state(app_state.clone());

    // O canal persistente tem router próprio porque o estado dele carrega a
    // credencial: o middleware confere a abertura, e o handler reconfere cada
    // quadro. O limite de corpo é desligado aqui porque o corpo é a geração
    // inteira, não um quadro - cada quadro dentro do fluxo continua limitado
    // pelo enquadramento do canal.
    let link_state = LinkState {
        app: app_state.clone(),
        auth: auth.clone().map(|auth| (auth, database.clone())),
    };
    let link_routes = Router::new().route(
        crate::link::LINK_PATH,
        post(handle_link).layer(DefaultBodyLimit::disable()),
    );
    let link_routes = if let Some(auth) = auth.clone() {
        link_routes.layer(middleware::from_fn_with_state(
            (auth, database.clone()),
            require_inference_auth,
        ))
    } else {
        link_routes
    };
    let link_routes = link_routes.with_state(link_state);

    let tp_routes = Router::new().route(
        "/v1/tp/reduce",
        post(handle_tp_reduce).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
    );
    let tp_routes = if let Some(auth) = auth {
        tp_routes.layer(middleware::from_fn_with_state(
            (auth, database),
            require_inference_auth,
        ))
    } else {
        tp_routes
    };
    let tp_routes = tp_routes.with_state((tp_barrier, tp_size));

    let router = inference_routes
        .merge(link_routes)
        .merge(
            Router::new()
                .route("/health", get(handle_health))
                .with_state(app_state.clone()),
        )
        .merge(tp_routes);

    info!("inference server pronto em [::]:{port}");

    // Ver a nota gêmea em `gateway::start_gateway`: sem `tcp_nodelay`, toda
    // resposta de hop sai com Nagle ligado, e um ack de resposta direta é
    // exatamente a escrita pequena que ele segura. Quem o aplica agora é o
    // `serve`, que também decide entre TLS e texto claro por conexão.
    //
    // O desligamento gracioso virou uma corrida entre o laço de accept e o
    // watch de geração: quando a geração muda, paramos de aceitar. Conexão em
    // voo termina sozinha, como antes.
    // O desligamento gracioso é sequencial de propósito: primeiro o `serve`
    // para de aceitar e encerra as conexões que restaram, só depois a VRAM é
    // devolvida. Na ordem inversa uma requisição em voo tocaria um contexto já
    // liberado.
    let resultado = crate::serve::serve_with_shutdown(listener, router, tls, async move {
        loop {
            if stop_rx.changed().await.is_err() {
                break;
            }
            if *stop_rx.borrow() != generation {
                break;
            }
        }
    })
    .await;
    crate::relay::clear_local_inference(generation).await;

    // O aquecimento é cooperativo e pode estar lendo o GGUF enquanto o
    // servidor é reconfigurado. Sinalize a parada antes de liberar o motor,
    // para que nenhuma geração antiga continue admitindo experts no device.
    if let Some(runtime) = app_state
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .moe
        .clone()
    {
        runtime.cancel_device_warmup();
    }

    if let Some(handle) = local_barrier_handle {
        crate::tp::clear_local_barrier(handle);
    }

    // Explícito, e não pela contagem do `Arc`: uma cópia esquecida em qualquer
    // canto — foi o que as conexões desgarradas faziam — adiava o `Drop` para
    // sempre, e o `undeploy` respondia "pronto" com o modelo ainda na GPU.
    // Depois do drenar acima ninguém mais decodifica, então isto é seguro.
    // `into_inner` no envenenado: um handler que entrou em pânico deixa o
    // mutex marcado, e recusar a liberação por causa disso seria justamente
    // deixar a VRAM presa no caso em que ela mais precisa voltar.
    app_state
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .engine
        .unload();
    info!("memória do modelo devolvida na porta {port}");

    resultado?;
    batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// ── Socket ────────────────────────────────────────────────────────────────────

/// Um listener que atende IPv6 e IPv4 no mesmo socket, caindo para IPv4 puro
/// quando não há IPv6 na máquina.
///
/// `IPV6_V6ONLY` só vale se for desligado **antes** do `bind`, e é por isso que
/// o socket é montado à mão em vez de sair do `TcpListener::bind`. O caminho
/// antigo chamava `setsockopt` depois do bind: no Linux passava despercebido
/// porque `net.ipv6.bindv6only` já é 0 por padrão, mas no Windows o padrão é 1
/// e o servidor de inferência simplesmente não aceitaria nenhum peer IPv4 -
/// que é como todo o cluster da LAN fala.
///
/// `SO_REUSEADDR` continua fora de propósito: no Windows ele deixa outro
/// processo tomar uma porta já ligada, e é justamente o "address already in
/// use" que denuncia um servidor velho ainda de pé.
pub(crate) fn listener_dual_stack(port: u16) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    // Backlog: o mesmo que o `TcpListener::bind` da std usa.
    const BACKLOG: i32 = 128;

    let v6 = (|| -> std::io::Result<std::net::TcpListener> {
        let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        sock.set_only_v6(false)?;
        // O `TcpListener::bind` da std pede isto sozinho no Unix; um socket cru
        // nao. Sem a flag, cada conexao em TIME_WAIT nesta porta - e o gateway
        // deixa varias, sondando `/health` e mandando inferencia - impede o
        // proximo servidor de ligar por ate um minuto, que e mais que as dez
        // tentativas do supervisor. Toda implantacao de versao nova morria ai.
        sock.set_reuse_address(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.listen(BACKLOG)?;
        Ok(sock.into())
    })();

    match v6 {
        Ok(l) => Ok(l),
        Err(e) => {
            // Sem IPv6 na máquina: IPv4 puro ainda serve o cluster inteiro.
            tracing::debug!("bind IPv6 em [::]:{port} falhou ({e}); tentando IPv4");
            let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            sock.set_reuse_address(true)?;
            sock.bind(&SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port).into())?;
            sock.listen(BACKLOG)?;
            Ok(sock.into())
        }
    }
}

pub(crate) async fn bind_dual_stack(port: u16) -> Result<TcpListener> {
    let std_sock = listener_dual_stack(port)?;
    std_sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_sock)?)
}

// ── Error helpers ─────────────────────────────────────────────────────────────

/// One shared outbound client cache, used only when relaying to this node's own
/// next hop (`PipelineStep::supports(CAP_RELAY_CHAIN)`) — this server has no
/// other reason to make outbound HTTP calls. Same lazy-static pattern as
/// `webui::catalog_cache`.
///
/// É um `PeerClients` e não um `reqwest::Client` porque o próximo salto é um
/// peer: com o cluster em TLS ele precisa do certificado daquele node_id
/// fixado. O esquema global (`peer_client::tls_ativo`) é quem diz se há TLS -
/// este processo é o mesmo do nó, então a política já está aplicada aqui.
fn http_client() -> &'static crate::peer_client::PeerClients {
    static CLIENT: std::sync::OnceLock<crate::peer_client::PeerClients> =
        std::sync::OnceLock::new();
    let c = CLIENT
        .get_or_init(|| crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)));
    // A cada uso, não só na criação: o `OnceLock` nasce uma vez e o operador
    // pode virar a flag depois. `set_tls` só faz trabalho quando o valor muda,
    // então isto custa uma leitura atômica no caminho comum.
    c.set_tls(crate::peer_client::tls_ativo());
    c
}

/// Duração do último handshake TLS que este servidor aceitou, em ms.
///
/// Zero quando não há TLS, e zero também quando o salto reaproveitou uma
/// conexão do pool - que é a maioria deles. Ver o doc de
/// `SentFrame::tls_handshake_ms` sobre por que isso é a verdade e não uma
/// falha de medição.
fn last_handshake_ms(state: &SharedState) -> u64 {
    let Ok(state) = lock_inference_state(state) else {
        warn!("não foi possível ler o último handshake: mutex do estado envenenado");
        return 0;
    };
    state
        .tls_handshake_ms
        .as_ref()
        .map(|v| v.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

async fn handle_prefill_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/prefill", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_prefill(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

async fn handle_step_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/step", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_step(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

fn err400(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn err500(msg: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

fn context_per_session(n_ctx_total: u32, max_sessions: u32) -> u32 {
    n_ctx_total / max_sessions.max(1)
}

// ── /health ───────────────────────────────────────────────────────────────────

async fn handle_health(State(app): State<InferenceAppState>) -> Response {
    let scheduler = app.scheduler;
    let state = app.state;
    let s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => return err500(error),
    };
    // Keep in sync with network.rs's `self_announce`, gateway.rs's local peer
    // entry, and the capability constants in routing/pipeline.rs.
    let mut caps = vec![
        crate::pipeline::CAP_FRAME_BATCH,
        crate::pipeline::CAP_FRAME_DTYPE_F8,
        crate::pipeline::CAP_REMOTE_TOKEN_STREAM,
        crate::pipeline::CAP_LINK_STREAM,
        "relay/v1-chain",
        "relay/v2-direct",
        "speculate/v1-lookup",
        "speculate/v1-mtp-sliced",
    ];
    if s.engine.mtp_ready() {
        caps.push("speculate/v1-mtp");
    }
    if scheduler.config.enabled {
        caps.push(crate::pipeline::CAP_FRAME_BATCH);
    }
    if s.slot_dir.is_some() {
        caps.push(crate::pipeline::CAP_SLOT_PERSIST);
    }
    let n_ctx_total = s.engine.n_ctx();
    let n_ctx_per_session = context_per_session(n_ctx_total, s.max_sessions);
    let moe_telemetry = s.moe.as_ref().map(|runtime| runtime.executor_telemetry());
    let (moe_requested, moe_executor, moe_executor_dtypes, moe_executor_reason) =
        moe_health_summary(
            s.inference_path,
            moe_telemetry.as_ref(),
            s.moe_fallback_reason.as_deref(),
        );
    Json(json!({
        "status": "ok",
        "model_loaded": true,
        "start_layer": s.start_layer,
        "end_layer": s.end_layer,
        "original_block_count": s.original_block_count,
        "selected_path": s.inference_path.as_str(),
        "moe_requested": moe_requested,
        "moe_executor": moe_executor,
        "moe_executor_dtypes": moe_executor_dtypes,
        "moe_executor_reason": moe_executor_reason,
        "hidden_dim": s.engine.n_embd,
        "vocab_size": s.engine.n_vocab,
        "max_sessions": s.max_sessions,
        // Duas sequências de KV por sessão: a do prompt e a da geração. Ver
        // `llm::kv_seq_budget`, que explica por que e o que custa.
        "kv_seq_max": crate::llm::kv_seq_budget(s.max_sessions),
        "sessions_in_use": s.sessions.len(),
        "free_slots": s.seq_pool.len(),
        // Sessões paradas no fim do prompt, esperando o turno seguinte. É o
        // cache de prefixo visto do lado que o executa.
        "sessions_retained": s.sessions.values().filter(|x| x.retained).count(),
        "prefix_tokens_retained": s
            .sessions
            .values()
            .filter(|x| x.retained)
            .map(|x| x.prompt_len as u64)
            .sum::<u64>(),
        // Diagnóstico do motor: `kv_can_shift` é o K-shift do llama.cpp, e
        // **não** governa remoção parcial de KV. O reuso de prefixo deste
        // projeto não depende de nenhum dos dois - ver `Session`.
        "kv_can_shift": s.engine.kv_can_shift(),
        "n_ctx_total": n_ctx_total,
        // O engine reserva a janela pública para cada sessão. Se o fit reduzir
        // o total, a capacidade por sessão é a divisão efetiva, não o total
        // bruto que o llama.cpp devolve.
        "n_ctx_per_session": n_ctx_per_session,
        "offload_kqv": s.engine.offload_kqv(),
        "cache_type_k": s.engine.cache_type_k().as_str(),
        "cache_type_v": s.engine.cache_type_v().as_str(),
        "fit_memory_applied": s.engine.fit_memory_applied(),
        "n_gpu_layers_effective": s.engine.effective_n_gpu_layers(),
        "n_ubatch_effective": s.engine.effective_n_ubatch(),
        // Com que parâmetros de llama.cpp este contexto subiu, para o
        // operador comparar o nó com uma linha de comando do `llama-cli`.
        // Lido do contexto vivo, não do `EngineConfig` - ver
        // `LlmEngine::load_params`.
        "load_params": s.engine.load_params(),
        "profile_sync_enabled": crate::llm::profile_backend_sync_enabled(),
        // Onde as camadas e o cache KV ficaram de verdade, direto do
        // llama.cpp. `null` quando a ponte não respondeu - a interface diz
        // que não sabe, em vez de derivar um mapa de `n_gpu_layers`, que
        // erraria sempre que um override de buffer type entrasse.
        "placement": s.engine.placement(),
        "context_memory": s.engine.context_memory(),
        "inference_frame_capabilities": caps,
        "scheduler": scheduler.stats.json(&scheduler.config),
        // Diagnóstico puro — `llama_memory_can_shift`, o K-shift do
        // `--ctx-shift` de referência do llama.cpp. Não decide mais se a
        // especulação roda (ver o doc de `should_draft`); mantido aqui
        // porque é informação real do motor.
        "kv_can_shift": s.engine.kv_can_shift(),
        // A cabeça de rascunho `nextn` foi mesmo anexada? Depende do GGUF
        // carregado (fatia não-final não tem o bloco) e do interruptor do
        // operador, então nem a configuração nem o nome do arquivo respondem.
        "mtp_ready": s.engine.mtp_ready(),
        "mtp_allocation": s.engine.mtp_allocation(),
        "mtp_unavailable_reason": s.engine.mtp_unavailable_reason(),
        // Snapshots de KV cache (`--slot-save-path`): ligado quando há um
        // diretório configurado. `slot_count` são os `.json` legíveis nele.
        "slot_persist_enabled": s.slot_dir.is_some(),
        "slot_count": s
            .slot_dir
            .as_deref()
            .map(|dir| list_slots(dir).len())
            .unwrap_or(0),
    }))
    .into_response()
}

// ── /v1/tp/reduce ────────────────────────────────────────────────────────────

/// Uma parcial de um reduce de tensor parallelism (caminho C). Cada rank do
/// grupo faz um POST desta com a própria parcial e o mesmo `barrier_id`; o
/// endpoint (no primary/rank 0) acumula e devolve a soma quando todos chegam.
/// **Nunca** toca no mutex do `InferState`, e isso é a correção, não estilo.
///
/// `handle_prefill` segura esse mutex durante o decode inteiro, e é de dentro
/// desse decode que o callback do reduce bloqueia esperando a soma. Enquanto
/// este handler pegava o mesmo mutex para ler a barreira, a parcial que chegava
/// - do próprio rank 0 ou do worker - ficava presa esperando um lock que só
/// seria liberado quando o decode terminasse; e o decode só terminaria com a
/// soma que estava presa na porta. Deadlock exato de 18/08: os dois ranks
/// logavam o primeiro reduce, com 0,2 ms de diferença, e o pipeline parava ali.
///
/// A barreira vive fora do mutex (`ReduceBarrier` já é `Clone` e sincronizada
/// por dentro), então o caminho da coletiva nunca disputa com o decode.
///
/// A parcial viaja como **f32 cru em little-endian**, não como JSON. Um float
/// de 4 bytes vira ~12 caracteres em JSON, e a parcial de um prefill de 100
/// tokens tem 410 mil floats: ~5 MB de texto por reduce, 64 vezes por decode.
/// Isso estourava o limite de corpo do axum (413) e, quando cabia, gastava mais
/// tempo formatando número que somando. Cru é 4x menor e sem parse.
///
/// `barrier_id` e `rank` vão em cabeçalho, que é onde metadado de tamanho fixo
/// pertence.
pub(crate) async fn handle_tp_reduce(
    State((barrier, size)): State<(crate::collective::ReduceBarrier, u32)>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let barrier_id = match headers
        .get(crate::tp::TP_BARRIER_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) => v.to_string(),
        None => {
            return err400(format!(
                "falta o cabeçalho {}",
                crate::tp::TP_BARRIER_HEADER
            ))
        }
    };
    let rank: u32 = match headers
        .get(crate::tp::TP_RANK_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(v) => v,
        None => return err400(format!("falta o cabeçalho {}", crate::tp::TP_RANK_HEADER)),
    };
    if body.len() % 4 != 0 {
        return err400(format!(
            "parcial de {} bytes não é múltiplo de 4",
            body.len()
        ));
    }
    let data: Vec<f32> = body
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    match barrier.contribute(&barrier_id, rank, size, data) {
        Ok(rx) => match rx.await {
            Ok(sum) => {
                let mut out = Vec::with_capacity(sum.len() * 4);
                for v in &sum {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                (
                    [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                    out,
                )
                    .into_response()
            }
            Err(e) => err500(format!("barreira abandonada: {e}")),
        },
        Err(e) => err400(e),
    }
}

/// Repassa uma chamada de ciclo de vida de sessão aos demais ranks do grupo.
///
/// O corpo é remontado (não é um quadro binário como o decode), e o cabeçalho
/// de réplica evita fan-out infinito: quem recebe marcado não repassa.
fn replicate_session_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    rota: &str,
    session_id: &str,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // já é a réplica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar sessão: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let corpo = json!({ "session_id": session_id }).to_string().into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        rota,
        "session",
        corpo,
        Some("application/json"),
    );
}

fn replicate_session_rewind(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    session_id: &str,
    n_tokens: u32,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return;
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar rewind: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let body = json!({ "session_id": session_id, "n_tokens": n_tokens })
        .to_string()
        .into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        "/v1/session/rewind",
        "session",
        body,
        Some("application/json"),
    );
}

// ── /v1/session/slot ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SlotRequest {
    /// `"save"`, `"restore"` ou `"erase"`.
    action: String,
    /// Sessão viva a salvar (`save`). Ignorado em `restore`/`erase`.
    #[serde(default)]
    session_id: String,
    /// Nome do snapshot em disco (um componente de caminho seguro).
    slot_name: String,
    /// Nova sessão a criar em `restore`. Obrigatório nessa ação.
    #[serde(default)]
    dest_session_id: Option<String>,
}

/// `POST /v1/session/slot` - grava, lê ou apaga um snapshot de KV cache no
/// diretório local (`--slot-save-path`). O gateway compõe a ação de
/// implantação chamando esta rota em cada hop; ver `gateway::snapshot_deployment`.
async fn handle_slot(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SlotRequest>,
) -> Response {
    // TP: rank 0 repassa a mesma ação aos workers, que servem a mesma faixa e
    // gravam a própria cópia no disco deles. Ver `replicate_session_call`.
    replicate_slot_call(&app.state, &headers, &req);

    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let Some(dir) = state.slot_dir.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            "snapshots de KV desligados neste peer (defina slot_save_path)",
        )
            .into_response();
    };

    match req.action.as_str() {
        "save" => match state.save_slot(&req.session_id, &dir, &req.slot_name) {
            Ok(sidecar) => Json(json!({
                "saved": true,
                "slot_name": req.slot_name,
                "retained_tokens": sidecar.retained_tokens,
                "bin_bytes": sidecar.bin_bytes,
            }))
            .into_response(),
            Err(error) => err400(error),
        },
        "restore" => {
            let Some(dest) = req.dest_session_id.as_deref().filter(|d| !d.is_empty()) else {
                return err400("restore exige dest_session_id");
            };
            match state.restore_slot(&dir, &req.slot_name, dest) {
                Ok(outcome) => Json(json!({
                    "restored": true,
                    "session_id": dest,
                    "restored_tokens": outcome.restored_tokens,
                }))
                .into_response(),
                Err(error) => err400(error),
            }
        }
        "erase" => {
            drop(state);
            match erase_slot(&dir, &req.slot_name) {
                Ok(removed) => Json(json!({ "erased": removed })).into_response(),
                Err(error) => err400(error),
            }
        }
        other => err400(format!("ação de snapshot desconhecida: {other}")),
    }
}

fn replicate_slot_call(state: &SharedState, headers: &axum::http::HeaderMap, req: &SlotRequest) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // já é a réplica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar snapshot: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let body = json!({
        "action": req.action,
        "session_id": req.session_id,
        "slot_name": req.slot_name,
        "dest_session_id": req.dest_session_id,
    })
    .to_string()
    .into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        "/v1/session/slot",
        "session",
        body,
        Some("application/json"),
    );
}

// ── /v1/session/release ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReleaseRequest {
    session_id: String,
}

async fn handle_release(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ReleaseRequest>,
) -> Response {
    // A réplica não encerra sessão por conta própria (ver `tp::is_replica`),
    // então quem a libera é este release - que o gateway manda **só ao rank 0**,
    // porque o worker não é hop do pipeline. Sem repassar, a sessão do worker
    // fica segurando o slot para sempre e a requisição seguinte morre com
    // "seq pool esgotado (max 1 sessões simultâneas)" (18/08).
    replicate_session_call(&app.state, &headers, "/v1/session/release", &req.session_id);
    let mut s = match lock_inference_state(&app.state) {
        Ok(s) => s,
        Err(error) => return err500(error),
    };
    let existed = s.sessions.contains_key(&req.session_id);
    s.finish(&req.session_id);
    let free = s.seq_pool.len();
    let max = s.max_sessions;
    drop(s);
    Json(json!({
        "released": existed,
        "free_slots": free,
        "max_sessions": max,
    }))
    .into_response()
}

// ── /v1/session/rewind ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RewindRequest {
    session_id: String,
    n_tokens: u32,
}

/// Confirma que a sessão ficou parada no fim do prompt, pronta para o turno
/// seguinte.
///
/// **Não recua nada.** A geração deste turno correu numa sequência de KV
/// separada (ver `Session`), então reter o prompt é apagar essa sequência
/// inteira - remoção total, que todo motor aceita. Antes daqui saía o
/// `"este modelo não aceita recuar a sessão para o prompt"` que descartava o
/// prefixo em 75 das 176 requisições de um dia de produção, e o
/// `"sessão desconhecida"` de quando o EOS destruía a sessão antes deste
/// pedido chegar.
///
/// Idempotente por construção: o fim normal do turno já reteve, e chamar de
/// novo confere o mesmo estado. Só divergência real de contagem é recusa.
async fn handle_rewind(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RewindRequest>,
) -> Response {
    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let Some(prompt_len) = state
        .sessions
        .get(&req.session_id)
        .map(|session| session.prompt_len)
    else {
        return Json(json!({ "rewound": false, "reason": "sessão desconhecida" })).into_response();
    };
    if req.n_tokens != prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!(
                "prompt do gateway não bate com o desta sessão: {} != {prompt_len}",
                req.n_tokens
            ),
        }))
        .into_response();
    }
    let Some(retained) = state.retain_prompt(&req.session_id) else {
        return Json(json!({
            "rewound": false,
            "reason": "sessão sem prompt para reter",
        }))
        .into_response();
    };
    drop(state);
    replicate_session_rewind(&app.state, &headers, &req.session_id, req.n_tokens);
    Json(json!({ "rewound": true, "n_past": retained })).into_response()
}

// ── /v1/session/rollback ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RollbackRequest {
    session_id: String,
    /// How many of the batch's positions are confirmed valid — see
    /// `rollback_from`'s doc comment.
    accept_count: u32,
    /// How many positions the speculative decode actually wrote.
    batch_width: u32,
}

/// Discard the KV cache entries for whatever part of a speculative batch
/// didn't get confirmed. Fanned out by the gateway to *every* hop in the
/// plan after each speculative round (`gateway::rollback_sessions`, mirroring
/// `release_sessions`'s existing shape) — every hop decoded the full batch
/// into its own cache, so every hop needs this, not just the tail. Same
/// best-effort ethos as `/v1/session/release`: an unknown or already-reaped
/// `session_id` is not an error, just a no-op.
async fn handle_rollback(
    State(app): State<InferenceAppState>,
    Json(req): Json<RollbackRequest>,
) -> Response {
    match rollback_session(
        &app.state,
        &req.session_id,
        req.accept_count,
        req.batch_width,
    ) {
        Ok(true) => Json(json!({"rolled_back": true})).into_response(),
        Ok(false) => Json(json!({"rolled_back": false})).into_response(),
        Err(error) => err500(error),
    }
}

fn rollback_session(
    state: &SharedState,
    session_id: &str,
    accept_count: u32,
    batch_width: u32,
) -> Result<bool> {
    let mut s = lock_inference_state(state)?;
    let Some((seq_id, n_past_after_batch)) = s
        .sessions
        .get(session_id)
        .map(|session| (session.seq_id(), session.n_past))
    else {
        return Ok(false);
    };
    let new_n_past = rollback_from(n_past_after_batch, batch_width, accept_count);
    if new_n_past < n_past_after_batch && !s.engine.kv_remove(seq_id, new_n_past as i32, -1) {
        return Ok(false);
    }
    if let Some(session) = s.sessions.get_mut(session_id) {
        session.n_past = new_n_past;
    }
    Ok(true)
}

// ── /v1/prefill ───────────────────────────────────────────────────────────────

/// Abre o decode nos **dois** papéis e, no rank 0, põe os workers para rodar o
/// mesmo lote.
///
/// TP só funciona em lockstep: todo rank percorre o mesmo grafo sobre os mesmos
/// tokens, e as parciais se encontram na barreira. O gateway só fala com o rank
/// 0 (o worker é excluído do pipeline de propósito), então é aqui que os outros
/// ranks entram no decode. Sem isto o rank 0 postava a parcial e esperava para
/// sempre - o travamento de 18/08.
///
/// O quadro é repassado byte a byte, com o id do decode no cabeçalho. Quem
/// recebe o cabeçalho é réplica: abre o decode com o id recebido e **não**
/// retransmite, senão o grupo se multiplicaria a cada salto.
fn open_tp_decode(state: &SharedState, headers: &axum::http::HeaderMap, rota: &str, body: &Bytes) {
    if headers.get(BATCH_CHILD_HEADER).is_some() {
        return;
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível abrir decode TP: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1) else {
        return;
    };
    if let Some(id) = headers
        .get(crate::tp::TP_DECODE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        crate::tp::begin_replica_decode(id);
        return;
    }
    if tp.rank != 0 {
        // Worker recebendo quadro sem cabeçalho: veio de fora do grupo. Abrir
        // um decode aqui produziria reduces com id que ninguém mais conhece.
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    crate::tp::begin_decode(&id);
    crate::tp::broadcast_decode(&tp.members, rota, &id, body.to_vec());
}

/// A sub-faixa que o remetente mandou executar, conferida contra a fatia que
/// este nó carregou.
///
/// Quem manda decide qual pedaço roda; este nó só confere que consegue rodar
/// exatamente aquilo. O que ele **não** faz é ajustar em silêncio: rodar a
/// fatia inteira quando pediram um pedaço é exatamente o modo de falha que
/// devolve resposta errada sem erro nenhum.
///
/// Devolve o índice do bloco local onde a decodificação deve parar, ou `None`
/// quando o pedido cobre a fatia até o fim (o caso comum, grafo inteiro).
fn resolve_exec_range(
    meta: &Value,
    slice_start: u32,
    slice_end: u32,
) -> std::result::Result<Option<u32>, String> {
    let exec_start = meta.get("exec_start").and_then(|v| v.as_u64());
    let exec_end = meta.get("exec_end").and_then(|v| v.as_u64());
    let (Some(exec_start), Some(exec_end)) = (exec_start, exec_end) else {
        // Sem instrução não há o que conferir: roda a fatia inteira, que é o
        // contrato de quem só carrega uma faixa e é dono dela inteira.
        return Ok(None);
    };
    let (exec_start, exec_end) = (exec_start as u32, exec_end as u32);
    if exec_end < exec_start {
        return Err(format!("sub-faixa invertida: L{exec_start}-{exec_end}"));
    }
    if exec_start < slice_start || exec_end > slice_end {
        return Err(format!(
            "sub-faixa L{exec_start}-{exec_end} sai da fatia carregada L{slice_start}-{slice_end}"
        ));
    }
    if exec_start > slice_start {
        // O limite que o captain conhece: o estado oculto entra pelo embedding
        // do llama.cpp, que sempre cai no bloco 0 da fatia. Começar acima da
        // camada inicial exigiria pular blocos da cabeça, e o motor não sabe
        // fazer isso hoje. Recusar alto é o contrato; refazer L{slice_start}-
        // L{exec_start} em silêncio seria a resposta errada sem erro.
        return Err(format!(
            "entrar no meio da faixa ainda não é possível: pediram começar em L{exec_start}, \
             mas esta fatia começa em L{slice_start} e o motor sempre entra pelo primeiro bloco. \
             Mande a sub-faixa a partir de L{slice_start}, ou dê esta parte a um peer que comece em L{exec_start}"
        ));
    }
    if exec_end == slice_end {
        return Ok(None);
    }
    Ok(Some(exec_end - slice_start))
}

async fn handle_prefill(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    // Ver a nota gêmea em `handle_step`: o cabeçalho basta para saber se o
    // quadro é um lote.
    if wire::peek_batch_header(&body).is_ok_and(|header| header.item_count > 1) {
        match wire::decode_batch(&body) {
            Ok(batch) => return handle_prefill_batch(app, headers, batch).await,
            Err(error) => return err400(format!("quadro inválido: {error}")),
        }
    }
    // A one-item frame is still a batch frame: the gateway's
    // `BatchCoordinator` deliberately sends it through the same path. TP has
    // one decode id and one collective barrier per request, so keep the
    // complete top-level request under the same gate as a multi-item batch.
    // Fan-out children already hold the gate in `handle_prefill_batch` and
    // mark themselves with `BATCH_CHILD_HEADER`, otherwise they would try to
    // acquire the same non-reentrant mutex again.
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_request_guard = if tp_active && headers.get(BATCH_CHILD_HEADER).is_none() {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let state = app.state.clone();
    let scheduler = app.scheduler.clone();
    let processing_started = Instant::now();
    open_tp_decode(&state, &headers, "/v1/prefill", &body);
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;
    let wire_dtype = match meta.get("dtype").and_then(|value| value.as_str()) {
        Some(value) => match WireDtype::parse(value) {
            Ok(dtype) => dtype,
            Err(error) => return err400(error),
        },
        None => return err400("quadro sem dtype de estado oculto"),
    };

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__session__")
        .to_string();
    let chunk_index = meta
        .get("chunk_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let prefill_sequence_id = meta
        .get("prefill_sequence_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let direct_forward_order = app.direct_forward_order.clone();
    let chunk_size = meta
        .get("chunk_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(512) as u32;
    let reuse_prefix = meta
        .get("reuse_prefix")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cached_tokens = meta
        .get("cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let prefix_match_tokens = meta
        .get("prefix_match_tokens")
        .or_else(|| {
            meta.get("prompt_progress")
                .and_then(|v| v.get("prefix_match_tokens"))
        })
        .and_then(|v| v.as_u64())
        .unwrap_or(cached_tokens as u64) as u32;
    let stable_len = meta
        .get("stable_len")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32);
    let start_pos = meta
        .get("start_pos")
        .and_then(|v| v.as_u64())
        .map(|value| value as u32);
    // Same next_hops-derives-is_last rule as handle_step — see its comment.
    let next_hops: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = next_hops
        .as_ref()
        .map(|hops| hops.is_empty())
        .unwrap_or_else(|| {
            meta.get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    // What *this* request says about chunking — correct as received for a
    // middle/tail hop (which just passes it through); wrong for the head,
    // which always gets `false` here (the sender can't know in advance) and
    // must override it with its own determination before relaying onward —
    // see the relay branch below and `owns_tokens` in `prefill_blocking`.
    let is_final_chunk = meta
        .get("is_final_chunk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let prompt = meta
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let chat_template = meta.get("chat_template").and_then(|v| v.as_bool());
    // A composição dos tokens deste pedaço, gerada na cabeça (que tokenizou o
    // prompt) e ecoada até a cauda — é o que alimenta o `observe` da cabeça
    // `nextn` no `decode_hidden` (ver `prefill_blocking`). Ausente é peer antigo.
    let chunk_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Scale the hidden state once before moving into spawn_blocking.
    let input_prepare_started = Instant::now();
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };
    let inbound_hidden_state_bytes = match wire::tensor_payload_len(
        tensor
            .as_ref()
            .map(|(values, shape)| (values.as_slice(), shape.clone())),
        wire_dtype,
    ) {
        Ok(bytes) => bytes,
        Err(error) => return err400(error),
    };
    let input_prepare_us = input_prepare_started.elapsed().as_micros() as u64;
    // Neither of `prefill_blocking`'s non-final replies echoes `session_id`
    // back — a relay branch that forwards to the next hop needs its own
    // copy, taken before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    // A sub-faixa que o remetente mandou executar. Recusa alta quando este nó
    // não consegue rodar exatamente aquilo - nunca ajuste silencioso.
    let capture_block = {
        let (slice_start, slice_end) = {
            let st = match lock_inference_state(&state) {
                Ok(st) => st,
                Err(error) => return err500(error),
            };
            (st.start_layer, st.end_layer)
        };
        match resolve_exec_range(&meta, slice_start, slice_end) {
            Ok(block) => block,
            Err(motivo) => return err400(motivo),
        }
    };

    let queued_at = Instant::now();
    let result = match scheduler.submit_prefill(PrefillJob {
        sid_key,
        chunk_index,
        chunk_size,
        reuse_prefix,
        cached_tokens,
        prefix_match_tokens,
        start_pos,
        stable_len,
        is_last,
        is_final_chunk,
        wire_dtype,
        prompt,
        chat_template,
        tensor,
        capture_block,
        chunk_tokens,
        queued_at,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("fila de inferência cheia (limite {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("scheduler de inferência encerrado");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida")
                || e.to_string().contains("fora de ordem")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (code, e.to_string()).into_response();
        }
        Err(e) => return err500(e),
    };
    reply.timings.frame_decode_us = frame_decode_us;
    reply.timings.input_prepare_us = input_prepare_us;

    // Same reasoning as handle_step: decode once so the relay branch below
    // can inspect what came back, patch in this hop's own processing_ms/
    // node_timings either way, and only then decide whether to relay or
    // just re-encode and return.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor) = match wire::decode(&reply.bytes) {
        Ok(v) => v,
        Err(e) => return err500(e),
    };
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta — ver o bloco gêmeo em `handle_step`. A diferença é o
    // `prompt_progress`: no anel borbulhado a cabeça sobrescrevia
    // `more_chunks`/`n_prompt_tokens` na resposta que passava de volta por ela
    // (`overlay_head_authoritative_fields`). Sem volta, ela manda os dois
    // **adiante**, e quem entrega copia — só a cabeça tokenizou o prompt, e sem
    // isso todo prompt de mais de um pedaço seria truncado no primeiro.
    if let Some(reply_to) = direct_reply_to(&meta) {
        let outbound_hidden_state_bytes = match wire::tensor_payload_len(
            local_tensor
                .as_ref()
                .map(|(values, shape)| (values.as_slice(), shape.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => bytes,
            Err(error) => return err500(error),
        };
        let hop_timings = crate::direct::accumulate(
            &meta,
            crate::direct::hop_entry_with_state(
                &local_meta["processing_ms"],
                &local_meta["node_timings"],
                inbound_bytes,
                inbound_hidden_state_bytes,
                outbound_hidden_state_bytes,
                wire_dtype,
            ),
        );
        let hops = next_hops.unwrap_or_default();

        if hops.is_empty() {
            let mut result = local_meta;
            apply_prompt_progress(&meta, &mut result);
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            let failure = crate::relay::RelayFailure {
                failed_node_id: hops[0].node_id.clone(),
                kind: "protocol".into(),
                message: "hop intermediário não produziu estado oculto".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = json!({
            "session_id": sid_for_forward,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": reuse_prefix_for_forward(&local_meta, reuse_prefix, chunk_index),
            "is_final_chunk": is_final_chunk_for_forward(&local_meta, is_final_chunk),
            "prompt_progress": prompt_progress_for_forward(&meta, &local_meta),
            "hop_timings": hop_timings,
        });
        if let Some(sequence_id) = &prefill_sequence_id {
            forward["prefill_sequence_id"] = json!(sequence_id);
        }
        for field in ["start_pos", "next_pos", "stable_len"] {
            if let Some(value) = local_meta.get(field) {
                forward[field] = value.clone();
            }
        }
        preserve_batch_identity(&meta, &mut forward);
        if let Some(tokens) = local_meta.get("tokens") {
            forward["tokens"] = tokens.clone();
        }
        if chunk_index == 0 {
            if let Some(sequence_id) = &prefill_sequence_id {
                if let Err(error) = direct_forward_order.ensure_sequence(sequence_id) {
                    let failure = crate::relay::RelayFailure {
                        failed_node_id: hops[0].node_id.clone(),
                        kind: "protocol".into(),
                        message: error.to_string(),
                    };
                    tokio::spawn(async move {
                        crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
                    });
                    return direct_ack();
                }
            }
        }
        tokio::spawn(forward_direct_ordered(
            hops,
            "/v1/prefill",
            forward,
            residual,
            shape,
            wire_dtype,
            crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
            reply_to,
            Some(direct_forward_order),
            prefill_sequence_id,
            Some(chunk_index),
        ));
        let progress =
            owns_tokens(&local_meta).then(|| prompt_progress_for_forward(&meta, &local_meta));
        return direct_ack_with_progress(progress);
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode_batch_single(
            &local_meta,
            local_tensor
                .as_ref()
                .map(|(v, s)| (v.as_slice(), s.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    // Relay branch: this hop is not the tail, and the sender wants the rest
    // of the chain reached directly instead of relaying back through it.
    let hops = next_hops.expect("should_relay implies next_hops is Some");
    let (residual, shape) = local_tensor.expect("should_relay implies local_tensor is Some");
    let is_final_chunk_for_forward = is_final_chunk_for_forward(&local_meta, is_final_chunk);

    let mut bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/prefill",
        {
            let mut relay_meta = json!({
                "session_id": sid_for_forward,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "reuse_prefix": reuse_prefix_for_forward(&local_meta, reuse_prefix, chunk_index),
                "is_final_chunk": is_final_chunk_for_forward,
                // O próximo hop recebe a própria sub-faixa, pelo mesmo motivo
                // que este recebeu a sua: quem manda é que decide o pedaço.
                "exec_start": hops[0].exec_start,
                "exec_end": hops[0].exec_end,
            });
            if let Some(tokens) = local_meta.get("tokens") {
                relay_meta["tokens"] = tokens.clone();
            }
            for field in ["start_pos", "next_pos", "stable_len"] {
                if let Some(value) = local_meta.get(field) {
                    relay_meta[field] = value.clone();
                }
            }
            if let Some(value) = local_meta.get("prefix_match_tokens") {
                relay_meta["prefix_match_tokens"] = value.clone();
            }
            preserve_batch_identity(&meta, &mut relay_meta);
            relay_meta
        },
        (&residual, shape),
        wire_dtype,
        crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };

    overlay_head_authoritative_fields(&local_meta, &mut bubbled);

    match wire::encode_batch_single(&bubbled, None, wire_dtype) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

fn owns_tokens(local_meta: &Value) -> bool {
    local_meta
        .get("owns_tokens")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Only the head (`owns_tokens`) can know whether this chunk was the last
/// one — everyone else just passes through whatever `is_final_chunk` it was
/// told, since it has no `prompt_tokens` of its own to check that against.
/// Qual decisão de prefixo o hop seguinte deve obedecer.
///
/// A do peer que tokenizou, quando ele a informou (`reuse_prefix` no próprio
/// meta), não a que o gateway pediu. A cabeça pode ter recomeçado a sessão -
/// prefixo divergente, sessão despejada - e um hop reusando enquanto a cabeça
/// recomeçou deixaria as posições do pipeline desalinhadas. Peer que não
/// informa nada cai no pedido original, que é o comportamento anterior.
pub(crate) fn reuse_prefix_for_forward(
    local_meta: &Value,
    requested: bool,
    chunk_index: u32,
) -> bool {
    if chunk_index != 0 {
        return false;
    }
    local_meta
        .get("reuse_prefix")
        .and_then(|value| value.as_bool())
        .unwrap_or(requested)
}

fn is_final_chunk_for_forward(local_meta: &Value, received_is_final_chunk: bool) -> bool {
    if owns_tokens(local_meta) {
        !local_meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    } else {
        received_is_final_chunk
    }
}

/// The verified fix: only the head's own `more_chunks`/`n_prompt_tokens` are
/// authoritative. A bubbled ring response's `more_chunks`/`n_prompt_tokens`
/// came from the *tail* (always `false`/absent there, since it never owns
/// the prompt) unless overwritten — leaving it as-is would silently
/// truncate every multi-chunk prompt on a ring pipeline after one chunk.
pub(crate) fn overlay_head_authoritative_fields(local_meta: &Value, bubbled: &mut Value) {
    if owns_tokens(local_meta) {
        for field in [
            "more_chunks",
            "n_prompt_tokens",
            "start_pos",
            "next_pos",
            // Quantos tokens do prompt não foram reprocessados, e se o prefixo
            // foi de fato aproveitado. Só a cabeça sabe - a cauda responde
            // sobre um prompt que ela nunca tokenizou.
            "reuse_prefix",
            "cached_tokens",
            "prefix_match_tokens",
            "stable_len",
        ] {
            if let Some(value) = local_meta.get(field) {
                bubbled[field] = value.clone();
            }
        }
    }
}

/// Os mesmos dois campos autoritativos, empurrados **adiante** em vez de
/// sobrepostos na volta — é o que a resposta direta precisa, já que a cabeça
/// nunca mais vê o resultado. Quem não é a cabeça repassa intacto o que
/// recebeu.
pub(crate) fn prompt_progress_for_forward(inbound_meta: &Value, local_meta: &Value) -> Value {
    if owns_tokens(local_meta) {
        return json!({
            "more_chunks": local_meta["more_chunks"].clone(),
            "n_prompt_tokens": local_meta["n_prompt_tokens"].clone(),
            "start_pos": local_meta["start_pos"].clone(),
            "next_pos": local_meta["next_pos"].clone(),
            // Ver `overlay_head_authoritative_fields`: só a cabeça sabe.
            "reuse_prefix": local_meta["reuse_prefix"].clone(),
            "cached_tokens": local_meta["cached_tokens"].clone(),
            "prefix_match_tokens": local_meta["prefix_match_tokens"].clone(),
            "stable_len": local_meta["stable_len"].clone(),
        });
    }
    inbound_meta
        .get("prompt_progress")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A ponta da cadeia copia para o resultado o que a cabeça apurou do prompt.
/// Sem isto o gateway leria o `more_chunks: false` que a cauda sempre responde
/// — ela não tokenizou nada — e pararia o prefill no primeiro pedaço.
pub(crate) fn apply_prompt_progress(inbound_meta: &Value, result: &mut Value) {
    let Some(progress) = inbound_meta.get("prompt_progress") else {
        return;
    };
    for field in [
        "more_chunks",
        "n_prompt_tokens",
        "start_pos",
        "next_pos",
        "reuse_prefix",
        "cached_tokens",
        "prefix_match_tokens",
        "stable_len",
    ] {
        if let Some(value) = progress.get(field) {
            if !value.is_null() {
                result[field] = value.clone();
            }
        }
    }
}

/// Preserve the identity assigned by `BatchCoordinator` when a one-item
/// frame takes the regular handler path. `encode_batch_single` cannot infer
/// that identity from the scheduler reply, so dropping these fields makes the
/// coordinator unable to match the response to its waiting request.
pub(crate) fn preserve_batch_identity(request_meta: &Value, response_meta: &mut Value) {
    for field in ["item_id", "batch_id"] {
        if let Some(value) = request_meta.get(field) {
            response_meta[field] = value.clone();
        }
    }
}

/// Recusa um pedaço de prefill maior que o `n_batch` do contexto **antes** de
/// chamar `llama_decode`, que dispara `GGML_ASSERT(n_tokens_all <=
/// cparams.n_batch)` e aborta o processo inteiro. O gateway já clampa o tamanho
/// do pedaço em `prompt_chunk_tokens`; esta guarda cobre um head cujo `n_batch`
/// é menor que o do nó que planejou, e o item isolado que o scheduler deixa
/// passar. O erro nomeia os knobs a ajustar.
fn guard_chunk_fits_batch(engine: &LlmEngine, n_tokens: usize, chunk_index: u32) -> Result<()> {
    let n_batch = engine.n_batch();
    if n_tokens as u64 > n_batch as u64 {
        bail!(
            "pedaço {chunk_index} tem {n_tokens} tokens, acima do n_batch={n_batch} deste nó; \
             o llama.cpp abortaria. Reduza prefill_chunk_tokens para <= n_batch, ou aumente \
             n_batch, nos ajustes avançados da implantação"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prefill_blocking(
    state: SharedState,
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    reuse_prefix: bool,
    cached_tokens: u32,
    prefix_match_tokens: u32,
    start_pos: Option<u32>,
    stable_len: Option<u32>,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    chat_template: Option<bool>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    capture_block: Option<u32>,
    // A composição dos tokens deste pedaço, vinda da cabeça via `tokens` na
    // wire. Só tem sentido no branch de estado oculto (a cabeça tokeniza o
    // prompt; hops do meio ecoam): alimenta o `observe` da cabeça `nextn`
    // dentro do `decode_hidden`. `None` é peer antigo que não encaminha o
    // campo — o `decode_hidden` desliga o MTP com aviso.
    chunk_tokens: Option<Vec<i32>>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut is_final_chunk = is_final_chunk;
    let owns_prompt_tokens = prompt.is_some();
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    // O prefill espera no mesmo mutex do decode: um prompt grande de outra
    // sessão segura este aqui, e é a mesma espera invisível.
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    timings.selected_path = s.inference_path;

    // ── Session setup ────────────────────────────────────────────────────────

    let setup_started = Instant::now();
    // Reusar exige a sessão viva aqui. Ela pode ter sido despejada ou reciclada
    // desde o turno anterior; nesse caso este peer simplesmente recomeça, e é a
    // resposta dele (`reuse_prefix` no meta) que os hops seguintes obedecem.
    let want_reuse = reuse_prefix
        && chunk_index == 0
        && s.sessions
            .get(&sid_key)
            .is_some_and(|session| session.retained);
    s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
    let mut prefix = PrefixDecision::default();
    // Quem não tokeniza recebe a fronteira de quem tokenizou, para partir o
    // prompt na mesma posição - ver `Session::stable_len`.
    if let Some(stable) = stable_len {
        s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
    }

    // First peer: tokenize the prompt on chunk 0.
    // We must not hold a &mut Session borrow while calling engine methods, so
    // compute everything we need, store results, then re-borrow.
    if let Some(ref text) = prompt {
        if chunk_index == 0 {
            let use_tmpl = use_chat_template(chat_template, s.chat_template);
            let no_think = s.no_thinking;
            let formatted = if use_tmpl {
                s.engine
                    .apply_chat_template(&[("user", text)], true, no_think)?
            } else {
                text.clone()
            };
            let add_bos = !use_tmpl;
            let tokens = s.engine.tokenize(&formatted, add_bos, true);
            let stable = stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
            prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
        }
    }
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    // ── Rollback / order check ───────────────────────────────────────────────
    let expected_pos = expected_prefill_pos(start_pos, chunk_index, s.sessions[&sid_key].n_past)?;
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if session.n_past > expected_pos {
            warn!(
                "refazendo chunk {chunk_index} da sessão {}… (pos {expected_pos})",
                &sid_key[..8.min(sid_key.len())]
            );
            let seq_id = session.seq_id();
            if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                bail!(
                    "este modelo não aceita descartar posições do meio do cache, \
                     então refazer o pedaço {chunk_index} corromperia a sessão"
                );
            }
            s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
        } else if session.n_past < expected_pos {
            bail!(
                "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                session.n_past
            );
        }
    }

    // ── Decode ───────────────────────────────────────────────────────────────
    // Extract the session values we need as plain integers, drop the borrow,
    // then call the engine (which needs &mut s.engine).
    // A partir da fronteira, o prompt deixa de ser reutilizável e passa a viver
    // na cópia de geração junto com os tokens gerados - ver `Session::stable_len`.
    // Um pedaço nunca atravessa a fronteira: ele para nela, e o seguinte começa
    // já na outra sequência.
    let stable = s.sessions[&sid_key].stable_len;
    if expected_pos >= stable {
        s.open_generation(&sid_key)?;
    }
    let (n_past, seq_id, tokens_for_chunk) = {
        let session = &s.sessions[&sid_key];
        let toks = if tensor.is_none() {
            let start = expected_pos as usize;
            let mut end = (expected_pos + chunk_size) as usize;
            if expected_pos < stable {
                end = end.min(stable as usize);
            }
            session
                .prompt_tokens
                .get(start..end.min(session.prompt_tokens.len()))
                .unwrap_or(&[])
                .to_vec()
        } else {
            vec![]
        };
        (session.n_past, session.seq_id(), toks)
    };

    // Onde parar, quando o remetente pediu menos que a fatia inteira. Antes do
    // decode e sob o mesmo lock, que é o que torna o ajuste válido só para
    // esta decodificação.
    if let Err(e) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            bail!("{e}");
        }
    }

    let input_was_hidden = tensor.is_some();
    let selected_moe = s.inference_path.is_moe();
    let mut moe_forwarded = false;
    let tensor = if let Some((hidden, shape)) = tensor {
        let hidden = s.apply_moe_hidden(hidden, shape[0], capture_block, &mut timings)?;
        moe_forwarded = selected_moe
            && s.engine.moe_external_executor_enabled()
            && !s.engine.moe_hook_enabled();
        Some((hidden, shape))
    } else {
        None
    };

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    if let Some((hidden, shape)) = tensor {
        n_tokens = shape[0];
        guard_chunk_fits_batch(&s.engine, n_tokens, chunk_index)?;
        engine_timing = s.engine.decode_hidden(
            &hidden,
            &shape,
            n_tokens,
            n_past,
            seq_id,
            !is_last,
            chunk_tokens.as_deref(),
        )?;
    } else {
        if tokens_for_chunk.is_empty() {
            bail!("chunk {chunk_index} não tem tokens");
        }
        n_tokens = tokens_for_chunk.len();
        guard_chunk_fits_batch(&s.engine, n_tokens, chunk_index)?;
        engine_timing = s
            .engine
            .decode_tokens(&tokens_for_chunk, n_past, seq_id, !is_last)?;
    }
    if is_last
        && selected_moe
        && s.engine.moe_external_executor_enabled()
        && !moe_forwarded
        && !s.engine.moe_hook_enabled()
    {
        let residual = s.engine.take_residual(n_tokens)?;
        let _ = s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
        moe_forwarded = true;
    }
    timings.merge_engine_moe(&engine_timing);
    moe_forwarded |= engine_timing.moe_forward_us != 0;
    if is_last {
        record_terminal_engine(&mut timings, s.inference_path, moe_forwarded);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    timings.tp = crate::tp::take_decode_telemetry();
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    // ── Session update ───────────────────────────────────────────────────────
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        session.n_past += n_tokens as u32;
        session.last_used = Instant::now();
    }

    let (total_tokens, n_past_now) = {
        let session = &s.sessions[&sid_key];
        (session.prompt_tokens.len() as u32, session.n_past)
    };
    let owns_tokens = total_tokens > 0; // this peer has prompt_tokens → token path
    let more = owns_tokens && (n_past_now < total_tokens);
    if owns_tokens && !more {
        is_final_chunk = true;
    }
    // A fronteira entre prompt e geração, anotada por cada peer no pedaço em
    // que o prompt acaba. É para cá que o fim do turno devolve a sessão, e é o
    // que o `/v1/session/rewind` confere. Hops do meio não tokenizam nada, e
    // por isso recebem `is_final_chunk` da cabeça em vez de deduzi-lo.
    if is_final_chunk {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        session.prompt_len = n_past_now.min(session.stable_len);
    }

    // ── Sampling / residual ──────────────────────────────────────────────────
    let sampling = s.sampling.clone();

    if is_last {
        if !is_final_chunk {
            let encode_started = Instant::now();
            let mut partial =
                partial_prefill_meta(more, total_tokens as usize, expected_pos, n_past_now);
            apply_prefix_decision(
                &mut partial,
                chunk_index,
                prefix,
                reuse_prefix,
                cached_tokens,
                prefix_match_tokens,
                owns_prompt_tokens,
            );
            apply_stable_len(&mut partial, stable);
            let bytes = wire::encode(&partial, None, wire_dtype)?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            return Ok(TimedReply { bytes, timings });
        }
        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_profiled(logits, &recent, &sampling);
        let done = s.engine.is_generation_stop(&recent, next_id);
        let token_text_started = Instant::now();
        let mut text = if done {
            String::new()
        } else {
            let piece = s.engine.token_to_piece_bytes(next_id, false);
            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .feed_token_piece(&piece)
        };
        let token_text_us = token_text_started.elapsed().as_micros() as u64;
        if done {
            finish_terminal_stream(&mut s.sessions, &sid_key, &mut text)?;
        }
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        // A réplica de um grupo TP não encerra sessão: quem decide o fim da
        // geração é o rank 0. Ver `tp::is_replica`.
        //
        // Encerrar aqui destruía a sessão antes de o rewind do gateway chegar,
        // e o rewind respondia "sessão desconhecida" - o prefixo nunca entrava
        // no cache. O fim do turno agora **retém** o prompt: some só a
        // sequência da geração. Liberar de vez continua sendo ato explícito do
        // gateway (`/v1/session/release`).
        if done && !crate::tp::is_replica() {
            s.retain_prompt(&sid_key);
        }
        timings.sampling_us += logits_access_us
            + sampling_timing.logits_clone_us
            + sampling_timing.repetition_penalty_us
            + sampling_timing.softmax_us
            + sampling_timing.top_k_us
            + sampling_timing.top_p_us
            + sampling_timing.selection_us
            + token_text_us;
        timings.logits_access_us += logits_access_us;
        timings.logits_clone_us += sampling_timing.logits_clone_us;
        timings.repetition_penalty_us += sampling_timing.repetition_penalty_us;
        timings.softmax_us += sampling_timing.softmax_us;
        timings.top_k_us += sampling_timing.top_k_us;
        timings.top_p_us += sampling_timing.top_p_us;
        timings.selection_us += sampling_timing.selection_us;
        timings.token_text_us += token_text_us;
        let encode_started = Instant::now();
        let n_prompt_tokens = s
            .sessions
            .get(&sid_key)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid_key}' desapareceu antes da resposta"))?
            .prompt_tokens
            .len();
        let mut reply = terminal_prefill_meta(
            n_prompt_tokens,
            expected_pos,
            expected_pos + n_tokens as u32,
            done,
        );
        apply_prefix_decision(
            &mut reply,
            chunk_index,
            prefix,
            reuse_prefix,
            cached_tokens,
            prefix_match_tokens,
            owns_prompt_tokens,
        );
        apply_stable_len(&mut reply, stable);
        reply["token_id"] = json!(next_id);
        reply["text"] = json!(text);
        let bytes = wire::encode(&reply, None, wire_dtype)?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let mut residual = s.engine.take_residual(n_tokens)?;
    if !input_was_hidden {
        residual.values =
            s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
    }
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let n_prompt_tokens = s.sessions[&sid_key].prompt_tokens.len();
    let mut reply_meta = json!({
        "done": false,
        "more_chunks": more,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": expected_pos,
        "next_pos": n_past_now,
        // Only meaningful when true: whether *this* peer is the one that
        // tokenized the prompt, i.e. whether `more_chunks`/`n_prompt_tokens`
        // above are its own authoritative determination rather than the
        // `false`/`0` placeholder every other peer replies with. Ring
        // mode's relay-and-bubble uses this to know which hop's reply is
        // allowed to overwrite what the tail bubbles back — see
        // `handle_prefill`.
        "owns_tokens": owns_tokens,
    });
    apply_prefix_decision(
        &mut reply_meta,
        chunk_index,
        prefix,
        reuse_prefix,
        cached_tokens,
        prefix_match_tokens,
        owns_prompt_tokens,
    );
    apply_stable_len(&mut reply_meta, stable);
    // A composição real deste pedaço, para a cauda manter a cabeça `nextn` em
    // dia (ver o doc de `chunk_tokens`). A cabeça conhece os tokens; hops do
    // meio ecoam o que receberam.
    if owns_tokens {
        reply_meta["tokens"] = json!(tokens_for_chunk);
    } else if let Some(tokens) = &chunk_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual.values, residual.shape.clone())),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}

struct PreparedPrefill {
    sid_key: String,
    seq_id: u32,
    n_past: u32,
    n_tokens: usize,
    start_pos: u32,
    chunk_index: u32,
    /// O que este peer executou com o prefixo retido - ver `apply_prefix_decision`.
    prefix: PrefixDecision,
    /// Decisão recebida da cabeça para ecoar quando este peer não tokeniza.
    requested_reuse: bool,
    requested_cached_tokens: u32,
    requested_prefix_match_tokens: u32,
    /// A fronteira medida - ver `Session::stable_len`.
    stable_len: u32,
    is_last: bool,
    is_final_chunk: bool,
    owns_tokens: bool,
    tokens_for_chunk: Vec<i32>,
    chunk_tokens: Option<Vec<i32>>,
    input: PreparedPrefillInput,
    wire_dtype: WireDtype,
    capture_block: Option<u32>,
    queued_at: Instant,
}

enum PreparedPrefillInput {
    Tokens(Vec<i32>),
    Hidden {
        values: Vec<f32>,
        shape: TensorShape,
    },
}

fn prefill_batch_blocking(
    state: SharedState,
    jobs: Vec<(PrefillJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => {
            let message = error.to_string();
            return jobs
                .into_iter()
                .map(|(_, reply)| (reply, Err(anyhow::anyhow!(message.clone()))))
                .collect();
        }
    };
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let selected_path = s.inference_path;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let PrefillJob {
            sid_key,
            chunk_index,
            chunk_size,
            reuse_prefix,
            cached_tokens,
            prefix_match_tokens,
            start_pos,
            stable_len,
            is_last,
            is_final_chunk,
            wire_dtype,
            prompt,
            chat_template,
            tensor,
            capture_block,
            chunk_tokens,
            queued_at,
        } = job;

        let result = (|| -> Result<PreparedPrefill> {
            // Mesmo contrato do caminho não-lotado (`prefill_blocking`): reuso
            // só com a sessão viva, e quem tokeniza decide se o prefixo serve.
            let want_reuse = reuse_prefix
                && chunk_index == 0
                && s.sessions
                    .get(&sid_key)
                    .is_some_and(|session| session.retained);
            s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
            let mut prefix = PrefixDecision::default();
            // Ver `prefill_blocking`: quem não tokeniza recebe a fronteira.
            if let Some(stable) = stable_len {
                s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
            }
            if let Some(text) = prompt.as_deref() {
                if chunk_index == 0 {
                    let use_tmpl = use_chat_template(chat_template, s.chat_template);
                    let no_think = s.no_thinking;
                    let formatted = if use_tmpl {
                        s.engine
                            .apply_chat_template(&[("user", text)], true, no_think)?
                    } else {
                        text.to_string()
                    };
                    let tokens = s.engine.tokenize(&formatted, !use_tmpl, true);
                    let stable =
                        stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
                    prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
                }
            }

            let expected_pos =
                expected_prefill_pos(start_pos, chunk_index, s.sessions[&sid_key].n_past)?;
            {
                let session = s.sessions.get(&sid_key).unwrap();
                if session.n_past > expected_pos {
                    let seq_id = session.seq_id();
                    if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                        bail!(
                            "este modelo não aceita descartar posições do meio do cache, \
                             então refazer o pedaço {chunk_index} corromperia a sessão"
                        );
                    }
                    s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
                } else if session.n_past < expected_pos {
                    bail!(
                        "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                        session.n_past
                    );
                }
            }

            // Ver `prefill_blocking`: o pedaço para na fronteira, e daí em
            // diante decodifica na cópia de geração.
            let stable = s.sessions[&sid_key].stable_len;
            if expected_pos >= stable {
                s.open_generation(&sid_key)?;
            }
            let (seq_id, n_past, prompt_tokens, input, n_tokens) = {
                let session = &s.sessions[&sid_key];
                if let Some((hidden, shape)) = tensor {
                    let n_tokens = shape.first().copied().unwrap_or(0);
                    crate::llm::validate_hidden_tensor(
                        hidden.len(),
                        &shape,
                        n_tokens,
                        s.engine.n_embd,
                        s.engine.n_embd_inp,
                        s.engine.n_embd_out,
                        &format!("hidden state de {sid_key}"),
                    )?;
                    (
                        session.seq_id(),
                        session.n_past,
                        Vec::new(),
                        PreparedPrefillInput::Hidden {
                            values: hidden,
                            shape,
                        },
                        n_tokens,
                    )
                } else {
                    let start = expected_pos as usize;
                    let mut end = start.saturating_add(chunk_size as usize);
                    if expected_pos < stable {
                        end = end.min(stable as usize);
                    }
                    let tokens = session
                        .prompt_tokens
                        .get(start..end.min(session.prompt_tokens.len()))
                        .unwrap_or(&[])
                        .to_vec();
                    if tokens.is_empty() {
                        bail!("chunk {chunk_index} não tem tokens");
                    }
                    let n_tokens = tokens.len();
                    (
                        session.seq_id(),
                        session.n_past,
                        tokens.clone(),
                        PreparedPrefillInput::Tokens(tokens),
                        n_tokens,
                    )
                }
            };
            let owns_tokens =
                !prompt_tokens.is_empty() || !s.sessions[&sid_key].prompt_tokens.is_empty();

            Ok(PreparedPrefill {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                start_pos: expected_pos,
                chunk_index,
                prefix,
                requested_reuse: reuse_prefix,
                requested_cached_tokens: cached_tokens,
                requested_prefix_match_tokens: prefix_match_tokens,
                stable_len: stable,
                is_last,
                is_final_chunk,
                owns_tokens,
                tokens_for_chunk: prompt_tokens,
                chunk_tokens,
                input,
                wire_dtype,
                capture_block,
                queued_at,
            })
        })();

        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }

    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    // Onde parar vale para o grafo inteiro, não por item: um lote com pedidos
    // de sub-faixas diferentes não tem uma resposta certa. O agrupamento em
    // lote é conveniência de desempenho; ele não pode decidir isto por conta.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "lote com sub-faixas diferentes: cada uma pararia num bloco, e o grafo é um só"
                )),
            ));
        }
        return replies;
    }
    if let Err(e) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!("{e}"))));
            }
            return replies;
        }
    }

    let mut transform_error = None;
    let mut moe_input_items: HashMap<String, MoeTimings> = HashMap::new();
    for (_, item) in &mut prepared {
        if let PreparedPrefillInput::Hidden { values, .. } = &mut item.input {
            let mut transform_timings = NodeTimings::default();
            match s.apply_moe_hidden(
                std::mem::take(values),
                item.n_tokens,
                capture_block,
                &mut transform_timings,
            ) {
                Ok(transformed) => *values = transformed,
                Err(error) => {
                    transform_error = Some(error);
                    break;
                }
            }
            if !transform_timings.moe.executors.is_empty() {
                moe_input_items.insert(item.sid_key.clone(), transform_timings.moe);
            }
        }
    }
    if let Some(error) = transform_error {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }

    // Um `llama_decode` com mais tokens que `n_batch` aborta o processo. O
    // scheduler já corta o lote em `n_batch`, mas deixa passar um item isolado
    // maior que isso; recusa aqui é um pedido perdido, não o nó inteiro.
    let batch_tokens: usize = prepared.iter().map(|(_, item)| item.n_tokens).sum();
    if let Err(error) = guard_chunk_fits_batch(&s.engine, batch_tokens, prepared[0].1.chunk_index) {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }

    let decode_result = match &prepared[0].1.input {
        PreparedPrefillInput::Tokens(_) => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedPrefillInput::Tokens(tokens) => tokens.clone(),
                        PreparedPrefillInput::Hidden { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedPrefillInput::Hidden { .. } => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedPrefillInput::Hidden { values, .. } => values.clone(),
                        PreparedPrefillInput::Tokens(_) => Vec::new(),
                    },
                    shape: match &item.input {
                        PreparedPrefillInput::Hidden { shape, .. } => shape.clone(),
                        PreparedPrefillInput::Tokens(_) => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };

    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let session = s.sessions.get_mut(&item.sid_key).unwrap();
            session.n_past = session.n_past.saturating_add(item.n_tokens as u32);
            session.last_used = Instant::now();
            let total_tokens = session.prompt_tokens.len() as u32;
            let n_past_now = session.n_past;
            let more = item.owns_tokens && n_past_now < total_tokens;
            let mut is_final_chunk = item.is_final_chunk;
            if item.owns_tokens && !more {
                is_final_chunk = true;
            }
            // Ver `prefill_blocking`: a fronteira prompt/geração desta sessão.
            if is_final_chunk {
                let session = s.sessions.get_mut(&item.sid_key).unwrap();
                session.prompt_len = n_past_now.min(session.stable_len);
            }

            let mut timings = NodeTimings {
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };
            timings.merge_engine_moe(&engine_timings);
            if let Some(moe) = moe_input_items.remove(&item.sid_key) {
                timings.merge_moe(&moe);
                timings.record_engine(crate::executor::ExecutionEngine::Moe);
            }
            timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);

            if item.is_last {
                if !is_final_chunk {
                    let started = Instant::now();
                    let mut meta = partial_prefill_meta(
                        more,
                        total_tokens as usize,
                        item.start_pos,
                        item.start_pos + item.n_tokens as u32,
                    );
                    apply_prefix_decision(
                        &mut meta,
                        item.chunk_index,
                        item.prefix,
                        item.requested_reuse,
                        item.requested_cached_tokens,
                        item.requested_prefix_match_tokens,
                        item.owns_tokens,
                    );
                    apply_stable_len(&mut meta, item.stable_len);
                    let bytes = wire::encode(&meta, None, item.wire_dtype)?;
                    timings.encode_us = started.elapsed().as_micros() as u64;
                    return Ok(TimedReply { bytes, timings });
                }

                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let piece = s.engine.token_to_piece_bytes(next_id, false);
                let mut text = s
                    .sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .feed_token_piece(&piece);
                let done = next_id == s.engine.eos_token();
                if done {
                    finish_terminal_stream(&mut s.sessions, &item.sid_key, &mut text)?;
                }
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                // Ver `prefill_blocking`: o fim do turno retém o prompt em vez
                // de destruir a sessão.
                if done && !crate::tp::is_replica() {
                    s.retain_prompt(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us;
                let token_text_started = Instant::now();
                let mut meta = json!({
                    "done": done,
                    "token_id": next_id,
                    "text": text,
                    "more_chunks": false,
                    "n_prompt_tokens": total_tokens,
                    "start_pos": item.start_pos,
                    "next_pos": item.start_pos + item.n_tokens as u32,
                });
                apply_prefix_decision(
                    &mut meta,
                    item.chunk_index,
                    item.prefix,
                    item.requested_reuse,
                    item.requested_cached_tokens,
                    item.requested_prefix_match_tokens,
                    item.owns_tokens,
                );
                apply_stable_len(&mut meta, item.stable_len);
                let bytes = wire::encode(&meta, None, item.wire_dtype)?;
                timings.token_text_us = token_text_started.elapsed().as_micros() as u64;
                timings.encode_us = 0;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let input_was_hidden = matches!(&item.input, PreparedPrefillInput::Hidden { .. });
            let (mut residual, residual_shape) =
                output.residual_for(&item.sid_key, s.engine.n_embd)?;
            if !input_was_hidden {
                residual =
                    s.apply_moe_hidden(residual, item.n_tokens, item.capture_block, &mut timings)?;
            }
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let mut meta = json!({
                "done": false,
                "more_chunks": more,
                "n_prompt_tokens": total_tokens,
                "start_pos": item.start_pos,
                "next_pos": n_past_now,
                "owns_tokens": item.owns_tokens,
            });
            apply_prefix_decision(
                &mut meta,
                item.chunk_index,
                item.prefix,
                item.requested_reuse,
                item.requested_cached_tokens,
                item.requested_prefix_match_tokens,
                item.owns_tokens,
            );
            apply_stable_len(&mut meta, item.stable_len);
            if item.owns_tokens {
                meta["tokens"] = json!(item.tokens_for_chunk);
            } else if let Some(tokens) = &item.chunk_tokens {
                meta["tokens"] = json!(tokens);
            }
            let started = Instant::now();
            let bytes = wire::encode(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.encode_us = started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

fn step_batch_blocking(
    state: SharedState,
    jobs: Vec<(StepJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    if jobs.iter().any(|(job, _)| {
        job.remote_stream
            || job.speculate.is_some()
            || job.incoming_draft.is_some()
            || job.mtp_k.is_some()
    }) {
        return jobs
            .into_iter()
            .map(|(job, reply)| {
                let result = step_blocking(
                    state.clone(),
                    job.sid_key,
                    job.is_last,
                    job.remote_stream,
                    job.wire_dtype,
                    job.token_id,
                    job.tensor,
                    job.capture_block,
                    job.speculate,
                    job.incoming_draft,
                    job.batch_tokens,
                    job.mtp_k,
                    job.queued_at,
                );
                (reply, result)
            })
            .collect();
    }

    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => {
            let message = error.to_string();
            return jobs
                .into_iter()
                .map(|(_, reply)| (reply, Err(anyhow::anyhow!(message.clone()))))
                .collect();
        }
    };
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let selected_path = s.inference_path;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let StepJob {
            sid_key,
            is_last,
            wire_dtype,
            token_id,
            tensor,
            capture_block,
            queued_at,
            ..
        } = job;
        let result = (|| -> Result<PreparedStep> {
            // Tudo daqui em diante é token gerado, então nada mais toca a
            // sequência do prompt - ver `InferState::open_generation`.
            s.open_generation(&sid_key)?;
            let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
                anyhow::anyhow!("sessão '{}' desconhecida - faça o prefill antes", sid_key)
            })?;
            session.last_used = Instant::now();
            let seq_id = session.seq_id();
            let n_past = session.n_past;
            let input = if let Some((hidden, shape)) = tensor {
                let n_tokens = shape.first().copied().unwrap_or(0);
                crate::llm::validate_hidden_tensor(
                    hidden.len(),
                    &shape,
                    n_tokens,
                    s.engine.n_embd,
                    s.engine.n_embd_inp,
                    s.engine.n_embd_out,
                    &format!("hidden state de {sid_key}"),
                )?;
                PreparedStepInput::Hidden {
                    values: hidden,
                    shape,
                }
            } else {
                let token =
                    token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
                s.sessions
                    .get_mut(&sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(token);
                PreparedStepInput::Tokens { token }
            };
            let n_tokens = match &input {
                PreparedStepInput::Tokens { .. } => 1,
                PreparedStepInput::Hidden { shape, .. } => shape[0],
            };
            Ok(PreparedStep {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                is_last,
                wire_dtype,
                capture_block,
                input,
                queued_at,
            })
        })();
        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }
    // Um único grafo não consegue parar em dois blocos diferentes. Recusar o
    // lote é seguro; escolher um dos limites faria uma sessão executar mais
    // ou menos camadas que o remetente pediu.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "lote com sub-faixas diferentes: cada uma pararia num bloco, e o grafo é um só"
                )),
            ));
        }
        return replies;
    }
    if let Err(error) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(error.to_string()))));
            }
            return replies;
        }
    }
    let mut transform_error = None;
    let mut moe_input_items: HashMap<String, MoeTimings> = HashMap::new();
    for (_, item) in &mut prepared {
        if let PreparedStepInput::Hidden { values, shape } = &mut item.input {
            let mut transform_timings = NodeTimings::default();
            match s.apply_moe_hidden(
                std::mem::take(values),
                shape[0],
                capture_block,
                &mut transform_timings,
            ) {
                Ok(transformed) => *values = transformed,
                Err(error) => {
                    transform_error = Some(error);
                    break;
                }
            }
            if !transform_timings.moe.executors.is_empty() {
                moe_input_items.insert(item.sid_key.clone(), transform_timings.moe);
            }
        }
    }
    if let Some(error) = transform_error {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }
    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    let decode_result = match &prepared[0].1.input {
        PreparedStepInput::Tokens { .. } => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedStepInput::Tokens { token } => vec![*token],
                        PreparedStepInput::Hidden { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedStepInput::Hidden { .. } => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedStepInput::Hidden { values, .. } => values.clone(),
                        PreparedStepInput::Tokens { .. } => Vec::new(),
                    },
                    shape: match &item.input {
                        PreparedStepInput::Hidden { shape, .. } => shape.clone(),
                        PreparedStepInput::Tokens { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };
    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let new_n_past = s.sessions[&item.sid_key]
                .n_past
                .saturating_add(item.n_tokens as u32);
            s.sessions.get_mut(&item.sid_key).unwrap().n_past = new_n_past;
            let mut timings = NodeTimings {
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };
            timings.merge_engine_moe(&engine_timings);
            if let Some(moe) = moe_input_items.remove(&item.sid_key) {
                timings.merge_moe(&moe);
                timings.record_engine(crate::executor::ExecutionEngine::Moe);
            }
            timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);

            if item.is_last {
                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let token_text_started = Instant::now();
                let piece = s.engine.token_to_piece_bytes(next_id, false);
                let mut text = s
                    .sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .feed_token_piece(&piece);
                let token_text_us = token_text_started.elapsed().as_micros() as u64;
                let done = next_id == s.engine.eos_token();
                if done {
                    finish_terminal_stream(&mut s.sessions, &item.sid_key, &mut text)?;
                }
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                // Fim de turno retém o prompt - ver `InferState::retain_prompt`.
                if done && !crate::tp::is_replica() {
                    s.retain_prompt(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.token_text_us = token_text_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us
                    + token_text_us;
                let encode_started = Instant::now();
                let bytes = wire::encode(
                    &json!({"done": done, "token_id": next_id, "text": text}),
                    None,
                    item.wire_dtype,
                )?;
                timings.encode_us = encode_started.elapsed().as_micros() as u64;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let input_was_hidden = matches!(&item.input, PreparedStepInput::Hidden { .. });
            let (mut residual, residual_shape) =
                output.residual_for(&item.sid_key, s.engine.n_embd)?;
            if !input_was_hidden {
                residual =
                    s.apply_moe_hidden(residual, item.n_tokens, item.capture_block, &mut timings)?;
            }
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let tokens = match &item.input {
                PreparedStepInput::Tokens { token } => vec![*token],
                PreparedStepInput::Hidden { .. } => Vec::new(),
            };
            let mut meta = json!({"done": false});
            if !tokens.is_empty() {
                meta["tokens"] = json!(tokens);
            }
            let encode_started = Instant::now();
            let bytes = wire::encode(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.encode_us = encode_started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

struct PreparedStep {
    sid_key: String,
    seq_id: u32,
    n_past: u32,
    n_tokens: usize,
    is_last: bool,
    wire_dtype: WireDtype,
    capture_block: Option<u32>,
    input: PreparedStepInput,
    queued_at: Instant,
}

enum PreparedStepInput {
    Tokens {
        token: i32,
    },
    Hidden {
        values: Vec<f32>,
        shape: TensorShape,
    },
}

// ── /v1/step ─────────────────────────────────────────────────────────────────

async fn handle_step(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    // Só o cabeçalho é lido aqui: um quadro de um item - o caso normal de
    // todo salto interno - não pode pagar uma cópia do estado oculto inteiro
    // só para descobrir que não é um lote.
    if wire::peek_batch_header(&body).is_ok_and(|header| header.item_count > 1) {
        match wire::decode_batch(&body) {
            Ok(batch) => return handle_step_batch(app, headers, batch).await,
            Err(error) => return err400(format!("quadro inválido: {error}")),
        }
    }
    // See the matching guard in `handle_prefill`: a one-item batch must not
    // overwrite the decode/barrier state of another TP request while its
    // ranks are still in the collective.
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_request_guard = if tp_active && headers.get(BATCH_CHILD_HEADER).is_none() {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let state = app.state.clone();
    let scheduler = app.scheduler.clone();
    open_tp_decode(&state, &headers, "/v1/step", &body);
    let processing_started = Instant::now();
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;
    let wire_dtype = match meta.get("dtype").and_then(|value| value.as_str()) {
        Some(value) => match WireDtype::parse(value) {
            Ok(dtype) => dtype,
            Err(error) => return err400(error),
        },
        None => return err400("quadro sem dtype de estado oculto"),
    };

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__session__")
        .to_string();
    // Present and non-empty only when the sender negotiated relay mode
    // (`PipelinePlan::supports_relay_chain`) — the ordered chain from *our*
    // next hop through the tail. An empty list still parses as `Some(vec![])`
    // and behaves exactly like `None`: this hop is the tail either way.
    let next_hops: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = next_hops
        .as_ref()
        .map(|hops| hops.is_empty())
        .unwrap_or_else(|| {
            meta.get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    let remote_stream = meta
        .get("remote_stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let token_id = meta
        .get("token_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    // Present only on the request to the head — see `step_blocking`'s
    // `speculate` param doc.
    let speculate = Speculate::from_meta(&meta);
    // Present on every non-head request in a speculative round — see
    // `step_blocking`'s `incoming_draft` param doc for why `None` and
    // `Some(vec![])` mean different things here.
    let incoming_draft: Option<Vec<i32>> = meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A composição do lote decodificado na cabeça, ecoada de hop em hop até a
    // cauda — é o que alimenta o `observe` da cabeça `nextn` (ver o doc de
    // `batch_tokens` em `step_blocking`). Ausente é peer antigo.
    let batch_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A largura de lote que o gateway quer na rodada seguinte, gerada na
    // cabeça (do `speculate`) e ecoada até a cauda (ver o doc de `mtp_k`).
    let mtp_k: Option<u32> = meta.get("mtp_k").and_then(|v| v.as_u64()).map(|v| v as u32);

    let input_prepare_started = Instant::now();
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };
    let inbound_hidden_state_bytes = match wire::tensor_payload_len(
        tensor
            .as_ref()
            .map(|(values, shape)| (values.as_slice(), shape.clone())),
        wire_dtype,
    ) {
        Ok(bytes) => bytes,
        Err(error) => return err400(error),
    };
    let input_prepare_us = input_prepare_started.elapsed().as_micros() as u64;
    // A sub-faixa só é válida se este peer consegue executá-la exatamente.
    // Sem os campos, o contrato antigo continua rodando a fatia inteira.
    let capture_block = {
        let (slice_start, slice_end) = match lock_inference_state(&state) {
            Ok(st) => (st.start_layer, st.end_layer),
            Err(error) => return err500(error),
        };
        match resolve_exec_range(&meta, slice_start, slice_end) {
            Ok(block) => block,
            Err(motivo) => return err400(motivo),
        }
    };
    // Neither of `step_blocking`'s replies echoes `session_id` back — a
    // relay branch that forwards to the next hop needs its own copy, taken
    // before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    let queued_at = Instant::now();
    let result = match scheduler.submit_step(StepJob {
        sid_key,
        is_last,
        remote_stream,
        wire_dtype,
        token_id,
        tensor,
        capture_block,
        speculate,
        incoming_draft,
        batch_tokens,
        mtp_k,
        queued_at,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("fila de inferência cheia (limite {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("scheduler de inferência encerrado");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (code, e.to_string()).into_response();
        }
        Err(e) => return err500(e),
    };
    reply.timings.frame_decode_us = frame_decode_us;
    reply.timings.input_prepare_us = input_prepare_us;

    // Decode what step_blocking already encoded so we can tell a residual
    // (needs relaying, if `next_hops` says so) from a final answer (the
    // tail's `{token_id, text, done}`, with no tensor) — and, either way, to
    // patch in this hop's own `processing_ms`/`node_timings` before it goes
    // any further. `handle_prefill` does the identical patch for the same
    // reason.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor) = match wire::decode(&reply.bytes) {
        Ok(v) => v,
        Err(e) => return err500(e),
    };
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta: quem terminar entrega ao gateway, e o que sai daqui é
    // só um aceite. Cada hop descreve a si mesmo numa lista que viaja na ida.
    if let Some(reply_to) = direct_reply_to(&meta) {
        let outbound_hidden_state_bytes = match wire::tensor_payload_len(
            local_tensor
                .as_ref()
                .map(|(values, shape)| (values.as_slice(), shape.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => bytes,
            Err(error) => return err500(error),
        };
        let hop_timings = crate::direct::accumulate(
            &meta,
            crate::direct::hop_entry_with_state(
                &local_meta["processing_ms"],
                &local_meta["node_timings"],
                inbound_bytes,
                inbound_hidden_state_bytes,
                outbound_hidden_state_bytes,
                wire_dtype,
            ),
        );
        let hops = next_hops.unwrap_or_default();

        if hops.is_empty() {
            // Cauda: o resultado sai daqui direto para quem pediu.
            let mut result = local_meta;
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            // Não somos a cauda e não temos residual: a cadeia não anda. Dizer
            // isso ao gateway agora vale mais que deixá-lo esperar o prazo.
            let failure = crate::relay::RelayFailure {
                failed_node_id: hops[0].node_id.clone(),
                kind: "protocol".into(),
                message: "hop intermediário não produziu estado oculto".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = step_forward_meta(&sid_for_forward, &meta, &local_meta, &hops[0]);
        forward["hop_timings"] = json!(hop_timings);
        tokio::spawn(forward_direct(
            hops, "/v1/step", forward, residual, shape, wire_dtype,
            1, // a decode step has never retried — see relay_and_bubble's doc comment
            reply_to,
        ));
        return direct_ack();
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode_batch_single(
            &local_meta,
            local_tensor
                .as_ref()
                .map(|(v, s)| (v.as_slice(), s.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    // Relay branch: this hop is not the tail, and the sender wants the rest
    // of the chain reached directly instead of relaying back through it.
    let hops = next_hops.expect("should_relay implies next_hops is Some");
    let (residual, shape) = local_tensor.expect("should_relay implies local_tensor is Some");
    let forward_meta = step_forward_meta(&sid_for_forward, &meta, &local_meta, &hops[0]);

    let bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/step",
        forward_meta,
        (&residual, shape),
        wire_dtype,
        1, // a decode step has never retried — see relay_and_bubble's doc comment
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };
    if remote_stream {
        let accept_count = bubbled
            .get("accept_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let batch_width = bubbled
            .get("batch_width")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if batch_width > 0 && accept_count < batch_width {
            match rollback_session(&state, &sid_for_forward, accept_count, batch_width) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return err500("rollback especulativo remoto recusado no hop");
                }
            }
        }
    }
    match wire::encode_batch_single(&bubbled, None, wire_dtype) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Estado do canal persistente: o do servidor de inferência, mais a credencial
/// que precisa ser reconferida a cada quadro. Ver
/// [`inference_credential_is_valid`].
#[derive(Clone)]
struct LinkState {
    app: InferenceAppState,
    auth: Option<(crate::auth::AuthState, Option<Arc<crate::db::PomDb>>)>,
}

/// Serve a ligação persistente da aresta da pipeline. Cada quadro que chega
/// pelo fluxo é despachado pela mesma tabela de rotas em processo que o
/// caminho por requisição usa (`InProcessInference::dispatch`), então canal e
/// requisição executam exatamente o mesmo código de inferência - e passam pela
/// mesma conferência de credencial.
async fn handle_link(
    State(state): State<LinkState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Response {
    let token = inference_credential_from_headers(&headers);
    crate::link::serve(body, move |path, bytes| {
        let state = state.clone();
        let token = token.clone();
        async move {
            if let Some((config, database)) = &state.auth {
                let Some(token) = token else {
                    return Err(anyhow::anyhow!("quadro de canal sem credencial"));
                };
                if !inference_credential_is_valid(config, database.as_ref(), &token).await {
                    return Err(anyhow::anyhow!("credencial de inferência inválida"));
                }
            }
            use crate::relay::InProcessInference;
            state.app.dispatch(&path, bytes).await
        }
    })
}

/// Starts a decode session owned by the remote peer. The response only
/// acknowledges ownership; generated tokens are sent back through the
/// bounded callback inbox in opportunistic batches.
async fn handle_step_stream(
    State(app): State<InferenceAppState>,
    _headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(error) => return err400(format!("quadro inválido: {error}")),
    };
    if tensor.is_some() {
        return err400("transporte remoto por lotes não aceita estado oculto");
    }
    let Some(reply_to) = meta.get("reply_to").cloned() else {
        return err400("transporte remoto por lotes sem reply_to");
    };
    if reply_to.get("ticket").and_then(Value::as_str).is_none() {
        return err400("transporte remoto por lotes sem ticket");
    }
    let Some(dtype) = meta
        .get("dtype")
        .and_then(Value::as_str)
        .and_then(|value| WireDtype::parse(value).ok())
    else {
        return err400("quadro sem dtype de estado oculto");
    };
    if meta.get("session_id").and_then(Value::as_str).is_none()
        || meta.get("token_id").and_then(Value::as_i64).is_none()
    {
        return err400("transporte remoto por lotes sem sessão ou token inicial");
    }

    let (producer_tx, producer_rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let producer = remote_stream_producer(app, meta, dtype, producer_tx);
        let emitter = remote_stream_emitter(reply_to, producer_rx);
        let _ = tokio::join!(producer, emitter);
    });

    match wire::encode(
        &json!({"stream_accepted": true, "processing_ms": 0, "node_timings": {}}),
        None,
        dtype,
    ) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

struct RemoteStreamToken {
    token_id: u32,
    text: String,
    done: bool,
    processing_ms: u64,
    node_timings: Option<Value>,
    speculated: bool,
    speculation_active: bool,
    speculation_disabled_reason: Option<String>,
    next_draft: Option<Vec<i32>>,
}

enum RemoteStreamItem {
    Token(RemoteStreamToken),
    Error(String),
}

fn remote_stream_install_draft(
    step_meta: &mut Value,
    next_draft: Option<&[i32]>,
    disabled_reason: Option<&str>,
) -> bool {
    let Some(object) = step_meta.as_object_mut() else {
        return false;
    };
    let Some(speculate) = object.get_mut("speculate").and_then(Value::as_object_mut) else {
        return false;
    };
    if let Some(draft) = next_draft.filter(|draft| !draft.is_empty()) {
        speculate.insert("draft".into(), json!(draft));
        true
    } else {
        object.remove("speculate");
        if let Some(reason) = disabled_reason {
            object.insert("speculation_disabled_reason".into(), json!(reason));
        }
        false
    }
}

async fn remote_stream_producer(
    app: InferenceAppState,
    mut step_meta: Value,
    dtype: WireDtype,
    sender: mpsc::Sender<RemoteStreamItem>,
) {
    step_meta.as_object_mut().map(|object| {
        object.remove("reply_to");
    });
    loop {
        let body = match wire::encode(&step_meta, None, dtype) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                let _ = sender
                    .send(RemoteStreamItem::Error(error.to_string()))
                    .await;
                return;
            }
        };
        let response = handle_step(State(app.clone()), axum::http::HeaderMap::new(), body).await;
        if !response.status().is_success() {
            let body = axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT)
                .await
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|error| error.to_string());
            let _ = sender
                .send(RemoteStreamItem::Error(format!(
                    "step remoto respondeu com erro: {body}"
                )))
                .await;
            return;
        }
        let response_body =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(body) => body,
                Err(error) => {
                    let _ = sender
                        .send(RemoteStreamItem::Error(error.to_string()))
                        .await;
                    return;
                }
            };
        let (reply_meta, tensor) = match wire::decode(&response_body) {
            Ok(value) => value,
            Err(error) => {
                let _ = sender
                    .send(RemoteStreamItem::Error(error.to_string()))
                    .await;
                return;
            }
        };
        if tensor.is_some() {
            let _ = sender
                .send(RemoteStreamItem::Error(
                    "step remoto devolveu estado oculto em vez de token".into(),
                ))
                .await;
            return;
        }

        let processing_ms = reply_meta
            .get("processing_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let node_timings = reply_meta.get("node_timings").cloned();
        let done = reply_meta
            .get("done")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let accepted = reply_meta
            .get("accepted")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry.get("token_id")?.as_u64()? as u32,
                            entry.get("text")?.as_str()?.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let requested_mtp = Speculate::from_meta(&step_meta)
            .is_some_and(|speculate| speculate.source == DraftSource::Mtp);
        let next_draft = reply_meta
            .get("next_draft")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_i64)
                    .map(|value| value as i32)
                    .collect::<Vec<_>>()
            })
            .filter(|draft| !draft.is_empty());
        let speculated = reply_meta
            .get("speculated")
            .and_then(Value::as_bool)
            .unwrap_or(!accepted.is_empty());
        let speculation_active = reply_meta
            .get("speculation_active")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                if requested_mtp {
                    next_draft.is_some()
                } else {
                    speculated
                }
            });
        let speculation_disabled_reason = (!speculation_active && requested_mtp).then(|| {
            reply_meta
                .get("speculation_disabled_reason")
                .and_then(Value::as_str)
                .unwrap_or(SPECULATION_REASON_MODEL_WITHOUT_MTP)
                .to_string()
        });
        if requested_mtp {
            remote_stream_install_draft(
                &mut step_meta,
                next_draft.as_deref(),
                speculation_disabled_reason.as_deref(),
            );
        }
        if !accepted.is_empty() {
            for (index, (token_id, text)) in accepted.iter().enumerate() {
                if sender
                    .send(RemoteStreamItem::Token(RemoteStreamToken {
                        token_id: *token_id,
                        text: text.clone(),
                        done: done && index + 1 == accepted.len(),
                        processing_ms: if index == 0 { processing_ms } else { 0 },
                        node_timings: (index == 0).then(|| node_timings.clone()).flatten(),
                        speculated,
                        speculation_active,
                        speculation_disabled_reason: speculation_disabled_reason.clone(),
                        next_draft: (index == 0).then(|| next_draft.clone()).flatten(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if done {
                return;
            }
            // Every hop in a remote stream already rolled back its own KV
            // before bubbling this reply. The counts remain in the callback
            // for the gateway's telemetry, but no rollback request is sent
            // after the frame.
            if let Some(token_id) = accepted.last().map(|(token_id, _)| *token_id) {
                step_meta["token_id"] = json!(token_id);
            }
            continue;
        }

        let Some(token_id) = reply_meta.get("token_id").and_then(Value::as_u64) else {
            let _ = sender
                .send(RemoteStreamItem::Error(
                    "step remoto não devolveu token".into(),
                ))
                .await;
            return;
        };
        if sender
            .send(RemoteStreamItem::Token(RemoteStreamToken {
                token_id: token_id as u32,
                text: reply_meta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                done,
                processing_ms,
                node_timings,
                speculated,
                speculation_active,
                speculation_disabled_reason,
                next_draft,
            }))
            .await
            .is_err()
        {
            return;
        }
        if done {
            return;
        }
        step_meta["token_id"] = json!(token_id);
    }
}

fn aggregate_remote_node_timings(items: &[RemoteStreamItem]) -> Value {
    let mut merged = serde_json::Map::new();
    for item in items {
        let RemoteStreamItem::Token(token) = item else {
            continue;
        };
        let Some(Value::Object(values)) = &token.node_timings else {
            continue;
        };
        for (key, value) in values {
            if key.ends_with("_ms") {
                let total = merged.get(key).and_then(Value::as_f64).unwrap_or(0.0)
                    + value.as_f64().unwrap_or(0.0);
                merged.insert(key.clone(), json!(total));
            } else if key == "engine_mismatch" {
                let mismatch = merged.get(key).and_then(Value::as_bool).unwrap_or(false)
                    || value.as_bool().unwrap_or(false);
                merged.insert(key.clone(), json!(mismatch));
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

fn remote_stream_batch(items: &[RemoteStreamItem]) -> (Value, Option<String>, bool) {
    let mut tokens = Vec::new();
    let mut processing_ms = 0_u64;
    let mut done = false;
    let mut error = None;
    let mut status = None;
    let mut next_draft = None;
    for item in items {
        match item {
            RemoteStreamItem::Token(token) => {
                tokens.push(json!({
                    "token_id": token.token_id,
                    "text": token.text,
                }));
                processing_ms = processing_ms.saturating_add(token.processing_ms);
                done |= token.done;
                status = Some((
                    token.speculated,
                    token.speculation_active,
                    token.speculation_disabled_reason.clone(),
                ));
                if next_draft.is_none() {
                    next_draft = token.next_draft.clone();
                }
            }
            RemoteStreamItem::Error(message) => error = Some(message.clone()),
        }
    }
    let terminal = done || error.is_some();
    let (speculated, speculation_active, speculation_disabled_reason) =
        status.unwrap_or((false, false, None));
    let batch = json!({
        "accepted": tokens,
        "done": done && error.is_none(),
        "processing_ms": processing_ms,
        "node_timings": aggregate_remote_node_timings(items),
        "speculated": speculated,
        "speculation_active": speculation_active,
        "speculation_disabled_reason": speculation_disabled_reason,
        "next_draft": next_draft,
    });
    (batch, error, terminal)
}

async fn remote_stream_emitter(reply_to: Value, receiver: mpsc::Receiver<RemoteStreamItem>) {
    emit_remote_stream_batches(reply_to, receiver, |reply_to, batch| async move {
        crate::direct::deliver_stream_batch(http_client(), &reply_to, batch).await
    })
    .await;
}

async fn emit_remote_stream_batches<F, Fut>(
    reply_to: Value,
    mut receiver: mpsc::Receiver<RemoteStreamItem>,
    mut deliver: F,
) where
    F: FnMut(Value, Value) -> Fut,
    Fut: Future<Output = bool>,
{
    while let Some(first) = receiver.recv().await {
        let items = crate::prefill::drain_opportunistic_batch(first, &mut receiver);
        let (batch, error, terminal) = remote_stream_batch(&items);
        let has_tokens = batch
            .get("accepted")
            .and_then(Value::as_array)
            .is_some_and(|tokens| !tokens.is_empty());
        if !has_tokens && error.is_some() {
            let error_batch = json!({
                "accepted": [],
                "done": false,
                "processing_ms": 0,
                "node_timings": {},
                "error": error,
            });
            let _ = deliver(reply_to.clone(), error_batch).await;
            return;
        }
        if !deliver(reply_to.clone(), batch).await {
            return;
        }
        // Preserve tokens that were already generated before an error. The
        // error is terminal, but it must not make the gateway discard the
        // prefix carried by the same opportunistic batch.
        if let Some(error) = error {
            let error_batch = json!({
                "accepted": [],
                "done": false,
                "processing_ms": 0,
                "node_timings": {},
                "error": error,
            });
            let _ = deliver(reply_to.clone(), error_batch).await;
        }
        if terminal {
            return;
        }
    }
}

#[cfg(test)]
mod remote_stream_tests {
    use super::*;

    fn token(token_id: u32, text: &str, done: bool) -> RemoteStreamItem {
        token_state(token_id, text, done, false, false, None)
    }

    fn token_state(
        token_id: u32,
        text: &str,
        done: bool,
        speculated: bool,
        speculation_active: bool,
        next_draft: Option<Vec<i32>>,
    ) -> RemoteStreamItem {
        RemoteStreamItem::Token(RemoteStreamToken {
            token_id,
            text: text.into(),
            done,
            processing_ms: 3,
            node_timings: Some(json!({"decode_ms": 2, "engine_mismatch": false})),
            speculated,
            speculation_active,
            speculation_disabled_reason: None,
            next_draft,
        })
    }

    #[test]
    fn lote_remoto_preserva_conteudo_ordem_e_tempos() {
        let (batch, error, terminal) =
            remote_stream_batch(&[token(4, "A", false), token(9, "ção", true)]);

        assert_eq!(batch["accepted"][0], json!({"token_id": 4, "text": "A"}));
        assert_eq!(batch["accepted"][1], json!({"token_id": 9, "text": "ção"}));
        assert_eq!(batch["processing_ms"], 6);
        assert_eq!(batch["node_timings"]["decode_ms"], 4.0);
        assert_eq!(batch["done"], true);
        assert!(error.is_none());
        assert!(terminal);
    }

    #[test]
    fn lote_remoto_transporta_o_rascunho_no_mesmo_callback() {
        let item = RemoteStreamItem::Token(RemoteStreamToken {
            token_id: 4,
            text: "A".into(),
            done: false,
            processing_ms: 3,
            node_timings: None,
            speculated: false,
            speculation_active: true,
            speculation_disabled_reason: None,
            next_draft: Some(vec![5, 6]),
        });

        let (batch, error, terminal) = remote_stream_batch(&[item]);

        assert_eq!(batch["next_draft"], json!([5, 6]));
        assert_eq!(batch["speculated"], false);
        assert_eq!(batch["speculation_active"], true);
        assert!(error.is_none());
        assert!(!terminal);
    }

    #[test]
    fn rascunho_remoto_e_instalado_sem_criar_rodada_extra() {
        let mut step_meta = json!({
            "token_id": 4,
            "speculate": {"k": 4, "m": 0, "source": "mtp"}
        });

        assert!(remote_stream_install_draft(
            &mut step_meta,
            Some(&[5, 6]),
            None,
        ));
        assert_eq!(step_meta["speculate"]["draft"], json!([5, 6]));

        // The next invocation consumes this installed draft. The transport
        // still has one metadata frame per decode round, with no probe frame
        // inserted solely to carry the draft.
        assert_eq!(
            Speculate::from_meta(&step_meta).unwrap().draft,
            Some(vec![5, 6])
        );
    }

    #[test]
    fn rascunho_remoto_desativado_remove_o_estado_com_razao() {
        let mut step_meta = json!({
            "speculate": {"k": 4, "m": 0, "source": "mtp", "draft": [5, 6]}
        });

        assert!(!remote_stream_install_draft(
            &mut step_meta,
            None,
            Some(SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK),
        ));
        assert!(step_meta.get("speculate").is_none());
        assert_eq!(
            step_meta["speculation_disabled_reason"],
            SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK
        );
    }

    #[test]
    fn erro_depois_de_tokens_mantem_o_prefixo_e_e_terminal() {
        let (batch, error, terminal) = remote_stream_batch(&[
            token(4, "prefixo", false),
            RemoteStreamItem::Error("falha no peer seguinte".into()),
        ]);

        assert_eq!(batch["accepted"][0]["text"], "prefixo");
        assert_eq!(batch["done"], false);
        assert_eq!(error.as_deref(), Some("falha no peer seguinte"));
        assert!(terminal);
    }

    #[tokio::test]
    async fn oito_tokens_e_rascunho_cruzam_em_um_unico_callback() {
        let (items_tx, items_rx) = mpsc::channel(16);
        items_tx
            .send(token_state(0, "t0", false, false, true, Some(vec![8, 9])))
            .await
            .expect("token enfileirado");
        for id in 1..8 {
            items_tx
                .send(token_state(
                    id,
                    &format!("t{id}"),
                    id == 7,
                    false,
                    true,
                    None,
                ))
                .await
                .expect("token enfileirado");
        }
        drop(items_tx);

        let (deliveries_tx, mut deliveries_rx) = mpsc::channel(8);
        emit_remote_stream_batches(
            json!({"ticket": "test"}),
            items_rx,
            move |_reply_to, batch| {
                let deliveries_tx = deliveries_tx.clone();
                async move { deliveries_tx.send(batch).await.is_ok() }
            },
        )
        .await;

        let batch = deliveries_rx.recv().await.expect("callback");
        assert_eq!(batch["accepted"].as_array().unwrap().len(), 8);
        assert_eq!(batch["accepted"][0]["token_id"], 0);
        assert_eq!(batch["accepted"][7]["token_id"], 7);
        assert_eq!(batch["next_draft"], json!([8, 9]));
        assert!(
            deliveries_rx.try_recv().is_err(),
            "oito tokens e o rascunho, um callback"
        );
    }
}

/// Monta o metadado que um hop de decode encaminha ao próximo peer.
///
/// A faixa carregada do próximo peer não basta para definir o que ele deve
/// executar: uma mesma fatia pode ser usada por ramos diferentes. Por isso a
/// sub-faixa é escolhida pelo `PipelineStep` do próximo hop e viaja em cada
/// quadro `/v1/step`.
fn step_forward_meta(
    session_id: &str,
    request_meta: &Value,
    local_meta: &Value,
    next: &PipelineStep,
) -> Value {
    let mut meta = json!({
        "session_id": session_id,
        "exec_start": next.exec_start,
        "exec_end": next.exec_end,
    });
    preserve_batch_identity(request_meta, &mut meta);
    for field in ["draft", "tokens", "mtp_k", "remote_stream"] {
        if let Some(value) = local_meta.get(field).or_else(|| request_meta.get(field)) {
            meta[field] = value.clone();
        }
    }
    meta
}

/// Relay a residual to `hops[0]` and bubble its reply back: the eventual
/// tail's real answer, plus this hop's own already-known contribution
/// (`local_processing_ms`/`local_node_timings`, computed by the caller from
/// its own `NodeTimings`) prepended onto whatever `hop_timings` bubbled up
/// from further down the chain. Returns the merged `meta` — *not* yet
/// re-encoded — so a caller with phase-specific fields to overlay (prefill's
/// `more_chunks`/`n_prompt_tokens`, see `handle_prefill`) can add them
/// before the final `wire::encode`.
///
/// `base_meta` carries whatever the phase needs beyond what every relay call
/// needs (`is_last`/`scale`/`next_hops`, added here): just `session_id` for
/// `/v1/step`, plus `chunk_index`/`chunk_size`/`is_final_chunk` for
/// `/v1/prefill`. `attempts` follows the same rule `send_chunk`/`step_ring`
/// already do at the gateway: `CHUNK_ATTEMPTS` for a prefill chunk (retry-safe,
/// KV-rollback protected), `1` for a decode step (not).
///
/// Split out from `handle_step`/`handle_prefill` (both call this with the
/// same `hops`/tensor shape, differing only in `path` and `base_meta`) so
/// it's directly testable against a fake next-hop server — see the
/// `relay_tests` module — without needing a loaded `LlmEngine`.
async fn relay_and_bubble(
    http: &crate::peer_client::PeerClients,
    hops: &[PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], TensorShape),
    dtype: WireDtype,
    attempts: u32,
    local_processing_ms: Value,
    local_node_timings: Value,
) -> std::result::Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["exec_start"] = json!(next.exec_start);
    base_meta["exec_end"] = json!(next.exec_end);

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http,
        next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some(tensor),
        dtype,
        dtype.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if sent.tensor.is_some() {
                // A fully-bubbled reply is always the tail's final answer,
                // which never carries a tensor — only a *forward* (not yet
                // relayed) residual does. Getting here means some hop in the
                // chain didn't relay when it should have; surviving loud
                // rather than silently dropping the tensor.
                warn!(
                    "resposta de {} veio com tensor num retransmissão já borbulhada — descartando",
                    next.node_id
                );
            }
            let hop_timings = crate::relay::bubble_hop_timings(&sent);
            let mut bubbled = sent.meta.clone();
            bubbled["hop_timings"] = json!(hop_timings);
            // Ours, not the next hop's: our caller needs these to build our
            // own entry, the same way we just built the next hop's.
            bubbled["processing_ms"] = local_processing_ms;
            bubbled["node_timings"] = local_node_timings;
            Ok(bubbled)
        }
        Err(e) => {
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            Err((StatusCode::BAD_GATEWAY, Json(failure)).into_response())
        }
    }
}

// ── Resposta direta (`relay/v2-direct`) ──────────────────────────────────────

/// Para onde o resultado desta cadeia deve ir, quando quem despachou pediu
/// resposta direta. Ausente em todo quadro do modo hub e do anel borbulhado —
/// é este campo, e só ele, que liga o caminho novo.
pub(crate) fn direct_reply_to(meta: &Value) -> Option<Value> {
    meta.get("reply_to").filter(|v| !v.is_null()).cloned()
}

/// O que este hop devolve a quem o chamou quando a cadeia responde direto: um
/// aceite, e nada mais. A resposta de verdade não passa por aqui — ver
/// [`crate::direct`].
pub(crate) fn direct_ack() -> Response {
    direct_ack_with_progress(None)
}

pub(crate) fn direct_ack_with_progress(progress: Option<Value>) -> Response {
    let mut meta = json!({"accepted": true});
    if let Some(progress) = progress {
        meta["prompt_progress"] = progress;
    }
    match wire::encode_batch_single(&meta, None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Repassa o residual ao próximo hop e some. Roda numa task própria: quem nos
/// chamou já foi aceito e não espera mais nada daqui.
///
/// Falhou o repasse, quem denuncia somos nós — direto ao gateway, nomeando o
/// nó que realmente morreu. É o caminho que substitui o `RelayFailure` subindo
/// pela pilha aninhada do anel borbulhado.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_direct(
    hops: Vec<PipelineStep>,
    path: &'static str,
    base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
) {
    forward_direct_ordered(
        hops, path, base_meta, residual, shape, dtype, attempts, reply_to, None, None, None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn forward_direct_ordered(
    hops: Vec<PipelineStep>,
    path: &'static str,
    mut base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
    order: Option<DirectForwardOrder>,
    sequence_id: Option<String>,
    chunk_index: Option<u32>,
) {
    let next = hops[0].clone();
    let remaining = &hops[1..];
    let terminal = base_meta
        .get("is_final_chunk")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let turn = match (order, sequence_id, chunk_index) {
        (Some(order), Some(sequence_id), Some(chunk_index)) => {
            match order.wait_turn(sequence_id, chunk_index).await {
                Ok(turn) => Some(turn),
                Err(error) => {
                    let failure = crate::relay::RelayFailure {
                        failed_node_id: next.node_id.clone(),
                        kind: "protocol".into(),
                        message: error.to_string(),
                    };
                    crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
                    return;
                }
            }
        }
        _ => None,
    };
    let destination_hop = base_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["exec_start"] = json!(next.exec_start);
    base_meta["exec_end"] = json!(next.exec_end);
    base_meta["reply_to"] = reply_to.clone();

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http_client(),
        &next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some((&residual, shape)),
        dtype,
        dtype.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if let Some(turn) = turn {
                turn.finish(true, terminal);
            }
            crate::direct::deliver_transport(http_client(), &reply_to, destination_hop, &sent)
                .await;
        }
        Err(e) => {
            if let Some(turn) = turn {
                turn.finish(false, false);
            }
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            warn!(
                "repasse direto para {} falhou: {} - avisando o gateway",
                failure.failed_node_id, failure.message
            );
            crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
        }
    }
}

fn issue_kind_label(kind: Option<crate::relay::SendIssueKind>) -> &'static str {
    match kind {
        Some(crate::relay::SendIssueKind::Timeout) => "timeout",
        Some(crate::relay::SendIssueKind::Transport) => "transport",
        Some(crate::relay::SendIssueKind::Http) => "http",
        Some(crate::relay::SendIssueKind::Protocol) => "protocol",
        None => "unknown",
    }
}

/// Search `history` for the most recent prior occurrence of its own trailing
/// `m` tokens, and propose whatever followed there as a draft continuation
/// of up to `k - 1` tokens. An empty result (no history, no match, or the
/// match runs off the end of `history` with nothing after it) means "draft
/// nothing" — the caller then decodes exactly `[confirmed_token]`, identical
/// to today's non-speculative behavior. This never looks at model weights or
/// probabilities — it's a plain array search — which is exactly why
/// verification (`longest_match`, below) can never trust it: the draft is
/// only ever a guess about what to *batch*, never a substitute for sampling.
/// `m` is the **longest** n-gram tried, not the only one: the search walks
/// `m, m-1, … 1` and stops at the first length that matches, so a window that
/// finds nothing degrades into a shorter, weaker guess instead of into no
/// guess at all. Longer wins because it is stronger evidence; how much of the
/// continuation is trusted scales with the matched length (`m` tokens of
/// context authorise at most `m + 1` guesses), because a one-token match
/// pointing anywhere in the history is nearly a coin flip and every extra
/// drafted token widens the batch on every hop of the ring.
/// Vale rascunhar nesta rodada?
///
/// Especular é apostar: um rascunho recusado tem de ser desfeito do cache
/// antes da rodada seguinte (`gateway::rollback_sessions`, via
/// `/v1/session/rollback` -> `kv_remove` -> `llama_memory_seq_rm`).
///
/// `kv_can_shift` não decide se a cabeça monta o lote. A cauda, porém, usa
/// essa evidência antes de criar o próximo `next_draft`: em modelos
/// híbridos/recorrentes que reportam `false`, não há rollback especulativo
/// seguro para iniciar outra rodada e a resposta leva
/// `hybrid_tail_no_rollback`. O caminho remoto faz o rollback local em cada
/// hop antes de borbulhar a resposta; o caminho legado mantém o endpoint
/// `handle_rollback` e o fan-out do gateway.
pub(crate) fn mtp_tail_speculation_reason(
    mtp_ready: bool,
    mtp_unavailable_reason: Option<&str>,
    kv_can_shift: bool,
) -> Option<&'static str> {
    if !mtp_ready {
        return Some(
            if mtp_unavailable_reason
                .is_some_and(|reason| reason == crate::llm::MTP_NO_NEXTN_REASON)
            {
                SPECULATION_REASON_TAIL_WITHOUT_NEXTN
            } else {
                SPECULATION_REASON_MODEL_WITHOUT_MTP
            },
        );
    }
    if !kv_can_shift {
        return Some(SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK);
    }
    None
}

pub(crate) fn should_draft(speculate: Option<Speculate>, mtp_ready: bool) -> bool {
    match speculate {
        None => false,
        Some(s) if s.source == DraftSource::Mtp => {
            // O rascunho pode nascer aqui (bloco `nextn` local, máquina
            // única) ou vir pronto do gateway (a cauda de um pipeline
            // fatiado o produziu na rodada anterior).
            mtp_ready || s.draft.is_some()
        }
        Some(_) => true,
    }
}

/// O convite a rascunhar que a cabeça recebe numa rodada.
///
/// `source` é o campo que separa os dois mecanismos, e ele existe porque eles
/// **não** são a mesma coisa com números diferentes: `Lookup` é uma busca por
/// repetição no texto já gerado (`find_draft`, nenhum peso envolvido) e `Mtp`
/// é o bloco `nextn` treinado dentro do GGUF (`crate::mtp`). Um `speculate`
/// sem `source` vem de um gateway antigo e vale `Lookup`, que é o que ele
/// sempre quis dizer.
#[derive(Debug, Clone)]
pub(crate) struct Speculate {
    /// Largura do lote: o token confirmado mais até `k - 1` palpites.
    pub k: u32,
    /// Janela de história do `find_draft`. Ignorada pelo `Mtp`, que não olha
    /// história nenhuma.
    pub m: u32,
    pub source: DraftSource,
    /// Rascunho pronto, produzido pela cauda num pipeline fatiado. A cabeça usa
    /// este em vez de chamar `mtp_draft`. `None` significa que a cabeça deve
    /// produzir o próprio rascunho (modelo inteiro) ou que não há rascunho.
    pub draft: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSource {
    Lookup,
    Mtp,
}

impl DraftSource {
    /// O nome no fio (wire), espelhando o `DraftSource::wire` de `prefill.rs`.
    pub(crate) fn wire(&self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

impl Speculate {
    /// Lê o convite do campo `speculate` do quadro.
    ///
    /// Público ao crate porque a bancada `spec_lab` fala o protocolo de
    /// verdade: uma segunda cópia desta leitura ali passaria a valer como
    /// contrato sem nunca ter sido conferida contra esta.
    pub(crate) fn from_meta(meta: &Value) -> Option<Self> {
        let v = meta.get("speculate")?;
        let draft = v.get("draft").and_then(|d| {
            d.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64())
                    .map(|v| v as i32)
                    .collect()
            })
        });
        Some(Speculate {
            k: v.get("k")?.as_u64()? as u32,
            m: v.get("m")?.as_u64()? as u32,
            source: match v.get("source").and_then(|s| s.as_str()) {
                Some("mtp") => DraftSource::Mtp,
                _ => DraftSource::Lookup,
            },
            draft,
        })
    }

    /// Serializa o convite para o campo `speculate` do quadro - a contraparte
    /// de `from_meta`. O caminho direto legado usa o valor preenchido pelo
    /// gateway; no stream remoto, a cabeça preenche o próprio valor antes da
    /// rodada seguinte.
    pub(crate) fn to_meta(&self) -> Value {
        let mut v = json!({"k": self.k, "m": self.m, "source": self.source.wire()});
        if let Some(draft) = &self.draft {
            v["draft"] = json!(draft);
        }
        v
    }
}

pub(crate) fn find_draft(history: &[i32], m: usize, k: u32) -> Vec<i32> {
    let extra = k.saturating_sub(1) as usize;
    if extra == 0 {
        return Vec::new();
    }
    for len in (1..=m.max(1)).rev() {
        if history.len() <= len {
            continue;
        }
        let needle = &history[history.len() - len..];
        let allowed = extra.min(len + 1);
        // Most recent first: `start` walks backward from the last position a
        // *prior* (non-overlapping-with-needle) occurrence could begin.
        for start in (0..history.len() - len).rev() {
            if &history[start..start + len] != needle {
                continue;
            }
            let continuation_start = start + len;
            if continuation_start >= history.len() {
                continue;
            }
            let continuation_end = (continuation_start + allowed).min(history.len());
            return history[continuation_start..continuation_end].to_vec();
        }
    }
    Vec::new()
}

/// How many leading positions `sampled` and `drafted` agree on. `sampled`
/// holds one real, freshly-sampled token per decoded position (always
/// computed, whether or not it ends up "accepted"); `drafted` holds what was
/// guessed for positions `1..`, one shorter than `sampled` since position 0
/// is the already-confirmed seed token with nothing in `drafted` to compare
/// it against. The accepted output is always `sampled[..=result]` — the
/// matched prefix plus one more real sample, which is the genuine next token
/// whether it means "the draft's next unverified guess" (full match) or
/// "the correction at the first place the draft was wrong" (partial match).
pub(crate) fn longest_match(sampled: &[i32], drafted: &[i32]) -> usize {
    sampled
        .iter()
        .zip(drafted)
        .take_while(|(a, b)| a == b)
        .count()
}

/// New `n_past` after discarding the speculative positions that weren't
/// confirmed. `batch_width` positions were decoded into the KV cache;
/// `accept_count` of them (the confirmed seed token plus every draft token
/// that matched — see `longest_match`) are valid and kept, the rest must be
/// rolled back with `LlmEngine::kv_remove`. Saturating throughout: a caller
/// passing a degenerate `accept_count > batch_width` gets "roll back
/// nothing" rather than a panic or a wrapped position.
fn rollback_from(n_past_after_batch: u32, batch_width: u32, accept_count: u32) -> u32 {
    n_past_after_batch.saturating_sub(batch_width.saturating_sub(accept_count))
}

#[cfg(test)]
mod chat_template_tests {
    use super::use_chat_template;

    #[test]
    fn request_mode_overrides_the_legacy_server_default() {
        assert!(use_chat_template(Some(true), false));
        assert!(!use_chat_template(Some(false), true));
        assert!(use_chat_template(None, true));
    }
}

#[cfg(test)]
mod speculative_tests {
    use super::*;

    fn lookup(k: u32, m: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m,
            source: DraftSource::Lookup,
            draft: None,
        })
    }

    fn mtp(k: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m: 0,
            source: DraftSource::Mtp,
            draft: None,
        })
    }

    /// Um pedido de lookup sempre rascunha - a decisão de aceitar o desfazer
    /// não é mais estática (`kv_can_shift`), é o `seq_rm` de verdade em
    /// `handle_rollback`, checada em tempo real. Ver o doc de `should_draft`.
    #[test]
    fn a_lookup_round_always_drafts_when_asked() {
        assert!(should_draft(lookup(4, 3), false));
        assert!(should_draft(lookup(4, 3), true));
    }

    /// Pedir MTP a um motor sem a cabeça `nextn` e sem rascunho pronto é o
    /// caso normal de uma fatia que não é a final: recusar é a resposta
    /// certa, e o gateway lê a recusa (`speculated: false`) e para de pedir.
    /// Rascunhar por lookup no lugar seria trocar o mecanismo por baixo de
    /// quem pediu.
    #[test]
    fn mtp_asked_of_an_engine_without_the_nextn_head_is_refused() {
        assert!(!should_draft(mtp(4), false));
        assert!(should_draft(mtp(4), true));
    }

    #[test]
    fn razoes_da_cauda_separam_nextn_modelo_e_hibrido() {
        assert_eq!(
            mtp_tail_speculation_reason(false, Some(crate::llm::MTP_NO_NEXTN_REASON), true,),
            Some(SPECULATION_REASON_TAIL_WITHOUT_NEXTN)
        );
        assert_eq!(
            mtp_tail_speculation_reason(false, Some("falha de alocação"), true),
            Some(SPECULATION_REASON_MODEL_WITHOUT_MTP)
        );
        assert_eq!(
            mtp_tail_speculation_reason(true, None, false),
            Some(SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK)
        );
        assert_eq!(mtp_tail_speculation_reason(true, None, true), None);
    }

    /// Um rascunho MTP já pronto (a cauda de um pipeline fatiado produziu na
    /// rodada anterior) faz a cabeça aceitar mesmo sem a cabeça `nextn` local
    /// - ela só precisa verificar o lote, não gerá-lo.
    #[test]
    fn a_premade_mtp_draft_is_accepted_without_the_local_nextn_head() {
        let mut spec = mtp(4).unwrap();
        spec.draft = Some(vec![1, 2, 3]);
        assert!(should_draft(Some(spec), false));
    }

    /// Sem especulacao pedida, o resto nao importa.
    #[test]
    fn without_speculation_asked_there_is_no_draft() {
        assert!(!should_draft(None, true));
        assert!(!should_draft(None, false));
    }

    /// O campo `source` é o que separa os dois mecanismos no fio. Um quadro
    /// de gateway antigo não o traz, e "ausente" tem que continuar querendo
    /// dizer lookup: era a única especulação que existia quando ele foi
    /// escrito.
    #[test]
    fn a_frame_without_a_draft_source_still_means_lookup() {
        let spec =
            Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 3}})).expect("k e m bastam");
        assert_eq!(spec.source, DraftSource::Lookup);
        assert_eq!((spec.k, spec.m), (4, 3));
    }

    #[test]
    fn a_frame_asking_for_mtp_says_so() {
        let spec = Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 0, "source": "mtp"}}))
            .expect("quadro completo");
        assert_eq!(spec.source, DraftSource::Mtp);
    }

    #[test]
    fn a_frame_without_speculate_asks_for_no_draft_at_all() {
        assert!(Speculate::from_meta(&json!({"session_id": "x"})).is_none());
    }

    #[test]
    fn find_draft_returns_nothing_without_a_match() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 3, 4), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_proposes_what_followed_the_match() {
        // Trailing 3 tokens [1,2,3] last occurred at index 1..4; [7,1]
        // followed it there.
        assert_eq!(find_draft(&[9, 1, 2, 3, 7, 1, 2, 3], 3, 3), vec![7, 1]);
    }

    #[test]
    fn find_draft_needs_more_history_than_just_the_needle() {
        assert_eq!(find_draft(&[1, 2, 3], 3, 5), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_k_of_one_never_drafts_anything() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 2, 1), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_picks_the_most_recent_occurrence() {
        // [1,2,3] occurs at 0..3 and 4..7; the earlier one is followed by
        // 99, the later (more recent) one by 88. The more recent one must win.
        let history = [1, 2, 3, 99, 1, 2, 3, 88, 1, 2, 3];
        assert_eq!(find_draft(&history, 3, 2), vec![88]);
    }

    /// A janela é o **maior** n-grama tentado, não o único. Sem o recuo, um
    /// texto onde só o último token se repete não rascunha nada e a
    /// especulação fica ociosa justamente onde ela é barata (verificar K
    /// tokens num MoE custa quase o mesmo que verificar 1).
    #[test]
    fn find_draft_falls_back_to_a_shorter_ngram_when_the_window_does_not_match() {
        // Janela 3 = [7,8,9], que nunca ocorreu antes. O sufixo [9] ocorreu,
        // seguido de 42.
        let history = [1, 9, 42, 43, 7, 8, 9];
        assert_eq!(find_draft(&history, 3, 4), vec![42, 43]);
    }

    /// O n-grama mais longo continua tendo precedência: ele é a evidência mais
    /// forte, e um casamento curto que aponte para outro lugar não pode
    /// atropelá-lo.
    #[test]
    fn find_draft_prefers_the_longest_ngram_available() {
        //           0  1  2  3   4  5  6  7
        let history = [5, 6, 7, 90, 9, 70, 5, 6, 7];
        // Janela 3 ([5,6,7]) casa em 0..3, seguido de 90. O sufixo [7] casa
        // mais tarde (índice 2) — mas o de 3 vence.
        assert_eq!(find_draft(&history, 3, 2), vec![90]);
    }

    /// Um casamento de um token só é evidência fraca; ele rascunha menos que um
    /// casamento longo. Rejeição não corrompe nada (a verificação é sempre
    /// contra o token amostrado de verdade), mas cada token rascunhado a mais
    /// alarga o batch de todo hop do anel.
    #[test]
    fn find_draft_trusts_a_short_match_for_fewer_tokens() {
        let history = [4, 1, 2, 3, 9, 9, 9, 4];
        // Sufixo [4] casa no índice 0; k=8 pediria 7 tokens, mas um casamento
        // de comprimento 1 só autoriza 2.
        assert_eq!(find_draft(&history, 1, 8), vec![1, 2]);
    }

    #[test]
    fn longest_match_stops_at_the_first_disagreement() {
        assert_eq!(longest_match(&[1, 2, 9, 4], &[1, 2, 3]), 2);
    }

    #[test]
    fn longest_match_of_a_full_match_is_the_drafts_whole_length() {
        assert_eq!(longest_match(&[1, 2, 3, 4], &[1, 2, 3]), 3);
    }

    #[test]
    fn longest_match_against_an_empty_draft_is_zero() {
        assert_eq!(longest_match(&[1, 2, 3], &[]), 0);
    }

    #[test]
    fn rollback_from_a_full_match_is_a_no_op() {
        assert_eq!(rollback_from(100, 4, 4), 100);
    }

    #[test]
    fn rollback_from_a_partial_match_removes_the_gap() {
        // n_past was 96 before the batch, 4 positions were decoded, only 2
        // (the seed + one draft token) were confirmed — 2 must come off.
        assert_eq!(rollback_from(100, 4, 2), 98);
    }

    #[test]
    fn rollback_from_saturates_instead_of_underflowing() {
        assert_eq!(rollback_from(100, 4, 0), 96);
        // Degenerate: accept_count larger than batch_width still can't
        // remove more than the whole batch.
        assert_eq!(rollback_from(100, 4, 9), 100);
    }
}

#[allow(clippy::too_many_arguments)]
fn step_blocking(
    state: SharedState,
    sid_key: String,
    is_last: bool,
    remote_stream: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    capture_block: Option<u32>,
    // Present only for the head — everyone else either forwards
    // `incoming_draft` untouched (not the tail) or verifies against it (the
    // tail). Ver `Speculate` para por que a fonte do rascunho viaja junto.
    speculate: Option<Speculate>,
    // What a previous hop already decided, forwarded via the wire `draft`
    // field. `None` means this round isn't speculative at all (the
    // pre-existing, single-token wire format applies); `Some(vec![])` means
    // it is speculative but nothing matched — still a batch of one, but
    // through the *new* wire format, since the caller committed to it by
    // asking to speculate in the first place.
    incoming_draft: Option<Vec<i32>>,
    // A composição do lote decodificado na cabeça, viajando pela wire (`tokens`)
    // para a cauda. É o que alimenta o `observe` da cabeça `nextn` dentro do
    // `decode_hidden`: a cauda sabe *quais* tokens o tronco decodificou, e sem
    // eles a cabeça de rascunho não se mantém em dia. `None` é peer antigo que
    // não encaminha o campo — aí o `decode_hidden` desliga o MTP com aviso.
    batch_tokens: Option<Vec<i32>>,
    // Largura de lote que o gateway quer na rodada seguinte, vinda da cabeça
    // via `mtp_k` na wire. Só a cauda usa (para rascunhar a medida certa); a
    // cabeça a ecoa a partir do próprio `speculate` (`mtp_k_hint`).
    mtp_k: Option<u32>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    timings.selected_path = s.inference_path;

    let setup_started = Instant::now();
    // A geração deste turno corre numa cópia do KV do prompt: a partir daqui
    // nada mais escreve na sequência do prompt, e é isso que a deixa
    // reutilizável no turno seguinte sem remover nada do cache.
    s.open_generation(&sid_key)?;
    let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
        anyhow::anyhow!("sessão '{}' desconhecida — faça o prefill antes", sid_key)
    })?;
    session.last_used = Instant::now();
    let n_past = session.n_past;
    let seq_id = session.seq_id();
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    let mut head_draft: Option<Vec<i32>> = None;
    // A composição do lote que este hop decodificou, para o reply não-cauda.
    // Inicializada nos dois branches (tensor e cabeça) antes de qualquer
    // leitura, por isso sem valor inicial — ver o uso no reply_meta.
    let reply_tokens;
    // Ecoado no reply não-cauda: na cabeça nasce do `speculate` (mtp_k_hint),
    // nos hops do meio repassa o que chegou da wire. É como a cauda descobre a
    // largura de lote que o gateway quer na rodada seguinte.
    let mut mtp_k_echo: Option<u32> = None;
    // O limite é por decodificação, não por sessão. O mesmo lock serializa
    // esta troca e o decode que a consome.
    if let Err(error) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            bail!("{error}");
        }
    }
    let input_was_hidden = tensor.is_some();
    let selected_moe = s.inference_path.is_moe();
    let mut moe_forwarded = false;
    let tensor = if let Some((hidden, shape)) = tensor {
        let hidden = s.apply_moe_hidden(hidden, shape[0], capture_block, &mut timings)?;
        moe_forwarded = selected_moe
            && s.engine.moe_external_executor_enabled()
            && !s.engine.moe_hook_enabled();
        Some((hidden, shape))
    } else {
        None
    };
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state. `decode_hidden` already
        // handles any `n_tokens` — a speculative batch needs no special
        // handling here beyond what chunked prefill already exercises. A cauda
        // de pipeline fatiado tem o bloco `nextn`: o `decode_hidden` alimenta
        // o `observe` com a composição do lote (`batch_tokens`, vinda da
        // cabeça via `tokens` na wire); sem ela, ele desliga o MTP com aviso.
        n_tokens = shape[0];
        let logits_all = incoming_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_hidden(
            &hidden,
            &shape,
            n_tokens,
            n_past,
            seq_id,
            logits_all,
            batch_tokens.as_deref(),
        )?;
        // `batch_tokens` segue vivo: a cauda usa-o no gate do `next_draft`
        // (verificação) e no caminho plain.
        reply_tokens = batch_tokens.clone();
        // `Option<u32>` é Copy — o `mtp_k` do parâmetro continua vivo para o
        // caminho plain da cauda.
        mtp_k_echo = mtp_k;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
        // Only the tail pushed to `recent_tokens` before this fix, so on a
        // real (head != tail) multi-hop pipeline the head's own history was
        // permanently empty — harmless until something needed it (repetition
        // penalty already reads only the tail's copy, correctly), but
        // prompt-lookup drafting below needs the head's history to search.
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(tid);

        // A largura de lote que o gateway quer na rodada seguinte, extraída do
        // `speculate` que a cabeça recebeu e ecoada na resposta — é como a
        // cauda descobre a medida do rascunho (ver `mtp_k` no doc do parâmetro).
        let mtp_k_hint = speculate
            .as_ref()
            .filter(|s| s.source == DraftSource::Mtp)
            .map(|s| s.k);
        let pode_rascunhar = should_draft(speculate.clone(), s.engine.mtp_ready());
        let batch: Vec<i32> = if let Some(spec) = speculate.filter(|_| pode_rascunhar) {
            let draft: Vec<i32> = if let Some(premade) = spec.draft {
                premade
            } else {
                match spec.source {
                    DraftSource::Mtp => {
                        let p_min = s.mtp_min_probability;
                        s.engine
                            .mtp_draft(tid, n_past, seq_id, spec.k.saturating_sub(1), p_min)
                    }
                    DraftSource::Lookup => {
                        let session = &s.sessions[&sid_key];
                        let history: Vec<i32> = session
                            .prompt_tokens
                            .iter()
                            .chain(session.recent_tokens.iter())
                            .copied()
                            .collect();
                        find_draft(&history, spec.m as usize, spec.k)
                    }
                }
            };
            let mut batch = Vec::with_capacity(1 + draft.len());
            batch.push(tid);
            batch.extend_from_slice(&draft);
            head_draft = Some(draft);
            batch
        } else {
            vec![tid]
        };
        n_tokens = batch.len();
        let logits_all = head_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_tokens(&batch, n_past, seq_id, logits_all)?;
        // A composição do lote decodificado na cabeça — o que a cauda precisa
        // para o `observe`. Viaja no reply não-cauda e é ecoada até ela.
        reply_tokens = Some(batch);
        // Ecoa a largura de lote pedida para a cauda rascunhar. `mtp_k` do
        // parâmetro fica vivo para o caminho plain da cauda (o `or` usa uma
        // cópia).
        if let Some(k) = mtp_k_hint.or(mtp_k) {
            mtp_k_echo = Some(k);
        }
    }
    if is_last
        && selected_moe
        && s.engine.moe_external_executor_enabled()
        && !moe_forwarded
        && !s.engine.moe_hook_enabled()
    {
        let residual = s.engine.take_residual(n_tokens)?;
        let _ = s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
        moe_forwarded = true;
    }
    timings.merge_engine_moe(&engine_timing);
    moe_forwarded |= engine_timing.moe_forward_us != 0;
    if is_last {
        record_terminal_engine(&mut timings, s.inference_path, moe_forwarded);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    s.sessions.get_mut(&sid_key).unwrap().n_past += n_tokens as u32;

    // Whichever hop decided it — computed just now (the head) or forwarded
    // from one that did (everyone else) — this is what the tail verifies
    // against, and what a non-tail relay branch echoes onward.
    let draft = head_draft.or(incoming_draft);

    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        if let Some(draft) = draft {
            // ── Speculative verification ─────────────────────────────────
            //
            // Never trusts the draft: every position is sampled for real,
            // in order, through the exact same `sample_token_profiled` a
            // plain step uses — the draft only ever decided what got
            // batched, never what got output. Accepting positions this way
            // makes the result identical in distribution to running the
            // single-token path `keep` times in a row (see the "prompt
            // lookup decoding" writeup this shipped with for the full
            // argument); it is not an approximation.
            let verification_started = Instant::now();
            let mut recent = s.sessions[&sid_key].recent_tokens.clone();
            let mut sampled: Vec<i32> = Vec::with_capacity(n_tokens);
            let mut pieces: Vec<Vec<u8>> = Vec::with_capacity(n_tokens);
            for i in 0..n_tokens {
                let logits = s.engine.logits_ith(i as i32);
                let (id, _timing) = sample_token_profiled(logits, &recent, &sampling);
                recent.push(id);
                let is_stop = s.engine.is_generation_stop(&recent[..recent.len() - 1], id);
                pieces.push(if is_stop {
                    Vec::new()
                } else {
                    s.engine.token_to_piece_bytes(id, false)
                });
                sampled.push(id);
                if is_stop {
                    // Nothing past a model EOS or turn terminator is worth
                    // sampling, regardless of what the rest of the draft guessed.
                    break;
                }
            }
            timings.sampling_us += verification_started.elapsed().as_micros() as u64;

            let accept_len = longest_match(&sampled, &draft);
            // A cabeça `nextn` guarda uma linha de estado oculto por posição
            // decodificada; qual delas alimenta a próxima rodada só se sabe
            // depois de a verificação dizer até onde o rascunho valeu. Sem
            // isto ela pareia o token confirmado com o `h` de uma posição que
            // foi descartada, e o rascunho seguinte degenera em silêncio.
            s.engine.mtp_accept(accept_len);

            let keep = (accept_len + 1).min(sampled.len());
            let accepted_ids = &sampled[..keep];
            let done = accepted_ids.last().is_some_and(|id| {
                *id == eos
                    || (sampled.last() == Some(id)
                        && s.engine
                            .is_generation_stop(&recent[..recent.len() - 1], *id))
            });
            // Only accepted positions may advance the session's byte stream.
            // Feeding rejected draft positions would leave their UTF-8 tail
            // in the next request even though those tokens never reached the
            // consumer.
            let mut accepted_texts: Vec<String> = pieces
                .iter()
                .take(keep)
                .map(|piece| {
                    s.sessions
                        .get_mut(&sid_key)
                        .unwrap()
                        .feed_token_piece(piece)
                })
                .collect();
            if done {
                let mut tail = String::new();
                finish_terminal_stream(&mut s.sessions, &sid_key, &mut tail)?;
                if let Some(last) = accepted_texts.last_mut() {
                    last.push_str(&tail);
                }
            }

            // Rascunho para a rodada seguinte. Numa máquina só ele alimenta
            // o mesmo processo; num pipeline fatiado ele viaja no reply que já
            // sobe pela cadeia. O stream remoto o instala na cabeça antes do
            // próximo `step`; o caminho direto legado o leva ao gateway para
            // injeção - sempre sem travessia extra.
            //
            // O `batch_tokens` presente é o que garante que esta é a cauda de
            // um pipeline (a composição do lote veio de fora, via `tokens`):
            // numa máquina só a cabeça é a própria cauda e não há para onde
            // devolver o rascunho.
            let mut speculation_disabled_reason = None;
            let mtp_tail_reason = mtp_k.is_some().then(|| {
                mtp_tail_speculation_reason(
                    s.engine.mtp_ready(),
                    s.engine.mtp_unavailable_reason(),
                    s.engine.kv_can_shift(),
                )
            });
            let next_draft: Option<Vec<i32>> =
                if !done && batch_tokens.is_some() && mtp_tail_reason.flatten().is_none() {
                    // A sessão já avançou `n_tokens`; só as primeiras `keep`
                    // posições foram confirmadas, então o próximo slot livre do
                    // tronco é `n_past - (n_tokens - keep)`.
                    let session_n_past = s.sessions[&sid_key].n_past;
                    let n_past_next = session_n_past - (n_tokens as u32 - keep as u32);
                    let id0 = *accepted_ids.last().unwrap_or(&eos);
                    let p_min = s.mtp_min_probability;
                    let dr =
                        s.engine
                            .mtp_draft(id0, n_past_next, seq_id, draft.len() as u32, p_min);
                    if dr.is_empty() {
                        speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                        None
                    } else {
                        Some(dr)
                    }
                } else {
                    speculation_disabled_reason = mtp_tail_reason.flatten();
                    None
                };

            if remote_stream {
                let n_past_after_batch = s.sessions[&sid_key].n_past;
                let new_n_past = rollback_from(n_past_after_batch, n_tokens as u32, keep as u32);
                if new_n_past < n_past_after_batch
                    && !s.engine.kv_remove(seq_id, new_n_past as i32, -1)
                {
                    bail!("rollback especulativo remoto recusado na cauda");
                }
                s.sessions.get_mut(&sid_key).unwrap().n_past = new_n_past;
            }

            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .recent_tokens
                .extend_from_slice(accepted_ids);
            // Ver a nota gêmea abaixo: réplica não encerra sessão. E o fim de
            // turno retém o prompt - ver `InferState::retain_prompt`.
            if done && !crate::tp::is_replica() {
                s.retain_prompt(&sid_key);
            }

            let accepted: Vec<Value> = accepted_ids
                .iter()
                .zip(accepted_texts)
                .map(|(id, text)| json!({"token_id": id, "text": text}))
                .collect();
            let encode_started = Instant::now();
            let bytes = wire::encode(
                &json!({
                    "done": done,
                    "accepted": accepted,
                    "accept_count": keep as u32,
                    "batch_width": n_tokens as u32,
                    "speculated": true,
                    "speculation_active": next_draft.is_some(),
                    "speculation_disabled_reason": speculation_disabled_reason,
                    "next_draft": next_draft,
                }),
                None,
                wire_dtype,
            )?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            return Ok(TimedReply { bytes, timings });
        }

        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_profiled(logits, &recent, &sampling);
        let done = s.engine.is_generation_stop(&recent, next_id);
        let token_text_started = Instant::now();
        let mut text = if done {
            String::new()
        } else {
            let piece = s.engine.token_to_piece_bytes(next_id, false);
            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .feed_token_piece(&piece)
        };
        let token_text_us = token_text_started.elapsed().as_micros() as u64;
        if done {
            finish_terminal_stream(&mut s.sessions, &sid_key, &mut text)?;
        }
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        // A réplica de um grupo TP não encerra sessão: quem decide o fim da
        // geração é o rank 0. Ver `tp::is_replica`. E o fim de turno retém o
        // prompt - ver `InferState::retain_prompt`.
        if done && !crate::tp::is_replica() {
            s.retain_prompt(&sid_key);
        }
        timings.sampling_us += logits_access_us
            + sampling_timing.logits_clone_us
            + sampling_timing.repetition_penalty_us
            + sampling_timing.softmax_us
            + sampling_timing.top_k_us
            + sampling_timing.top_p_us
            + sampling_timing.selection_us
            + token_text_us;
        timings.logits_access_us += logits_access_us;
        timings.logits_clone_us += sampling_timing.logits_clone_us;
        timings.repetition_penalty_us += sampling_timing.repetition_penalty_us;
        timings.softmax_us += sampling_timing.softmax_us;
        timings.top_k_us += sampling_timing.top_k_us;
        timings.top_p_us += sampling_timing.top_p_us;
        timings.selection_us += sampling_timing.selection_us;
        timings.token_text_us += token_text_us;

        // A cauda de um pipeline fatiado também rascunha na rodada em que a
        // cabeça recusou — a primeira, quando ainda não há rascunho pronto
        // para viajar. O `mtp_k` veio da cabeça (via `mtp_k` na wire): é a
        // largura de lote que o gateway quer na rodada seguinte. O
        // `mtp_accept(0)` confirma o `pending_h` da linha do token recém
        // decodificado; sem ele o draft seguinte usaria a linha de uma posição
        // anterior e parearia errado. Rodada de modelo inteiro recusada não
        // tem `mtp_k` — nada a devolver, e não há segunda máquina esperando.
        let mut speculation_disabled_reason = None;
        let mtp_tail_reason = mtp_k.is_some().then(|| {
            mtp_tail_speculation_reason(
                s.engine.mtp_ready(),
                s.engine.mtp_unavailable_reason(),
                s.engine.kv_can_shift(),
            )
        });
        let next_draft: Option<Vec<i32>> = if !done && mtp_tail_reason.flatten().is_none() {
            if let Some(k) = mtp_k {
                s.engine.mtp_accept(0);
                let session_n_past = s.sessions[&sid_key].n_past;
                let p_min = s.mtp_min_probability;
                let dr =
                    s.engine
                        .mtp_draft(next_id, session_n_past, seq_id, k.saturating_sub(1), p_min);
                if dr.is_empty() {
                    speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                    None
                } else {
                    Some(dr)
                }
            } else {
                None
            }
        } else {
            speculation_disabled_reason = mtp_tail_reason.flatten();
            None
        };
        let encode_started = Instant::now();
        let mut reply = json!({"done": done, "token_id": next_id, "text": text});
        reply["speculated"] = json!(false);
        reply["speculation_active"] = json!(next_draft.is_some());
        reply["speculation_disabled_reason"] = json!(speculation_disabled_reason);
        if let Some(nd) = &next_draft {
            reply["next_draft"] = json!(nd);
        }
        let bytes = wire::encode(&reply, None, wire_dtype)?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let mut residual = s.engine.take_residual(n_tokens)?;
    if !input_was_hidden {
        residual.values =
            s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
    }
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let mut reply_meta = json!({"done": false});
    // Whoever called us needs this to keep threading it toward the tail —
    // see `draft`'s own comment above for why it's carried this way rather
    // than recomputed at each hop.
    if let Some(draft) = &draft {
        reply_meta["draft"] = json!(draft);
    }
    // A composição do lote decodificado (ver `batch_tokens`) e a largura de
    // lote da próxima rodada (ver `mtp_k`) viajam da cabeça para a cauda —
    // são o que mantém a cabeça `nextn` da cauda em dia e rascunhando a
    // medida certa.
    if let Some(tokens) = &reply_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    if let Some(k) = mtp_k_echo {
        reply_meta["mtp_k"] = json!(k);
    }
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual.values, residual.shape.clone())),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}
