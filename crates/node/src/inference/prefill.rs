//! Feeding the prompt through the pipeline in chunks.
//!
//! The prompt used to cross as one block: peer 1 decoded every token in a
//! single `llama_batch` and shipped the whole hidden-state tensor to peer 2.
//! That made memory scale with the prompt on both sides, forced the prompt to
//! fit in `n_batch`, and put a multi-gigabyte body on the wire — on an internet
//! link, a request that dies.
//!
//! Here the prompt advances in chunks. Each chunk is decoded at its own
//! position and its hidden state travels on its own, which means:
//!
//! * memory stops scaling with the prompt;
//! * a lost chunk costs one chunk, not the request — it is simply resent, and
//!   the receiving peer rewinds its KV cache to that chunk's position so a
//!   repeat is never counted twice;
//! * the prompt no longer has to fit in a single batch.
//!
//! What travels is defined in [`crate::wire`], where precision and encoding are
//! a parameter rather than an assumption.

use crate::infer_server::Speculate;
use crate::infer_tunnel::TunnelOutboundSender;
use crate::pipeline::PipelinePlan;
use crate::wire::{FrameEncoding, WireDtype};
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::Notify;
use tracing::info;

/// Prompt tokens pushed through the pipeline per round trip.
///
/// The chunk is the unit of retry and the unit of memory, so it wants to be
/// small; it is also one round trip per peer, so it wants to be large. 512
/// tokens is 8 MiB of f16 hidden state per hop — about a second on the link
/// this cluster actually has, which keeps a retry cheap while leaving the
/// per-chunk overhead well under the transfer time it covers.
pub const DEFAULT_CHUNK_TOKENS: u32 = 512;

/// Cancelamento compartilhado entre o stream HTTP e o prefill. O `Notify`
/// acorda uma chamada em voo; o `AtomicBool` torna as checagens entre hops e
/// chunks baratas e sem await.
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
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

    async fn cancelled(&self) {
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

fn check_cancelled(cancellation: &Cancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(anyhow::Error::new(GenerationCancelled));
    }
    Ok(())
}

async fn cancelable<F, T>(cancellation: &Cancellation, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        result = future => result,
        _ = cancellation.cancelled() => Err(anyhow::Error::new(GenerationCancelled)),
    }
}

/// Attempts per chunk before the request is given up on. Lives in `relay.rs`
/// (`crate::relay::CHUNK_ATTEMPTS`) since a ring-mode relaying hop needs the
/// same number for its own outbound prefill calls — a prefill chunk is
/// retry-safe (the KV-rollback idempotency in `infer_server.rs`), a decode
/// step is not, so the two must not share one constant that later drifts.
use crate::relay::CHUNK_ATTEMPTS;

/// What the last peer produced once the whole prompt is in.
#[derive(Debug, Clone)]
pub struct PrefillOutcome {
    pub token_id: u32,
    pub text: String,
    pub done: bool,
    /// Prompt length as counted by the peer that tokenised it.
    pub prompt_tokens: u32,
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
    node_samples_ms: Vec<f64>,
    #[serde(skip)]
    selected_path_seen: bool,
}

impl PhaseTrace {
    fn record(
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

        self.node_samples_ms.push(node_decode_ms);
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

    fn observe_moe_executor(&mut self, next: Option<String>) {
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

    fn observe_engine(&mut self, next: crate::executor::ExecutionEngine) {
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

    fn observe_path(&mut self, next: Option<String>) {
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

    fn refresh_engine_mismatch(&mut self) {
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
        self.engine_mismatch = expected != crate::executor::ExecutionEngine::Unknown
            && !self.executed_engines.contains(&expected);
    }

    fn merge(&mut self, next: &PhaseTrace) {
        self.processing_ms += next.processing_ms;
        self.round_trip_ms += next.round_trip_ms;
        self.network_ms += next.network_ms;
        self.bytes_sent += next.bytes_sent;
        self.bytes_received += next.bytes_received;
        self.calls += next.calls;
        self.client_encode_ms += next.client_encode_ms;
        self.response_headers_ms += next.response_headers_ms;
        self.response_body_ms += next.response_body_ms;
        self.client_decode_ms += next.client_decode_ms;
        self.tls_calls += next.tls_calls;
        self.tls_handshake_ms += next.tls_handshake_ms;
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
        self.node_samples_ms
            .extend_from_slice(&next.node_samples_ms);
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
            self.moe_executor_diagnostics = next.moe_executor_diagnostics;
        }
        self.refresh_percentiles();
    }

    fn refresh_percentiles(&mut self) {
        self.node_p50_ms = percentile(&self.node_samples_ms, 50);
        self.node_p95_ms = percentile(&self.node_samples_ms, 95);
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

#[derive(Debug, Clone, Serialize)]
pub struct RequestTrace {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub prefill_ms: u64,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub hops: Vec<HopTrace>,
    pub issues: Vec<TraceIssue>,
    pub dropped_issues: u32,
    pub complete: bool,
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
struct BatchRouteKey {
    model: String,
    deployment_id: String,
    phase: Phase,
    config_key: String,
    next_hop_id: String,
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

const TRACE_ISSUE_LIMIT: usize = 32;

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
        Self {
            model: model.into(),
            input_tokens: 0,
            output_tokens: 0,
            prefill_ms: 0,
            total_bytes_sent: 0,
            total_bytes_received: 0,
            issues: Vec::new(),
            dropped_issues: 0,
            complete: false,
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

    pub fn from_plan(model: &str, plan: &PipelinePlan) -> Self {
        let steps: Vec<_> = plan
            .steps
            .iter()
            .map(|step| (step.node_id.as_str(), step.start_layer, step.end_layer))
            .collect();
        Self::new(model, &steps)
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
        self.total_bytes_sent += bytes_sent;
        self.total_bytes_received += bytes_received;
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
    }

    pub fn merge(&mut self, other: &RequestTrace) {
        self.total_bytes_sent += other.total_bytes_sent;
        self.total_bytes_received += other.total_bytes_received;
        for (current, next) in self.hops.iter_mut().zip(&other.hops) {
            current.prefill.merge(&next.prefill);
            current.decode.merge(&next.decode);
        }
    }
}

fn percentile(samples: &[f64], percentile: u64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let index = ((sorted.len() - 1) * percentile as usize + 50) / 100;
    sorted[index]
}

/// Push the whole prompt through the pipeline, chunk by chunk.
fn head_prefill_meta(
    session_id: &str,
    chunk_index: u32,
    chunk_size: u32,
    is_last: bool,
    prompt: Option<&str>,
    chat_template: bool,
    reuse_prefix: bool,
    start_pos: Option<u32>,
) -> serde_json::Value {
    json!({
        "session_id": session_id,
        "chunk_index": chunk_index,
        "chunk_size": chunk_size,
        "is_last": is_last,
        "is_final_chunk": false,
        "prompt": prompt,
        "chat_template": chat_template,
        "reuse_prefix": reuse_prefix,
        "start_pos": start_pos,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
) -> Result<PrefillOutcome> {
    check_cancelled(cancellation)?;
    let chunk_size = chunk_size.max(1);
    let n_peers = plan.steps.len();
    if n_peers == 0 {
        return Err(anyhow!("pipeline sem peers"));
    }
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return run_direct(
            plan,
            model,
            session_id,
            prompt,
            chat_template,
            chunk_size,
            dtype,
            http,
            routes,
            direct,
            tunnel,
            cancellation,
            reuse_prefix,
        )
        .await;
    }
    if plan.supports_relay_chain() {
        return run_ring(
            plan,
            model,
            session_id,
            prompt,
            chat_template,
            chunk_size,
            dtype,
            http,
            routes,
            tunnel,
            cancellation,
            reuse_prefix,
        )
        .await;
    }

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let mut next_start_pos: Option<u32> = None;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        check_cancelled(cancellation)?;
        // ── First peer: it owns the tokenizer and decides whether more remain.
        let head_meta = head_prefill_meta(
            session_id,
            chunk_index,
            chunk_size,
            n_peers == 1,
            (chunk_index == 0).then_some(prompt),
            chat_template,
            reuse_prefix && chunk_index == 0,
            next_start_pos,
        );

        let (mut meta, mut tensor) = send_chunk(
            http,
            routes,
            &plan.steps[0],
            tunnel,
            &head_meta,
            None,
            dtype,
            FrameEncoding::F16,
            chunk_index,
            &mut trace,
            0,
            Phase::Prefill,
            cancellation,
        )
        .await?;

        if let Some(n) = meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
            prompt_tokens = n as u32;
        }
        let more = meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_final_chunk = !more;
        let chunk_start_pos = meta
            .get("start_pos")
            .and_then(|value| value.as_u64())
            .unwrap_or((chunk_index * chunk_size) as u64) as u32;
        let chunk_next_pos =
            meta.get("next_pos")
                .and_then(|value| value.as_u64())
                .unwrap_or(chunk_start_pos as u64 + chunk_size as u64) as u32;

        // ── The rest of the pipeline, carrying this chunk's hidden state.
        for (i, step) in plan.steps.iter().enumerate().skip(1) {
            check_cancelled(cancellation)?;
            let is_last = i == n_peers - 1;
            let Some((values, shape)) = tensor.take() else {
                return Err(anyhow!(
                    "peer {} não devolveu estado oculto no pedaço {chunk_index}",
                    plan.steps[i - 1].node_id
                ));
            };
            let mut body_meta = json!({
                "session_id": session_id,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "is_last": is_last,
                "is_final_chunk": is_final_chunk,
                "reuse_prefix": reuse_prefix && chunk_index == 0,
                "start_pos": chunk_start_pos,
                "scale": 1.0,
            });
            if let Some(tokens) = meta.get("tokens") {
                body_meta["tokens"] = tokens.clone();
            }
            let (m, t) = send_chunk(
                http,
                routes,
                step,
                tunnel,
                &body_meta,
                Some((&values, shape)),
                dtype,
                step.frame_encoding(),
                chunk_index,
                &mut trace,
                i,
                Phase::Prefill,
                cancellation,
            )
            .await?;
            meta = m;
            tensor = t;
        }

        if is_final_chunk {
            let token_id = meta
                .get("token_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
                as u32;
            let chunks = chunk_index + 1;
            info!(
                "prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens ({prompt_tokens} tokens)"
            );
            return Ok(PrefillOutcome {
                token_id,
                text: meta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                done: meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
                prompt_tokens,
                chunks,
                trace: {
                    trace.input_tokens = prompt_tokens;
                    trace.prefill_ms = started.elapsed().as_millis() as u64;
                    trace
                },
            });
        }

        next_start_pos = Some(chunk_next_pos);
        chunk_index += 1;
    }
}

/// Same chunk-by-chunk loop as `run`, but each chunk makes exactly one
/// outbound call — to the head — instead of one call per hop. The head
/// relays to its own next hop, which relays to its own, and so on; the
/// eventual answer (and, for the head, its authoritative `more_chunks`/
/// `n_prompt_tokens` — see the overlay fix in `infer_server.rs`'s
/// `handle_prefill`) bubbles back through that same chain of calls.
#[allow(clippy::too_many_arguments)]
async fn run_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let mut next_start_pos: Option<u32> = None;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        check_cancelled(cancellation)?;
        let mut head_meta = head_prefill_meta(
            session_id,
            chunk_index,
            chunk_size,
            next_hops.is_empty(),
            (chunk_index == 0).then_some(prompt),
            chat_template,
            reuse_prefix && chunk_index == 0,
            next_start_pos,
        );
        head_meta["next_hops"] = json!(next_hops);
        head_meta["exec_start"] = json!(head.exec_start);
        head_meta["exec_end"] = json!(head.exec_end);

        let sent = cancelable(
            cancellation,
            crate::relay::send_frame_or_tunnel(
                http,
                head,
                tunnel,
                "/v1/prefill",
                &head_meta,
                None,
                dtype,
                head.frame_encoding(),
                crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
                &head.node_id,
                |_issue| {},
            ),
        )
        .await
        .map_err(|e| {
            if !is_cancelled(&e) {
                routes.mark_failure(&head.node_id, &head.inference_url);
            }
            ring_relay_error(&head.node_id, e)
        })?;

        for (i, entry) in crate::relay::bubble_hop_timings(&sent)
            .into_iter()
            .enumerate()
        {
            trace.record_hop_from_json(i, Phase::Prefill, &entry);
        }

        if let Some(n) = sent.meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
            prompt_tokens = n as u32;
        }
        let more = sent
            .meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_final_chunk = !more;
        let chunk_next_pos = sent
            .meta
            .get("next_pos")
            .and_then(|value| value.as_u64())
            .unwrap_or(chunk_index as u64 * chunk_size as u64 + chunk_size as u64)
            as u32;

        if is_final_chunk {
            let token_id = sent
                .meta
                .get("token_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
                as u32;
            let chunks = chunk_index + 1;
            info!(
                "prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens ({prompt_tokens} tokens) [anel]"
            );
            return Ok(PrefillOutcome {
                token_id,
                text: sent
                    .meta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                done: sent
                    .meta
                    .get("done")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                prompt_tokens,
                chunks,
                trace: {
                    trace.input_tokens = prompt_tokens;
                    trace.prefill_ms = started.elapsed().as_millis() as u64;
                    trace
                },
            });
        }

        next_start_pos = Some(chunk_next_pos);
        chunk_index += 1;
    }
}

/// Same chunk-by-chunk loop as `run_ring`, but the chain does not come back:
/// each chunk is dispatched once, to the head, carrying a `reply_to`, and the
/// tail delivers the answer straight to this process (see [`crate::direct`]).
/// One less crossing of the link per chunk than the ring, and — the point of
/// the whole thing — a cost that no longer depends on whether the head happens
/// to live on the machine that took the request. A prefill chunk is ~5 MB of
/// hidden state, so this is also most of the time to the first word.
#[allow(clippy::too_many_arguments)]
async fn run_direct(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let mut next_start_pos: Option<u32> = None;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        check_cancelled(cancellation)?;
        let mut ticket = direct.ticket();
        let mut head_meta = head_prefill_meta(
            session_id,
            chunk_index,
            chunk_size,
            next_hops.is_empty(),
            (chunk_index == 0).then_some(prompt),
            chat_template,
            reuse_prefix && chunk_index == 0,
            next_start_pos,
        );
        head_meta["next_hops"] = json!(next_hops);
        head_meta["reply_to"] = direct.reply_to(ticket.id());
        head_meta["exec_start"] = json!(head.exec_start);
        head_meta["exec_end"] = json!(head.exec_end);

        let dispatched = Instant::now();
        let ack = cancelable(
            cancellation,
            crate::relay::send_frame_or_tunnel(
                http,
                head,
                tunnel,
                "/v1/prefill",
                &head_meta,
                None,
                dtype,
                head.frame_encoding(),
                crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
                &head.node_id,
                |_issue| {},
            ),
        );
        let meta = cancelable(
            cancellation,
            crate::direct::await_result(ack, &mut ticket, &head.node_id),
        )
        .await
        .map_err(|e| {
            if !is_cancelled(&e) {
                routes.mark_failure(&head.node_id, &head.inference_url);
            }
            ring_relay_error(&head.node_id, e)
        })?;
        if let Some(error) = direct_failure(plan, routes, &meta) {
            return Err(error);
        }
        let chunk_next_pos = meta
            .get("next_pos")
            .and_then(|value| value.as_u64())
            .unwrap_or(chunk_index as u64 * chunk_size as u64 + chunk_size as u64)
            as u32;

        record_direct_hops(
            &mut trace,
            Phase::Prefill,
            &meta,
            dispatched.elapsed().as_millis() as u64,
        );

        if let Some(n) = meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
            prompt_tokens = n as u32;
        }
        let more = meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !more {
            let token_id = meta
                .get("token_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
                as u32;
            let chunks = chunk_index + 1;
            info!(
                "prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens \
                 ({prompt_tokens} tokens) [resposta direta]"
            );
            return Ok(PrefillOutcome {
                token_id,
                text: meta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                done: meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
                prompt_tokens,
                chunks,
                trace: {
                    trace.input_tokens = prompt_tokens;
                    trace.prefill_ms = started.elapsed().as_millis() as u64;
                    trace
                },
            });
        }

        next_start_pos = Some(chunk_next_pos);
        chunk_index += 1;
    }
}

/// A cadeia respondeu, mas com a falha de um hop no lugar do resultado.
///
/// Vale mais que o prazo estourado: nomeia o nó que morreu — que quase nunca é
/// aquele para quem despachamos — e marca **esse** para nova medição de rota.
fn direct_failure(
    plan: &PipelinePlan,
    routes: &crate::route::RouteTable,
    meta: &serde_json::Value,
) -> Option<anyhow::Error> {
    let failure = crate::direct::result_failure(meta)?;
    if let Some(step) = plan
        .steps
        .iter()
        .find(|s| s.node_id == failure.failed_node_id)
    {
        routes.mark_failure(&step.node_id, &step.inference_url);
    }
    Some(anyhow!(
        "pipeline falhou em {}: {} — {}",
        failure.failed_node_id,
        failure.kind,
        failure.message
    ))
}

/// Dobra a telemetria acumulada na ida para dentro do `RequestTrace`, na mesma
/// forma que o anel produzia na volta — ver `crate::direct::credit_transport`
/// para o que só o gateway sabe e por que ele lança onde lança.
fn record_direct_hops(
    trace: &mut RequestTrace,
    phase: Phase,
    meta: &serde_json::Value,
    total_ms: u64,
) {
    let mut hops: Vec<serde_json::Value> = meta
        .get("hop_timings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let result_bytes = meta
        .get("result_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    crate::direct::credit_transport(&mut hops, total_ms, result_bytes);
    for (i, entry) in hops.iter().enumerate() {
        trace.record_hop_from_json(i, phase, entry);
    }
}

/// One chunk to one peer, retried on transport failure.
///
/// Retrying is safe because the peer rewinds its KV cache to this chunk's
/// position before redoing it — a repeat can never be counted twice. Thin
/// wrapper over `relay::send_frame`, which also backs the ring-mode relay
/// calls a hop makes to its own next hop (`infer_server.rs`); this function
/// just supplies the `/v1/prefill` path, the retry count, and turns
/// `SendIssue`s into `TraceIssue`s on the gateway's `RequestTrace`.
#[allow(clippy::too_many_arguments)]
async fn send_chunk(
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    meta: &serde_json::Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    chunk_index: u32,
    trace: &mut RequestTrace,
    hop: usize,
    phase: Phase,
    cancellation: &Cancellation,
) -> Result<(serde_json::Value, Option<(Vec<f32>, [usize; 2])>)> {
    check_cancelled(cancellation)?;
    let node_id = &step.node_id;
    let peer_label = format!("{node_id} (pedaço {chunk_index})");
    let mut issues = Vec::new();
    let result = cancelable(
        cancellation,
        crate::relay::send_frame_or_tunnel(
            http,
            step,
            tunnel,
            "/v1/prefill",
            meta,
            tensor,
            dtype,
            encoding,
            CHUNK_ATTEMPTS,
            &peer_label,
            |issue| issues.push(issue),
        ),
    )
    .await;

    for issue in issues {
        trace.record_issue(TraceIssue::new(
            Some(hop),
            node_id,
            phase,
            match issue.kind {
                crate::relay::SendIssueKind::Timeout => TraceIssueKind::Timeout,
                crate::relay::SendIssueKind::Transport => TraceIssueKind::Transport,
                crate::relay::SendIssueKind::Http => TraceIssueKind::Http,
                crate::relay::SendIssueKind::Protocol => TraceIssueKind::Protocol,
            },
            issue.attempt,
            issue.message,
        ));
    }

    let sent = result.map_err(|e| {
        // Esperar o ciclo de 5 minutos significaria falhar tudo que chegasse
        // nesse meio tempo. A proxima medicao reescolhe. Uma recuperação bem
        // sucedida via túnel nunca chega aqui — só o insucesso final marca a
        // rota como ruim.
        if !is_cancelled(&e) {
            routes.mark_failure(node_id, &step.inference_url);
        }
        e
    })?;
    let processing_ms = sent
        .meta
        .get("processing_ms")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    trace.record_hop(
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
    Ok((sent.meta, sent.tensor))
}

/// One generated token through the whole pipeline, framed like the prefill.
///
/// `speculate` é o convite a rascunhar que o gateway anexa no caminho fatiado
/// (MTP): `None` mantém a rodada no formato antigo de um token; `Some` faz a
/// cabeça montar o lote — com o rascunho pronto da cauda (`draft`) ou
/// produzindo o próprio. A resposta é sempre a mesma `SpeculativeStep`: uma
/// rodada sem especulação tem `accepted.len() == 1` e `speculated == false`.
#[allow(clippy::too_many_arguments)]
pub async fn step(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
) -> Result<SpeculativeStep> {
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return step_direct(
            plan, model, session_id, token_id, dtype, http, routes, direct, tunnel, speculate,
        )
        .await;
    }
    if plan.supports_relay_chain() {
        return step_ring(
            plan, model, session_id, token_id, dtype, http, routes, tunnel, speculate,
        )
        .await;
    }

    // O caminho hub também passa pelo relay comum. Além de manter pinning,
    // credencial e telemetria iguais ao anel, isso permite agrupar chamadas
    // concorrentes que chegam ao mesmo hop.
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, [usize; 2])> = None;
    let mut meta = serde_json::Value::Null;
    let mut trace = RequestTrace::from_plan(model, plan);

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let body_meta = if i == 0 {
            let mut m = json!({
                "session_id": session_id,
                "is_last": is_last,
                "token_id": token_id,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            if let Some(spec) = &speculate {
                m["speculate"] = spec.to_meta();
            }
            m
        } else {
            let mut m = json!({
                "session_id": session_id,
                "is_last": is_last,
                "scale": 1.0,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            // Thread o que a cabeça decidiu na ida — o rascunho, a composição
            // do lote (`tokens`, para o `observe` da cauda) e a largura de
            // lote da próxima rodada (`mtp_k`).
            if let Some(draft) = meta.get("draft") {
                m["draft"] = draft.clone();
            }
            if let Some(tokens) = meta.get("tokens") {
                m["tokens"] = tokens.clone();
            }
            if let Some(mtp_k) = meta.get("mtp_k") {
                m["mtp_k"] = mtp_k.clone();
            }
            m
        };

        let payload = tensor.take();
        let sent = crate::relay::send_frame_or_tunnel(
            http,
            peer,
            tunnel,
            "/v1/step",
            &body_meta,
            payload
                .as_ref()
                .map(|(values, shape)| (values.as_slice(), *shape)),
            dtype,
            peer.frame_encoding(),
            1,
            &peer.node_id,
            |_issue| {},
        )
        .await
        .map_err(|error| {
            routes.mark_failure(&peer.node_id, &peer.inference_url);
            anyhow!("peer {} não respondeu: {error}", peer.node_id)
        })?;
        let processing_ms = sent
            .meta
            .get("processing_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        trace.record_hop(
            i,
            Phase::Decode,
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
        meta = sent.meta;
        tensor = sent.tensor;
    }

    speculative_step_from_meta(&meta, trace)
}

/// One generated token, relayed hop-to-hop instead of gateway-to-every-hop —
/// see `PipelinePlan::supports_relay_chain`. This process makes exactly one
/// outbound call (to the head); every other hop is reached by the head (and,
/// transitively, each hop after it) relaying directly, and the tail's answer
/// bubbles all the way back through that same chain of calls.
#[allow(clippy::too_many_arguments)]
async fn step_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
) -> Result<SpeculativeStep> {
    let mut trace = RequestTrace::from_plan(model, plan);
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut body_meta = json!({
        "session_id": session_id,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(spec) = &speculate {
        body_meta["speculate"] = spec.to_meta();
    }

    let sent = crate::relay::send_frame_or_tunnel(
        http,
        head,
        tunnel,
        "/v1/step",
        &body_meta,
        None,
        dtype,
        head.frame_encoding(),
        1, // decode has never retried — see relay::send_frame's doc comment
        &head.node_id,
        |_issue| {},
    )
    .await
    .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;

    for (i, entry) in crate::relay::bubble_hop_timings(&sent)
        .into_iter()
        .enumerate()
    {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    speculative_step_from_meta(&sent.meta, trace)
}

/// One generated token, dispatched once and answered once — the decode twin of
/// [`run_direct`].
///
/// This process makes exactly one outbound call, to the head, and then waits on
/// the ticket. The head relays hop to hop and the tail hands the token straight
/// back here, so the token crosses each link once instead of once out and once
/// back. That is the whole difference between `2N` traversals and `N + 1`, and
/// the reason the tokens-per-second stops depending on which machine happens to
/// hold layer 0.
#[allow(clippy::too_many_arguments)]
async fn step_direct(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
) -> Result<SpeculativeStep> {
    let mut trace = RequestTrace::from_plan(model, plan);
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut ticket = direct.ticket();
    let mut body_meta = json!({
        "session_id": session_id,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
        "reply_to": direct.reply_to(ticket.id()),
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(spec) = &speculate {
        body_meta["speculate"] = spec.to_meta();
    }

    let dispatched = Instant::now();
    let ack = crate::relay::send_frame_or_tunnel(
        http,
        head,
        tunnel,
        "/v1/step",
        &body_meta,
        None,
        dtype,
        head.frame_encoding(),
        1, // decode has never retried — see relay::send_frame's doc comment
        &head.node_id,
        |_issue| {},
    );
    let meta = crate::direct::await_result(ack, &mut ticket, &head.node_id)
        .await
        .map_err(|e| {
            routes.mark_failure(&head.node_id, &head.inference_url);
            ring_relay_error(&head.node_id, e)
        })?;
    if let Some(error) = direct_failure(plan, routes, &meta) {
        return Err(error);
    }

    record_direct_hops(
        &mut trace,
        Phase::Decode,
        &meta,
        dispatched.elapsed().as_millis() as u64,
    );

    speculative_step_from_meta(&meta, trace)
}

/// If the failure carries a `RelayFailure` from further down the chain (a
/// hop's own outbound call to *its* next hop failed), name the peer that
/// actually failed instead of `head_node_id` — the one this process
/// happened to call directly.
fn ring_relay_error(head_node_id: &str, e: anyhow::Error) -> anyhow::Error {
    match crate::relay::extract_relay_failure(&e) {
        Some(failure) => anyhow!(
            "pipeline falhou em {} (retransmitido por {head_node_id}): {} — {}",
            failure.failed_node_id,
            failure.kind,
            failure.message
        ),
        None => e,
    }
}

// ── Speculative decoding (prompt-lookup) ────────────────────────────────────

/// Outcome of one speculative round: possibly several confirmed tokens
/// instead of the usual one, in generation order.
#[derive(Debug)]
pub struct SpeculativeStep {
    pub accepted: Vec<(u32, String)>,
    pub done: bool,
    /// How many of the batch's positions the tail confirmed — passed
    /// straight to `gateway::rollback_sessions` afterward.
    pub accept_count: u32,
    /// How many positions the head actually decoded (`1 + draft.len()`).
    pub batch_width: u32,
    /// Whether this round actually was speculative.
    ///
    /// The head is free to decline — `should_draft` says no for an MTP round
    /// with no local `nextn` head and no premade draft yet (round one of a
    /// sliced pipeline) — and it says so by omitting `draft`, which puts the
    /// whole round back on the plain one-token wire format (see
    /// `step_blocking`'s `incoming_draft` doc). `false` means exactly that
    /// happened, and asking the same chain to speculate again would just
    /// repeat the refusal.
    pub speculated: bool,
    /// Rascunho que a cauda de um pipeline fatiado produziu para a **próxima**
    /// rodada (bloco `nextn`), trazido na resposta direta. O gateway o guarda
    /// e o injeta no `speculate.draft` da rodada seguinte — zero travessia
    /// extra. `None` quando esta rodada não foi uma cauda de pipeline, ou a
    /// geração terminou.
    pub next_draft: Option<Vec<i32>>,
    pub trace: RequestTrace,
}

/// One speculative round: the head drafts up to `k` tokens from its own
/// recent history (searching a trailing window of `m`), the whole batch is
/// decoded through the pipeline in a single pass, and the tail verifies it —
/// see `infer_server.rs`'s `step_blocking` for where the actual drafting and
/// verification happen; this function only orchestrates getting the batch
/// there and the answer back, exactly like `step` does for a single token.
/// Composes with either relay mode transparently: ring when negotiated,
/// hub otherwise — same rule `step`/`run` already follow.
#[allow(clippy::too_many_arguments)]
pub async fn step_speculative(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<SpeculativeStep> {
    if plan.supports_relay_chain() {
        return step_speculative_ring(
            plan, model, session_id, token_id, k, m, source, dtype, http, routes, tunnel,
        )
        .await;
    }
    step_speculative_hub(
        plan, model, session_id, token_id, k, m, source, dtype, http, routes, tunnel,
    )
    .await
}

/// De onde a cabeça deve tirar o rascunho desta rodada.
///
/// Viaja no quadro (`speculate.source`) porque quem decide é o gateway, que é
/// o único que vê o plano inteiro e as capacidades anunciadas, e quem executa
/// é a cabeça. Ver `infer_server::Speculate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftSource {
    /// `find_draft`: repetição no texto já gerado.
    Lookup,
    /// O bloco `nextn` do próprio modelo (`crate::mtp`).
    Mtp,
}

impl DraftSource {
    fn wire(self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn step_speculative_hub(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<SpeculativeStep> {
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, [usize; 2])> = None;
    let mut meta = serde_json::Value::Null;
    let mut trace = RequestTrace::from_plan(model, plan);

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let body_meta = if i == 0 {
            json!({
                "session_id": session_id,
                "is_last": is_last,
                "token_id": token_id,
                "speculate": {"k": k, "m": m, "source": source.wire()},
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            })
        } else {
            let mut m = json!({
                "session_id": session_id,
                "is_last": is_last,
                "scale": 1.0,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            // Keep threading whatever the head decided — see `step_blocking`'s
            // `draft`/`incoming_draft` doc comments for why absent vs. an
            // empty array mean different things here.
            if let Some(draft) = meta.get("draft") {
                m["draft"] = draft.clone();
            }
            m
        };

        let payload = tensor.take();
        let as_ref = payload.as_ref().map(|(v, s)| (v.as_slice(), *s));
        let peer_label = format!("{} (especulativo)", peer.node_id);
        let sent = crate::relay::send_frame_or_tunnel(
            http,
            peer,
            tunnel,
            "/v1/step",
            &body_meta,
            as_ref,
            dtype,
            peer.frame_encoding(),
            1, // decode has never retried — see relay::send_frame's doc comment
            &peer_label,
            |_issue| {},
        )
        .await
        .map_err(|e| {
            routes.mark_failure(&peer.node_id, &peer.inference_url);
            e
        })?;

        let processing_ms = sent
            .meta
            .get("processing_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        trace.record_hop(
            i,
            Phase::Decode,
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
        meta = sent.meta;
        tensor = sent.tensor;
    }

    speculative_step_from_meta(&meta, trace)
}

#[allow(clippy::too_many_arguments)]
async fn step_speculative_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<SpeculativeStep> {
    let mut trace = RequestTrace::from_plan(model, plan);
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let head_meta = json!({
        "session_id": session_id,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "speculate": {"k": k, "m": m, "source": source.wire()},
        "next_hops": next_hops,
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });

    let sent = crate::relay::send_frame_or_tunnel(
        http,
        head,
        tunnel,
        "/v1/step",
        &head_meta,
        None,
        dtype,
        head.frame_encoding(),
        1,
        &head.node_id,
        |_issue| {},
    )
    .await
    .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;

    for (i, entry) in crate::relay::bubble_hop_timings(&sent)
        .into_iter()
        .enumerate()
    {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    speculative_step_from_meta(&sent.meta, trace)
}

/// Parse the tail's reply to a speculative round — shared by the hub and ring
/// paths, which only differ in how they get to that reply.
///
/// Two shapes are legitimate, and reading only the first one is what killed
/// every generation on a round the head declined to speculate:
///
/// * `{done, accepted, accept_count, batch_width}` — the tail verified a
///   batch, which is what happens when the head drafted;
/// * `{done, token_id, text}` — the plain one-token reply, which is what the
///   protocol *requires* when `draft` never reached the tail because the head
///   declined to draft (`should_draft`).
///
/// `batch_width: 1` on the second shape is not a guess. The tail answers in
/// that shape only when no `draft` arrived, every relay branch threads `draft`
/// through untouched (`handle_step`, `forward_direct`, `step_speculative_hub`),
/// and the head omits it only when it also batched a single token. So nothing
/// beyond the seed token was ever decoded, and there is no KV position to roll
/// back.
fn speculative_step_from_meta(
    meta: &serde_json::Value,
    trace: RequestTrace,
) -> Result<SpeculativeStep> {
    let done = meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
    // O rascunho da cauda para a próxima rodada (pipeline fatiado) — presente
    // nas duas formas de resposta quando a cauda tem o bloco `nextn`.
    let next_draft: Option<Vec<i32>> = meta.get("next_draft").and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|v| v.as_i64())
            .map(|v| v as i32)
            .collect()
    });
    let accepted: Vec<(u32, String)> = meta
        .get("accepted")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = entry.get("token_id")?.as_u64()? as u32;
                    let text = entry.get("text")?.as_str()?.to_string();
                    Some((id, text))
                })
                .collect()
        })
        .unwrap_or_default();

    if accepted.is_empty() {
        let Some(token_id) = meta.get("token_id").and_then(|v| v.as_u64()) else {
            return Err(anyhow!(
                "último peer não devolveu token nenhum na rodada especulativa"
            ));
        };
        return Ok(SpeculativeStep {
            accepted: vec![(
                token_id as u32,
                meta.get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )],
            done,
            accept_count: 1,
            batch_width: 1,
            speculated: false,
            next_draft,
            trace,
        });
    }

    let accept_count = meta
        .get("accept_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(accepted.len() as u64) as u32;
    let batch_width = meta
        .get("batch_width")
        .and_then(|v| v.as_u64())
        .unwrap_or(accept_count.max(1) as u64) as u32;
    Ok(SpeculativeStep {
        accepted,
        done,
        accept_count,
        batch_width,
        speculated: true,
        next_draft,
        trace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelamento_acorda_chamada_em_voo() {
        let cancellation = Cancellation::new();
        cancellation.cancel();

        let error = cancelable(&cancellation, async {
            std::future::pending::<Result<(), anyhow::Error>>().await
        })
        .await
        .unwrap_err();
        assert!(is_cancelled(&error));
    }

    #[test]
    fn chat_head_frame_delivers_prompt_with_template_mode() {
        let meta = head_prefill_meta(
            "session",
            0,
            512,
            true,
            Some("Oi, teste"),
            true,
            false,
            None,
        );
        assert_eq!(meta["prompt"], "Oi, teste");
        assert_eq!(meta["chat_template"], true);
    }

    fn route_item(
        item_id: &str,
        model: &str,
        deployment_id: &str,
        phase: Phase,
        config_key: &str,
        next_hop_id: &str,
    ) -> BatchRouteItem {
        BatchRouteItem {
            item_id: item_id.into(),
            session_id: format!("session-{item_id}"),
            model: model.into(),
            deployment_id: deployment_id.into(),
            phase,
            config_key: config_key.into(),
            next_hop_id: next_hop_id.into(),
        }
    }

    #[test]
    fn batch_route_groups_only_items_with_the_same_hop_contract() {
        let groups = BatchRoute::group([
            route_item("a", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-a"),
            route_item("b", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-a"),
            route_item("c", "qwen", "deploy-1", Phase::Decode, "ctx-1", "peer-b"),
            route_item("d", "qwen", "deploy-2", Phase::Decode, "ctx-1", "peer-a"),
            route_item("e", "qwen", "deploy-1", Phase::Prefill, "ctx-1", "peer-a"),
            route_item("f", "qwen", "deploy-1", Phase::Decode, "ctx-2", "peer-a"),
        ]);

        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups[0]
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(groups[0].next_hop_id, "peer-a");
        assert_eq!(groups[1].items[0].item_id, "c");
        assert_eq!(groups[2].items[0].item_id, "d");
        assert_eq!(groups[3].items[0].item_id, "e");
        assert_eq!(groups[4].items[0].item_id, "f");
    }

    #[test]
    fn batch_route_preserves_session_identity_inside_the_group() {
        let item = route_item(
            "request-1",
            "qwen",
            "deploy-1",
            Phase::Decode,
            "ctx",
            "peer-a",
        );
        let session_id = item.session_id.clone();
        let groups = BatchRoute::group([item]);
        assert_eq!(groups[0].items[0].session_id, session_id);
    }

    // O número de pedaços não é calculado em lugar nenhum: `run` avança até o
    // peer da cabeça dizer que não há mais (`more_chunks`), que é o único que
    // sabe quantos tokens o prompt virou. A aritmética que existia aqui, e os
    // testes dela, não tinham chamador na produção.

    #[test]
    fn hop_trace_separates_peer_compute_from_transport_time() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            1_024,
            2_048,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &json!({"decode_ms": 5}),
        );

        assert_eq!(trace.hops[0].prefill.processing_ms, 7);
        assert_eq!(trace.hops[0].prefill.network_ms, 11);
        assert_eq!(trace.hops[0].prefill.bytes_sent, 1_024);
        assert_eq!(trace.hops[0].prefill.bytes_received, 2_048);
    }

    /// A espera pelo mutex do motor tem que chegar a telemetria com nome.
    ///
    /// Numa medicao de 2026-08-14 ela era 194 ms por token - 63% do tempo da
    /// requisicao - e aparecia na tela apenas como "nao atribuido", porque
    /// caia no vao entre a entrada do handler e o primeiro medidor. Um numero
    /// grande sem nome e pior que um numero grande: manda procurar no lugar
    /// errado.
    #[test]
    fn the_wait_for_the_engine_lock_reaches_the_trace() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Decode,
            250,
            240,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({"decode_ms": 34, "lock_wait_ms": 194}),
        );

        assert_eq!(trace.hops[0].decode.node_lock_wait_ms, 194.0);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 34.0);
    }

    #[test]
    fn hop_trace_preserva_fracao_dos_tempos_do_no() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);

        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 2,
                "processing_ms": 1,
                "bytes_sent": 1,
                "bytes_received": 2,
                "node_timings": {
                    "decode_ms": 0.999,
                    "backend_completion_ms": 0.999,
                },
            }),
        );

        let phase = &trace.hops[0].decode;
        assert!((phase.node_decode_ms as f64 - 0.999).abs() < 1e-12);
        assert!((phase.node_backend_completion_ms as f64 - 0.999).abs() < 1e-12);
    }

    #[test]
    fn hop_trace_publica_o_motor_real_e_nao_inventa_o_de_peer_antigo() {
        let mut moe = RequestTrace::new("qwen", &[("moe", 0, 15)]);
        moe.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "selected_path": "moe/q4_0",
                "executed_engine": "moe",
            }),
        );
        assert_eq!(
            moe.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::Moe
        );
        assert_eq!(
            moe.hops[0].decode.selected_path.as_deref(),
            Some("moe/q4_0")
        );

        let mut delegated = RequestTrace::new("qwen", &[("delegated", 0, 15)]);
        delegated.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({
                "selected_path": "moe/q4_0",
                "executed_engine": "llama.cpp",
            }),
        );
        assert_eq!(
            delegated.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::LlamaCpp
        );
        assert!(delegated.hops[0].decode.engine_mismatch);

        let mut old = RequestTrace::new("qwen", &[("old", 0, 15)]);
        old.record_hop(
            0,
            Phase::Decode,
            1,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            TlsHop::default(),
            &json!({}),
        );
        assert_eq!(
            old.hops[0].decode.executed_engine,
            crate::executor::ExecutionEngine::Unknown
        );
    }

    #[test]
    fn hop_trace_propaga_executor_moe_dtype_e_motivo_por_fase() {
        let mut trace = RequestTrace::new("qwen", &[("moe", 0, 39)]);
        trace.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 10,
                "processing_ms": 9,
                "bytes_sent": 1,
                "bytes_received": 2,
                "node_timings": {
                    "selected_path": "moe/q4_0",
                    "executed_engine": "moe",
                    "moe_executor": "cpu-reference",
                    "moe_executors": ["cpu-reference"],
                    "moe_executor_dtypes": [21, 22, 23],
                    "moe_executor_reason": "dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado"
                }
            }),
        );

        let phase = &trace.hops[0].decode;
        assert_eq!(phase.moe_executor.as_deref(), Some("cpu-reference"));
        assert_eq!(phase.moe_executors, vec!["cpu-reference"]);
        assert_eq!(phase.moe_executor_dtypes, vec![21, 22, 23]);
        assert_eq!(
            phase.moe_executor_reason.as_deref(),
            Some("dtypes IQ3_S, IQ2_S e IQ4_XS sem ponte CUDA; CPUReference selecionado")
        );
    }

    #[test]
    fn hop_trace_fica_desconhecido_se_as_chamadas_do_mesmo_hop_divergirem() {
        let mut trace = RequestTrace::new("qwen", &[("mixed", 0, 15)]);
        for (path, engine) in [("moe/q4_0", "llama.cpp"), ("moe/q4_0", "moe")] {
            trace.record_hop(
                0,
                Phase::Prefill,
                1,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                TlsHop::default(),
                &json!({
                    "selected_path": path,
                    "executed_engine": engine,
                }),
            );
        }
        assert_eq!(
            trace.hops[0].prefill.executed_engine,
            crate::executor::ExecutionEngine::Unknown
        );
        assert!(trace.hops[0].prefill.mixed_execution);
        assert!(!trace.hops[0].prefill.engine_mismatch);
    }

    /// Ring mode reconstructs a trace from a bubbled `hop_timings` array
    /// entry (`record_hop_from_json`) instead of from a live HTTP response
    /// (`record_hop`) — the two have to agree exactly, or ring and hub mode
    /// would report different numbers for the identical request.
    #[test]
    fn record_hop_from_json_matches_record_hop() {
        let node = json!({"decode_ms": 10, "residual_ms": 2});

        let mut direct = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        direct.record_hop(
            0,
            Phase::Decode,
            70,
            13,
            100,
            200,
            1,
            2,
            3,
            4,
            TlsHop::default(),
            &node,
        );

        let mut from_json = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        from_json.record_hop_from_json(
            0,
            Phase::Decode,
            &json!({
                "round_trip_ms": 70,
                "processing_ms": 13,
                "bytes_sent": 100,
                "bytes_received": 200,
                "client_encode_ms": 1,
                "response_headers_ms": 2,
                "response_body_ms": 3,
                "client_decode_ms": 4,
                "node_timings": node,
            }),
        );

        assert_eq!(direct.hops[0], from_json.hops[0]);
        assert_eq!(direct.total_bytes_sent, from_json.total_bytes_sent);
        assert_eq!(direct.total_bytes_received, from_json.total_bytes_received);
    }

    /// A missing `hop_timings` entry (index past the plan's step count) must
    /// be a no-op, same as `record_hop` already is — a malformed or short
    /// bubble must not panic on `self.hops[hop]`.
    #[test]
    fn record_hop_from_json_on_an_unknown_index_is_a_no_op() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop_from_json(5, Phase::Decode, &json!({"round_trip_ms": 70}));
        assert_eq!(trace.total_bytes_sent, 0);
    }

    #[test]
    fn merged_token_traces_keep_per_call_timings_and_percentiles() {
        let mut request = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        let mut token = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        token.record_hop(
            0,
            Phase::Decode,
            30,
            20,
            100,
            200,
            2,
            25,
            1,
            1,
            TlsHop::default(),
            &json!({"decode_ms": 11, "residual_ms": 4, "top_p_ms": 3, "backend_completion_ms": 9}),
        );

        request.merge(&token);

        let hop = &request.hops[0];
        assert_eq!(hop.decode.calls, 1);
        assert_eq!(hop.decode.client_encode_ms, 2);
        assert_eq!(hop.decode.node_decode_ms, 11.0);
        assert_eq!(hop.decode.node_residual_ms, 4.0);
        assert_eq!(hop.decode.node_top_p_ms, 3.0);
        assert_eq!(hop.decode.node_backend_completion_ms, 9.0);
        assert_eq!(hop.decode.node_p50_ms, 11.0);
        assert_eq!(hop.decode.node_p95_ms, 11.0);
        // A decode-only trace must leave prefill untouched.
        assert_eq!(hop.prefill.calls, 0);
        assert_eq!(hop.prefill.node_decode_ms, 0.0);
    }

    #[test]
    fn hop_trace_keeps_prefill_and_decode_transport_independent() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            10,
            6,
            100,
            200,
            1,
            8,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );
        trace.record_hop(
            0,
            Phase::Decode,
            20,
            9,
            10,
            20,
            1,
            18,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );

        assert_eq!(trace.hops[0].prefill.network_ms, 4);
        assert_eq!(trace.hops[0].decode.network_ms, 11);
    }

    #[test]
    fn hop_trace_keeps_the_observed_round_trip_for_execution_summary() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0,
            Phase::Prefill,
            18,
            7,
            100,
            200,
            1,
            8,
            1,
            0,
            TlsHop::default(),
            &json!({}),
        );

        assert_eq!(trace.hops[0].prefill.round_trip_ms, 18);
    }

    // ── As duas formas legítimas de resposta a uma rodada especulativa ──────
    //
    // Ler só a primeira era o defeito de 2026-08-15: a cabeça que recusa
    // rascunhar responde na segunda, e o gateway matava a geração no primeiro
    // token. A prova ponta a ponta está em `spec_lab.rs`.

    fn trace_vazio() -> RequestTrace {
        RequestTrace::new("qwen", &[("no1", 0, 15)])
    }

    #[test]
    fn uma_resposta_de_um_token_e_uma_rodada_de_um_token() {
        let passo = speculative_step_from_meta(
            &json!({"done": false, "token_id": 42, "text": "x"}),
            trace_vazio(),
        )
        .expect("a resposta de um token é legítima, não erro");

        assert_eq!(passo.accepted, vec![(42u32, "x".to_string())]);
        assert_eq!(passo.accept_count, 1);
        assert_eq!(
            passo.batch_width, 1,
            "sem rascunho nada foi decodificado além do token semeado"
        );
        assert!(!passo.done);
        assert!(!passo.speculated, "esta rodada não especulou");
    }

    /// Sem isto a geração ignoraria o EOS e só pararia no teto de tokens.
    #[test]
    fn o_fim_da_resposta_de_um_token_e_respeitado() {
        let passo = speculative_step_from_meta(
            &json!({"done": true, "token_id": 7, "text": ""}),
            trace_vazio(),
        )
        .expect("legítima");
        assert!(passo.done);
    }

    #[test]
    fn uma_rodada_verificada_continua_sendo_lida_como_especulativa() {
        let passo = speculative_step_from_meta(
            &json!({
                "done": false,
                "accepted": [{"token_id": 1, "text": "a"}, {"token_id": 2, "text": "b"}],
                "accept_count": 2,
                "batch_width": 4,
            }),
            trace_vazio(),
        )
        .expect("legítima");

        assert_eq!(passo.accepted.len(), 2);
        assert_eq!(passo.accept_count, 2);
        assert_eq!(passo.batch_width, 4);
        assert!(passo.speculated);
    }

    /// Uma resposta que não é nem uma coisa nem outra continua sendo erro:
    /// seguir em silêncio com token nenhum seria pior que a falha.
    #[test]
    fn uma_resposta_sem_token_nenhum_continua_sendo_erro() {
        assert!(speculative_step_from_meta(&json!({"done": false}), trace_vazio()).is_err());
    }

    #[test]
    fn a_trace_keeps_its_issue_budget() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        for attempt in 1..=33 {
            trace.record_issue(TraceIssue::new(
                Some(0),
                "no1",
                Phase::Prefill,
                TraceIssueKind::Transport,
                attempt,
                "offline",
            ));
        }
        assert_eq!(trace.issues.len(), 32);
        assert_eq!(trace.dropped_issues, 1);
    }
}
