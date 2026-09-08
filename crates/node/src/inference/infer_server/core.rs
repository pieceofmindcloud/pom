use super::*;

pub(super) const SESSION_IDLE_TIMEOUT_SECS: u64 = 900;
/// Upper bound for one binary f16 inference frame received from a peer. Keep
/// it in lockstep with the wire validator; the relay splits coalesced payloads
/// before this limit and the handler applies it before decoding the body.
pub(super) const INFERENCE_FRAME_BODY_LIMIT: usize = wire::MAX_FRAME_PAYLOAD_BYTES;
pub(super) const BATCH_CHILD_HEADER: &str = "x-pom-batch-child";

pub(crate) const SPECULATION_REASON_TAIL_WITHOUT_NEXTN: &str = "tail_without_nextn";
pub(crate) const SPECULATION_REASON_MODEL_WITHOUT_MTP: &str = "model_without_mtp";
pub(crate) const SPECULATION_REASON_HYBRID_TAIL_NO_ROLLBACK: &str = "hybrid_tail_no_rollback";
pub(crate) const SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE: &str =
    "peer_without_sliced_mtp_wire";
pub(super) const SPECULATION_REASON_TAIL_DRAFT_EMPTY: &str = "tail_mtp_draft_empty";

/// Resolve a preferência persistida pelo deploy para o seletor operacional.
///
/// Uma escolha explícita na UI não pode ser sobrescrita por uma variável do
/// processo. `Auto` continua aceitando `POM_INFERENCE_MODE` para preservar o
/// override operacional existente; sem ele, `executor::select` decide pela
/// metadata GGUF real.
pub(super) fn configured_inference_mode(
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
pub(super) struct MoeTimings {
    pub(super) forward_us: u64,
    pub(super) executors: Vec<crate::moe::runtime::MoeExecutorKind>,
    pub(super) dtypes: Vec<u32>,
    pub(super) reasons: Vec<String>,
    pub(super) diagnostics: Option<crate::moe::runtime::MoeExecutorDiagnostics>,
}

impl MoeTimings {
    pub(super) fn record_executor(&mut self, executor: crate::moe::runtime::MoeExecutorKind) {
        if !self.executors.contains(&executor) {
            self.executors.push(executor);
        }
    }

    pub(super) fn record_telemetry(
        &mut self,
        telemetry: &crate::moe::runtime::MoeExecutorTelemetry,
    ) {
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

    pub(super) fn record_forward(&mut self, elapsed_us: u64) {
        self.forward_us = self.forward_us.saturating_add(elapsed_us);
    }

    pub(super) fn record_reason(&mut self, reason: &str) {
        if !self.reasons.iter().any(|existing| existing == reason) {
            self.reasons.push(reason.to_string());
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
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
pub(super) struct NodeTimings {
    pub(super) frame_decode_us: u64,
    pub(super) input_prepare_us: u64,
    pub(super) queue_us: u64,
    /// Espera pelo mutex do motor, antes de qualquer trabalho.
    ///
    /// O `llama_context` não é seguro para uso concorrente, então todo decode
    /// deste nó serializa aqui. Sem este contador a espera caía no vão entre a
    /// entrada do handler e o primeiro medidor, e aparecia na tela só como
    /// "não atribuído" - 194 ms por token numa medição de 2026-08-14, 63% do
    /// tempo da requisição, invisível.
    pub(super) lock_wait_us: u64,
    pub(super) setup_us: u64,
    pub(super) decode_us: u64,
    pub(super) backend_enqueue_us: u64,
    pub(super) backend_completion_us: u64,
    pub(super) residual_capture_d2h_sync_us: u64,
    pub(super) residual_us: u64,
    pub(super) logits_access_us: u64,
    pub(super) logits_clone_us: u64,
    pub(super) repetition_penalty_us: u64,
    pub(super) softmax_us: u64,
    pub(super) top_k_us: u64,
    pub(super) top_p_us: u64,
    pub(super) selection_us: u64,
    pub(super) token_text_us: u64,
    pub(super) sampling_us: u64,
    pub(super) encode_us: u64,
    pub(super) selected_path: crate::executor::InferencePath,
    pub(super) executed_engines: Vec<crate::executor::ExecutionEngine>,
    pub(super) moe: MoeTimings,
    pub(super) tp: Option<crate::tp::TpDecodeTelemetry>,
    /// Contadores de tokens do MTP neste hop e nesta chamada de step.
    /// Incrementos acontecem uma vez por lote, nunca no laço por token.
    pub(super) mtp_draft_tokens_proposed: u64,
    pub(super) mtp_draft_tokens_accepted: u64,
    pub(super) mtp_draft_tokens_rejected: u64,
}

impl NodeTimings {
    pub(super) fn record_engine(&mut self, engine: crate::executor::ExecutionEngine) {
        if engine != crate::executor::ExecutionEngine::Unknown
            && !self.executed_engines.contains(&engine)
        {
            self.executed_engines.push(engine);
        }
    }

    pub(super) fn executed_engine(&self) -> crate::executor::ExecutionEngine {
        match self.executed_engines.as_slice() {
            [engine] => *engine,
            _ => crate::executor::ExecutionEngine::Unknown,
        }
    }

    pub(super) fn mixed_execution(&self) -> bool {
        self.executed_engines.len() > 1
    }

    pub(super) fn engine_mismatch(&self) -> bool {
        let expected = self.selected_path.execution_engine();
        if self.executed_engines.is_empty() {
            return false;
        }
        if self.mixed_execution() {
            return !self.executed_engines.contains(&expected);
        }
        self.executed_engine() != expected
    }

    pub(super) fn record_moe_executor(&mut self, executor: crate::moe::runtime::MoeExecutorKind) {
        self.moe.record_executor(executor);
    }

    pub(super) fn record_moe_telemetry(
        &mut self,
        telemetry: crate::moe::runtime::MoeExecutorTelemetry,
    ) {
        self.moe.record_telemetry(&telemetry);
    }

    pub(super) fn record_moe_forward(&mut self, elapsed_us: u64) {
        self.moe.record_forward(elapsed_us);
    }

    pub(super) fn record_moe_reason(&mut self, reason: &str) {
        self.moe.record_reason(reason);
    }

    pub(super) fn merge_moe(&mut self, other: &MoeTimings) {
        self.moe.merge(other);
    }

    pub(super) fn merge_engine_moe(&mut self, other: &crate::llm::DecodeTimings) {
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

    pub(super) fn moe_executor(&self) -> &'static str {
        match self.moe.executors.as_slice() {
            [executor] => executor.as_str(),
            [] => "not-run",
            _ => "mixed",
        }
    }

    pub(super) fn json(&self) -> Value {
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
            "mtp_draft_tokens_proposed": self.mtp_draft_tokens_proposed,
            "mtp_draft_tokens_accepted": self.mtp_draft_tokens_accepted,
            "mtp_draft_tokens_rejected": self.mtp_draft_tokens_rejected,
            "tp": self
                .tp
                .as_ref()
                .map(|tp| serde_json::to_value(tp).unwrap_or(Value::Null)),
        })
    }
}

pub(super) fn moe_health_summary(
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

pub(crate) struct TimedReply {
    pub(super) bytes: Vec<u8>,
    pub(super) timings: NodeTimings,
}

pub(super) struct PrefillJob {
    pub(super) sid_key: String,
    pub(super) chunk_index: u32,
    pub(super) chunk_size: u32,
    pub(super) reuse_prefix: bool,
    /// A decisão da cabeça que os hops intermediários só repassam. Um hop que
    /// não tokeniza não pode substituir `true` por seu `PrefixDecision::default`.
    pub(super) cached_tokens: u32,
    /// Candidato de prefixo comum, inclusive quando a decisão foi fria.
    pub(super) prefix_match_tokens: u32,
    pub(super) start_pos: Option<u32>,
    /// Fronteira entre o prompt reutilizável e o rabicho deste turno, decidida
    /// por quem tokenizou - ver `Session::stable_len`.
    pub(super) stable_len: Option<u32>,
    /// Bloco local onde parar, quando o remetente pediu menos que a fatia
    /// inteira. `None` é a fatia toda. Ver `resolve_exec_range`.
    pub(super) capture_block: Option<u32>,
    pub(super) is_last: bool,
    pub(super) is_final_chunk: bool,
    pub(super) wire_dtype: WireDtype,
    pub(super) prompt: Option<String>,
    pub(super) chat_template: Option<bool>,
    pub(super) tensor: Option<(Vec<f32>, TensorShape)>,
    pub(super) chunk_tokens: Option<Vec<i32>>,
    pub(super) queued_at: Instant,
}

pub(super) struct StepJob {
    pub(super) sid_key: String,
    pub(super) is_last: bool,
    pub(super) remote_stream: bool,
    pub(super) wire_dtype: WireDtype,
    pub(super) token_id: Option<i32>,
    pub(super) tensor: Option<(Vec<f32>, TensorShape)>,
    /// Bloco local onde parar, quando o remetente pediu menos que a fatia
    /// inteira. `None` é a fatia toda. Ver `resolve_exec_range`.
    pub(super) capture_block: Option<u32>,
    pub(super) speculate: Option<Speculate>,
    pub(super) incoming_draft: Option<Vec<i32>>,
    pub(super) batch_tokens: Option<Vec<i32>>,
    pub(super) mtp_k: Option<u32>,
    pub(super) queued_at: Instant,
}

pub(super) enum InferenceCommand {
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
    pub(super) fn into_queue_item(self) -> crate::scheduler::QueueItem<Self> {
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
pub(super) enum SchedulerSubmitError {
    Full { limit: usize },
    Closed,
}

#[derive(Clone)]
pub(super) struct SchedulerHandle {
    pub(super) tx: mpsc::Sender<InferenceCommand>,
    pub(super) limit: usize,
    pub(super) admission: Arc<Semaphore>,
    pub(super) stats: Arc<SchedulerStats>,
    pub(super) config: crate::scheduler::SchedulerConfig,
}

#[derive(Default)]
pub(super) struct SchedulerStats {
    pub(super) queue_depth: AtomicUsize,
    pub(super) last_batch_size: AtomicUsize,
    pub(super) last_batch_tokens: AtomicUsize,
    pub(super) total_batches: AtomicU64,
    pub(super) total_items: AtomicU64,
}

impl SchedulerStats {
    pub(super) fn json(&self, config: &crate::scheduler::SchedulerConfig) -> Value {
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
    pub(super) fn submit_prefill(
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

    pub(super) fn submit_step(
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
pub(crate) struct InferenceAppState {
    pub(super) state: SharedState,
    pub(super) scheduler: SchedulerHandle,
    /// Counts compute requests executing in this inference process. The
    /// gateway shares this tracker so `/api/ui/node` reports the local fact.
    pub(super) loads: Arc<crate::pipeline::LoadTracker>,
    pub(super) load_node_id: String,
    /// TP exposes one global decode/barrier identity per process. Serialize
    /// complete inbound batches so a second request cannot replace that id
    /// while the first batch is waiting for its workers.
    pub(super) tp_batch_gate: Arc<tokio::sync::Mutex<()>>,
    /// Keeps direct prefill forwarding in chunk order while the next chunk is
    /// already being computed by the preceding hop.
    pub(super) direct_forward_order: DirectForwardOrder,
}

#[derive(Clone, Default)]
pub(super) struct DirectForwardOrder {
    pub(super) sequences: Arc<Mutex<HashMap<String, Arc<DirectForwardSequence>>>>,
}

pub(super) struct DirectForwardSequence {
    pub(super) next_chunk: Mutex<u32>,
    pub(super) failed: Mutex<Option<String>>,
    pub(super) notify: tokio::sync::Notify,
}

pub(super) struct DirectForwardTurn {
    pub(super) owner: DirectForwardOrder,
    pub(super) key: String,
    pub(super) sequence: Arc<DirectForwardSequence>,
    pub(super) chunk_index: u32,
}

impl DirectForwardOrder {
    pub(super) fn ensure_sequence(&self, key: &str) -> Result<()> {
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

    pub(super) async fn wait_turn(
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
    pub(super) fn finish(self, success: bool, terminal: bool) {
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

pub(super) fn spawn_scheduler(
    state: SharedState,
    cfg: crate::scheduler::SchedulerConfig,
) -> SchedulerHandle {
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

pub(super) async fn run_scheduler(
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

pub(super) fn execute_scheduled_batch(
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

pub(super) fn execute_one_command(state: SharedState, command: InferenceCommand) {
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

pub(super) fn inference_frame_body_limit() -> usize {
    INFERENCE_FRAME_BODY_LIMIT
}

pub(super) fn lock_inference_state<T>(state: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex do estado de inferência envenenado"))
}

pub(super) fn finish_terminal_stream(
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
    fn telemetria_mtp_expoe_contadores_de_tokens() {
        let timings = NodeTimings {
            mtp_draft_tokens_proposed: 7,
            mtp_draft_tokens_accepted: 5,
            mtp_draft_tokens_rejected: 2,
            ..Default::default()
        };

        let value = timings.json();

        assert_eq!(value["mtp_draft_tokens_proposed"], 7);
        assert_eq!(value["mtp_draft_tokens_accepted"], 5);
        assert_eq!(value["mtp_draft_tokens_rejected"], 2);
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
    fn health_dense_nao_sinaliza_executor_moe() {
        let (requested, executor, dtypes, reason) =
            moe_health_summary(crate::executor::InferencePath::DenseLlamaCpp, None, None);

        assert!(!requested);
        assert_eq!(executor, "not-requested");
        assert!(dtypes.is_empty());
        assert!(reason.is_none());
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
            Arc::new(ModelCache::new()),
            false,
            Arc::new(crate::pipeline::LoadTracker::new()),
            "test-node".to_owned(),
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
