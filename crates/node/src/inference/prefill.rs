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

use crate::pipeline::PipelinePlan;
use crate::wire::{self, FrameEncoding, WireDtype};
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Prompt tokens pushed through the pipeline per round trip.
///
/// The chunk is the unit of retry and the unit of memory, so it wants to be
/// small; it is also one round trip per peer, so it wants to be large. 512
/// tokens is 8 MiB of f16 hidden state per hop — about a second on the link
/// this cluster actually has, which keeps a retry cheap while leaving the
/// per-chunk overhead well under the transfer time it covers.
pub const DEFAULT_CHUNK_TOKENS: u32 = 512;

/// Attempts per chunk before the request is given up on.
const CHUNK_ATTEMPTS: u32 = 3;

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
#[derive(Debug, Clone, Default, Serialize)]
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
    pub node_setup_ms: u64,
    pub node_decode_ms: u64,
    /// Time to hand the batch to the backend — not the time it ran.
    pub node_backend_enqueue_ms: u64,
    /// Queued backend work plus already-queued output transfer, measured with
    /// `llama_synchronize`. Zero unless the peer runs with
    /// `RDMA_P2P_PROFILE_SYNC=1`; it is a diagnostic boundary, not kernel time.
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

#[derive(Debug, Clone, Serialize)]
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

/// How many chunks a prompt of `tokens` needs.
pub fn chunk_count(tokens: u32, chunk_size: u32) -> u32 {
    let size = chunk_size.max(1);
    tokens.div_ceil(size).max(1)
}

/// Push the whole prompt through the pipeline, chunk by chunk.
pub async fn run(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    chunk_size: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
) -> Result<PrefillOutcome> {
    let chunk_size = chunk_size.max(1);
    let n_peers = plan.steps.len();
    if n_peers == 0 {
        return Err(anyhow!("pipeline sem peers"));
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
            &plan.steps[0].inference_url,
            &plan.steps[0].node_id,
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
                &step.inference_url,
                &step.node_id,
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

/// One chunk to one peer, retried on transport failure.
///
/// Retrying is safe because the peer rewinds its KV cache to this chunk's
/// position before redoing it — a repeat can never be counted twice.
#[allow(clippy::too_many_arguments)]
async fn send_chunk(
    http: &reqwest::Client,
    inference_url: &str,
    node_id: &str,
    meta: &serde_json::Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    chunk_index: u32,
    trace: &mut RequestTrace,
    hop: usize,
    phase: Phase,
) -> Result<(serde_json::Value, Option<(Vec<f32>, [usize; 2])>)> {
    let encode_started = Instant::now();
    let body = match (tensor, dtype) {
        (Some(tensor), WireDtype::F16) => wire::encode_with_encoding(meta, Some(tensor), encoding)?,
        (tensor, dtype) => wire::encode(meta, tensor, dtype)?,
    };
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;
    let url = format!("{inference_url}/v1/prefill");
    let mut last_error = String::new();

    for attempt in 1..=CHUNK_ATTEMPTS {
        let started = Instant::now();
        let sent = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(Duration::from_secs(600))
            .send()
            .await;

        match sent {
            Ok(r) if r.status().is_success() => {
                let response_headers_ms = started.elapsed().as_millis() as u64;
                let response_body_started = Instant::now();
                let bytes = r.bytes().await?;
                let response_body_ms = response_body_started.elapsed().as_millis() as u64;
                let client_decode_started = Instant::now();
                let decoded = match wire::decode(&bytes) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        trace.record_issue(TraceIssue::new(
                            Some(hop),
                            node_id,
                            phase,
                            TraceIssueKind::Protocol,
                            attempt,
                            error.to_string(),
                        ));
                        return Err(error.into());
                    }
                };
                let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
                let processing_ms = decoded
                    .0
                    .get("processing_ms")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                trace.record_hop(
                    hop,
                    phase,
                    started.elapsed().as_millis() as u64,
                    processing_ms,
                    body.len() as u64,
                    bytes.len() as u64,
                    client_encode_ms,
                    response_headers_ms,
                    response_body_ms,
                    client_decode_ms,
                    &decoded.0["node_timings"],
                );
                return Ok(decoded);
            }
            Ok(r) => {
                let code = r.status();
                let text = r.text().await.unwrap_or_default();
                trace.record_issue(TraceIssue::new(
                    Some(hop),
                    node_id,
                    phase,
                    TraceIssueKind::Http,
                    attempt,
                    format!("HTTP {code}: {text}"),
                ));
                // A refusal is the peer's considered answer, not a hiccup;
                // repeating it would only waste the link.
                return Err(anyhow!(
                    "peer {node_id} recusou o pedaço {chunk_index}: {code} {text}"
                ));
            }
            Err(e) => {
                last_error = e.to_string();
                trace.record_issue(TraceIssue::new(
                    Some(hop),
                    node_id,
                    phase,
                    if e.is_timeout() {
                        TraceIssueKind::Timeout
                    } else {
                        TraceIssueKind::Transport
                    },
                    attempt,
                    &last_error,
                ));
                warn!(
                    "pedaço {chunk_index} para {node_id} falhou (tentativa {attempt}/{CHUNK_ATTEMPTS}): {last_error}"
                );
                if attempt < CHUNK_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
            }
        }
    }

    Err(anyhow!(
        "pedaço {chunk_index} não chegou em {node_id} após {CHUNK_ATTEMPTS} tentativas: {last_error}"
    ))
}

/// One generated token through the whole pipeline, framed like the prefill.
pub async fn step(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
) -> Result<(u32, String, bool, RequestTrace)> {
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
            .map_err(|e| anyhow!("peer {} não respondeu: {e}", peer.node_id))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_shorter_than_one_chunk_is_a_single_chunk() {
        assert_eq!(chunk_count(1, 512), 1);
        assert_eq!(chunk_count(511, 512), 1);
        assert_eq!(chunk_count(512, 512), 1);
    }

    #[test]
    fn a_partial_last_chunk_still_counts() {
        assert_eq!(chunk_count(513, 512), 2);
        assert_eq!(chunk_count(1024, 512), 2);
        assert_eq!(chunk_count(1025, 512), 3);
    }

    /// The captain's target: a 250k-token prompt has to become a bounded number
    /// of bounded pieces, which is the whole point of the change.
    #[test]
    fn a_very_long_prompt_becomes_many_bounded_chunks() {
        assert_eq!(chunk_count(250_000, 512), 489);
    }

    #[test]
    fn an_empty_prompt_never_produces_zero_chunks() {
        assert_eq!(chunk_count(0, 512), 1);
    }

    #[test]
    fn a_zero_chunk_size_does_not_divide_by_zero() {
        assert_eq!(chunk_count(10, 0), 10);
    }

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
