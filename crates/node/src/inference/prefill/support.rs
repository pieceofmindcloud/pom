use super::*;

/// Prompt tokens pushed through the pipeline per round trip.
///
/// The chunk is the unit of retry and the unit of memory, so it wants to be
/// small; it is also one round trip per peer, so it wants to be large. 512
/// tokens is 8 MiB of f16 hidden state per hop — about a second on the link
/// this cluster actually has, which keeps a retry cheap while leaving the
/// per-chunk overhead well under the transfer time it covers.
pub const DEFAULT_CHUNK_TOKENS: u32 = 512;

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Maximum number of generated tokens the remote emitter drains from its
/// queue in one callback. This is a cap, not a target: a token is sent as soon
/// as the emitter is free, and no timer fills the gap.
pub(crate) const REMOTE_STREAM_BATCH_LIMIT: usize = 8;
/// One control frame starts generation on a remote head. Subsequent tokens
/// return through `crate::direct::REMOTE_STREAM_RESULT_PATH` in batches.
pub(crate) const REMOTE_STREAM_PATH: &str = "/v1/step-stream";

/// Cancelamento compartilhado entre o stream HTTP e o prefill. O `Notify`
/// acorda uma chamada em voo; o `AtomicBool` torna as checagens entre hops e
/// chunks baratas e sem await.
#[derive(Clone, Default)]
pub struct Cancellation {
    pub(super) cancelled: Arc<AtomicBool>,
    pub(super) notify: Arc<Notify>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::Release) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[derive(Debug)]
pub struct GenerationCancelled;

impl std::fmt::Display for GenerationCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("geração cancelada pelo cliente")
    }
}

impl std::error::Error for GenerationCancelled {}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GenerationCancelled>().is_some()
}

/// Um pedaço do prefill falhou **depois** de a cadeia já ter aceitado pedaços
/// anteriores. Viaja anexado ao erro (`anyhow` context) como diagnóstico do
/// ponto onde uma retomada seria possível apenas se o estado remoto fosse
/// conhecido. Uma resposta de transporte perdida não prova isso, portanto o
/// gateway faz reconstrução fria em vez de usar este ponto cegamente. Só é
/// emitido para `from_chunk > 0`.
#[derive(Debug, Clone, Copy)]
pub struct PrefillResumable {
    pub from_chunk: u32,
    pub start_pos: u32,
    pub prompt_tokens: u32,
    pub cached_tokens: u32,
}

impl std::fmt::Display for PrefillResumable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prefill retomável do pedaço {} (posição {})",
            self.from_chunk, self.start_pos
        )
    }
}

/// O ponto de retomada anexado a um erro de prefill, se houver.
pub fn resume_point(error: &anyhow::Error) -> Option<PrefillResumable> {
    error.downcast_ref::<PrefillResumable>().copied()
}

pub(super) fn check_cancelled(cancellation: &Cancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(anyhow::Error::new(GenerationCancelled));
    }
    Ok(())
}

/// Takes the first item and whatever is already ready in the queue. Keeping
/// this synchronous is intentional: an emitter never waits to complete a
/// batch, so a slow producer gives one-item batches immediately.
pub(crate) fn drain_opportunistic_batch<T>(first: T, receiver: &mut mpsc::Receiver<T>) -> Vec<T> {
    let mut batch = vec![first];
    while batch.len() < REMOTE_STREAM_BATCH_LIMIT {
        match receiver.try_recv() {
            Ok(item) => batch.push(item),
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break
            }
        }
    }
    batch
}

pub(super) async fn cancelable<F, T>(cancellation: &Cancellation, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = future => result,
        _ = cancellation.cancelled() => Err(anyhow::Error::new(GenerationCancelled)),
    }
}

/// Janela de sobreposição do prefill.
///
/// A profundidade é o número de pedaços que o gateway mantém em voo ao mesmo
/// tempo pela cadeia: um pedaço por perna/hop é o que mantém todos os enlaces
/// e todas as GPUs ocupados em paralelo em vez de um estágio só. Dois limites
/// a contêm: a contagem (`acquire`, teto de oito) e o orçamento de bytes de
/// estado oculto em voo no gateway (`reserve_bytes`, teto fixo de 512 MiB, que
/// só morde com `n_embd` grande ou janela alta). `1` desliga a sobreposição.
#[derive(Clone, Debug)]
pub(crate) struct PrefillOverlapWindow {
    pub(super) max_inflight_chunks: usize,
    pub(super) permits: Arc<Semaphore>,
    pub(super) byte_permits: Arc<Semaphore>,
}

impl PrefillOverlapWindow {
    pub(crate) const DEFAULT_MAX_INFLIGHT_CHUNKS: usize = 8;
    pub(crate) const MAX_INFLIGHT_CHUNKS: usize = 8;
    /// Teto de bytes de estado oculto (em memória, `f32`) que o gateway mantém
    /// em voo ao mesmo tempo pelo pipeline de prefill. O gate por contagem é o
    /// limite principal; este é a rede de segurança para modelos de `n_embd`
    /// grande.
    pub(crate) const INFLIGHT_BYTE_BUDGET: usize = 512 * 1024 * 1024;

    pub(crate) fn new(requested: usize) -> Self {
        let max_inflight_chunks = requested.clamp(1, Self::MAX_INFLIGHT_CHUNKS);
        Self {
            max_inflight_chunks,
            permits: Arc::new(Semaphore::new(max_inflight_chunks)),
            byte_permits: Arc::new(Semaphore::new(Self::INFLIGHT_BYTE_BUDGET)),
        }
    }

    pub(crate) fn max_inflight_chunks(&self) -> usize {
        self.max_inflight_chunks
    }

    /// Uma vaga na janela. O permit viaja com o pedaço até o downstream dele
    /// terminar; é isso que limita quantos pedaços cruzam a cadeia ao mesmo
    /// tempo (`run`/`run_direct` guardam o permit dentro da future de cada
    /// pedaço).
    pub(crate) async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("janela de sobreposição foi encerrada"))
    }

    /// Reserva `amount` bytes do orçamento em voo, esperando um pedaço anterior
    /// terminar se preciso. Um `amount` sozinho maior que o orçamento é
    /// limitado ao orçamento (o pedaço passa, mas segura a janela inteira),
    /// então nunca trava. O permit devolvido libera os bytes ao cair.
    pub(crate) async fn reserve_bytes(&self, amount: usize) -> OwnedSemaphorePermit {
        let want = amount.clamp(1, Self::INFLIGHT_BYTE_BUDGET) as u32;
        self.byte_permits
            .clone()
            .acquire_many_owned(want)
            .await
            .expect("semáforo de bytes do prefill nunca é fechado")
    }
}

/// Attempts per chunk before the request is given up on. Lives in `relay.rs`
/// (`crate::relay::CHUNK_ATTEMPTS`) since a ring-mode relaying hop needs the
/// same number for its own outbound prefill calls. A prefill chunk gets one
/// attempt because a lost response leaves its KV application ambiguous; a
/// decode step is also not retried, so the two must not share one constant that
/// later drifts.
/// What the last peer produced once the whole prompt is in.
#[derive(Debug, Clone)]
pub struct PrefillOutcome {
    pub token_id: u32,
    pub text: String,
    pub done: bool,
    /// Prompt length as counted by the peer that tokenised it.
    pub prompt_tokens: u32,
    /// Tokens actually left in `prompt_seq` after the volatile template tail
    /// and generation were separated. This is the value `/v1/session/rewind`
    /// confirms; it can be smaller than `prompt_tokens` by construction.
    pub retained_tokens: u32,
    /// Quantos tokens do prompt a cabeça **não** reprocessou porque já estavam
    /// no KV desta sessão.
    ///
    /// É a medida do que foi *executado*, não do que o gateway escolheu: quem
    /// tokeniza é quem decide se o prefixo retido servia. Zero num turno frio,
    /// num prefixo que divergiu e num modelo qualquer que tenha recomeçado a
    /// sessão. Vai para o log e para `usage.prompt_tokens_details.cached_tokens`.
    pub cached_tokens: u32,
    pub chunks: u32,
    pub trace: RequestTrace,
}

/// Everything one hop spent inside a single phase. Prefill and decode are kept
/// apart because they are different workloads: prefill moves a whole prompt
/// through the peer once, decode moves one token at a time. Averaging them
/// together hides which of the two is slow.
/// O que uma chamada de salto conta sobre o canal que ela usou.
///
/// Existe como struct, e não como dois parâmetros soltos, porque `record_hop`
/// já carrega nove posicionais - dois `u64` a mais ali seriam trocados de
/// ordem no primeiro chamador distraído.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TlsHop {
    /// A chamada foi cifrada.
    pub on: bool,
    /// Handshake medido no servidor, em ms. Zero quando a conexão veio do pool.
    pub handshake_ms: u64,
}

/// Uma travessia observada de um frame entre dois processos. Os valores são
/// por chamada, não créditos residuais atribuídos ao primeiro hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportTrace {
    pub transport: crate::relay::InferenceTransport,
    pub fallback: bool,
    pub round_trip_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

pub(super) const TELEMETRY_SAMPLE_LIMIT: usize = 1_024;
pub(super) const MAX_TRANSPORTS: usize = 4;
pub(super) const MAX_TRANSPORT_SWITCHES: usize = MAX_TRANSPORTS * MAX_TRANSPORTS;

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct RttDistribution {
    pub(super) min_ms: u64,
    pub(super) max_ms: u64,
    pub(super) samples: Vec<u64>,
    pub(super) seen: u64,
}

impl RttDistribution {
    pub(super) fn record(&mut self, value: u64) {
        self.min_ms = if self.seen == 0 {
            value
        } else {
            self.min_ms.min(value)
        };
        self.max_ms = self.max_ms.max(value);
        self.seen = self.seen.saturating_add(1);
        if self.samples.len() < TELEMETRY_SAMPLE_LIMIT {
            self.samples.push(value);
            return;
        }
        let slot = reservoir_slot(self.seen);
        if slot < TELEMETRY_SAMPLE_LIMIT {
            self.samples[slot] = value;
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        if other.seen == 0 {
            return;
        }
        self.min_ms = if self.seen == 0 {
            other.min_ms
        } else {
            self.min_ms.min(other.min_ms)
        };
        self.max_ms = self.max_ms.max(other.max_ms);
        let seen = self.seen.saturating_add(other.seen);
        for (offset, &sample) in other.samples.iter().enumerate() {
            let candidate_seen = self.seen.saturating_add(offset as u64 + 1);
            if self.samples.len() < TELEMETRY_SAMPLE_LIMIT {
                self.samples.push(sample);
            } else {
                let slot = reservoir_slot(candidate_seen);
                if slot < TELEMETRY_SAMPLE_LIMIT {
                    self.samples[slot] = sample;
                }
            }
        }
        self.seen = seen;
    }

    pub(super) fn percentile(&self, percentile: u64) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let index = ((sorted.len() - 1) * percentile as usize + 50) / 100;
        sorted[index]
    }
}

pub(super) fn reservoir_slot(seen: u64) -> usize {
    seen.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
        .wrapping_rem(seen.max(1)) as usize
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransportSummaryEntry {
    pub transport: crate::relay::InferenceTransport,
    pub calls: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub rtt_min_ms: u64,
    pub rtt_median_ms: u64,
    pub rtt_p95_ms: u64,
    pub rtt_max_ms: u64,
    pub fallback_calls: u32,
    #[serde(skip)]
    pub(super) rtt: RttDistribution,
}

impl TransportSummaryEntry {
    pub(super) fn new(transport: crate::relay::InferenceTransport) -> Self {
        Self {
            transport,
            calls: 0,
            bytes_sent: 0,
            bytes_received: 0,
            rtt_min_ms: 0,
            rtt_median_ms: 0,
            rtt_p95_ms: 0,
            rtt_max_ms: 0,
            fallback_calls: 0,
            rtt: RttDistribution::default(),
        }
    }

    pub(super) fn record(&mut self, trace: TransportTrace) {
        self.calls = self.calls.saturating_add(1);
        self.bytes_sent = self.bytes_sent.saturating_add(trace.bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(trace.bytes_received);
        if trace.fallback {
            self.fallback_calls = self.fallback_calls.saturating_add(1);
        }
        self.rtt.record(trace.round_trip_ms);
        self.refresh_percentiles();
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.bytes_sent = self.bytes_sent.saturating_add(other.bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(other.bytes_received);
        self.fallback_calls = self.fallback_calls.saturating_add(other.fallback_calls);
        self.rtt.merge(&other.rtt);
        self.refresh_percentiles();
    }

    pub(super) fn refresh_percentiles(&mut self) {
        self.rtt_min_ms = self.rtt.min_ms;
        self.rtt_median_ms = self.rtt.percentile(50);
        self.rtt_p95_ms = self.rtt.percentile(95);
        self.rtt_max_ms = self.rtt.max_ms;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransportSwitch {
    pub from: crate::relay::InferenceTransport,
    pub to: crate::relay::InferenceTransport,
    pub calls: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TransportSummary {
    pub transports: Vec<TransportSummaryEntry>,
    pub fallback_calls: u32,
    pub transport_switches: Vec<TransportSwitch>,
    #[serde(skip)]
    pub(super) first_transport: Option<crate::relay::InferenceTransport>,
    #[serde(skip)]
    pub(super) last_transport: Option<crate::relay::InferenceTransport>,
}

impl TransportSummary {
    pub(super) fn record(&mut self, trace: TransportTrace) {
        if self.first_transport.is_none() {
            self.first_transport = Some(trace.transport);
        }
        if let Some(previous) = self.last_transport {
            self.record_switch(previous, trace.transport);
        }
        self.last_transport = Some(trace.transport);
        if trace.fallback {
            self.fallback_calls = self.fallback_calls.saturating_add(1);
        }
        if let Some(entry) = self
            .transports
            .iter_mut()
            .find(|entry| entry.transport == trace.transport)
        {
            entry.record(trace);
            return;
        }
        if self.transports.len() >= MAX_TRANSPORTS {
            return;
        }
        let mut entry = TransportSummaryEntry::new(trace.transport);
        entry.record(trace);
        self.transports.push(entry);
    }

    pub(super) fn record_switch(
        &mut self,
        from: crate::relay::InferenceTransport,
        to: crate::relay::InferenceTransport,
    ) {
        if from == to {
            return;
        }
        if let Some(switch) = self
            .transport_switches
            .iter_mut()
            .find(|switch| switch.from == from && switch.to == to)
        {
            switch.calls = switch.calls.saturating_add(1);
        } else if self.transport_switches.len() < MAX_TRANSPORT_SWITCHES {
            self.transport_switches
                .push(TransportSwitch { from, to, calls: 1 });
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        if self.first_transport.is_none() {
            self.first_transport = other.first_transport;
        } else if let (Some(from), Some(to)) = (self.last_transport, other.first_transport) {
            self.record_switch(from, to);
        }
        for other_entry in &other.transports {
            if let Some(entry) = self
                .transports
                .iter_mut()
                .find(|entry| entry.transport == other_entry.transport)
            {
                entry.merge(other_entry);
            } else if self.transports.len() < MAX_TRANSPORTS {
                self.transports.push(other_entry.clone());
            }
        }
        self.fallback_calls = self.fallback_calls.saturating_add(other.fallback_calls);
        for switch in &other.transport_switches {
            if let Some(existing) = self
                .transport_switches
                .iter_mut()
                .find(|existing| existing.from == switch.from && existing.to == switch.to)
            {
                existing.calls = existing.calls.saturating_add(switch.calls);
            } else if self.transport_switches.len() < MAX_TRANSPORT_SWITCHES {
                self.transport_switches.push(switch.clone());
            }
        }
        self.last_transport = other.last_transport.or(self.last_transport);
    }
}

impl TransportTrace {
    pub(super) fn from_sent(sent: &crate::relay::SentFrame) -> Self {
        Self {
            transport: sent.transport,
            fallback: sent.transport_fallback,
            round_trip_ms: sent.round_trip_ms,
            bytes_sent: sent.bytes_sent,
            bytes_received: sent.bytes_received,
        }
    }

    pub(super) fn from_json(entry: &serde_json::Value) -> Self {
        Self {
            transport: entry
                .get("transport")
                .and_then(|value| value.as_str())
                .map(crate::relay::InferenceTransport::from_wire)
                .unwrap_or(crate::relay::InferenceTransport::Unknown),
            fallback: entry
                .get("transport_fallback")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            round_trip_ms: entry
                .get("round_trip_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            bytes_sent: entry
                .get("bytes_sent")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            bytes_received: entry
                .get("bytes_received")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PhaseTrace {
    /// Time the peer reported for itself (`processing_ms` on the wire).
    pub processing_ms: u64,
    /// Sum of the observed call round trips for this phase, including peer
    /// processing. The frontend uses this for the execution RTT total.
    pub round_trip_ms: u64,
    /// Round trip minus what the peer claimed: the residual HTTP time.
    pub network_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Bytes of hidden-state tensor sent by this phase, without frame headers.
    pub hidden_state_bytes_sent: u64,
    /// Bytes of hidden-state tensor received by this phase, without headers.
    pub hidden_state_bytes_received: u64,
    /// Effective precision observed in this phase. None means no sample or a
    /// phase that mixed formats, which is intentionally visible as unknown.
    pub hidden_state_dtype: Option<WireDtype>,
    pub calls: u32,
    pub client_encode_ms: u64,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    pub client_decode_ms: u64,
    /// Chamadas deste salto que foram cifradas.
    ///
    /// Contagem e não booleano porque uma fase soma várias chamadas: se só
    /// parte delas foi por TLS, a tela precisa poder dizer isso em vez de
    /// escolher um dos dois e mentir sobre o resto.
    pub tls_calls: u32,
    /// Soma dos handshakes TLS deste salto, medidos no servidor.
    ///
    /// Quase sempre zero: o handshake acontece uma vez por conexão e as
    /// conexões ficam no pool. É custo de estabelecimento, não custo por token.
    pub tls_handshake_ms: u64,
    /// Contagem do estado do pool reqwest observado nesta fase.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_new_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_reused_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_unknown_calls: u32,
    /// Distribuição limitada das chamadas que cruzaram este hop.
    ///
    /// Min/max são exatos. Depois de 1.024 amostras, os quantis usam um
    /// reservoir limitado para que nem a memória do trace nem a resposta
    /// cresçam por token.
    pub transport_summary: TransportSummary,
    pub node_frame_decode_ms: f64,
    pub node_input_prepare_ms: f64,
    pub node_queue_ms: f64,
    /// Espera pelo mutex do motor no nó: o preço de outra requisição estar
    /// decodificando ali. É o que separa "o nó é lento" de "o nó está ocupado".
    pub node_lock_wait_ms: f64,
    pub node_setup_ms: f64,
    pub node_decode_ms: f64,
    /// Time to hand the batch to the backend — not the time it ran.
    pub node_backend_enqueue_ms: f64,
    /// Queued backend work plus already-queued output transfer, measured with
    /// `llama_synchronize`. A zero value means no completion duration was
    /// recorded; the effective `POM_PROFILE_SYNC` state is published separately
    /// by `/health` and must never be inferred from this timing.
    pub node_backend_completion_ms: f64,
    /// Tempo do forward MoE externo, medido fora de `llama_decode`.
    ///
    /// O hook do llama.cpp e opt-in e registra o forward depois que ele
    /// retorna; sem este campo o trace confundia esse custo com o restante do
    /// backend e deixava a soma impossível de fechar.
    pub node_moe_forward_ms: f64,
    /// Device-to-host copy of the residual, measured inside the capture hook.
    pub node_residual_capture_d2h_sync_ms: f64,
    pub node_residual_ms: f64,
    pub node_logits_access_ms: f64,
    pub node_logits_clone_ms: f64,
    pub node_repetition_penalty_ms: f64,
    pub node_softmax_ms: f64,
    pub node_top_k_ms: f64,
    pub node_top_p_ms: f64,
    pub node_selection_ms: f64,
    pub node_token_text_ms: f64,
    pub node_sampling_ms: f64,
    pub node_encode_ms: f64,
    pub node_first_token_produced_ms: f64,
    pub node_first_token_serialized_ms: f64,
    /// Percentiles over the peer's own `decode_ms`, per call in this phase.
    pub node_p50_ms: f64,
    pub node_p95_ms: f64,
    /// Caminho que o backend selecionou a partir do GGUF.
    pub selected_path: Option<String>,
    /// Motor único quando a fase foi homogênea; `unknown` para mistura ou
    /// ausência de amostra.
    pub executed_engine: crate::executor::ExecutionEngine,
    /// Todos os motores observados, preservando a evidência de uma fase mista.
    pub executed_engines: Vec<crate::executor::ExecutionEngine>,
    pub engine_mismatch: bool,
    pub mixed_execution: bool,
    pub moe_executor: Option<String>,
    pub moe_executors: Vec<String>,
    pub moe_executor_dtypes: Vec<u32>,
    pub moe_executor_reason: Option<String>,
    pub moe_executor_diagnostics: Option<crate::moe::runtime::MoeExecutorDiagnostics>,
    #[serde(skip)]
    pub(super) node_samples_ms: Vec<f64>,
    #[serde(skip)]
    pub(super) node_samples_seen: u64,
    #[serde(skip)]
    pub(super) selected_path_seen: bool,
    #[serde(skip)]
    pub(super) hidden_state_dtype_mixed: bool,
}

impl PhaseTrace {
    pub(super) fn record_connection_state(&mut self, state: crate::peer_client::ConnectionState) {
        match state {
            crate::peer_client::ConnectionState::New => self.connection_new_calls += 1,
            crate::peer_client::ConnectionState::Reused => self.connection_reused_calls += 1,
            crate::peer_client::ConnectionState::Unknown => self.connection_unknown_calls += 1,
        }
    }

    pub(super) fn record_transport(&mut self, transport: TransportTrace) {
        self.transport_summary.record(transport);
    }

    pub(super) fn record(
        &mut self,
        round_trip_ms: u64,
        processing_ms: u64,
        bytes_sent: u64,
        bytes_received: u64,
        client_encode_ms: u64,
        response_headers_ms: u64,
        response_body_ms: u64,
        client_decode_ms: u64,
        tls: TlsHop,
        node: &serde_json::Value,
    ) {
        let node_ms = |key: &str| node[key].as_f64().unwrap_or(0.0);
        let node_decode_ms = node_ms("decode_ms");
        let selected_path = node
            .get("selected_path")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let engines = node
            .get("executed_engines")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(crate::executor::ExecutionEngine::from_telemetry)
                    .filter(|engine| *engine != crate::executor::ExecutionEngine::Unknown)
                    .collect::<Vec<_>>()
            })
            .filter(|engines| !engines.is_empty())
            .unwrap_or_else(|| {
                node.get("executed_engine")
                    .and_then(|value| value.as_str())
                    .map(crate::executor::ExecutionEngine::from_telemetry)
                    .into_iter()
                    .filter(|engine| *engine != crate::executor::ExecutionEngine::Unknown)
                    .collect()
            });
        let moe_executor = node
            .get("moe_executor")
            .and_then(|value| value.as_str())
            .filter(|value| *value != "not-run")
            .map(str::to_owned);
        let moe_executors = node
            .get("moe_executors")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let moe_executor_dtypes = node
            .get("moe_executor_dtypes")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_u64())
                    .filter_map(|value| u32::try_from(value).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let moe_executor_reason = node
            .get("moe_executor_reason")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let moe_executor_diagnostics = node
            .get("moe_executor_diagnostics")
            .and_then(|value| serde_json::from_value(value.clone()).ok());

        self.processing_ms += processing_ms;
        self.round_trip_ms += round_trip_ms;
        self.network_ms += round_trip_ms.saturating_sub(processing_ms);
        self.bytes_sent += bytes_sent;
        self.bytes_received += bytes_received;
        self.calls += 1;
        self.client_encode_ms += client_encode_ms;
        self.response_headers_ms += response_headers_ms;
        self.response_body_ms += response_body_ms;
        self.client_decode_ms += client_decode_ms;
        if tls.on {
            self.tls_calls += 1;
        }
        self.tls_handshake_ms += tls.handshake_ms;
        self.node_frame_decode_ms += node_ms("frame_decode_ms");
        self.node_input_prepare_ms += node_ms("input_prepare_ms");
        self.node_queue_ms += node_ms("queue_ms");
        self.node_lock_wait_ms += node_ms("lock_wait_ms");
        self.node_setup_ms += node_ms("setup_ms");
        self.node_decode_ms += node_decode_ms;
        self.node_backend_enqueue_ms += node_ms("backend_enqueue_ms");
        self.node_backend_completion_ms += node_ms("backend_completion_ms");
        self.node_moe_forward_ms += node_ms("moe_forward_ms");
        self.node_residual_capture_d2h_sync_ms += node_ms("residual_capture_d2h_sync_ms");
        self.node_residual_ms += node_ms("residual_ms");
        self.node_logits_access_ms += node_ms("logits_access_ms");
        self.node_logits_clone_ms += node_ms("logits_clone_ms");
        self.node_repetition_penalty_ms += node_ms("repetition_penalty_ms");
        self.node_softmax_ms += node_ms("softmax_ms");
        self.node_top_k_ms += node_ms("top_k_ms");
        self.node_top_p_ms += node_ms("top_p_ms");
        self.node_selection_ms += node_ms("selection_ms");
        self.node_token_text_ms += node_ms("token_text_ms");
        self.node_sampling_ms += node_ms("sampling_ms");
        self.node_encode_ms += node_ms("encode_ms");
        self.node_first_token_produced_ms = self
            .node_first_token_produced_ms
            .max(node_ms("first_token_produced_ms"));
        self.node_first_token_serialized_ms = self
            .node_first_token_serialized_ms
            .max(node_ms("first_token_serialized_ms"));

        self.record_node_sample(node_decode_ms);
        self.observe_path(selected_path);
        for engine in engines {
            self.observe_engine(engine);
        }
        self.observe_moe_executor(moe_executor);
        for executor in moe_executors {
            self.observe_moe_executor(Some(executor));
        }
        for dtype in moe_executor_dtypes {
            if !self.moe_executor_dtypes.contains(&dtype) {
                self.moe_executor_dtypes.push(dtype);
            }
        }
        if let Some(reason) = moe_executor_reason {
            if self.moe_executor_reason.is_none() {
                self.moe_executor_reason = Some(reason);
            }
        }
        if moe_executor_diagnostics.is_some() {
            self.moe_executor_diagnostics = moe_executor_diagnostics;
        }
        self.refresh_percentiles();
    }

    pub(super) fn observe_moe_executor(&mut self, next: Option<String>) {
        let Some(next) = next else {
            return;
        };
        if !self.moe_executors.contains(&next) {
            self.moe_executors.push(next);
        }
        self.moe_executor = match self.moe_executors.as_slice() {
            [executor] => Some(executor.clone()),
            [] => None,
            _ => Some("mixed".to_owned()),
        };
    }

    pub(super) fn observe_engine(&mut self, next: crate::executor::ExecutionEngine) {
        if next != crate::executor::ExecutionEngine::Unknown
            && !self.executed_engines.contains(&next)
        {
            self.executed_engines.push(next);
        }
        self.executed_engine = match self.executed_engines.as_slice() {
            [engine] => *engine,
            _ => crate::executor::ExecutionEngine::Unknown,
        };
        self.mixed_execution = self.executed_engines.len() > 1;
        self.refresh_engine_mismatch();
    }

    pub(super) fn observe_path(&mut self, next: Option<String>) {
        let Some(next) = next else {
            return;
        };
        if self.selected_path_seen {
            if self.selected_path.as_deref() != Some(next.as_str()) {
                self.selected_path = None;
            }
        } else {
            self.selected_path = Some(next);
            self.selected_path_seen = true;
        }
        self.refresh_engine_mismatch();
    }

    pub(super) fn refresh_engine_mismatch(&mut self) {
        let Some(path) = self.selected_path.as_deref() else {
            self.engine_mismatch = false;
            return;
        };
        let expected = if path.starts_with("moe/") {
            crate::executor::ExecutionEngine::Moe
        } else if path.starts_with("dense/") {
            crate::executor::ExecutionEngine::LlamaCpp
        } else {
            crate::executor::ExecutionEngine::Unknown
        };
        self.engine_mismatch = !self.executed_engines.is_empty()
            && expected != crate::executor::ExecutionEngine::Unknown
            && !self.executed_engines.contains(&expected);
    }

    pub(super) fn observe_hidden_dtype(&mut self, dtype: WireDtype) {
        if self.hidden_state_dtype_mixed {
            return;
        }
        match self.hidden_state_dtype {
            None => self.hidden_state_dtype = Some(dtype),
            Some(existing) if existing == dtype => {}
            Some(_) => {
                self.hidden_state_dtype = None;
                self.hidden_state_dtype_mixed = true;
            }
        }
    }

    pub(super) fn merge(&mut self, next: &PhaseTrace) {
        self.processing_ms += next.processing_ms;
        self.round_trip_ms += next.round_trip_ms;
        self.network_ms += next.network_ms;
        self.bytes_sent += next.bytes_sent;
        self.bytes_received += next.bytes_received;
        self.hidden_state_bytes_sent += next.hidden_state_bytes_sent;
        self.hidden_state_bytes_received += next.hidden_state_bytes_received;
        if next.hidden_state_dtype_mixed {
            self.hidden_state_dtype = None;
            self.hidden_state_dtype_mixed = true;
        } else if let Some(dtype) = next.hidden_state_dtype {
            self.observe_hidden_dtype(dtype);
        }
        self.calls += next.calls;
        self.client_encode_ms += next.client_encode_ms;
        self.response_headers_ms += next.response_headers_ms;
        self.response_body_ms += next.response_body_ms;
        self.client_decode_ms += next.client_decode_ms;
        self.tls_calls += next.tls_calls;
        self.tls_handshake_ms += next.tls_handshake_ms;
        self.connection_new_calls += next.connection_new_calls;
        self.connection_reused_calls += next.connection_reused_calls;
        self.connection_unknown_calls += next.connection_unknown_calls;
        self.transport_summary.merge(&next.transport_summary);
        self.node_frame_decode_ms += next.node_frame_decode_ms;
        self.node_input_prepare_ms += next.node_input_prepare_ms;
        self.node_queue_ms += next.node_queue_ms;
        self.node_lock_wait_ms += next.node_lock_wait_ms;
        self.node_setup_ms += next.node_setup_ms;
        self.node_decode_ms += next.node_decode_ms;
        self.node_backend_enqueue_ms += next.node_backend_enqueue_ms;
        self.node_backend_completion_ms += next.node_backend_completion_ms;
        self.node_residual_capture_d2h_sync_ms += next.node_residual_capture_d2h_sync_ms;
        self.node_residual_ms += next.node_residual_ms;
        self.node_logits_access_ms += next.node_logits_access_ms;
        self.node_logits_clone_ms += next.node_logits_clone_ms;
        self.node_repetition_penalty_ms += next.node_repetition_penalty_ms;
        self.node_softmax_ms += next.node_softmax_ms;
        self.node_top_k_ms += next.node_top_k_ms;
        self.node_top_p_ms += next.node_top_p_ms;
        self.node_selection_ms += next.node_selection_ms;
        self.node_token_text_ms += next.node_token_text_ms;
        self.node_sampling_ms += next.node_sampling_ms;
        self.node_encode_ms += next.node_encode_ms;
        self.node_first_token_produced_ms = self
            .node_first_token_produced_ms
            .max(next.node_first_token_produced_ms);
        self.node_first_token_serialized_ms = self
            .node_first_token_serialized_ms
            .max(next.node_first_token_serialized_ms);
        for &sample in &next.node_samples_ms {
            self.record_node_sample(sample);
        }
        self.node_samples_seen = self.node_samples_seen.saturating_add(
            next.node_samples_seen
                .saturating_sub(next.node_samples_ms.len() as u64),
        );
        if next.selected_path_seen {
            self.observe_path(next.selected_path.clone());
        }
        for engine in &next.executed_engines {
            self.observe_engine(*engine);
        }
        self.observe_moe_executor(next.moe_executor.clone());
        for executor in &next.moe_executors {
            self.observe_moe_executor(Some(executor.clone()));
        }
        for dtype in &next.moe_executor_dtypes {
            if !self.moe_executor_dtypes.contains(dtype) {
                self.moe_executor_dtypes.push(*dtype);
            }
        }
        if self.moe_executor_reason.is_none() {
            self.moe_executor_reason = next.moe_executor_reason.clone();
        }
        if next.moe_executor_diagnostics.is_some() {
            self.moe_executor_diagnostics = next.moe_executor_diagnostics.clone();
        }
        self.refresh_percentiles();
    }

    pub(super) fn refresh_percentiles(&mut self) {
        self.node_p50_ms = percentile(&self.node_samples_ms, 50);
        self.node_p95_ms = percentile(&self.node_samples_ms, 95);
    }

    pub(super) fn record_node_sample(&mut self, sample: f64) {
        self.node_samples_seen = self.node_samples_seen.saturating_add(1);
        if self.node_samples_ms.len() < TELEMETRY_SAMPLE_LIMIT {
            self.node_samples_ms.push(sample);
            return;
        }
        let slot = reservoir_slot(self.node_samples_seen);
        if slot < TELEMETRY_SAMPLE_LIMIT {
            self.node_samples_ms[slot] = sample;
            self.refresh_percentiles();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopTrace {
    pub node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub prefill: PhaseTrace,
    pub decode: PhaseTrace,
}

/// Estado de aquecimento observado por esta execução. `Unknown` é deliberado:
/// um peer antigo ou uma fronteira que não expõe o estado não deve virar
/// `cold` por inferência.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    Cold,
    Warm,
    #[default]
    Unknown,
}

/// Span fechado medido em um único host.
///
/// `started_at_ms` é wall clock apenas para correlação. `start_offset_ms` e
/// `duration_ms` vêm de `Instant` no mesmo host. Nunca se subtraem offsets de
/// máquinas diferentes. O `parallel_group` permite que a UI explique
/// sobreposição; o cálculo de caminho crítico usa a união dos intervalos.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceSpan {
    pub name: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub host: String,
    pub started_at_ms: u64,
    pub start_offset_ms: u64,
    pub duration_ms: u64,
    pub parallel_group: Option<String>,
}

impl TraceSpan {
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        parent_span_id: Option<String>,
        started_at_ms: u64,
        start_offset_ms: u64,
        duration_ms: u64,
        parallel_group: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            span_id: uuid::Uuid::new_v4().simple().to_string(),
            parent_span_id,
            host: host.into(),
            started_at_ms,
            start_offset_ms,
            duration_ms,
            parallel_group,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        name: &str,
        host: &str,
        start_offset_ms: u64,
        duration_ms: u64,
        parallel_group: Option<&str>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            span_id: format!("test-{name}"),
            parent_span_id: None,
            host: host.to_owned(),
            started_at_ms: 0,
            start_offset_ms,
            duration_ms,
            parallel_group: parallel_group.map(str::to_owned),
        }
    }
}

/// Marcos observáveis do primeiro token. Cada timestamp é local ao host que
/// o registrou; os campos de duração só são preenchidos quando o mesmo host
/// consegue medir ambas as pontas.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FirstTokenTrace {
    pub engine_produced_at_ms: Option<u64>,
    pub node_serialized_at_ms: Option<u64>,
    pub node_sent_at_ms: Option<u64>,
    pub coordinator_received_at_ms: Option<u64>,
    pub gateway_stream_at_ms: Option<u64>,
    pub client_received_at_ms: Option<u64>,
    pub engine_ttft_ms: Option<u64>,
    pub node_serialized_ms: Option<u64>,
    pub node_to_gateway_ms: Option<u64>,
    pub gateway_to_client_ms: Option<u64>,
}

impl FirstTokenTrace {
    fn is_empty(&self) -> bool {
        self.engine_produced_at_ms.is_none()
            && self.node_serialized_at_ms.is_none()
            && self.node_sent_at_ms.is_none()
            && self.coordinator_received_at_ms.is_none()
            && self.gateway_stream_at_ms.is_none()
            && self.client_received_at_ms.is_none()
            && self.engine_ttft_ms.is_none()
            && self.node_serialized_ms.is_none()
            && self.node_to_gateway_ms.is_none()
            && self.gateway_to_client_ms.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamingTrace {
    pub time_to_first_chunk_ms: Option<u64>,
    pub time_to_first_token_ms: Option<u64>,
    pub stream_duration_ms: Option<u64>,
    pub time_last_token_to_response_end_ms: Option<u64>,
    pub chunks: u64,
    pub bytes: u64,
    pub tokens_per_chunk: Option<f64>,
}

impl StreamingTrace {
    fn is_empty(&self) -> bool {
        self.time_to_first_chunk_ms.is_none()
            && self.time_to_first_token_ms.is_none()
            && self.stream_duration_ms.is_none()
            && self.time_last_token_to_response_end_ms.is_none()
            && self.chunks == 0
            && self.bytes == 0
            && self.tokens_per_chunk.is_none()
    }
}

/// União dos intervalos observados em um host. Abrange spans inclusivos e
/// paralelos uma vez só, por isso não confunde soma de trabalho com caminho
/// crítico.
pub fn critical_path_ms(spans: &[TraceSpan], host: &str) -> u64 {
    let mut intervals: Vec<_> = spans
        .iter()
        .filter(|span| span.host == host && span.duration_ms > 0)
        .map(|span| {
            (
                span.start_offset_ms,
                span.start_offset_ms.saturating_add(span.duration_ms),
            )
        })
        .collect();
    intervals.sort_unstable_by_key(|(start, _)| *start);

    let mut covered_until = 0;
    let mut covered: u64 = 0;
    for (start, end) in intervals {
        if end <= covered_until {
            continue;
        }
        if start >= covered_until {
            covered = covered.saturating_add(end.saturating_sub(start));
        } else {
            covered = covered.saturating_add(end.saturating_sub(covered_until));
        }
        covered_until = end;
    }
    covered
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestTrace {
    pub trace_id: String,
    pub profile: crate::telemetry::Profile,
    pub request_state: RequestState,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<TraceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_node_total_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_node_accounted_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_node_unaccounted_ms: Option<u64>,
    #[serde(skip_serializing_if = "FirstTokenTrace::is_empty")]
    pub first_token: FirstTokenTrace,
    #[serde(skip_serializing_if = "StreamingTrace::is_empty")]
    pub streaming: StreamingTrace,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub decode_calls: u64,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub generated_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_decode_call: Option<f64>,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_new_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_reused_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_unknown_calls: u32,
    pub model: String,
    pub input_tokens: u32,
    /// Quantos dos `input_tokens` a cabeça **não** reprocessou porque já
    /// estavam no KV da sessão reaproveitada.
    ///
    /// Medida do executado, não da escolha: quem a preenche é a resposta da
    /// cabeça (`PrefillOutcome::cached_tokens`). Um turno frio, um prefixo que
    /// divergiu e um peer que recomeçou a sessão contam todos zero. Era
    /// exatamente esta a lacuna que fazia o cache parecer estar funcionando -
    /// o log dizia "sessão reusada" e a economia era nenhuma.
    pub cached_tokens: u32,
    /// Candidato comum com o prefixo retido. Pode ser maior que zero mesmo
    /// quando a divergência fez o peer reiniciar a sessão a frio.
    pub prefix_match_tokens: u32,
    /// Resultado observável da tentativa: `full`, `partial-cold`, `miss` ou
    /// `cold`. O gateway troca por `session-lost-cold` quando precisou
    /// reconstruir a sessão após um peer perder seu estado, e por `disk`
    /// quando o reuso veio de um snapshot automático em disco
    /// (`slot_persist_prefix_cache`, ver `inferencia.md` §3.1.1).
    pub prefix_reuse_status: Option<String>,
    /// Tokens que realmente precisaram atravessar a fronteira de inferência.
    /// Em um turno frio, ou após uma divergência parcial, é o prompt inteiro.
    pub prompt_tokens_transmitted: u32,
    pub output_tokens: u32,
    pub prefill_ms: u64,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub hops: Vec<HopTrace>,
    pub issues: Vec<TraceIssue>,
    pub dropped_issues: u32,
    pub complete: bool,
    pub transport_trace_complete: bool,
    pub prefill_overlap_enabled: bool,
    pub prefill_max_inflight_chunks: u32,
    pub prefill_overlapped_chunks: u32,
    /// Maior número de pedaços de prefill que estiveram em voo ao mesmo tempo
    /// nesta requisição. `<= prefill_max_inflight_chunks`; `1` quando a
    /// sobreposição não pegou (prompt de um pedaço só, janela em 1, modo anel).
    pub prefill_pipeline_depth_max: u32,
    /// Tokens draft que o caminho MTP propôs nesta requisição, somados entre
    /// todos os hops e steps.
    pub mtp_draft_tokens_proposed: u64,
    /// Tokens draft que passaram no critério de aceitação do MTP.
    pub mtp_draft_tokens_accepted: u64,
    /// Tokens draft que foram verificados mas não foram aceitos pelo MTP.
    pub mtp_draft_tokens_rejected: u64,
    /// `accepted / proposed`; nulo quando a requisição não propôs tokens MTP.
    pub mtp_acceptance_rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Prefill,
    Decode,
}

/// A request descriptor used before a hop is packed into `frame/v1-batch`.
/// Only requests with the same routing key may share a frame.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRouteItem {
    pub item_id: String,
    pub session_id: String,
    pub model: String,
    pub deployment_id: String,
    pub phase: Phase,
    pub config_key: String,
    pub next_hop_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BatchRouteKey {
    pub(super) model: String,
    pub(super) deployment_id: String,
    pub(super) phase: Phase,
    pub(super) config_key: String,
    pub(super) next_hop_id: String,
}

/// Compatible requests grouped for one particular next hop. Group order and
/// item order follow the input order, which keeps streaming deterministic for
/// each session and makes tests independent of hash-map iteration order.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRoute {
    pub model: String,
    pub deployment_id: String,
    pub phase: Phase,
    pub config_key: String,
    pub next_hop_id: String,
    pub items: Vec<BatchRouteItem>,
}

#[allow(dead_code)]
impl BatchRoute {
    pub fn group(items: impl IntoIterator<Item = BatchRouteItem>) -> Vec<Self> {
        let mut groups: Vec<(BatchRouteKey, Vec<BatchRouteItem>)> = Vec::new();
        for item in items {
            let key = BatchRouteKey {
                model: item.model.clone(),
                deployment_id: item.deployment_id.clone(),
                phase: item.phase,
                config_key: item.config_key.clone(),
                next_hop_id: item.next_hop_id.clone(),
            };
            if let Some((_, grouped)) = groups.iter_mut().find(|(existing, _)| *existing == key) {
                grouped.push(item);
            } else {
                groups.push((key, vec![item]));
            }
        }
        groups
            .into_iter()
            .map(|(key, items)| Self {
                model: key.model,
                deployment_id: key.deployment_id,
                phase: key.phase,
                config_key: key.config_key,
                next_hop_id: key.next_hop_id,
                items,
            })
            .collect()
    }
}

pub(super) const TRACE_ISSUE_LIMIT: usize = 32;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceIssueKind {
    Timeout,
    Transport,
    Http,
    Protocol,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceIssue {
    pub hop: Option<usize>,
    pub node_id: String,
    pub phase: Phase,
    pub kind: TraceIssueKind,
    pub attempt: u32,
    pub message: String,
}

impl TraceIssue {
    pub fn new(
        hop: Option<usize>,
        node_id: impl Into<String>,
        phase: Phase,
        kind: TraceIssueKind,
        attempt: u32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            hop,
            node_id: node_id.into(),
            phase,
            kind,
            attempt,
            message: message.into(),
        }
    }
}

impl RequestTrace {
    pub fn new(model: impl Into<String>, steps: &[(&str, u32, u32)]) -> Self {
        Self::new_with_trace_id(uuid::Uuid::new_v4().simple().to_string(), model, steps)
    }

    pub fn new_with_trace_id(
        trace_id: impl Into<String>,
        model: impl Into<String>,
        steps: &[(&str, u32, u32)],
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            profile: crate::telemetry::Profile::current(),
            request_state: RequestState::Unknown,
            spans: Vec::new(),
            outside_node_total_ms: None,
            outside_node_accounted_ms: None,
            outside_node_unaccounted_ms: None,
            first_token: FirstTokenTrace::default(),
            streaming: StreamingTrace::default(),
            server_ttft_ms: None,
            engine_ttft_ms: None,
            client_ttft_ms: None,
            decode_calls: 0,
            generated_tokens: 0,
            tokens_per_decode_call: None,
            connection_new_calls: 0,
            connection_reused_calls: 0,
            connection_unknown_calls: 0,
            model: model.into(),
            input_tokens: 0,
            cached_tokens: 0,
            prefix_match_tokens: 0,
            prefix_reuse_status: None,
            prompt_tokens_transmitted: 0,
            output_tokens: 0,
            prefill_ms: 0,
            total_bytes_sent: 0,
            total_bytes_received: 0,
            issues: Vec::new(),
            dropped_issues: 0,
            complete: false,
            transport_trace_complete: true,
            prefill_overlap_enabled: false,
            prefill_max_inflight_chunks: 1,
            prefill_overlapped_chunks: 0,
            prefill_pipeline_depth_max: 0,
            mtp_draft_tokens_proposed: 0,
            mtp_draft_tokens_accepted: 0,
            mtp_draft_tokens_rejected: 0,
            mtp_acceptance_rate: None,
            hops: steps
                .iter()
                .map(|(node_id, start_layer, end_layer)| HopTrace {
                    node_id: (*node_id).into(),
                    start_layer: *start_layer,
                    end_layer: *end_layer,
                    prefill: PhaseTrace::default(),
                    decode: PhaseTrace::default(),
                })
                .collect(),
        }
    }

    pub fn record_issue(&mut self, issue: TraceIssue) {
        if self.issues.len() < TRACE_ISSUE_LIMIT {
            self.issues.push(issue);
        } else {
            self.dropped_issues += 1;
        }
    }

    pub fn record_span(&mut self, span: TraceSpan) {
        if !self.profile.collects_spans() {
            return;
        }
        self.spans.push(span);
        self.refresh_outside_node_accounting();
    }

    pub fn set_accounted_spans(&mut self, spans: Vec<TraceSpan>) {
        if !self.profile.collects_spans() {
            self.spans.clear();
            self.outside_node_accounted_ms = None;
            self.outside_node_unaccounted_ms = None;
            return;
        }
        self.spans = spans;
        self.refresh_outside_node_accounting();
    }

    pub fn set_outside_node_total_ms(&mut self, total_ms: u64) {
        self.outside_node_total_ms = Some(total_ms);
        self.refresh_outside_node_accounting();
    }

    pub fn set_generated_tokens(&mut self, tokens: u32) {
        self.generated_tokens = tokens;
        self.tokens_per_decode_call =
            (self.decode_calls > 0).then(|| tokens as f64 / self.decode_calls as f64);
    }

    pub fn node_processing_ms(&self) -> u64 {
        self.hops
            .iter()
            .flat_map(|hop| [&hop.prefill, &hop.decode])
            .map(|phase| phase.processing_ms)
            .sum()
    }

    fn refresh_outside_node_accounting(&mut self) {
        let accounted = critical_path_ms(&self.spans, "gateway");
        self.outside_node_accounted_ms = self.outside_node_total_ms.map(|_| accounted);
        self.outside_node_unaccounted_ms = self
            .outside_node_total_ms
            .map(|total| total.saturating_sub(accounted));
    }

    pub(super) fn record_prefix_decision(&mut self, meta: &serde_json::Value, requested: bool) {
        self.prefix_match_tokens = meta
            .get("prefix_match_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        let reused = meta
            .get("reuse_prefix")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let prompt_tokens = meta
            .get("n_prompt_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        self.prefix_reuse_status = Some(
            if reused {
                "full"
            } else if self.prefix_match_tokens > 0
                && (prompt_tokens == 0 || self.prefix_match_tokens < prompt_tokens)
            {
                "partial-cold"
            } else if self.prefix_match_tokens > 0 {
                "full-cold"
            } else if requested {
                "miss"
            } else {
                "cold"
            }
            .to_owned(),
        );
    }

    pub(super) fn finish_prompt(&mut self, prompt_tokens: u32, cached_tokens: u32) {
        self.input_tokens = prompt_tokens;
        self.cached_tokens = cached_tokens;
        self.prompt_tokens_transmitted = prompt_tokens.saturating_sub(cached_tokens);
    }

    pub fn from_plan(model: &str, plan: &PipelinePlan) -> Self {
        Self::from_plan_with_trace_id(&uuid::Uuid::new_v4().simple().to_string(), model, plan)
    }

    pub fn from_plan_with_trace_id(trace_id: &str, model: &str, plan: &PipelinePlan) -> Self {
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|step| (step.node_id.as_str(), step.start_layer, step.end_layer))
            .collect();
        Self::new_with_trace_id(trace_id, model, &steps)
    }

    pub fn record_hop(
        &mut self,
        hop: usize,
        phase: Phase,
        round_trip_ms: u64,
        processing_ms: u64,
        bytes_sent: u64,
        bytes_received: u64,
        client_encode_ms: u64,
        response_headers_ms: u64,
        response_body_ms: u64,
        client_decode_ms: u64,
        tls: TlsHop,
        node: &serde_json::Value,
    ) {
        let first_token_produced_ms = node
            .get("first_token_produced_ms")
            .and_then(|value| value.as_f64())
            .filter(|value| *value > 0.0);
        let first_token_serialized_ms = node
            .get("first_token_serialized_ms")
            .and_then(|value| value.as_f64())
            .filter(|value| *value > 0.0);
        let first_token_produced_at_ms = node
            .get("first_token_produced_at_ms")
            .and_then(|value| value.as_u64())
            .filter(|value| *value > 0);
        let first_token_serialized_at_ms = node
            .get("first_token_serialized_at_ms")
            .and_then(|value| value.as_u64())
            .filter(|value| *value > 0);
        let Some(entry) = self.hops.get_mut(hop) else {
            return;
        };
        let slot = match phase {
            Phase::Prefill => &mut entry.prefill,
            Phase::Decode => &mut entry.decode,
        };
        slot.record(
            round_trip_ms,
            processing_ms,
            bytes_sent,
            bytes_received,
            client_encode_ms,
            response_headers_ms,
            response_body_ms,
            client_decode_ms,
            tls,
            node,
        );
        if phase == Phase::Decode {
            self.decode_calls = self.decode_calls.saturating_add(1);
            self.tokens_per_decode_call = (self.decode_calls > 0)
                .then(|| self.generated_tokens as f64 / self.decode_calls as f64);
        }
        let has_first_token_marker = first_token_produced_ms.is_some()
            || first_token_serialized_ms.is_some()
            || first_token_produced_at_ms.is_some()
            || first_token_serialized_at_ms.is_some();
        if self.engine_ttft_ms.is_none() && has_first_token_marker {
            self.engine_ttft_ms = first_token_produced_ms.map(|value| value.round() as u64);
            self.first_token.engine_ttft_ms = self.engine_ttft_ms;
            self.first_token.node_serialized_ms =
                first_token_serialized_ms.map(|value| value.round() as u64);
            self.first_token.node_to_gateway_ms = Some(
                round_trip_ms
                    .saturating_sub(processing_ms)
                    .saturating_sub(client_decode_ms),
            );
            self.first_token.engine_produced_at_ms = first_token_produced_at_ms;
            self.first_token.node_serialized_at_ms = first_token_serialized_at_ms;
        }
        self.record_mtp_counts(node);
        self.total_bytes_sent += bytes_sent;
        self.total_bytes_received += bytes_received;
    }

    fn record_mtp_counts(&mut self, node: &serde_json::Value) {
        let get = |key: &str| node.get(key).and_then(|value| value.as_u64()).unwrap_or(0);
        self.mtp_draft_tokens_proposed = self
            .mtp_draft_tokens_proposed
            .saturating_add(get("mtp_draft_tokens_proposed"));
        self.mtp_draft_tokens_accepted = self
            .mtp_draft_tokens_accepted
            .saturating_add(get("mtp_draft_tokens_accepted"));
        self.mtp_draft_tokens_rejected = self
            .mtp_draft_tokens_rejected
            .saturating_add(get("mtp_draft_tokens_rejected"));
        self.refresh_mtp_acceptance_rate();
    }

    fn refresh_mtp_acceptance_rate(&mut self) {
        self.mtp_acceptance_rate = (self.mtp_draft_tokens_proposed > 0)
            .then(|| self.mtp_draft_tokens_accepted as f64 / self.mtp_draft_tokens_proposed as f64);
    }

    pub fn record_hop_from_sent(
        &mut self,
        hop: usize,
        phase: Phase,
        sent: &crate::relay::SentFrame,
    ) {
        let processing_ms = sent
            .meta
            .get("processing_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        self.record_hop(
            hop,
            phase,
            sent.round_trip_ms,
            processing_ms,
            sent.bytes_sent,
            sent.bytes_received,
            sent.client_encode_ms,
            sent.response_headers_ms,
            sent.response_body_ms,
            sent.client_decode_ms,
            TlsHop {
                on: sent.tls,
                handshake_ms: sent.tls_handshake_ms,
            },
            &sent.meta["node_timings"],
        );
        self.record_hidden_state(
            hop,
            phase,
            sent.hidden_state_bytes_sent,
            sent.hidden_state_bytes_received,
            Some(sent.hidden_state_dtype),
        );
        self.record_connection_state(hop, phase, sent.connection_state);
    }

    pub(super) fn record_transport(&mut self, hop: usize, phase: Phase, transport: TransportTrace) {
        let Some(entry) = self.hops.get_mut(hop) else {
            return;
        };
        match phase {
            Phase::Prefill => entry.prefill.record_transport(transport),
            Phase::Decode => entry.decode.record_transport(transport),
        }
    }
    /// Same accumulation as `record_hop`, reading the numbers from one
    /// `hop_timings` array entry instead of taking them as call arguments —
    /// this is what lets ring mode reconstruct the identical trace hub mode
    /// builds live, from the array that bubbled back through the relay
    /// chain (`relay::bubble_hop_timings`) instead of from calls this
    /// process made itself.
    pub fn record_hop_from_json(&mut self, hop: usize, phase: Phase, entry: &serde_json::Value) {
        let get = |key: &str| entry.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        self.record_hop(
            hop,
            phase,
            get("round_trip_ms"),
            get("processing_ms"),
            get("bytes_sent"),
            get("bytes_received"),
            get("client_encode_ms"),
            get("response_headers_ms"),
            get("response_body_ms"),
            get("client_decode_ms"),
            TlsHop {
                on: entry.get("tls").and_then(|v| v.as_bool()).unwrap_or(false),
                handshake_ms: get("tls_handshake_ms"),
            },
            &entry["node_timings"],
        );
        let dtype = entry
            .get("hidden_state_dtype")
            .and_then(|value| value.as_str())
            .and_then(|value| WireDtype::parse(value).ok());
        self.record_hidden_state(
            hop,
            phase,
            get("hidden_state_bytes_sent"),
            get("hidden_state_bytes_received"),
            dtype,
        );
        if let Some(connection_state) = entry
            .get("connection_state")
            .and_then(|value| value.as_str())
        {
            let state = match connection_state {
                "new" => crate::peer_client::ConnectionState::New,
                "reused" => crate::peer_client::ConnectionState::Reused,
                _ => crate::peer_client::ConnectionState::Unknown,
            };
            self.record_connection_state(hop, phase, state);
        }
        if entry.get("transport").is_some() {
            self.record_transport(hop, phase, TransportTrace::from_json(entry));
        }
    }

    pub(super) fn record_hidden_state(
        &mut self,
        hop: usize,
        phase: Phase,
        bytes_sent: u64,
        bytes_received: u64,
        dtype: Option<WireDtype>,
    ) {
        let Some(entry) = self.hops.get_mut(hop) else {
            return;
        };
        let slot = match phase {
            Phase::Prefill => &mut entry.prefill,
            Phase::Decode => &mut entry.decode,
        };
        slot.hidden_state_bytes_sent += bytes_sent;
        slot.hidden_state_bytes_received += bytes_received;
        // Um plano de um salto que cobre o modelo inteiro não carrega estado
        // oculto. O relay ainda sabe qual precisão teria usado, mas publicar
        // essa configuração local do gateway como precisão observada fazia
        // dois gateways dizerem f8 e f16 para a mesma execução, ambos com 0 B.
        // Precisão só é evidência quando bytes cruzam o hop.
        if bytes_sent > 0 || bytes_received > 0 {
            if let Some(dtype) = dtype {
                slot.observe_hidden_dtype(dtype);
            }
        }
    }

    fn record_connection_state(
        &mut self,
        hop: usize,
        phase: Phase,
        state: crate::peer_client::ConnectionState,
    ) {
        let Some(entry) = self.hops.get_mut(hop) else {
            return;
        };
        match phase {
            Phase::Prefill => entry.prefill.record_connection_state(state),
            Phase::Decode => entry.decode.record_connection_state(state),
        }
        match state {
            crate::peer_client::ConnectionState::New => self.connection_new_calls += 1,
            crate::peer_client::ConnectionState::Reused => self.connection_reused_calls += 1,
            crate::peer_client::ConnectionState::Unknown => self.connection_unknown_calls += 1,
        }
    }

    pub fn merge(&mut self, other: &RequestTrace) {
        self.total_bytes_sent += other.total_bytes_sent;
        self.total_bytes_received += other.total_bytes_received;
        self.mtp_draft_tokens_proposed = self
            .mtp_draft_tokens_proposed
            .saturating_add(other.mtp_draft_tokens_proposed);
        self.mtp_draft_tokens_accepted = self
            .mtp_draft_tokens_accepted
            .saturating_add(other.mtp_draft_tokens_accepted);
        self.mtp_draft_tokens_rejected = self
            .mtp_draft_tokens_rejected
            .saturating_add(other.mtp_draft_tokens_rejected);
        self.refresh_mtp_acceptance_rate();
        self.transport_trace_complete &= other.transport_trace_complete;
        self.connection_new_calls += other.connection_new_calls;
        self.connection_reused_calls += other.connection_reused_calls;
        self.connection_unknown_calls += other.connection_unknown_calls;
        if self.engine_ttft_ms.is_none() {
            self.engine_ttft_ms = other.engine_ttft_ms;
            self.first_token = other.first_token.clone();
        }
        if self.prefix_reuse_status.is_none() {
            self.prefix_reuse_status = other.prefix_reuse_status.clone();
        }
        self.prefill_overlap_enabled |= other.prefill_overlap_enabled;
        self.prefill_max_inflight_chunks = self
            .prefill_max_inflight_chunks
            .max(other.prefill_max_inflight_chunks);
        self.prefill_overlapped_chunks += other.prefill_overlapped_chunks;
        self.prefill_pipeline_depth_max = self
            .prefill_pipeline_depth_max
            .max(other.prefill_pipeline_depth_max);
        for (current, next) in self.hops.iter_mut().zip(&other.hops) {
            current.prefill.merge(&next.prefill);
            current.decode.merge(&next.decode);
        }
    }
}

pub(super) fn percentile(samples: &[f64], percentile: u64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let index = ((sorted.len() - 1) * percentile as usize + 50) / 100;
    sorted[index]
}

/// Push the whole prompt through the pipeline, chunk by chunk.
pub(super) fn head_prefill_meta(
    session_id: &str,
    chunk_index: u32,
    chunk_size: u32,
    is_last: bool,
    prompt: Option<&str>,
    chat_template: bool,
    reuse_prefix: bool,
    start_pos: Option<u32>,
    trace_id: Option<&str>,
) -> serde_json::Value {
    let mut meta = json!({
        "session_id": session_id,
        "chunk_index": chunk_index,
        "chunk_size": chunk_size,
        "is_last": is_last,
        "is_final_chunk": false,
        "prompt": prompt,
        "chat_template": chat_template,
        "reuse_prefix": reuse_prefix,
        "start_pos": start_pos,
    });
    if let Some(trace_id) = trace_id {
        meta["trace_id"] = json!(trace_id);
    }
    meta
}

/// Codifica as imagens somente no primeiro quadro enviado à cabeça. O projector
/// é executado ali; os demais peers recebem apenas o estado oculto.
pub(super) fn add_images_to_head_meta(
    mut meta: serde_json::Value,
    images: Option<&[Vec<u8>]>,
) -> serde_json::Value {
    if let Some(images) = images.filter(|images| !images.is_empty()) {
        use base64::Engine as _;
        meta["images"] = json!(images
            .iter()
            .map(|image| base64::engine::general_purpose::STANDARD.encode(image))
            .collect::<Vec<_>>());
    }
    meta
}

/// Posições de prefill são absolutas na sessão, não são deriváveis do índice
/// lógico do pedaço. Um prompt curto pode terminar no token 42 e o pedaço 1
/// seguinte começa em 42, não em `1 * chunk_size`.
pub(super) fn required_prefill_position(
    meta: &serde_json::Value,
    field: &str,
    chunk_index: u32,
) -> Result<u32> {
    meta.get(field)
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .ok_or_else(|| {
            anyhow!("peer omitiu {field} no chunk {chunk_index}; posição absoluta obrigatória")
        })
}

/// No primeiro pedaço frio, zero é uma posição absoluta conhecida. Em todos
/// os outros casos, inventar a posição pelo índice pode misturar KV de
/// sessões diferentes e precisa abortar o caminho de reuso.
pub(super) fn observed_prefill_start(
    meta: &serde_json::Value,
    chunk_index: u32,
    reused_prefix: bool,
) -> Result<u32> {
    match meta.get("start_pos").and_then(|value| value.as_u64()) {
        Some(value) => Ok(value as u32),
        None if chunk_index == 0 && !reused_prefix => Ok(0),
        None => required_prefill_position(meta, "start_pos", chunk_index),
    }
}

/// Quantos tokens de prompt a cabeça informou ter aproveitado neste pedaço.
///
/// Só o pedaço 0 traz o campo, e só a cabeça o preenche. Peer que não o mande
/// (ou que tenha recomeçado a sessão) conta zero, que é a leitura conservadora
/// certa: telemetria de cache nunca deve inventar economia.
pub(super) fn cached_tokens_of(meta: &serde_json::Value) -> u32 {
    meta.get("cached_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as u32
}

/// A peer's measured prompt/generation frontier. When no frontier was measured
/// for this request, the full prompt count is the only safe retained length.
pub(super) fn retained_tokens_of(meta: &serde_json::Value, prompt_tokens: u32) -> u32 {
    meta.get("stable_len")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .unwrap_or(prompt_tokens)
}

/// Se a cabeça de fato aproveitou o prefixo. Ver `cached_tokens_of`.
pub(super) fn head_reused_prefix(meta: &serde_json::Value, requested: bool) -> bool {
    meta.get("reuse_prefix")
        .and_then(|value| value.as_bool())
        .unwrap_or(requested)
}

/// No modo de resposta direta, o ACK da cabeça carrega a progressão do
/// prompt dentro de `prompt_progress`: a resposta completa chega depois,
/// entregue pela cauda. O próximo chunk precisa ser despachado usando essa
/// posição publicada, mesmo quando os dois chunks ficam em voo ao mesmo
/// tempo.
pub(super) fn direct_prefill_progress(meta: &serde_json::Value) -> &serde_json::Value {
    meta.get("prompt_progress")
        .filter(|progress| progress.is_object())
        .unwrap_or(meta)
}
