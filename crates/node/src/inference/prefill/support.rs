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
/// as the emitter is free, and no timer fills the gap. One callback is in
/// flight at a time, so the cap bounds delivery at `limit / RTT`: at 8, a
/// 270 ms relayed path delivered 26 tok/s from a head producing 75. It
/// matches the producer's queue (`step.rs`, 64), so a batch takes everything
/// already generated.
pub(crate) const REMOTE_STREAM_BATCH_LIMIT: usize = 64;
/// One control frame starts generation on a remote head. Subsequent tokens
/// return through `crate::direct::REMOTE_STREAM_RESULT_PATH` in batches.
pub(crate) const REMOTE_STREAM_PATH: &str = "/v1/step-stream";

/// Cancellation shared between the HTTP stream and the prefill. The `Notify`
/// wakes an in-flight call; the `AtomicBool` makes the checks between hops and
/// chunks cheap and await-free.
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
        f.write_str("generation cancelled by the client")
    }
}

impl std::error::Error for GenerationCancelled {}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GenerationCancelled>().is_some()
}

/// Next chunk the gateway can resend without changing the remote KV. It is only
/// created when the transport confirms the chunk body was not delivered;
/// ambiguous failures use `PrefillRebuildRequired`.
#[derive(Debug, Clone)]
pub struct PrefillResumable {
    pub from_chunk: u32,
    pub start_pos: u32,
    pub prompt_tokens: u32,
    pub cached_tokens: u32,
    /// The ordered sequence of the direct relay must survive the resume.
    pub prefill_sequence_id: Option<String>,
    /// Partial trace accumulated before the failure. The resume adds the new
    /// observations without erasing the chunks that already reached the peer.
    pub trace: Option<Box<RequestTrace>>,
}

impl std::fmt::Display for PrefillResumable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{RESUMABLE_TEXT_PREFIX}{} (position {})",
            self.from_chunk, self.start_pos
        )
    }
}

/// The resume point attached to a prefill error, if there is one.
pub fn resume_point(error: &anyhow::Error) -> Option<PrefillResumable> {
    error.downcast_ref::<PrefillResumable>().cloned()
}

#[derive(Debug, Clone)]
pub struct PartialPrefillTrace {
    pub trace: Box<RequestTrace>,
}

impl std::fmt::Display for PartialPrefillTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(PARTIAL_TRACE_TEXT)
    }
}

impl std::error::Error for PartialPrefillTrace {}

const PARTIAL_TRACE_TEXT: &str = "partial prefill telemetry";
const RESUMABLE_TEXT_PREFIX: &str = "resumable prefill of chunk ";

/// The prefill error as a person reads it: the context chain without the
/// wrappers that only carry telemetry or a resume point. Printing just the
/// outermost context showed "telemetria parcial do prefill" and hid the cause.
pub fn failure_cause(error: &anyhow::Error) -> String {
    // Both wrappers are attached with `context`, which hides their type from
    // `chain()`; their fixed text is what identifies them.
    let parts: Vec<String> = error
        .chain()
        .map(|cause| cause.to_string())
        .filter(|text| text != PARTIAL_TRACE_TEXT && !text.starts_with(RESUMABLE_TEXT_PREFIX))
        .collect();
    if parts.is_empty() {
        error.to_string()
    } else {
        parts.join(": ")
    }
}

/// Partial trace retained by a failed prefill, whether or not it can resume.
pub fn partial_trace(error: &anyhow::Error) -> Option<RequestTrace> {
    error
        .downcast_ref::<PartialPrefillTrace>()
        .map(|partial| (*partial.trace).clone())
        .or_else(|| resume_point(error).and_then(|resume| resume.trace.map(|trace| *trace)))
}

pub(super) fn resume_with_trace(
    error: anyhow::Error,
    trace: &RequestTrace,
    elapsed: Duration,
) -> anyhow::Error {
    let mut trace = trace.clone();
    trace.prefill_ms = trace.prefill_ms.saturating_add(elapsed.as_millis() as u64);
    let partial = PartialPrefillTrace {
        trace: Box::new(trace.clone()),
    };
    if let Some(mut resume) = resume_point(&error) {
        resume.trace = Some(Box::new(trace));
        error.context(resume).context(partial)
    } else {
        error.context(partial)
    }
}

/// The remote session may have accepted the chunk, so it is only recoverable by a
/// cold rebuild. The reason is static so that the logs state precisely
/// why the KV and the previous cache cannot be reused.
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
            "unsafe resume of chunk {} (position {}): {}",
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

/// Prefill overlap window.
///
/// The depth is the number of chunks the gateway keeps in flight at the same
/// time through the chain: one chunk per leg/hop is what keeps all links
/// and all GPUs busy in parallel instead of a single stage. Two limits
/// contain it: the count (`acquire`, cap of eight) and the byte budget of
/// hidden state in flight at the gateway (`reserve_bytes`, fixed cap of 512 MiB, which
/// only bites with a large `n_embd` or a high window). `1` turns the overlap off.
#[derive(Clone, Debug)]
pub(crate) struct PrefillOverlapWindow {
    pub(super) max_inflight_chunks: usize,
    pub(super) permits: Arc<Semaphore>,
    pub(super) byte_permits: Arc<Semaphore>,
}

impl PrefillOverlapWindow {
    pub(crate) const DEFAULT_MAX_INFLIGHT_CHUNKS: usize = 8;
    pub(crate) const MAX_INFLIGHT_CHUNKS: usize = 8;
    /// Cap on bytes of hidden state (in memory, `f32`) the gateway keeps
    /// in flight at the same time through the prefill pipeline. The count gate is the
    /// main limit; this is the safety net for models with a large
    /// `n_embd`.
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

    /// A slot in the window. The permit travels with the chunk until its downstream
    /// finishes; this is what limits how many chunks cross the chain at the same
    /// time (`run`/`run_direct` keep the permit inside each chunk's
    /// future).
    pub(crate) async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("overlap window was closed"))
    }

    /// Reserves `amount` bytes of the in-flight budget, waiting for an earlier chunk
    /// to finish if needed. An `amount` that alone exceeds the budget is
    /// clamped to the budget (the chunk passes, but holds the whole window),
    /// so it never blocks. The returned permit releases the bytes when dropped.
    pub(crate) async fn reserve_bytes(&self, amount: usize) -> OwnedSemaphorePermit {
        let want = amount.clamp(1, Self::INFLIGHT_BYTE_BUDGET) as u32;
        self.byte_permits
            .clone()
            .acquire_many_owned(want)
            .await
            .expect("the prefill byte semaphore is never closed")
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
    /// `probe_result` of the last peer when the prefill carried labels of a
    /// typed decision: `{labels, probabilities, mass}`.
    pub probe: Option<serde_json::Value>,
    pub text: String,
    pub done: bool,
    /// Prompt length as counted by the peer that tokenised it.
    pub prompt_tokens: u32,
    /// Tokens actually left in `prompt_seq` after the volatile template tail
    /// and generation were separated. This is the value `/v1/session/rewind`
    /// confirms; it can be smaller than `prompt_tokens` by construction.
    pub retained_tokens: u32,
    /// How many prompt tokens the head did **not** reprocess because they were already
    /// in this session's KV.
    ///
    /// It is the measure of what was *executed*, not of what the gateway chose: whoever
    /// tokenizes decides whether the retained prefix was usable. Zero on a cold turn,
    /// on a prefix that diverged and on any model that restarted the
    /// session. It goes to the log and to `usage.prompt_tokens_details.cached_tokens`.
    pub cached_tokens: u32,
    pub chunks: u32,
    pub trace: RequestTrace,
}

/// Everything one hop spent inside a single phase. Prefill and decode are kept
/// apart because they are different workloads: prefill moves a whole prompt
/// through the peer once, decode moves one token at a time. Averaging them
/// together hides which of the two is slow.
/// What a hop call reports about the channel it used.
///
/// It exists as a struct, and not as two loose parameters, because `record_hop`
/// already carries nine positionals - two more `u64` there would be swapped
/// in order by the first distracted caller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TlsHop {
    /// The call was encrypted.
    pub on: bool,
    /// Handshake measured on the server, in ms. Zero when the connection came from the pool.
    pub handshake_ms: u64,
}

/// An observed crossing of a frame between two processes. The values are
/// per call, not residual credits assigned to the first hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportTrace {
    pub transport: crate::relay::InferenceTransport,
    pub fallback: bool,
    /// Caller's wall-clock: from queueing the frame until the response arrives.
    /// **Includes** the remote node running the step/prefill handler.
    pub round_trip_ms: u64,
    /// `round_trip_ms` minus the `processing_ms` the remote node reported for itself:
    /// the wire transport cost, without the compute on the other side. It is the number
    /// the telemetry card should show per leg - a full `round_trip`
    /// never comes close to the network RTT measured at 0.5 ms between co-located nodes.
    pub network_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// QUIC request body send plus `finish` wait, when measured. It is absent
    /// for other transports.
    pub request_body_send_us: Option<u64>,
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

/// Quantiles of the cycle observed by the gateway between requesting a decode
/// round and delivering the first confirmed token to the client. The reservoir is
/// used only in normal mode to keep the cost bounded; the per-stage
/// spans remain opt-in under profiling.
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

/// Cheap aggregates of the callback path. The quantiles use the same
/// bounded reservoir as the common crossings, so there is no per-token vector
/// in normal mode.
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
    /// Wait between the start of the cycle and the token being ready at the producer.
    /// Measures compute/production still in progress, without calling it a queue.
    pub remote_stream_producer_wait_total_ms: u64,
    pub remote_stream_producer_wait_p50_ms: u64,
    pub remote_stream_producer_wait_p95_ms: u64,
    /// Time the producer waited for a permit of the bounded channel after
    /// finishing the token's production. Zero proves there was no
    /// `Sender` backpressure in this run.
    pub remote_stream_channel_send_wait_total_ms: u64,
    pub remote_stream_channel_send_wait_p50_ms: u64,
    pub remote_stream_channel_send_wait_p95_ms: u64,
    /// Wall time spent in the producer's nested hop before its token became
    /// ready. It includes downstream compute and transport, so it is the
    /// producer's legitimate cycle time rather than local queueing.
    pub remote_stream_downstream_wait_total_ms: u64,
    pub remote_stream_downstream_wait_p50_ms: u64,
    pub remote_stream_downstream_wait_p95_ms: u64,
    /// Time the already ready token stayed in the channel before the emitter.
    /// May include backpressure when the bounded channel is full.
    pub remote_stream_queue_wait_total_ms: u64,
    pub remote_stream_queue_wait_p50_ms: u64,
    pub remote_stream_queue_wait_p95_ms: u64,
    /// Time spent awaiting the callback delivery that preceded this batch.
    /// The first batch is zero and the final callback has no following batch
    /// to carry its duration.
    pub remote_stream_delivery_total_ms: u64,
    pub remote_stream_delivery_p50_ms: u64,
    pub remote_stream_delivery_p95_ms: u64,
    /// Interval observed at the gateway between received token batches. In a
    /// one-token batch, it is the per-token delivery cycle; larger batches are
    /// counted as one cycle, without inventing internal intervals.
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
        self.downstream_wait.merge(&other.downstream_wait);
        self.queue_wait.merge(&other.queue_wait);
        self.delivery.merge(&other.delivery);
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

/// Detailed opt-in span. The timestamps are relative to the monotonic domain
/// of the peer that produced them, never differences between machine clocks.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct TelemetrySpan {
    pub trace_id: String,
    pub request_id: String,
    pub pipeline_id: String,
    pub peer_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    /// Names the observed boundary. The duration is only interpretable together with
    /// this name: events such as `client-emit` may have zero duration, while
    /// `transport-round-trip` covers a complete call.
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
    /// The response may have been released while the remote receipts are still
    /// on the way. This separates the end of generation from the end of collection.
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
    /// Aggregate QUIC request body send and finish wait. A batch records this
    /// once for the shared physical request, not once per contained item.
    pub quic_request_body_send_us: u64,
    pub quic_request_body_send_calls: u32,
    /// Full RTT, including the remote node's compute. Kept for the execution RTT
    /// total and compatibility with the old frontend.
    pub rtt_min_ms: u64,
    pub rtt_median_ms: u64,
    pub rtt_p95_ms: u64,
    pub rtt_max_ms: u64,
    /// Transport-only RTT: `round_trip - processing_ms` of the remote. It is what the
    /// card's leg should display; without it a QUIC link between co-located nodes
    /// showed ~76 ms of "RTT" that was compute on the other side.
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
    /// A channel's entry built from aggregates measured elsewhere (the stage links of a plugin pipeline). The
    /// round trips are averaged: `median` carries the mean and `p95` the maximum, because no sample list travels.
    pub(crate) fn from_aggregates(
        transport: crate::relay::InferenceTransport,
        calls: u32,
        bytes_sent: u64,
        bytes_received: u64,
        fallback_calls: u32,
        round_trip: (u64, u64, u64),
        network: (u64, u64, u64),
    ) -> Self {
        let mut entry = Self::new(transport);
        entry.calls = calls;
        entry.bytes_sent = bytes_sent;
        entry.bytes_received = bytes_received;
        entry.fallback_calls = fallback_calls;
        (entry.rtt_min_ms, entry.rtt_median_ms, entry.rtt_max_ms) = round_trip;
        entry.rtt_p95_ms = round_trip.2;
        (entry.net_rtt_min_ms, entry.net_rtt_median_ms, entry.net_rtt_max_ms) = network;
        entry.net_rtt_p95_ms = network.2;
        entry
    }
}

impl TransportSummaryEntry {
    pub(super) fn new(transport: crate::relay::InferenceTransport) -> Self {
        Self {
            transport,
            calls: 0,
            bytes_sent: 0,
            bytes_received: 0,
            quic_request_body_send_us: 0,
            quic_request_body_send_calls: 0,
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
        if trace.transport == crate::relay::InferenceTransport::Quic {
            if let Some(send_us) = trace.request_body_send_us {
                self.quic_request_body_send_us =
                    self.quic_request_body_send_us.saturating_add(send_us);
                self.quic_request_body_send_calls =
                    self.quic_request_body_send_calls.saturating_add(1);
            }
        }
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
        self.quic_request_body_send_us = self
            .quic_request_body_send_us
            .saturating_add(other.quic_request_body_send_us);
        self.quic_request_body_send_calls = self
            .quic_request_body_send_calls
            .saturating_add(other.quic_request_body_send_calls);
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

/// An observed leg of the pipeline's logical cycle.
///
/// `from_node_id -> to_node_id` is the direction of the crossing that was observed at the
/// executor. The value remains the duration of the observed transport
/// operation (the old RTT distribution), not an inferred half of a
/// round-trip clock. The map uses each leg once, so it does not add a
/// nonexistent return between forwarding peers.
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

/// Aggregated decomposition of the cycle, separated by workload so that prefill does not
/// change the RTT shown for decode. In normal mode it keeps only counters,
/// histograms and one entry per leg, with no per-token span.
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
            request_body_send_us: sent.request_body_send_us,
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
            request_body_send_us: entry
                .get("request_body_send_us")
                .and_then(|value| value.as_u64()),
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
    /// How many decode events were represented by this phase's
    /// aggregates. In ordinary calls it is one; a remote-stream batch may carry
    /// several tokens without turning the call counter into a token
    /// counter.
    pub reported_spans: u64,
    pub client_encode_ms: u64,
    /// Subset of client frame encode/decode spent on f32<->f16 conversion.
    pub client_f16_encode_ms: Option<f64>,
    pub client_f16_decode_ms: Option<f64>,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    pub client_decode_ms: u64,
    /// Calls of this hop that were encrypted.
    ///
    /// A count and not a boolean because a phase sums several calls: if only
    /// some of them went over TLS, the screen needs to be able to say so instead of
    /// picking one of the two and lying about the rest.
    pub tls_calls: u32,
    /// Sum of this hop's TLS handshakes, measured on the server.
    ///
    /// Almost always zero: the handshake happens once per connection and the
    /// connections stay in the pool. It is an establishment cost, not a per-token cost.
    pub tls_handshake_ms: u64,
    /// Count of the reqwest pool state observed in this phase.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_new_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_reused_calls: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub connection_unknown_calls: u32,
    /// Bounded distribution of the calls that crossed this hop.
    ///
    /// Min/max are exact. After 1,024 samples, the quantiles use a bounded
    /// reservoir so that neither the trace's memory nor the response
    /// grow per token.
    pub transport_summary: TransportSummary,
    pub node_frame_decode_ms: f64,
    pub node_input_prepare_ms: f64,
    pub node_queue_ms: f64,
    /// Subset of the queue that was waiting for the scheduler's selection/admission.
    /// Normally coincides with `node_queue_ms`; the separation exists to
    /// name the reclaim grace of retained sessions without subtracting times.
    pub node_scheduler_wait_ms: f64,
    /// Observed reasons for the scheduler wait in this phase.
    pub scheduler_wait_reasons: Vec<String>,
    /// Wait for the engine mutex on the node: the price of another request being
    /// decoded there. It is what separates "the node is slow" from "the node is busy".
    pub node_lock_wait_ms: f64,
    pub node_setup_ms: f64,
    pub node_decode_ms: f64,
    /// Time measured for the `nextn` block, outside `node_decode_ms` and before the
    /// response serialization. When present, it explains the MTP production that
    /// a remote-stream cycle legitimately waits for.
    pub node_mtp_draft_ms: f64,
    /// Sum of the `nextn` head's `observe` on the tail (see `mtp_observe_ms`).
    pub node_mtp_observe_ms: f64,
    /// Time spent in the backend evaluation API call. This is neither queue
    /// wait nor a guarantee of pure device-kernel time.
    pub node_backend_call_ms: f64,
    /// Queued backend work plus already-queued output transfer, measured with
    /// `llama_synchronize`. A zero value means no synchronization duration was
    /// recorded; the effective `POM_PROFILE_SYNC` state is published separately
    /// by `/health` and must never be inferred from this timing.
    pub node_backend_sync_ms: f64,
    /// Device-to-host copy of the residual, measured inside the capture hook.
    pub node_residual_capture_d2h_sync_ms: f64,
    /// Scheduler ask-to-ready wall, including graph evaluation and its sync.
    pub node_residual_capture_ready_wait_ms: f64,
    /// `ggml_backend_tensor_get` call wall. CUDA may include D2H and stream
    /// synchronization; this is not an isolated DMA measurement.
    pub node_residual_capture_tensor_get_ms: f64,
    pub node_residual_capture_postprocess_ms: f64,
    pub node_residual_capture_bytes: u64,
    pub node_residual_capture_copies: u64,
    pub node_residual_wire_f16_encode_ms: Option<f64>,
    pub node_residual_wire_f16_decode_ms: Option<f64>,
    pub node_residual_wire_f8_conversion_ms: f64,
    pub node_residual_wire_frame_serialize_ms: f64,
    pub node_residual_wire_encode_calls: u64,
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
    /// Path the backend selected from the GGUF.
    pub selected_path: Option<String>,
    /// Single engine when the phase was homogeneous; `unknown` for a mix or
    /// absence of samples.
    pub executed_engine: crate::executor::ExecutionEngine,
    /// All observed engines, preserving the evidence of a mixed phase.
    pub executed_engines: Vec<crate::executor::ExecutionEngine>,
    pub engine_mismatch: bool,
    pub mixed_execution: bool,
    #[serde(skip)]
    pub(super) node_samples_ms: Vec<f64>,
    #[serde(skip)]
    pub(super) node_samples_seen: u64,
    #[serde(skip)]
    pub(super) selected_path_seen: bool,
    #[serde(skip)]
    pub(super) hidden_state_dtype_mixed: bool,
}

fn add_optional_ms(total: &mut Option<f64>, sample: Option<f64>) {
    if let Some(sample) = sample {
        *total = Some((*total).unwrap_or(0.0) + sample);
    }
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

        self.processing_ms += processing_ms;
        self.round_trip_ms += round_trip_ms;
        self.network_ms += round_trip_ms.saturating_sub(processing_ms);
        self.bytes_sent += bytes_sent;
        self.bytes_received += bytes_received;
        self.calls += 1;
        self.reported_spans = self.reported_spans.saturating_add(1);
        self.client_encode_ms += client_encode_ms;
        add_optional_ms(
            &mut self.client_f16_encode_ms,
            node.get("client_f16_encode_ms")
                .and_then(serde_json::Value::as_f64),
        );
        add_optional_ms(
            &mut self.client_f16_decode_ms,
            node.get("client_f16_decode_ms")
                .and_then(serde_json::Value::as_f64),
        );
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
        self.node_mtp_observe_ms += node_ms("mtp_observe_ms");
        self.node_backend_call_ms += node_ms("backend_call_ms");
        self.node_backend_sync_ms += node_ms("backend_sync_ms");
        self.node_residual_capture_d2h_sync_ms += node_ms("residual_capture_d2h_sync_ms");
        self.node_residual_capture_ready_wait_ms += node_ms("residual_capture_ready_wait_ms");
        self.node_residual_capture_tensor_get_ms += node_ms("residual_capture_tensor_get_ms");
        self.node_residual_capture_postprocess_ms += node_ms("residual_capture_postprocess_ms");
        self.node_residual_capture_bytes = self.node_residual_capture_bytes.saturating_add(
            node.get("residual_capture_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        self.node_residual_capture_copies = self.node_residual_capture_copies.saturating_add(
            node.get("residual_capture_copies")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        add_optional_ms(
            &mut self.node_residual_wire_f16_encode_ms,
            node.get("residual_wire_f16_encode_ms")
                .and_then(serde_json::Value::as_f64),
        );
        add_optional_ms(
            &mut self.node_residual_wire_f16_decode_ms,
            node.get("residual_wire_f16_decode_ms")
                .and_then(serde_json::Value::as_f64),
        );
        self.node_residual_wire_f8_conversion_ms += node_ms("residual_wire_f8_conversion_ms");
        self.node_residual_wire_frame_serialize_ms += node_ms("residual_wire_frame_serialize_ms");
        self.node_residual_wire_encode_calls = self.node_residual_wire_encode_calls.saturating_add(
            node.get("residual_wire_encode_calls")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
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
        self.refresh_percentiles();
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
        let expected = if path.starts_with("dense/") {
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
        add_optional_ms(&mut self.client_f16_encode_ms, next.client_f16_encode_ms);
        add_optional_ms(&mut self.client_f16_decode_ms, next.client_f16_decode_ms);
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
        self.node_mtp_observe_ms += next.node_mtp_observe_ms;
        self.node_backend_call_ms += next.node_backend_call_ms;
        self.node_backend_sync_ms += next.node_backend_sync_ms;
        self.node_residual_capture_d2h_sync_ms += next.node_residual_capture_d2h_sync_ms;
        self.node_residual_capture_ready_wait_ms += next.node_residual_capture_ready_wait_ms;
        self.node_residual_capture_tensor_get_ms += next.node_residual_capture_tensor_get_ms;
        self.node_residual_capture_postprocess_ms += next.node_residual_capture_postprocess_ms;
        self.node_residual_capture_bytes = self
            .node_residual_capture_bytes
            .saturating_add(next.node_residual_capture_bytes);
        self.node_residual_capture_copies = self
            .node_residual_capture_copies
            .saturating_add(next.node_residual_capture_copies);
        add_optional_ms(
            &mut self.node_residual_wire_f16_encode_ms,
            next.node_residual_wire_f16_encode_ms,
        );
        add_optional_ms(
            &mut self.node_residual_wire_f16_decode_ms,
            next.node_residual_wire_f16_decode_ms,
        );
        self.node_residual_wire_f8_conversion_ms += next.node_residual_wire_f8_conversion_ms;
        self.node_residual_wire_frame_serialize_ms += next.node_residual_wire_frame_serialize_ms;
        self.node_residual_wire_encode_calls = self
            .node_residual_wire_encode_calls
            .saturating_add(next.node_residual_wire_encode_calls);
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

/// Warm-up state observed by this execution. `Unknown` is deliberate:
/// an old peer or a boundary that does not expose the state must not become
/// `cold` by inference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    Cold,
    Warm,
    #[default]
    Unknown,
}

/// Closed span measured on a single host.
///
/// `started_at_ms` is wall clock only for correlation. `start_offset_ms` and
/// `duration_ms` come from `Instant` on the same host. Offsets from different
/// machines are never subtracted. The `parallel_group` lets the UI explain
/// overlap; the critical path calculation uses the union of the intervals.
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

/// Observable milestones of the first token. Each timestamp is local to the host that
/// recorded it; the duration fields are only filled in when the same host
/// can measure both ends.
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

/// Union of the intervals observed on a host. It covers inclusive and
/// parallel spans only once, so it does not confuse the sum of work with the critical
/// path.
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
    /// Probability of each label when the request was a typed decision
    /// (`/v1/systemone`): `{labels, probabilities, mass}` of the last peer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<serde_json::Value>,
    /// Typed decision summary (`/v1/systemone`), see `DecisionProbe::trace_summary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev: Option<serde_json::Value>,
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
    /// How many crossings ended up over HTTPS after a persistent/tunnel
    /// transport failed. It is not inferred from the transport name: it is only
    /// incremented when the transport layer carries the explicit reason.
    pub https_fallback_calls: u64,
    /// Bounded reasons, preserved to explain why the normal path
    /// was not used. An entry may appear more than once if the same
    /// edge falls over on several tokens.
    pub https_fallback_reasons: Vec<String>,
    pub model: String,
    pub input_tokens: u32,
    /// How many of the `input_tokens` the head did **not** reprocess because they were already
    /// in the reused session's KV.
    ///
    /// A measure of what was executed, not of the choice: whoever fills it in is the head's
    /// response (`PrefillOutcome::cached_tokens`). A cold turn, a prefix that
    /// diverged and a peer that restarted the session all count zero. This was
    /// exactly the gap that made the cache look like it was working -
    /// the log said "session reused" and the saving was nil.
    pub cached_tokens: u32,
    /// Candidate in common with the retained prefix. It may be greater than zero even
    /// when the divergence made the peer restart the session cold.
    pub prefix_match_tokens: u32,
    /// Observable result of the attempt: `full`, `partial-cold`, `miss` or
    /// `cold`. The gateway swaps it for `session-lost-cold` when it had to
    /// rebuild the session after a peer lost its state, and for `disk`
    /// when the reuse came from an automatic snapshot on disk
    /// (`slot_persist_prefix_cache`, see `inference.md` §3.1.1).
    pub prefix_reuse_status: Option<String>,
    /// Local reason for the gateway not having found an in-memory session.
    /// Distinguishes empty cache, different route, prompt that diverged and busy
    /// lease, without inferring the reason from the peer's result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_prefix_miss_reason: Option<String>,
    /// Tokens that actually had to cross the inference boundary.
    /// On a cold turn, or after a partial divergence, it is the whole prompt.
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
    /// Effective size chosen for the chunks on this route.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub prefill_chunk_tokens: u32,
    pub prefill_overlapped_chunks: u32,
    /// Largest number of prefill chunks that were in flight at the same time
    /// in this request. `<= prefill_max_inflight_chunks`; `1` when the
    /// overlap did not take hold (single-chunk prompt, window at 1, ring mode).
    pub prefill_pipeline_depth_max: u32,
    /// State of the collection itself. `wall_clock_ms` is a duration measured by the
    /// gateway; overlap/residual fields are only filled in when there are explicit
    /// time intervals, never by subtracting sums.
    pub telemetry: TelemetrySummary,
    /// Frame spans and per-token events only when
    /// `POM_PROFILE_SYNC=1` is active. A token event has zero duration
    /// when the batch carries no individual intervals, instead of repeating the
    /// batch's cost as if each token had been measured separately.
    pub profiling_spans: Option<Vec<TelemetrySpan>>,
    /// Cheap distribution of the cycle from a round to delivery at the client.
    /// Unlike `profiling_spans`, it also exists in normal mode to
    /// reveal p95/p99 and jitter propagation without allocating a span per token.
    #[serde(skip_serializing_if = "TokenCycleTelemetry::is_empty")]
    pub token_cycle: TokenCycleTelemetry,
    /// Directional legs of the logical cycle observed on the direct path. Includes
    /// the gateway's entry, the forwarding transitions and the final return when
    /// the delivery receipt reached the gateway.
    #[serde(skip_serializing_if = "PipelineCycleTelemetry::is_empty")]
    pub pipeline_cycle: PipelineCycleTelemetry,
    /// Draft tokens the MTP path proposed in this request, summed across
    /// all hops and steps.
    pub mtp_draft_tokens_proposed: u64,
    /// Draft tokens that passed the MTP acceptance criterion.
    pub mtp_draft_tokens_accepted: u64,
    /// Draft tokens that were verified but not accepted by MTP.
    pub mtp_draft_tokens_rejected: u64,
    /// `accepted / proposed`; null when the request proposed no MTP tokens.
    pub mtp_acceptance_rate: Option<f64>,
    /// Prefill/decode split: the session the head fetched from the prefill machine before decoding (its bytes, the
    /// transfer and the wait for the prefill machine's save). None when the turn did not fetch one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_transfer: Option<SessionTransferTrace>,
    #[serde(skip)]
    expected_decode_spans: u64,
    /// `/v1/step` rounds the gateway sent and got back. Every hop reports one
    /// decode span per round, so this is the exact expectation.
    #[serde(skip)]
    decode_rounds: u64,
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

/// A session the head of a turn fetched from a peer (`plugin_session::pull_from`): from `source_node_id` to
/// `target_node_id`, `bytes` in `transfer_ms`, after `save_wait_ms` waiting for the source's save to be written.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct SessionTransferTrace {
    pub source_node_id: String,
    pub target_node_id: String,
    pub tokens: u64,
    pub bytes: u64,
    pub megabytes: f64,
    pub transfer_ms: u64,
    pub save_wait_ms: u64,
    pub megabytes_per_sec: Option<f64>,
}

impl SessionTransferTrace {
    pub fn new(source: &str, target: &str, tokens: u64, bytes: u64, transfer_ms: u64, save_wait_ms: u64) -> Self {
        let megabytes = bytes as f64 / 1_000_000.0;
        Self {
            source_node_id: source.to_owned(),
            target_node_id: target.to_owned(),
            tokens,
            bytes,
            megabytes,
            transfer_ms,
            save_wait_ms,
            megabytes_per_sec: (transfer_ms > 0).then(|| megabytes * 1000.0 / transfer_ms as f64),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Prefill,
    Decode,
}

/// Context of a detailed sample. The context only exists when profiling
/// mode was explicitly requested and there is a token index to correlate
/// the peers. `Instant` never crosses the wire: each span publishes offsets in the
/// local monotonic domain, preserving the impossibility of subtracting clocks
/// of different machines.
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
            probe: None,
            jev: None,
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
            gateway_prefix_miss_reason: None,
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
            prefill_chunk_tokens: 0,
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
            session_transfer: None,
            expected_decode_spans: 0,
            decode_rounds: 0,
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

    /// `decode_calls` is **derived from the hops**, not a counter of its own.
    ///
    /// The gateway's continuation loop merges each leg's trace into the base
    /// trace (`merge`), and the merge always knew how to add up `PhaseTrace::calls` but
    /// never carried a standalone counter: the summary said "not measured" while the
    /// per-hop table showed 111 calls in the same run. Deriving removes
    /// the chance of the two diverging again.
    ///
    /// It is the **maximum** across the hops, not the sum: one decode round touches
    /// each hop once, so summing would multiply the rounds by the pipeline size and
    /// `tokens_per_decode_call` - the speculation's yield -
    /// would drop by that same factor.
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
        let mut node_timings = sent
            .meta
            .get("node_timings")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !node_timings.is_object() {
            node_timings = serde_json::json!({});
        }
        node_timings["client_f16_encode_ms"] =
            serde_json::json!(sent.client_f16_encode_ns as f64 / 1_000_000.0);
        node_timings["client_f16_decode_ms"] =
            serde_json::json!(sent.client_f16_decode_ns as f64 / 1_000_000.0);
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
            &node_timings,
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

    /// Records a crossing in the cycle in its real direction. The sample is
    /// aggregated by edge and transport, without allocating a span per token in normal
    /// mode.
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

    /// Records the last leg, which belongs to no `HopTrace`: the tail
    /// delivers the result to the gateway by a separate operation.
    pub(super) fn record_cycle_return(
        &mut self,
        phase: Phase,
        from_node_id: &str,
        transport: TransportTrace,
    ) {
        self.record_cycle_transport(phase, from_node_id, "gateway", transport);
    }

    /// Records the control crossing that opened a `remote-stream` without
    /// counting it as decode. The ACK confirms the stream opened, but is not
    /// a token span and cannot inflate the observed completeness.
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

    /// Receives detailed spans attached to a peer response. The ID is the
    /// deduplication unit because the same span may appear in
    /// `node_timings` and also in a bubbled entry of `hop_timings`.
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

    /// Records a callback without turning its aggregates into a second
    /// response. Repeated IDs are ignored on merge; an order different from
    /// arrival remains valid. The window is bounded so that deduplication
    /// does not turn telemetry into per-request memory.
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

    /// One `/v1/step` round came back from every hop.
    pub fn note_decode_round(&mut self) {
        self.decode_rounds = self.decode_rounds.saturating_add(1);
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
        // path has no callback frames: each hop reports one span per decode
        // round, and a round yields one token plus the MTP drafts it
        // accepted. Counting output tokens alone expected ~3.8x the spans of a
        // speculative run and reported 26% coverage on a complete trace.
        // A typed decision is prefill-only: the last peer scores the labels
        // from the prompt's logits and no `/v1/step` round runs, even though
        // the answer counts as one output token.
        // Estimating the rounds from the tokens is off by one either way: the
        // first token comes from the prefill, and an end-of-generation token
        // accepted as a draft counts as accepted but is never emitted. It
        // reported 98.9% of a trace in which every hop answered every round.
        // The rounds the gateway counted are exact; the estimate is only for
        // a path that does not count them.
        let decode_rounds = if self.probe.is_some() || self.jev.is_some() {
            0
        } else if self.decode_rounds > 0 {
            self.decode_rounds
        } else {
            (self.output_tokens as u64).saturating_sub(self.mtp_draft_tokens_accepted)
        };
        let expected_decode_spans = self
            .expected_decode_spans
            .max(decode_rounds.saturating_mul(self.hops.len() as u64));
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
        let mut node_timings = entry
            .get("node_timings")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if !node_timings.is_object() {
            node_timings = serde_json::json!({});
        }
        for key in ["client_f16_encode_ms", "client_f16_decode_ms"] {
            if let Some(value) = entry.get(key) {
                node_timings[key] = value.clone();
            }
        }
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
            &node_timings,
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
        // A one-hop plan that covers the whole model carries no hidden
        // state. The relay still knows which precision it would have used, but publishing
        // that local gateway setting as observed precision made
        // two gateways report f8 and f16 for the same run, both with 0 B.
        // Precision is only evidence when bytes cross the hop.
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
            self.decode_rounds = self.decode_rounds.saturating_add(other.decode_rounds);

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
        if self.gateway_prefix_miss_reason.is_none() {
            self.gateway_prefix_miss_reason = other.gateway_prefix_miss_reason.clone();
        }
        self.prefill_overlap_enabled |= other.prefill_overlap_enabled;
        self.prefill_max_inflight_chunks = self
            .prefill_max_inflight_chunks
            .max(other.prefill_max_inflight_chunks);
        self.prefill_chunk_tokens = self.prefill_chunk_tokens.max(other.prefill_chunk_tokens);
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

/// A typed decision (`/v1/systemone`): the answer labels the last peer scores
/// and, for the head, where the reusable part of the prompt ends.
///
/// With no labels the probe scores nothing and only carries the retain
/// boundary: that is how a chat request with retrieved context appended
/// (`crate::rag`) keeps the conversation, and not the context, in the KV.
///
/// `retain_prefix_bytes` is a byte offset into the prompt text sent to the
/// head. Everything before it is the same for every request with this
/// question (instructions and answer format); the state and the options come
/// after it. The head keeps only that part in the retained KV sequence, so the
/// next decision with the same question reuses it instead of prefilling it
/// again. `question`, `question_type` and `options` never leave the gateway:
/// they only label the `jev` trace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecisionProbe {
    pub labels: Vec<String>,
    pub retain_prefix_bytes: Option<usize>,
    pub question: Option<String>,
    pub question_type: Option<String>,
    pub options: Vec<String>,
}

impl DecisionProbe {
    /// The `jev` object of the request trace, from the last peer's
    /// `probe_result` and the prefill measurements.
    pub fn trace_summary(
        &self,
        probe_result: &serde_json::Value,
        prefill_ms: u64,
        cached_tokens: u32,
    ) -> serde_json::Value {
        let raw: Vec<f64> = probe_result["probabilities"]
            .as_array()
            .map(|values| values.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect())
            .unwrap_or_default();
        let total: f64 = raw.iter().sum();
        let normalized: Vec<f64> = raw
            .iter()
            .map(|p| if total > 0.0 { p / total } else { 0.0 })
            .collect();
        let best = normalized.iter().enumerate().fold(
            None,
            |best: Option<(usize, f64)>, (i, p)| match best {
                Some((_, top)) if top >= *p => best,
                _ => Some((i, *p)),
            },
        );
        let round = |value: f64| (value * 1e6).round() / 1e6;
        let probabilities: serde_json::Map<String, serde_json::Value> = self
            .labels
            .iter()
            .zip(&normalized)
            .map(|(label, p)| (label.clone(), json!(round(*p))))
            .collect();
        let mut summary = json!({
            "enabled": true,
            "candidates": self.labels.len(),
            "labels": self.labels,
            "probabilities": probabilities,
            "mass": round(probe_result["mass"].as_f64().unwrap_or(0.0)),
            "probe_tokens": 1,
            "prefill_ms": prefill_ms,
            "cache_hit_tokens": cached_tokens,
        });
        if let Some(ms) = probe_result["probe_ms"].as_f64() {
            summary["probe_ms"] = json!(round(ms));
        }
        if let Some((index, p)) = best {
            summary["selected"] = json!(self.labels.get(index));
            summary["confidence"] = json!(round(p));
            if let Some(option) = self.options.get(index) {
                summary["selected_option"] = json!(option);
            }
        }
        if let Some(question) = &self.question {
            summary["question"] = json!(question);
        }
        if let Some(kind) = &self.question_type {
            summary["question_type"] = json!(kind);
        }
        if !self.options.is_empty() {
            summary["options"] = json!(self.options);
        }
        summary
    }
}

/// Carries the labels of a typed decision to the last peer, which answers with
/// the probability of each one in `probe_result`, and tells the head how far the
/// prompt is reusable (`retain_prefix_bytes`).
pub(super) fn add_probe_to_head_meta(
    mut meta: serde_json::Value,
    probe: Option<&DecisionProbe>,
) -> serde_json::Value {
    if let Some(probe) = probe {
        if !probe.labels.is_empty() {
            meta["probe"] = json!(probe.labels);
        }
        // The retain boundary stands on its own: a request whose prompt ends
        // with retrieved context (`crate::rag`) carries it without labels.
        if let Some(bytes) = probe.retain_prefix_bytes {
            meta["retain_prefix_bytes"] = json!(bytes);
        }
    }
    meta
}

/// Encodes the images only in the first frame sent to the head. The projector
/// runs there; the other peers receive only the hidden state.
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

/// Prefill positions are absolute in the session, not derivable from the chunk's
/// logical index. A short prompt may end at token 42 and the following chunk 1
/// starts at 42, not at `1 * chunk_size`.
pub(super) fn required_prefill_position(
    meta: &serde_json::Value,
    field: &str,
    chunk_index: u32,
) -> Result<u32> {
    meta.get(field)
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .ok_or_else(|| {
            anyhow!("peer omitted {field} at chunk {chunk_index}; absolute position required")
        })
}

/// On the first cold chunk, zero is a known absolute position. In all
/// other cases, inventing the position from the index may mix KV of
/// different sessions and must abort the reuse path.
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

/// How many prompt tokens the head reported having reused in this chunk.
///
/// Only chunk 0 carries the field, and only the head fills it in. A peer that does not send it
/// (or that restarted the session) counts zero, which is the right conservative
/// reading: cache telemetry must never invent savings.
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

/// Whether the head actually reused the prefix. See `cached_tokens_of`.
pub(super) fn head_reused_prefix(meta: &serde_json::Value, requested: bool) -> bool {
    meta.get("reuse_prefix")
        .and_then(|value| value.as_bool())
        .unwrap_or(requested)
}

/// In direct-response mode, the head's ACK carries the prompt progression
/// inside `prompt_progress`: the complete response arrives later,
/// delivered by the tail. The next chunk must be dispatched using that
/// published position, even when both chunks are in flight at the same
/// time.
pub(super) fn direct_prefill_progress(meta: &serde_json::Value) -> &serde_json::Value {
    meta.get("prompt_progress")
        .filter(|progress| progress.is_object())
        .unwrap_or(meta)
}
