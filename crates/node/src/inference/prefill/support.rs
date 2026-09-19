use super::*;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// Prompt tokens pushed through the pipeline per round trip.
///
/// The chunk is the unit of retry and the unit of memory, so it wants to be
/// small; it is also one round trip per peer, so it wants to be large. 512
/// tokens is 8 MiB of f16 hidden state per hop - about a second on the link
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

/// Próximo chunk que o gateway pode reenviar sem alterar o KV remoto. Só é
/// criado quando o transporte confirma que o corpo do chunk não foi entregue;
/// falhas ambíguas usam `PrefillRebuildRequired`.
#[derive(Debug, Clone)]
pub struct PrefillResumable {
    pub from_chunk: u32,
    pub start_pos: u32,
    pub prompt_tokens: u32,
    pub cached_tokens: u32,
    /// A sequência ordenada do relay direto precisa sobreviver à retomada.
    pub prefill_sequence_id: Option<String>,
    /// Trace parcial acumulado antes da falha. A retomada acrescenta as novas
    /// observações sem apagar os chunks que já chegaram ao peer.
    pub trace: Option<Box<RequestTrace>>,
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
    error.downcast_ref::<PrefillResumable>().cloned()
}

pub(super) fn resume_with_trace(
    error: anyhow::Error,
    trace: &RequestTrace,
    elapsed: Duration,
) -> anyhow::Error {
    let Some(mut resume) = resume_point(&error) else {
        return error;
    };
    let mut trace = trace.clone();
    trace.prefill_ms = trace.prefill_ms.saturating_add(elapsed.as_millis() as u64);
    resume.trace = Some(Box::new(trace));
    error.context(resume)
}

/// A sessão remota pode ter aceitado o chunk, então só é recuperável por uma
/// reconstrução fria. O motivo é estático para que os logs delimitem com
/// precisão por que o KV e o cache anterior não podem ser reaproveitados.
#[derive(Debug, Clone, Copy)]
pub struct PrefillRebuildRequired {
    pub from_chunk: u32,
    pub start_pos: u32,
    pub reason: &'static str,
}

impl std::fmt::Display for PrefillRebuildRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "retoma insegura do pedaço {} (posição {}): {}",
            self.from_chunk, self.start_pos, self.reason
        )
    }
}

pub fn rebuild_required(error: &anyhow::Error) -> Option<PrefillRebuildRequired> {
    error.downcast_ref::<PrefillRebuildRequired>().copied()
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
    /// Wall-clock do chamador: enfileirar o frame até a resposta chegar.
    /// **Inclui** o nó remoto rodando o handler de step/prefill.
    pub round_trip_ms: u64,
    /// `round_trip_ms` menos o `processing_ms` que o nó remoto reportou de si:
    /// o custo de transporte de fio, sem o compute do outro lado. É o número
    /// que o card de telemetria deve mostrar por perna - um `round_trip` cheio
    /// nunca chega perto do RTT de rede medido em 0,5 ms entre nós co-locados.
    pub network_ms: u64,
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

/// Quantis do ciclo observado pelo gateway entre solicitar uma rodada de
/// decode e entregar o primeiro token confirmado ao cliente. O reservatório é
/// usado apenas no modo normal para manter o custo limitado; os spans por
/// estágio continuam opt-in no profiling.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TokenCycleTelemetry {
    pub samples: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
    #[serde(skip)]
    distribution: RttDistribution,
}

impl TokenCycleTelemetry {
    pub(super) fn record(&mut self, value: u64) {
        self.distribution.record(value);
        self.refresh();
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.distribution.merge(&other.distribution);
        self.refresh();
    }

    fn refresh(&mut self) {
        self.samples = self.distribution.seen;
        self.p50_ms = self.distribution.percentile(50);
        self.p95_ms = self.distribution.percentile(95);
        self.p99_ms = self.distribution.percentile(99);
        self.max_ms = self.distribution.max_ms;
    }

    pub fn is_empty(&self) -> bool {
        self.samples == 0
    }
}

/// Agregados baratos do caminho de callbacks. Os quantis usam o mesmo
/// reservoir limitado das travessias comuns, portanto não há vetor por token
/// no modo normal.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RemoteStreamTelemetry {
    pub open_ms: Option<u64>,
    pub lifetime_ms: Option<u64>,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub first_frame_latency_ms: Option<u64>,
    pub frame_latency_min_ms: u64,
    pub frame_latency_p50_ms: u64,
    pub frame_latency_p95_ms: u64,
    pub frame_latency_p99_ms: u64,
    pub frame_latency_max_ms: u64,
    pub remote_compute_total_ms: u64,
    pub remote_compute_min_ms: u64,
    pub remote_compute_p50_ms: u64,
    pub remote_compute_p95_ms: u64,
    pub remote_compute_p99_ms: u64,
    pub remote_compute_max_ms: u64,
    pub remote_stream_wait_total_ms: u64,
    pub remote_stream_wait_p50_ms: u64,
    pub remote_stream_wait_p95_ms: u64,
    /// Espera entre o início do ciclo e o token ficar pronto no produtor.
    /// Mede compute/produção ainda em andamento, sem chamá-la de fila.
    pub remote_stream_producer_wait_total_ms: u64,
    pub remote_stream_producer_wait_p50_ms: u64,
    pub remote_stream_producer_wait_p95_ms: u64,
    /// Tempo em que o produtor aguardou um permit do canal limitado depois
    /// de concluir a produção do token. Zero prova que não houve
    /// contrapressão do `Sender` nesta execução.
    pub remote_stream_channel_send_wait_total_ms: u64,
    pub remote_stream_channel_send_wait_p50_ms: u64,
    pub remote_stream_channel_send_wait_p95_ms: u64,
    /// Wall time spent in the producer's nested hop before its token became
    /// ready. It includes downstream compute and transport, so it is the
    /// producer's legitimate cycle time rather than local queueing.
    pub remote_stream_downstream_wait_total_ms: u64,
    pub remote_stream_downstream_wait_p50_ms: u64,
    pub remote_stream_downstream_wait_p95_ms: u64,
    /// Tempo em que o token já pronto permaneceu no canal antes do emitter.
    /// Pode incluir backpressure quando o canal limitado está cheio.
    pub remote_stream_queue_wait_total_ms: u64,
    pub remote_stream_queue_wait_p50_ms: u64,
    pub remote_stream_queue_wait_p95_ms: u64,
    /// Time spent awaiting the callback delivery that preceded this batch.
    /// The first batch is zero and the final callback has no following batch
    /// to carry its duration.
    pub remote_stream_delivery_total_ms: u64,
    pub remote_stream_delivery_p50_ms: u64,
    pub remote_stream_delivery_p95_ms: u64,
    /// Intervalo observado no gateway entre lotes de tokens recebidos. Em um
    /// lote de um token, é o ciclo de entrega por token; lotes maiores são
    /// contados como um ciclo, sem inventar intervalos internos.
    pub token_delivery_interval_min_ms: u64,
    pub token_delivery_interval_p50_ms: u64,
    pub token_delivery_interval_p95_ms: u64,
    pub token_delivery_interval_p99_ms: u64,
    pub token_delivery_interval_max_ms: u64,
    pub token_delivery_interval_samples: u64,
    #[serde(skip)]
    frame_latency: RttDistribution,
    #[serde(skip)]
    remote_compute: RttDistribution,
    #[serde(skip)]
    wait: RttDistribution,
    #[serde(skip)]
    producer_wait: RttDistribution,
    #[serde(skip)]
    channel_send_wait: RttDistribution,
    #[serde(skip)]
    downstream_wait: RttDistribution,
    #[serde(skip)]
    queue_wait: RttDistribution,
    #[serde(skip)]
    delivery: RttDistribution,
    #[serde(skip)]
    token_delivery_interval: RttDistribution,
}

impl RemoteStreamTelemetry {
    pub(super) fn open(&mut self, duration_ms: u64) {
        self.open_ms = Some(duration_ms);
    }

    pub(super) fn record_batch(&mut self, meta: &serde_json::Value) {
        let bytes = meta
            .get("stream_batch_bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        self.frames_received = self.frames_received.saturating_add(1);
        self.frames_sent = self.frames_sent.max(
            meta.get("remote_stream_frames_sent")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(self.frames_received),
        );
        self.bytes_received = self.bytes_received.saturating_add(bytes);
        self.bytes_sent = self.bytes_sent.saturating_add(bytes);
        if let Some(lifetime) = meta
            .get("remote_stream_lifetime_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.lifetime_ms = Some(self.lifetime_ms.unwrap_or(0).max(lifetime));
        }
        if let Some(value) = meta
            .get("remote_stream_first_frame_latency_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.first_frame_latency_ms.get_or_insert(value);
        }
        if let Some(value) = meta
            .get("remote_stream_frame_latency_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.frame_latency.record(value);
        }
        let compute = meta
            .get("remote_stream_remote_compute_ms")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                meta.get("processing_ms")
                    .and_then(serde_json::Value::as_u64)
            })
            .unwrap_or(0);
        self.remote_compute_total_ms = self.remote_compute_total_ms.saturating_add(compute);
        self.remote_compute.record(compute);
        if let Some(value) = meta
            .get("remote_stream_wait_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_wait_total_ms =
                self.remote_stream_wait_total_ms.saturating_add(value);
            self.wait.record(value);
        }
        if let Some(value) = meta
            .get("remote_stream_producer_wait_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_producer_wait_total_ms = self
                .remote_stream_producer_wait_total_ms
                .saturating_add(value);
            self.producer_wait.record(value);
        }
        if let Some(value) = meta
            .get("remote_stream_channel_send_wait_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_channel_send_wait_total_ms = self
                .remote_stream_channel_send_wait_total_ms
                .saturating_add(value);
            self.channel_send_wait.record(value);
        }
        if let Some(value) = meta
            .get("remote_stream_downstream_wait_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_downstream_wait_total_ms = self
                .remote_stream_downstream_wait_total_ms
                .saturating_add(value);
            self.downstream_wait.record(value);
        }
        if let Some(value) = meta
            .get("remote_stream_queue_wait_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_queue_wait_total_ms =
                self.remote_stream_queue_wait_total_ms.saturating_add(value);
            self.queue_wait.record(value);
        }
        if let Some(value) = meta
            .get("remote_stream_delivery_ms")
            .and_then(serde_json::Value::as_u64)
        {
            self.remote_stream_delivery_total_ms =
                self.remote_stream_delivery_total_ms.saturating_add(value);
            self.delivery.record(value);
        }
        self.refresh_percentiles();
    }

    pub(super) fn record_token_delivery_interval(&mut self, value: u64) {
        self.token_delivery_interval.record(value);
        self.refresh_percentiles();
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.open_ms = self.open_ms.or(other.open_ms);
        self.lifetime_ms = match (self.lifetime_ms, other.lifetime_ms) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (current, next) => current.or(next),
        };
        self.frames_sent = self.frames_sent.max(other.frames_sent);
        self.frames_received = self.frames_received.saturating_add(other.frames_received);
        self.bytes_sent = self.bytes_sent.saturating_add(other.bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(other.bytes_received);
        self.first_frame_latency_ms = self.first_frame_latency_ms.or(other.first_frame_latency_ms);
        self.remote_compute_total_ms = self
            .remote_compute_total_ms
            .saturating_add(other.remote_compute_total_ms);
        self.remote_stream_wait_total_ms = self
            .remote_stream_wait_total_ms
            .saturating_add(other.remote_stream_wait_total_ms);
        self.remote_stream_producer_wait_total_ms = self
            .remote_stream_producer_wait_total_ms
            .saturating_add(other.remote_stream_producer_wait_total_ms);
        self.remote_stream_channel_send_wait_total_ms = self
            .remote_stream_channel_send_wait_total_ms
            .saturating_add(other.remote_stream_channel_send_wait_total_ms);
        self.remote_stream_downstream_wait_total_ms = self
            .remote_stream_downstream_wait_total_ms
            .saturating_add(other.remote_stream_downstream_wait_total_ms);
        self.remote_stream_queue_wait_total_ms = self
            .remote_stream_queue_wait_total_ms
            .saturating_add(other.remote_stream_queue_wait_total_ms);
        self.remote_stream_delivery_total_ms = self
            .remote_stream_delivery_total_ms
            .saturating_add(other.remote_stream_delivery_total_ms);
        self.frame_latency.merge(&other.frame_latency);
        self.remote_compute.merge(&other.remote_compute);
        self.wait.merge(&other.wait);
        self.producer_wait.merge(&other.producer_wait);
        self.channel_send_wait.merge(&other.channel_send_wait);
        self.queue_wait.merge(&other.queue_wait);
        self.token_delivery_interval
            .merge(&other.token_delivery_interval);
        self.refresh_percentiles();
    }

    fn refresh_percentiles(&mut self) {
        self.frame_latency_min_ms = self.frame_latency.min_ms;
        self.frame_latency_p50_ms = self.frame_latency.percentile(50);
        self.frame_latency_p95_ms = self.frame_latency.percentile(95);
        self.frame_latency_p99_ms = self.frame_latency.percentile(99);
        self.frame_latency_max_ms = self.frame_latency.max_ms;
        self.remote_compute_min_ms = self.remote_compute.min_ms;
        self.remote_compute_p50_ms = self.remote_compute.percentile(50);
        self.remote_compute_p95_ms = self.remote_compute.percentile(95);
        self.remote_compute_p99_ms = self.remote_compute.percentile(99);
        self.remote_compute_max_ms = self.remote_compute.max_ms;
        self.remote_stream_wait_p50_ms = self.wait.percentile(50);
        self.remote_stream_wait_p95_ms = self.wait.percentile(95);
        self.remote_stream_producer_wait_p50_ms = self.producer_wait.percentile(50);
        self.remote_stream_producer_wait_p95_ms = self.producer_wait.percentile(95);
        self.remote_stream_channel_send_wait_p50_ms = self.channel_send_wait.percentile(50);
        self.remote_stream_channel_send_wait_p95_ms = self.channel_send_wait.percentile(95);
        self.remote_stream_downstream_wait_p50_ms = self.downstream_wait.percentile(50);
        self.remote_stream_downstream_wait_p95_ms = self.downstream_wait.percentile(95);
        self.remote_stream_queue_wait_p50_ms = self.queue_wait.percentile(50);
        self.remote_stream_queue_wait_p95_ms = self.queue_wait.percentile(95);
        self.remote_stream_delivery_p50_ms = self.delivery.percentile(50);
        self.remote_stream_delivery_p95_ms = self.delivery.percentile(95);
        self.token_delivery_interval_min_ms = self.token_delivery_interval.min_ms;
        self.token_delivery_interval_p50_ms = self.token_delivery_interval.percentile(50);
        self.token_delivery_interval_p95_ms = self.token_delivery_interval.percentile(95);
        self.token_delivery_interval_p99_ms = self.token_delivery_interval.percentile(99);
        self.token_delivery_interval_max_ms = self.token_delivery_interval.max_ms;
        self.token_delivery_interval_samples = self.token_delivery_interval.seen;
    }
}

/// Span detalhado opt-in. Os timestamps são relativos ao domínio monotônico
/// do peer que os produziu, nunca diferenças entre relógios de máquinas.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct TelemetrySpan {
    pub trace_id: String,
    pub request_id: String,
    pub pipeline_id: String,
    pub peer_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    /// Nomeia a fronteira observada. A duração só é interpretável junto com
    /// este nome: eventos como `client-emit` podem ter duração zero, enquanto
    /// `transport-round-trip` cobre uma chamada completa.
    pub name: String,
    pub phase: Phase,
    pub token_index: Option<u64>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
    pub transport: Option<String>,
    pub clock_domain: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TelemetrySummary {
    pub status: String,
    /// A resposta pode ter sido liberada enquanto os recibos remotos ainda
    /// estão no caminho. Isso separa o fim da geração do fim da coleta.
    pub response_complete: bool,
    pub expected_peers: u32,
    pub reported_peers: u32,
    pub missing_peers: Vec<String>,
    pub expected_decode_spans: u64,
    pub reported_decode_spans: u64,
    /// False when direct per-frame mode has observations but no expected
    /// total. The frontend must not render that state as `N/0 · 100%`.
    pub decode_span_expectation_available: bool,
    pub completeness_percent: f64,
    pub duplicate_spans: u64,
    pub late_spans: u64,
    pub orphan_spans: u64,
    pub wall_clock_ms: Option<u64>,
    pub critical_path_ms: Option<u64>,
    pub accumulated_compute_ms: u64,
    pub accumulated_transport_ms: u64,
    pub overlap_ms: Option<u64>,
    pub true_residual_ms: Option<u64>,
    pub remote_stream: RemoteStreamTelemetry,
}

impl Default for TelemetrySummary {
    fn default() -> Self {
        Self {
            status: "collecting".into(),
            response_complete: false,
            expected_peers: 0,
            reported_peers: 0,
            missing_peers: Vec::new(),
            expected_decode_spans: 0,
            reported_decode_spans: 0,
            decode_span_expectation_available: false,
            completeness_percent: 0.0,
            duplicate_spans: 0,
            late_spans: 0,
            orphan_spans: 0,
            wall_clock_ms: None,
            critical_path_ms: None,
            accumulated_compute_ms: 0,
            accumulated_transport_ms: 0,
            overlap_ms: None,
            true_residual_ms: None,
            remote_stream: RemoteStreamTelemetry::default(),
        }
    }
}

pub(super) fn profile_sync_enabled() -> bool {
    // The fleet already enables detailed backend profiling with this flag.
    // Reuse that single policy instead of creating a second environment
    // variable for the same profiling mode. The backend's own default remains
    // independent: spans are opt-in so normal inference keeps aggregates only.
    std::env::var("POM_PROFILE_SYNC")
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
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
    /// RTT cheio, incluindo o compute do nó remoto. Mantido para o total de
    /// RTT de execução e compatibilidade com o front antigo.
    pub rtt_min_ms: u64,
    pub rtt_median_ms: u64,
    pub rtt_p95_ms: u64,
    pub rtt_max_ms: u64,
    /// RTT só de transporte: `round_trip - processing_ms` remoto. É o que a
    /// perna do card deve exibir; sem ele um link QUIC entre nós co-locados
    /// aparecia com ~76 ms de "RTT" que era compute do outro lado.
    pub net_rtt_min_ms: u64,
    pub net_rtt_median_ms: u64,
    pub net_rtt_p95_ms: u64,
    pub net_rtt_max_ms: u64,
    pub fallback_calls: u32,
    #[serde(skip)]
    pub(super) rtt: RttDistribution,
    #[serde(skip)]
    pub(super) net_rtt: RttDistribution,
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
            net_rtt_min_ms: 0,
            net_rtt_median_ms: 0,
            net_rtt_p95_ms: 0,
            net_rtt_max_ms: 0,
            fallback_calls: 0,
            rtt: RttDistribution::default(),
            net_rtt: RttDistribution::default(),
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
        self.net_rtt.record(trace.network_ms);
        self.refresh_percentiles();
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.bytes_sent = self.bytes_sent.saturating_add(other.bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(other.bytes_received);
        self.fallback_calls = self.fallback_calls.saturating_add(other.fallback_calls);
        self.rtt.merge(&other.rtt);
        self.net_rtt.merge(&other.net_rtt);
        self.refresh_percentiles();
    }

    pub(super) fn refresh_percentiles(&mut self) {
        self.rtt_min_ms = self.rtt.min_ms;
        self.rtt_median_ms = self.rtt.percentile(50);
        self.rtt_p95_ms = self.rtt.percentile(95);
        self.rtt_max_ms = self.rtt.max_ms;
        self.net_rtt_min_ms = self.net_rtt.min_ms;
        self.net_rtt_median_ms = self.net_rtt.percentile(50);
        self.net_rtt_p95_ms = self.net_rtt.percentile(95);
        self.net_rtt_max_ms = self.net_rtt.max_ms;
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

/// Uma perna observada do ciclo lógico do pipeline.
///
/// `from_node_id -> to_node_id` é a direção da travessia que foi observada no
/// executor. O valor continua sendo a duração da operação de transporte
/// observada (a distribuição antiga de RTT), não uma metade inferida de um
/// relógio de ida e volta. O mapa usa cada perna uma vez, portanto não soma um
/// retorno inexistente entre peers de avanço.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PipelineCycleLeg {
    pub from_node_id: String,
    pub to_node_id: String,
    pub transport_summary: TransportSummary,
}

impl PipelineCycleLeg {
    fn new(from_node_id: &str, to_node_id: &str, trace: TransportTrace) -> Self {
        let mut transport_summary = TransportSummary::default();
        transport_summary.record(trace);
        Self {
            from_node_id: from_node_id.to_owned(),
            to_node_id: to_node_id.to_owned(),
            transport_summary,
        }
    }

    fn record(&mut self, trace: TransportTrace) {
        self.transport_summary.record(trace);
    }

    fn merge(&mut self, other: &Self) {
        self.transport_summary.merge(&other.transport_summary);
    }
}

/// Decomposição agregada do ciclo, separada por workload para que prefill não
/// altere o RTT mostrado para decode. No modo normal guarda apenas contadores,
/// histogramas e uma entrada por perna, sem span por token.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PipelineCycleTelemetry {
    pub prefill: Vec<PipelineCycleLeg>,
    pub decode: Vec<PipelineCycleLeg>,
}

impl PipelineCycleTelemetry {
    fn legs_mut(&mut self, phase: Phase) -> &mut Vec<PipelineCycleLeg> {
        match phase {
            Phase::Prefill => &mut self.prefill,
            Phase::Decode => &mut self.decode,
        }
    }

    fn record(
        &mut self,
        phase: Phase,
        from_node_id: &str,
        to_node_id: &str,
        trace: TransportTrace,
    ) {
        let legs = self.legs_mut(phase);
        if let Some(leg) = legs
            .iter_mut()
            .find(|leg| leg.from_node_id == from_node_id && leg.to_node_id == to_node_id)
        {
            leg.record(trace);
        } else {
            legs.push(PipelineCycleLeg::new(from_node_id, to_node_id, trace));
        }
    }

    fn merge(&mut self, other: &Self) {
        for (current, next) in [&mut self.prefill, &mut self.decode]
            .into_iter()
            .zip([&other.prefill, &other.decode])
        {
            for leg in next {
                if let Some(existing) = current.iter_mut().find(|candidate| {
                    candidate.from_node_id == leg.from_node_id
                        && candidate.to_node_id == leg.to_node_id
                }) {
                    existing.merge(leg);
                } else {
                    current.push(leg.clone());
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.prefill.is_empty() && self.decode.is_empty()
    }
}

impl TransportTrace {
    fn measured_network_ms(transport_rtt_us: Option<u64>) -> Option<u64> {
        transport_rtt_us.map(|microseconds| microseconds.saturating_add(999) / 1_000)
    }

    pub(super) fn from_sent(sent: &crate::relay::SentFrame) -> Self {
        let processing_ms = sent
            .meta
            .get("processing_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        Self {
            transport: sent.telemetry_transport(),
            fallback: sent.transport_fallback,
            round_trip_ms: sent.round_trip_ms,
            network_ms: Self::measured_network_ms(sent.transport_rtt_us)
                .unwrap_or_else(|| sent.round_trip_ms.saturating_sub(processing_ms)),
            bytes_sent: sent.bytes_sent,
            bytes_received: sent.bytes_received,
        }
    }

    pub(super) fn from_json(entry: &serde_json::Value) -> Self {
        let get = |key: &str| entry.get(key).and_then(|value| value.as_u64()).unwrap_or(0);
        let round_trip_ms = get("round_trip_ms");
        let processing_ms = get("processing_ms");
        let measured_network_ms = Self::measured_network_ms(
            entry
                .get("transport_rtt_us")
                .and_then(|value| value.as_u64()),
        );
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
            round_trip_ms,
            network_ms: measured_network_ms
                .unwrap_or_else(|| round_trip_ms.saturating_sub(processing_ms)),
            bytes_sent: get("bytes_sent"),
            bytes_received: get("bytes_received"),
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
    /// Quantos eventos de decode foram representados pelos agregados desta
    /// fase. Em chamadas comuns vale um; um lote remote-stream pode carregar
    /// vários tokens sem transformar o contador de chamadas em contador de
    /// tokens.
    pub reported_spans: u64,
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
    /// Subconjunto da fila que ficou aguardando seleção/admissão do scheduler.
    /// Normalmente coincide com `node_queue_ms`; a separação existe para
    /// nomear a carência de reclamação de sessões retidas sem subtrair tempos.
    pub node_scheduler_wait_ms: f64,
    /// Motivos observados para a espera do scheduler nesta fase.
    pub scheduler_wait_reasons: Vec<String>,
    /// Espera pelo mutex do motor no nó: o preço de outra requisição estar
    /// decodificando ali. É o que separa "o nó é lento" de "o nó está ocupado".
    pub node_lock_wait_ms: f64,
    pub node_setup_ms: f64,
    pub node_decode_ms: f64,
    /// Tempo medido do bloco `nextn`, fora de `node_decode_ms` e antes da
    /// serialização da resposta. Quando presente, explica a produção MTP que
    /// um ciclo de remote-stream aguarda legitimamente.
    pub node_mtp_draft_ms: f64,
    /// Time spent in the backend evaluation API call. This is neither queue
    /// wait nor a guarantee of pure device-kernel time.
    pub node_backend_call_ms: f64,
    /// Queued backend work plus already-queued output transfer, measured with
    /// `llama_synchronize`. A zero value means no synchronization duration was
    /// recorded; the effective `POM_PROFILE_SYNC` state is published separately
    /// by `/health` and must never be inferred from this timing.
    pub node_backend_sync_ms: f64,
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
        let node_ms = |key: &str| {
            node.get(key)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
        };
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
        self.reported_spans = self.reported_spans.saturating_add(1);
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
        self.node_scheduler_wait_ms += node_ms("scheduler_wait_ms");
        if let Some(reason) = node.get("scheduler_wait_reason").and_then(Value::as_str) {
            if !self
                .scheduler_wait_reasons
                .iter()
                .any(|seen| seen == reason)
            {
                self.scheduler_wait_reasons.push(reason.to_owned());
            }
        }
        self.node_lock_wait_ms += node_ms("lock_wait_ms");
        self.node_setup_ms += node_ms("setup_ms");
        self.node_decode_ms += node_decode_ms;
        self.node_mtp_draft_ms += node_ms("mtp_draft_ms");
        self.node_backend_call_ms += node_ms("backend_call_ms");
        self.node_backend_sync_ms += node_ms("backend_sync_ms");
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
        self.reported_spans = self.reported_spans.saturating_add(next.reported_spans);
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
        self.node_scheduler_wait_ms += next.node_scheduler_wait_ms;
        for reason in &next.scheduler_wait_reasons {
            if !self
                .scheduler_wait_reasons
                .iter()
                .any(|seen| seen == reason)
            {
                self.scheduler_wait_reasons.push(reason.clone());
            }
        }
        self.node_lock_wait_ms += next.node_lock_wait_ms;
        self.node_setup_ms += next.node_setup_ms;
        self.node_decode_ms += next.node_decode_ms;
        self.node_mtp_draft_ms += next.node_mtp_draft_ms;
        self.node_backend_call_ms += next.node_backend_call_ms;
        self.node_backend_sync_ms += next.node_backend_sync_ms;
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

#[derive(Debug, Clone)]
struct PendingDirectReceipt {
    inbox: crate::direct::ReplyInbox,
    ticket_id: String,
    phase: Phase,
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
    /// Quantas travessias terminaram em HTTPS depois que um transporte
    /// persistente/túnel falhou. Não é inferido pelo nome do transporte: só é
    /// incrementado quando a camada de transporte carrega a razão explícita.
    pub https_fallback_calls: u64,
    /// Razões limitadas, preservadas para explicar por que o caminho normal
    /// não foi usado. Uma entrada pode aparecer mais de uma vez se a mesma
    /// aresta cair em vários tokens.
    pub https_fallback_reasons: Vec<String>,
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
    /// Estado da própria coleta. `wall_clock_ms` é uma duração medida pelo
    /// gateway; campos de overlap/residual só são preenchidos quando há
    /// intervalos temporais explícitos, nunca por subtração de somatórios.
    pub telemetry: TelemetrySummary,
    /// Spans de frame e eventos por token somente quando
    /// `POM_PROFILE_SYNC=1` está ativo. Um evento de token tem duração zero
    /// quando o lote não traz intervalos individuais, em vez de repetir o
    /// custo do lote como se cada token tivesse sido medido separadamente.
    pub profiling_spans: Option<Vec<TelemetrySpan>>,
    /// Distribuição barata do ciclo de uma rodada até a entrega no cliente.
    /// Diferente de `profiling_spans`, existe também no modo normal para
    /// revelar p95/p99 e propagação de jitter sem alocar um span por token.
    #[serde(skip_serializing_if = "TokenCycleTelemetry::is_empty")]
    pub token_cycle: TokenCycleTelemetry,
    /// Pernas direcionais do ciclo lógico observadas no caminho direto. Inclui
    /// a entrada do gateway, as transições de avanço e o retorno final quando
    /// o recibo da entrega chegou ao gateway.
    #[serde(skip_serializing_if = "PipelineCycleTelemetry::is_empty")]
    pub pipeline_cycle: PipelineCycleTelemetry,
    /// Tokens draft que o caminho MTP propôs nesta requisição, somados entre
    /// todos os hops e steps.
    pub mtp_draft_tokens_proposed: u64,
    /// Tokens draft que passaram no critério de aceitação do MTP.
    pub mtp_draft_tokens_accepted: u64,
    /// Tokens draft que foram verificados mas não foram aceitos pelo MTP.
    pub mtp_draft_tokens_rejected: u64,
    /// `accepted / proposed`; nulo quando a requisição não propôs tokens MTP.
    pub mtp_acceptance_rate: Option<f64>,
    #[serde(skip)]
    expected_decode_spans: u64,
    #[serde(skip)]
    pending_direct_receipts: Vec<PendingDirectReceipt>,
    #[serde(skip)]
    direct_transport_receipt_ids: HashSet<String>,
    #[serde(skip)]
    remote_stream_frame_ids: HashSet<String>,
    #[serde(skip)]
    profiling_span_ids: HashSet<String>,
    #[serde(skip)]
    profiling_token_index: u64,
    #[serde(skip)]
    profiling_token_indices_absolute: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Prefill,
    Decode,
}

/// Contexto de uma amostra detalhada. O contexto só existe quando o modo de
/// profiling foi explicitamente pedido e há índice de token para correlacionar
/// os peers. `Instant` nunca atravessa a wire: cada span publica offsets no
/// domínio monotônico local, preservando a impossibilidade de subtrair relógios
/// de máquinas diferentes.
#[derive(Debug, Clone)]
pub struct TelemetryContext {
    pub trace_id: String,
    pub request_id: String,
    pub pipeline_id: String,
    pub peer_id: String,
    pub phase: Phase,
    pub token_index: u64,
    pub origin: Instant,
    pub transport: Option<String>,
}

impl TelemetryContext {
    pub fn from_meta(
        meta: &serde_json::Value,
        peer_id: impl Into<String>,
        phase: Phase,
        origin: Instant,
        transport: Option<String>,
    ) -> Option<Self> {
        if !profile_sync_enabled() {
            return None;
        }
        Some(Self {
            trace_id: meta
                .get("trace_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            request_id: meta
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| meta.get("session_id").and_then(serde_json::Value::as_str))
                .unwrap_or("unknown")
                .to_owned(),
            pipeline_id: meta
                .get("pipeline_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            peer_id: peer_id.into(),
            phase,
            token_index: meta
                .get("telemetry_token_index")
                .and_then(serde_json::Value::as_u64)?,
            origin,
            transport,
        })
    }

    pub fn span(
        &self,
        name: &str,
        start: Instant,
        end: Instant,
        parent_span_id: Option<String>,
        transport: Option<String>,
    ) -> TelemetrySpan {
        let start_ns = start
            .saturating_duration_since(self.origin)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let end_ns = end
            .saturating_duration_since(self.origin)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let duration_ns = end
            .saturating_duration_since(start)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        TelemetrySpan {
            trace_id: self.trace_id.clone(),
            request_id: self.request_id.clone(),
            pipeline_id: self.pipeline_id.clone(),
            peer_id: self.peer_id.clone(),
            span_id: format!(
                "{}:{}:{}:{}",
                self.peer_id, self.token_index, name, start_ns
            ),
            parent_span_id,
            name: name.to_owned(),
            phase: self.phase,
            token_index: Some(self.token_index),
            start_ns,
            end_ns,
            duration_ns,
            transport: transport.or_else(|| self.transport.clone()),
            clock_domain: "peer-monotonic".into(),
        }
    }
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
const TELEMETRY_DEDUPE_WINDOW: usize = 1_024;

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
    #[cfg(test)]
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
            https_fallback_calls: 0,
            https_fallback_reasons: Vec::new(),
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
            telemetry: TelemetrySummary::default(),
            profiling_spans: profile_sync_enabled().then(Vec::new),
            token_cycle: TokenCycleTelemetry::default(),
            pipeline_cycle: PipelineCycleTelemetry::default(),
            mtp_draft_tokens_proposed: 0,
            mtp_draft_tokens_accepted: 0,
            mtp_draft_tokens_rejected: 0,
            mtp_acceptance_rate: None,
            expected_decode_spans: 0,
            pending_direct_receipts: Vec::new(),
            direct_transport_receipt_ids: HashSet::new(),
            remote_stream_frame_ids: HashSet::new(),
            profiling_span_ids: HashSet::new(),
            profiling_token_index: 0,
            profiling_token_indices_absolute: false,
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

    #[cfg(test)]
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

    pub(super) fn track_direct_receipt(
        &mut self,
        inbox: crate::direct::ReplyInbox,
        ticket_id: impl Into<String>,
        phase: Phase,
    ) {
        let ticket_id = ticket_id.into();
        if !self
            .pending_direct_receipts
            .iter()
            .any(|pending| pending.ticket_id == ticket_id && pending.phase == phase)
        {
            self.pending_direct_receipts.push(PendingDirectReceipt {
                inbox,
                ticket_id,
                phase,
            });
        }
    }

    pub(super) fn has_pending_direct_receipt(&self) -> bool {
        !self.pending_direct_receipts.is_empty()
    }

    pub(super) fn mark_direct_transport_receipt(
        &mut self,
        ticket_id: &str,
        phase: Phase,
        hop_index: usize,
    ) -> bool {
        self.direct_transport_receipt_ids
            .insert(format!("{phase:?}:{ticket_id}:{hop_index}"))
    }

    /// Reconcile direct transport receipts after inference completes. The
    /// gateway returns the result immediately; some independent transport
    /// callbacks can arrive later, while the local inbox retains their trace
    /// snapshot. This reads that snapshot without adding a wait to token flow.
    pub(crate) fn reconcile_direct_receipts(&mut self) {
        let pending = std::mem::take(&mut self.pending_direct_receipts);
        for pending in pending {
            let Some(snapshot) = pending.inbox.trace_snapshot(&pending.ticket_id) else {
                self.transport_trace_complete = false;
                continue;
            };
            let traversals = snapshot
                .get("transport_traversals")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for traversal in &traversals {
                let Some(destination) = traversal
                    .get("hop_index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|index| index as usize)
                else {
                    self.transport_trace_complete = false;
                    continue;
                };
                let receipt_id = format!("{:?}:{}:{destination}", pending.phase, pending.ticket_id);
                if !self.direct_transport_receipt_ids.insert(receipt_id) {
                    continue;
                }
                let Some(to_node_id) = self.hops.get(destination).map(|hop| hop.node_id.clone())
                else {
                    self.transport_trace_complete = false;
                    continue;
                };
                let from_node_id = if destination == 0 {
                    "gateway".to_owned()
                } else if let Some(hop) = self.hops.get(destination.saturating_sub(1)) {
                    hop.node_id.clone()
                } else {
                    self.transport_trace_complete = false;
                    continue;
                };
                let mut receipt = traversal.clone();
                let processing_ms = self
                    .hops
                    .get(destination)
                    .map(|hop| match pending.phase {
                        Phase::Prefill => hop.prefill.processing_ms,
                        Phase::Decode => hop.decode.processing_ms,
                    })
                    .unwrap_or(0);
                if receipt
                    .get("processing_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    == 0
                    && processing_ms > 0
                {
                    receipt["processing_ms"] = json!(processing_ms);
                }
                let transport = TransportTrace::from_json(&receipt);
                self.record_transport(destination, pending.phase, transport.clone());
                self.record_cycle_transport(
                    pending.phase,
                    &from_node_id,
                    &to_node_id,
                    transport.clone(),
                );
                if let Some(measured_network_ms) = traversal
                    .get("transport_rtt_us")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|rtt_us| TransportTrace::measured_network_ms(Some(rtt_us)))
                {
                    if let Some(hop) = self.hops.get_mut(destination) {
                        let phase = match pending.phase {
                            Phase::Prefill => &mut hop.prefill,
                            Phase::Decode => &mut hop.decode,
                        };
                        phase.network_ms = phase
                            .network_ms
                            .saturating_sub(transport.round_trip_ms.saturating_sub(processing_ms))
                            .saturating_add(measured_network_ms);
                    }
                }
            }
            let complete = snapshot
                .get("transport_trace_complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && traversals.len() == self.hops.len();
            self.transport_trace_complete &= complete;
        }
    }

    #[cfg(test)]
    pub fn set_outside_node_total_ms(&mut self, total_ms: u64) {
        self.outside_node_total_ms = Some(total_ms);
        self.refresh_outside_node_accounting();
    }

    pub fn set_generated_tokens(&mut self, tokens: u32) {
        self.generated_tokens = tokens;
        self.refresh_decode_counters();
    }

    /// `decode_calls` é **derivado dos saltos**, não um contador próprio.
    ///
    /// O laço de continuação do gateway funde o traço de cada perna no traço
    /// base (`merge`), e a fusão sempre soube juntar `PhaseTrace::calls` mas
    /// nunca carregou um contador solto: o resumo dizia "não medido" enquanto a
    /// tabela por salto mostrava 111 chamadas na mesma execução. Derivar remove
    /// a chance de as duas divergirem outra vez.
    ///
    /// É o **máximo** entre os saltos, não a soma: uma rodada de decode toca
    /// cada salto uma vez, então somar multiplicaria as rodadas pelo tamanho do
    /// pipeline e `tokens_per_decode_call` - o rendimento da especulação -
    /// cairia por esse mesmo fator.
    pub(super) fn refresh_decode_counters(&mut self) {
        self.decode_calls = self
            .hops
            .iter()
            .map(|hop| hop.decode.calls as u64)
            .max()
            .unwrap_or(0);
        self.tokens_per_decode_call = (self.decode_calls > 0)
            .then(|| self.generated_tokens as f64 / self.decode_calls as f64);
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
            self.refresh_decode_counters();
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
        self.record_profiling_spans_from_json(node);
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
            sent.meta
                .get("node_timings")
                .unwrap_or(&serde_json::Value::Null),
        );
        if let Some(measured_network_ms) =
            TransportTrace::measured_network_ms(sent.transport_rtt_us)
        {
            if let Some(entry) = self.hops.get_mut(hop) {
                let slot = match phase {
                    Phase::Prefill => &mut entry.prefill,
                    Phase::Decode => &mut entry.decode,
                };
                slot.network_ms = slot
                    .network_ms
                    .saturating_sub(sent.round_trip_ms.saturating_sub(processing_ms))
                    .saturating_add(measured_network_ms);
            }
        }
        self.record_hidden_state(
            hop,
            phase,
            sent.hidden_state_bytes_sent,
            sent.hidden_state_bytes_received,
            Some(sent.hidden_state_dtype),
        );
        self.record_connection_state(hop, phase, sent.connection_state);
        self.record_https_fallback(
            sent.transport_fallback,
            sent.transport_fallback_reason.as_deref(),
        );
        for span in &sent.profiling_spans {
            if let Ok(value) = serde_json::to_value(span) {
                self.record_profiling_spans_from_json(&json!({
                    "profiling_spans": [value]
                }));
            }
        }
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

    /// Registra uma travessia no ciclo em sua direção real. A amostra é
    /// agregada por aresta e transporte, sem alocar um span por token no modo
    /// normal.
    pub(super) fn record_cycle_transport(
        &mut self,
        phase: Phase,
        from_node_id: &str,
        to_node_id: &str,
        transport: TransportTrace,
    ) {
        self.pipeline_cycle
            .record(phase, from_node_id, to_node_id, transport);
    }

    /// Registra a última perna, que não pertence a nenhum `HopTrace`: a cauda
    /// entrega o resultado ao gateway por uma operação separada.
    pub(super) fn record_cycle_return(
        &mut self,
        phase: Phase,
        from_node_id: &str,
        transport: TransportTrace,
    ) {
        self.record_cycle_transport(phase, from_node_id, "gateway", transport);
    }

    /// Registra a travessia de controle que abriu um `remote-stream` sem
    /// contá-la como decode. O ACK confirma a abertura do stream, mas não é
    /// um span de token e não pode inflar a completude observada.
    pub(super) fn record_control_transport(
        &mut self,
        hop: usize,
        phase: Phase,
        transport: TransportTrace,
        bytes_sent: u64,
        bytes_received: u64,
    ) {
        self.record_transport(hop, phase, transport);
        self.total_bytes_sent = self.total_bytes_sent.saturating_add(bytes_sent);
        self.total_bytes_received = self.total_bytes_received.saturating_add(bytes_received);
    }

    pub(super) fn record_transport_fallback_from_sent(&mut self, sent: &crate::relay::SentFrame) {
        self.record_https_fallback(
            sent.transport_fallback,
            sent.transport_fallback_reason.as_deref(),
        );
    }

    pub(super) fn record_remote_stream_open(&mut self, duration_ms: u64) {
        self.telemetry.remote_stream.open(duration_ms);
    }

    pub(crate) fn record_token_cycle(&mut self, duration_ms: u64) {
        self.token_cycle.record(duration_ms);
    }

    pub(super) fn record_remote_stream_token_delivery_interval(&mut self, duration_ms: u64) {
        self.telemetry
            .remote_stream
            .record_token_delivery_interval(duration_ms);
    }

    /// Recebe spans detalhados anexados a uma resposta de peer. O ID é a
    /// unidade de deduplicação porque o mesmo span pode aparecer no
    /// `node_timings` e também numa entrada borbulhada de `hop_timings`.
    pub(super) fn record_profiling_spans_from_json(&mut self, value: &serde_json::Value) {
        let Some(incoming) = value
            .get("profiling_spans")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        if self.profiling_spans.is_none() {
            return;
        }
        for raw in incoming {
            if self
                .profiling_spans
                .as_ref()
                .is_some_and(|spans| spans.len() >= TELEMETRY_SAMPLE_LIMIT)
            {
                break;
            }
            let Ok(span) = serde_json::from_value::<TelemetrySpan>(raw.clone()) else {
                continue;
            };
            if self.profiling_span_ids.insert(span.span_id.clone()) {
                if let Some(target) = self.profiling_spans.as_mut() {
                    target.push(span);
                }
            }
        }
    }

    /// Registra um callback sem transformar seus agregados em uma segunda
    /// resposta. IDs repetidos são ignorados no merge; uma ordem diferente da
    /// chegada continua válida. A janela é limitada para que a deduplicação
    /// não transforme a telemetria em memória por requisição.
    pub(super) fn record_remote_stream_batch(&mut self, meta: &serde_json::Value) -> bool {
        let frame_id = meta
            .get("stream_id")
            .and_then(|value| value.as_str())
            .zip(
                meta.get("remote_stream_frame_index")
                    .and_then(|value| value.as_u64()),
            )
            .map(|(stream, index)| format!("{stream}:{index}"));
        if let Some(frame_id) = frame_id {
            if self.remote_stream_frame_ids.contains(&frame_id) {
                self.telemetry.duplicate_spans = self.telemetry.duplicate_spans.saturating_add(1);
                return false;
            }
            if self.remote_stream_frame_ids.len() >= TELEMETRY_DEDUPE_WINDOW {
                self.remote_stream_frame_ids.clear();
            }
            self.remote_stream_frame_ids.insert(frame_id);
        }

        let accepted = meta
            .get("accepted")
            .and_then(|value| value.as_array())
            .map_or(0, Vec::len) as u64;
        self.expected_decode_spans = self
            .expected_decode_spans
            .saturating_add(accepted.saturating_mul(self.hops.len() as u64));
        self.telemetry.remote_stream.record_batch(meta);
        self.record_https_fallback(
            meta.get("remote_stream_transport_fallback")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            meta.get("remote_stream_transport_fallback_reason")
                .and_then(serde_json::Value::as_str),
        );
        self.record_profiling_spans_from_json(meta);

        if self.profiling_spans.is_none() {
            return true;
        }
        if self
            .profiling_spans
            .as_ref()
            .is_some_and(|spans| spans.len() >= TELEMETRY_SAMPLE_LIMIT)
        {
            return true;
        }
        let explicit_token_start = meta
            .get("accepted")
            .and_then(|value| value.as_array())
            .and_then(|values| values.first())
            .and_then(|value| value.get("token_index"))
            .and_then(|value| value.as_u64());
        let token_start = explicit_token_start.unwrap_or(self.profiling_token_index);
        if explicit_token_start.is_some() {
            self.profiling_token_indices_absolute = true;
            self.profiling_token_index = self
                .profiling_token_index
                .max(token_start.saturating_add(accepted));
        } else {
            self.profiling_token_index = self.profiling_token_index.saturating_add(accepted);
        }
        let spans = self
            .profiling_spans
            .as_mut()
            .expect("profiling_spans checked above");
        let frame_index = meta
            .get("remote_stream_frame_index")
            .and_then(|value| value.as_u64())
            .unwrap_or(spans.len() as u64);
        let end_ms = meta
            .get("remote_stream_lifetime_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let duration_ms = meta
            .get("remote_stream_frame_latency_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let end_ns = end_ms.saturating_mul(1_000_000);
        let duration_ns = duration_ms.saturating_mul(1_000_000);
        let string_field = |key: &str, fallback: &str| {
            meta.get(key)
                .and_then(|value| value.as_str())
                .unwrap_or(fallback)
                .to_owned()
        };
        let peer_id = self
            .hops
            .first()
            .map(|hop| hop.node_id.clone())
            .unwrap_or_else(|| "unknown".into());
        let stream_id = string_field("stream_id", "unknown");
        let frame_span_id = format!("remote-stream-{stream_id}-frame-{frame_index}");
        spans.push(TelemetrySpan {
            trace_id: string_field("trace_id", "remote-stream"),
            request_id: string_field("request_id", "unknown"),
            pipeline_id: string_field("pipeline_id", "unknown"),
            peer_id,
            span_id: frame_span_id.clone(),
            parent_span_id: None,
            name: "remote-stream-frame".into(),
            phase: Phase::Decode,
            token_index: None,
            start_ns: end_ns.saturating_sub(duration_ns),
            end_ns,
            duration_ns,
            transport: Some("remote-stream".into()),
            clock_domain: "peer-monotonic".into(),
        });
        for offset in 0..accepted {
            if spans.len() >= TELEMETRY_SAMPLE_LIMIT {
                break;
            }
            spans.push(TelemetrySpan {
                trace_id: string_field("trace_id", "remote-stream"),
                request_id: string_field("request_id", "unknown"),
                pipeline_id: string_field("pipeline_id", "unknown"),
                peer_id: self
                    .hops
                    .first()
                    .map(|hop| hop.node_id.clone())
                    .unwrap_or_else(|| "unknown".into()),
                span_id: format!("remote-stream-{stream_id}-token-{}", token_start + offset),
                parent_span_id: Some(frame_span_id.clone()),
                name: "token".into(),
                phase: Phase::Decode,
                token_index: Some(token_start + offset),
                // The callback identifies token membership and order, but the
                // batch does not contain a per-token compute interval. Keep
                // this as an event span instead of fabricating a duration.
                start_ns: end_ns,
                end_ns,
                duration_ns: 0,
                transport: Some("remote-stream".into()),
                clock_domain: "peer-monotonic".into(),
            });
        }
        true
    }

    pub fn refresh_telemetry(&mut self, wall_clock_ms: u64) {
        let expected_peers = self.hops.len() as u32;
        let decode_tracking = self.expected_decode_spans > 0
            || self.hops.iter().any(|hop| hop.decode.reported_spans > 0);
        let reported_peers = self
            .hops
            .iter()
            .filter(|hop| {
                if decode_tracking {
                    hop.decode.reported_spans > 0
                } else {
                    hop.prefill.calls > 0
                }
            })
            .count() as u32;
        let missing_peers = self
            .hops
            .iter()
            .filter(|hop| {
                if decode_tracking {
                    hop.decode.reported_spans == 0
                } else {
                    hop.prefill.calls == 0
                }
            })
            .map(|hop| hop.node_id.clone())
            .collect::<Vec<_>>();
        let reported_decode_spans = self
            .hops
            .iter()
            .map(|hop| hop.decode.reported_spans)
            .sum::<u64>();
        // Remote-stream callbacks count accepted tokens directly. The regular
        // path has no callback frames, so its completed output token count is
        // the authoritative expected span count for each pipeline hop.
        let expected_decode_spans = self
            .expected_decode_spans
            .max((self.output_tokens as u64).saturating_mul(self.hops.len() as u64));
        let completeness_percent = if expected_decode_spans > 0 {
            (reported_decode_spans as f64 / expected_decode_spans as f64 * 100.0).min(100.0)
        } else if expected_peers > 0 {
            reported_peers as f64 / expected_peers as f64 * 100.0
        } else {
            100.0
        };
        // A result can win the race before the independent transport receipts
        // reach the inbox. This is a pending transport reconciliation, not a
        // missing peer/span: the distributed span collection can already be
        // complete and must not downgrade the diagnosis to "incomplete".
        let spans_complete = missing_peers.is_empty()
            && (expected_decode_spans == 0 || reported_decode_spans >= expected_decode_spans);
        self.telemetry.status = if spans_complete && self.complete {
            "complete"
        } else if self.complete {
            "response_complete"
        } else {
            "incomplete"
        }
        .into();
        self.telemetry.response_complete = self.complete;
        self.telemetry.expected_peers = expected_peers;
        self.telemetry.reported_peers = reported_peers;
        self.telemetry.missing_peers = missing_peers;
        self.telemetry.expected_decode_spans = expected_decode_spans;
        self.telemetry.reported_decode_spans = reported_decode_spans;
        self.telemetry.decode_span_expectation_available = expected_decode_spans > 0;
        self.telemetry.completeness_percent = completeness_percent;
        self.telemetry.wall_clock_ms = Some(wall_clock_ms);
        self.telemetry.critical_path_ms = Some(wall_clock_ms);
        let gateway_span_durations = self
            .spans
            .iter()
            .filter(|span| span.host == "gateway" && span.duration_ms > 0)
            .map(|span| span.duration_ms)
            .collect::<Vec<_>>();
        if gateway_span_durations.is_empty() {
            self.telemetry.overlap_ms = None;
            self.telemetry.true_residual_ms = None;
        } else {
            let accounted_ms = critical_path_ms(&self.spans, "gateway");
            let summed_ms = gateway_span_durations
                .iter()
                .copied()
                .fold(0u64, u64::saturating_add);
            self.telemetry.overlap_ms = Some(summed_ms.saturating_sub(accounted_ms));
            self.telemetry.true_residual_ms = Some(wall_clock_ms.saturating_sub(accounted_ms));
        }
        self.telemetry.accumulated_compute_ms = self
            .hops
            .iter()
            .flat_map(|hop| [&hop.prefill, &hop.decode])
            .map(|phase| phase.processing_ms)
            .sum();
        self.telemetry.accumulated_transport_ms = self
            .hops
            .iter()
            .flat_map(|hop| [&hop.prefill, &hop.decode])
            .map(|phase| phase.network_ms)
            .sum();
    }

    /// Same accumulation as `record_hop`, reading the numbers from one
    /// `hop_timings` array entry instead of taking them as call arguments -
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
            entry
                .get("node_timings")
                .unwrap_or(&serde_json::Value::Null),
        );
        if let Some(measured_network_ms) = TransportTrace::measured_network_ms(
            entry
                .get("transport_rtt_us")
                .and_then(|value| value.as_u64()),
        ) {
            if let Some(entry) = self.hops.get_mut(hop) {
                let slot = match phase {
                    Phase::Prefill => &mut entry.prefill,
                    Phase::Decode => &mut entry.decode,
                };
                slot.network_ms = slot
                    .network_ms
                    .saturating_sub(get("round_trip_ms").saturating_sub(get("processing_ms")))
                    .saturating_add(measured_network_ms);
            }
        }
        let represented_spans = get("span_count");
        if represented_spans > 1 {
            if let Some(hop_entry) = self.hops.get_mut(hop) {
                let slot = match phase {
                    Phase::Prefill => &mut hop_entry.prefill,
                    Phase::Decode => &mut hop_entry.decode,
                };
                slot.reported_spans = slot
                    .reported_spans
                    .saturating_add(represented_spans.saturating_sub(1));
            }
        }
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
        self.record_https_fallback(
            entry
                .get("transport_fallback")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            entry
                .get("transport_fallback_reason")
                .and_then(|value| value.as_str()),
        );
        self.record_profiling_spans_from_json(entry);
    }

    fn record_https_fallback(&mut self, fallback: bool, reason: Option<&str>) {
        if !fallback {
            return;
        }
        self.https_fallback_calls = self.https_fallback_calls.saturating_add(1);
        if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
            if self.https_fallback_reasons.len() < TRACE_ISSUE_LIMIT {
                self.https_fallback_reasons.push(reason.to_owned());
            }
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
        for pending in &other.pending_direct_receipts {
            if !self.pending_direct_receipts.iter().any(|current| {
                current.ticket_id == pending.ticket_id && current.phase == pending.phase
            }) {
                self.pending_direct_receipts.push(pending.clone());
            }
        }
        self.direct_transport_receipt_ids
            .extend(other.direct_transport_receipt_ids.iter().cloned());
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
        self.token_cycle.merge(&other.token_cycle);
        self.pipeline_cycle.merge(&other.pipeline_cycle);
        self.connection_new_calls += other.connection_new_calls;
        self.connection_reused_calls += other.connection_reused_calls;
        self.connection_unknown_calls += other.connection_unknown_calls;
        self.https_fallback_calls = self
            .https_fallback_calls
            .saturating_add(other.https_fallback_calls);
        for reason in &other.https_fallback_reasons {
            if self.https_fallback_reasons.len() >= TRACE_ISSUE_LIMIT {
                break;
            }
            self.https_fallback_reasons.push(reason.clone());
        }
        if self.engine_ttft_ms.is_none() {
            self.engine_ttft_ms = other.engine_ttft_ms;
            self.first_token = other.first_token.clone();
        }
        self.telemetry.late_spans = self
            .telemetry
            .late_spans
            .saturating_add(other.telemetry.late_spans);
        self.telemetry.orphan_spans = self
            .telemetry
            .orphan_spans
            .saturating_add(other.telemetry.orphan_spans);
        self.telemetry.duplicate_spans = self
            .telemetry
            .duplicate_spans
            .saturating_add(other.telemetry.duplicate_spans);
        let mut merge_remote = true;
        for frame_id in &other.remote_stream_frame_ids {
            if self.remote_stream_frame_ids.contains(frame_id) {
                self.telemetry.duplicate_spans = self.telemetry.duplicate_spans.saturating_add(1);
                merge_remote = false;
            } else {
                if self.remote_stream_frame_ids.len() >= TELEMETRY_DEDUPE_WINDOW {
                    self.remote_stream_frame_ids.clear();
                }
                self.remote_stream_frame_ids.insert(frame_id.clone());
            }
        }
        if merge_remote {
            self.expected_decode_spans = self
                .expected_decode_spans
                .saturating_add(other.expected_decode_spans);
            self.telemetry
                .remote_stream
                .merge(&other.telemetry.remote_stream);
            let token_offset = self.profiling_token_index;
            let mut incoming = Vec::new();
            if let Some(next) = &other.profiling_spans {
                let remaining = self.profiling_spans.as_ref().map_or(0, |current| {
                    TELEMETRY_SAMPLE_LIMIT.saturating_sub(current.len())
                });
                for mut span in next.iter().take(remaining).cloned() {
                    if !other.profiling_token_indices_absolute {
                        if let Some(token_index) = span.token_index.as_mut() {
                            *token_index = token_index.saturating_add(token_offset);
                        }
                    }
                    if self.profiling_span_ids.insert(span.span_id.clone()) {
                        incoming.push(span);
                    }
                }
            }
            if let Some(current) = self.profiling_spans.as_mut() {
                current.extend(incoming);
            }
            self.profiling_token_indices_absolute |= other.profiling_token_indices_absolute;
            let next_index = if other.profiling_token_indices_absolute {
                other.profiling_token_index
            } else {
                other.profiling_token_index.saturating_add(token_offset)
            };
            self.profiling_token_index = self.profiling_token_index.max(next_index);
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
        self.refresh_decode_counters();
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
    thinking_enabled: bool,
    reuse_prefix: bool,
    start_pos: Option<u32>,
    trace_id: Option<&str>,
) -> serde_json::Value {
    let mut meta = json!({
        "session_id": session_id,
        "pipeline_session": true,
        "chunk_index": chunk_index,
        "chunk_size": chunk_size,
        "is_last": is_last,
        "is_final_chunk": false,
        "prompt": prompt,
        "chat_template": chat_template,
        "thinking_enabled": thinking_enabled,
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
