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

use crate::infer_tunnel::TunnelOutboundSender;
use crate::pipeline::PipelinePlan;
use crate::wire::{self, FrameEncoding, WireDtype};
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;
use std::time::{Duration, Instant};
use tracing::info;

/// Prompt tokens pushed through the pipeline per round trip.
///
/// The chunk is the unit of retry and the unit of memory, so it wants to be
/// small; it is also one round trip per peer, so it wants to be large. 512
/// tokens is 8 MiB of f16 hidden state per hop — about a second on the link
/// this cluster actually has, which keeps a retry cheap while leaving the
/// per-chunk overhead well under the transfer time it covers.
pub const DEFAULT_CHUNK_TOKENS: u32 = 512;

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
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PhaseTrace {
    /// Time the peer reported for itself (`processing_ms` on the wire).
    pub processing_ms: u64,
    /// Round trip minus what the peer claimed: the residual HTTP time.
    pub network_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub calls: u32,
    pub client_encode_ms: u64,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    pub client_decode_ms: u64,
    pub node_frame_decode_ms: u64,
    pub node_input_prepare_ms: u64,
    pub node_queue_ms: u64,
    /// Espera pelo mutex do motor no nó: o preço de outra requisição estar
    /// decodificando ali. É o que separa "o nó é lento" de "o nó está ocupado".
    pub node_lock_wait_ms: u64,
    pub node_setup_ms: u64,
    pub node_decode_ms: u64,
    /// Time to hand the batch to the backend — not the time it ran.
    pub node_backend_enqueue_ms: u64,
    /// Queued backend work plus already-queued output transfer, measured with
    /// `llama_synchronize`. Zero unless the peer runs with
    /// `RDMA_P2P_PROFILE_SYNC` (on by default; `=0` turns it off); it is a
    /// diagnostic boundary, not kernel time.
    pub node_backend_completion_ms: u64,
    /// Device-to-host copy of the residual, measured inside the capture hook.
    pub node_residual_capture_d2h_sync_ms: u64,
    pub node_residual_ms: u64,
    pub node_logits_access_ms: u64,
    pub node_logits_clone_ms: u64,
    pub node_repetition_penalty_ms: u64,
    pub node_softmax_ms: u64,
    pub node_top_k_ms: u64,
    pub node_top_p_ms: u64,
    pub node_selection_ms: u64,
    pub node_token_text_ms: u64,
    pub node_sampling_ms: u64,
    pub node_encode_ms: u64,
    /// Percentiles over the peer's own `decode_ms`, per call in this phase.
    pub node_p50_ms: u64,
    pub node_p95_ms: u64,
    #[serde(skip)]
    node_samples_ms: Vec<u64>,
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
        node: &serde_json::Value,
    ) {
        let node_ms = |key: &str| node[key].as_u64().unwrap_or(0);
        let node_decode_ms = node_ms("decode_ms");

        self.processing_ms += processing_ms;
        self.network_ms += round_trip_ms.saturating_sub(processing_ms);
        self.bytes_sent += bytes_sent;
        self.bytes_received += bytes_received;
        self.calls += 1;
        self.client_encode_ms += client_encode_ms;
        self.response_headers_ms += response_headers_ms;
        self.response_body_ms += response_body_ms;
        self.client_decode_ms += client_decode_ms;
        self.node_frame_decode_ms += node_ms("frame_decode_ms");
        self.node_input_prepare_ms += node_ms("input_prepare_ms");
        self.node_queue_ms += node_ms("queue_ms");
        self.node_lock_wait_ms += node_ms("lock_wait_ms");
        self.node_setup_ms += node_ms("setup_ms");
        self.node_decode_ms += node_decode_ms;
        self.node_backend_enqueue_ms += node_ms("backend_enqueue_ms");
        self.node_backend_completion_ms += node_ms("backend_completion_ms");
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
        self.refresh_percentiles();
    }

    fn merge(&mut self, next: &PhaseTrace) {
        self.processing_ms += next.processing_ms;
        self.network_ms += next.network_ms;
        self.bytes_sent += next.bytes_sent;
        self.bytes_received += next.bytes_received;
        self.calls += next.calls;
        self.client_encode_ms += next.client_encode_ms;
        self.response_headers_ms += next.response_headers_ms;
        self.response_body_ms += next.response_body_ms;
        self.client_decode_ms += next.client_decode_ms;
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
        self.node_samples_ms.extend_from_slice(&next.node_samples_ms);
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Prefill,
    Decode,
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

fn percentile(samples: &[u64], percentile: u64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) * percentile as usize + 50) / 100;
    sorted[index]
}

/// Push the whole prompt through the pipeline, chunk by chunk.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    chunk_size: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<PrefillOutcome> {
    let chunk_size = chunk_size.max(1);
    let n_peers = plan.steps.len();
    if n_peers == 0 {
        return Err(anyhow!("pipeline sem peers"));
    }
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return run_direct(
            plan, model, session_id, prompt, chunk_size, dtype, http, routes, direct, tunnel,
        )
        .await;
    }
    if plan.supports_relay_chain() {
        return run_ring(plan, model, session_id, prompt, chunk_size, dtype, http, routes, tunnel)
            .await;
    }

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        // ── First peer: it owns the tokenizer and decides whether more remain.
        let head_meta = json!({
            "session_id": session_id,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_last": n_peers == 1,
            // We cannot know whether this is the last chunk before the head
            // peer — which owns the tokenizer — tells us how many tokens the
            // prompt has. So we say false, and for a one-hop pipeline the head
            // peer overrides it from its own token count; see the
            // `hidden is None and not more` branch in inference_server.py.
            // For longer pipelines the flag is recomputed below, once
            // `more_chunks` has come back, and sent to the real tail.
            "is_final_chunk": false,
            "prompt": if chunk_index == 0 { Some(prompt) } else { None },
        });

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

        // ── The rest of the pipeline, carrying this chunk's hidden state.
        for (i, step) in plan.steps.iter().enumerate().skip(1) {
            let is_last = i == n_peers - 1;
            let Some((values, shape)) = tensor.take() else {
                return Err(anyhow!(
                    "peer {} não devolveu estado oculto no pedaço {chunk_index}",
                    plan.steps[i - 1].node_id
                ));
            };
            let body_meta = json!({
                "session_id": session_id,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "is_last": is_last,
                "is_final_chunk": is_final_chunk,
                "scale": 1.0,
            });
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
    chunk_size: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        let head_meta = json!({
            "session_id": session_id,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_last": next_hops.is_empty(),
            // Same reasoning as `run`'s identical comment: the head is the
            // only one who can know this, and overrides it locally before
            // relaying onward — we always say false here.
            "is_final_chunk": false,
            "prompt": if chunk_index == 0 { Some(prompt) } else { None },
            "next_hops": next_hops,
        });

        let sent = crate::relay::send_frame_or_tunnel(
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
        )
        .await
        .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;

        for (i, entry) in crate::relay::bubble_hop_timings(&sent).into_iter().enumerate() {
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
    chunk_size: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let started = Instant::now();
    let mut trace = RequestTrace::from_plan(model, plan);

    loop {
        let mut ticket = direct.ticket();
        let head_meta = json!({
            "session_id": session_id,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_last": next_hops.is_empty(),
            // Same reasoning as `run`'s identical comment.
            "is_final_chunk": false,
            "prompt": if chunk_index == 0 { Some(prompt) } else { None },
            "next_hops": next_hops,
            "reply_to": direct.reply_to(ticket.id()),
        });

        let dispatched = Instant::now();
        let ack = crate::relay::send_frame_or_tunnel(
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
    http: &reqwest::Client,
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
) -> Result<(serde_json::Value, Option<(Vec<f32>, [usize; 2])>)> {
    let node_id = &step.node_id;
    let peer_label = format!("{node_id} (pedaço {chunk_index})");
    let mut issues = Vec::new();
    let result = crate::relay::send_frame_or_tunnel(
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
        routes.mark_failure(node_id, &step.inference_url);
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
        &sent.meta["node_timings"],
    );
    Ok((sent.meta, sent.tensor))
}

/// One generated token through the whole pipeline, framed like the prefill.
#[allow(clippy::too_many_arguments)]
pub async fn step(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<(u32, String, bool, RequestTrace)> {
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return step_direct(plan, model, session_id, token_id, dtype, http, routes, direct, tunnel)
            .await;
    }
    if plan.supports_relay_chain() {
        return step_ring(plan, model, session_id, token_id, dtype, http, routes, tunnel).await;
    }

    // Nota: o laço hub abaixo (um POST por hop, sem retry) não passa pelo
    // túnel — replica manualmente o encode/POST/decode de `relay::send_frame`
    // em vez de reutilizá-lo, e essa duplicação está fora do escopo desta
    // passagem. Decodificação em modo hub já é o caminho mais curto e mais
    // sensível a latência por token; um fallback mais lento por hop deveria
    // ser medido antes de entrar aqui, não assumido.
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, [usize; 2])> = None;
    let mut meta = serde_json::Value::Null;
    let mut trace = RequestTrace::from_plan(model, plan);

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let body_meta = if i == 0 {
            json!({ "session_id": session_id, "is_last": is_last, "token_id": token_id })
        } else {
            json!({ "session_id": session_id, "is_last": is_last, "scale": 1.0 })
        };

        let payload = tensor.take();
        let as_ref = payload.as_ref().map(|(v, s)| (v.as_slice(), *s));
        let encode_started = Instant::now();
        let body = match (as_ref, dtype) {
            (Some(tensor), WireDtype::F16) => {
                wire::encode_with_encoding(&body_meta, Some(tensor), peer.frame_encoding())?
            }
            (tensor, dtype) => wire::encode(&body_meta, tensor, dtype)?,
        };
        let client_encode_ms = encode_started.elapsed().as_millis() as u64;

        let url = format!("{}/v1/step", peer.inference_url);
        let started = Instant::now();
        let bytes_sent = body.len() as u64;
        let resp = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| {
                routes.mark_failure(&peer.node_id, &peer.inference_url);
                anyhow!("peer {} não respondeu: {e}", peer.node_id)
            })?;
        let response_headers_ms = started.elapsed().as_millis() as u64;

        if !resp.status().is_success() {
            let code = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("peer {} respondeu {code}: {text}", peer.node_id));
        }
        let response_body_started = Instant::now();
        let bytes = resp.bytes().await?;
        let response_body_ms = response_body_started.elapsed().as_millis() as u64;
        let client_decode_started = Instant::now();
        let (m, t) = wire::decode(&bytes)?;
        let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
        let processing_ms = m
            .get("processing_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        trace.record_hop(
            i,
            Phase::Decode,
            started.elapsed().as_millis() as u64,
            processing_ms,
            bytes_sent,
            bytes.len() as u64,
            client_encode_ms,
            response_headers_ms,
            response_body_ms,
            client_decode_ms,
            &m["node_timings"],
        );
        meta = m;
        tensor = t;
    }

    let token = meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token"))? as u32;
    Ok((
        token,
        meta.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
        trace,
    ))
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
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<(u32, String, bool, RequestTrace)> {
    let mut trace = RequestTrace::from_plan(model, plan);
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let body_meta = json!({
        "session_id": session_id,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
    });

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

    for (i, entry) in crate::relay::bubble_hop_timings(&sent).into_iter().enumerate() {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    let token = sent
        .meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token"))? as u32;
    Ok((
        token,
        sent.meta
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        sent.meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
        trace,
    ))
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
    http: &reqwest::Client,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
) -> Result<(u32, String, bool, RequestTrace)> {
    let mut trace = RequestTrace::from_plan(model, plan);
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut ticket = direct.ticket();
    let body_meta = json!({
        "session_id": session_id,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
        "reply_to": direct.reply_to(ticket.id()),
    });

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

    let token = meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token"))? as u32;
    Ok((
        token,
        meta.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
        trace,
    ))
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
    /// The head is free to decline — `should_draft` says no whenever the
    /// engine's KV cache cannot shift, which is every hybrid or recurrent
    /// model — and it says so by omitting `draft`, which puts the whole round
    /// back on the plain one-token wire format (see `step_blocking`'s
    /// `incoming_draft` doc). `false` means exactly that happened, and asking
    /// the same chain to speculate again would just repeat the refusal.
    pub speculated: bool,
    pub trace: RequestTrace,
}

impl SpeculativeStep {
    /// O resultado de um [`step`] comum, na forma que o laço de decodificação
    /// já sabe consumir.
    ///
    /// Um token, largura um: nada foi decodificado além dele, então
    /// `accept_count == batch_width` e não há posição a rebobinar.
    pub fn of_one_token(
        (token_id, text, done, trace): (u32, String, bool, RequestTrace),
    ) -> Self {
        Self {
            accepted: vec![(token_id, text)],
            done,
            accept_count: 1,
            batch_width: 1,
            speculated: false,
            trace,
        }
    }
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
    http: &reqwest::Client,
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
    http: &reqwest::Client,
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
            })
        } else {
            let mut m = json!({ "session_id": session_id, "is_last": is_last, "scale": 1.0 });
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
    http: &reqwest::Client,
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

    for (i, entry) in crate::relay::bubble_hop_timings(&sent).into_iter().enumerate() {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    speculative_step_from_meta(&sent.meta, trace)
}

/// Parse the tail's reply to a speculative round — shared by the hub and ring
/// paths, which only differ in how they get to that reply.
///
/// Two shapes are legitimate, and reading only the first one is what killed
/// every generation on a model whose KV cache cannot shift:
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
        trace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &json!({"decode_ms": 34, "lock_wait_ms": 194}),
        );

        assert_eq!(trace.hops[0].decode.node_lock_wait_ms, 194);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 34);
    }

    /// Ring mode reconstructs a trace from a bubbled `hop_timings` array
    /// entry (`record_hop_from_json`) instead of from a live HTTP response
    /// (`record_hop`) — the two have to agree exactly, or ring and hub mode
    /// would report different numbers for the identical request.
    #[test]
    fn record_hop_from_json_matches_record_hop() {
        let node = json!({"decode_ms": 10, "residual_ms": 2});

        let mut direct = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        direct.record_hop(0, Phase::Decode, 70, 13, 100, 200, 1, 2, 3, 4, &node);

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
            &json!({"decode_ms": 11, "residual_ms": 4, "top_p_ms": 3, "backend_completion_ms": 9}),
        );

        request.merge(&token);

        let hop = &request.hops[0];
        assert_eq!(hop.decode.calls, 1);
        assert_eq!(hop.decode.client_encode_ms, 2);
        assert_eq!(hop.decode.node_decode_ms, 11);
        assert_eq!(hop.decode.node_residual_ms, 4);
        assert_eq!(hop.decode.node_top_p_ms, 3);
        assert_eq!(hop.decode.node_backend_completion_ms, 9);
        assert_eq!(hop.decode.node_p50_ms, 11);
        assert_eq!(hop.decode.node_p95_ms, 11);
        // A decode-only trace must leave prefill untouched.
        assert_eq!(hop.prefill.calls, 0);
        assert_eq!(hop.prefill.node_decode_ms, 0);
    }

    #[test]
    fn hop_trace_keeps_prefill_and_decode_transport_independent() {
        let mut trace = RequestTrace::new("qwen", &[("no1", 0, 15)]);
        trace.record_hop(
            0, Phase::Prefill, 10, 6, 100, 200, 1, 8, 1, 0, &json!({}),
        );
        trace.record_hop(
            0, Phase::Decode, 20, 9, 10, 20, 1, 18, 1, 0, &json!({}),
        );

        assert_eq!(trace.hops[0].prefill.network_ms, 4);
        assert_eq!(trace.hops[0].decode.network_ms, 11);
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
        let passo =
            speculative_step_from_meta(&json!({"done": true, "token_id": 7, "text": ""}), trace_vazio())
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
