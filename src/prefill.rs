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
use crate::wire::{self, WireDtype};
use anyhow::{anyhow, Result};
use serde_json::json;
use std::time::Duration;
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
}

/// How many chunks a prompt of `tokens` needs.
pub fn chunk_count(tokens: u32, chunk_size: u32) -> u32 {
    let size = chunk_size.max(1);
    tokens.div_ceil(size).max(1)
}

/// Push the whole prompt through the pipeline, chunk by chunk.
pub async fn run(
    plan: &PipelinePlan,
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

    loop {
        // ── First peer: it owns the tokenizer and decides whether more remain.
        let head_meta = json!({
            "session_id": session_id,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_last": n_peers == 1,
            // Only meaningful for a single-peer pipeline; for longer ones the
            // flag is recomputed below, once we know whether this is the tail.
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
            chunk_index,
        )
        .await?;

        if let Some(n) = meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
            prompt_tokens = n as u32;
        }
        let more = meta.get("more_chunks").and_then(|v| v.as_bool()).unwrap_or(false);
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
                chunk_index,
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
    chunk_index: u32,
) -> Result<(serde_json::Value, Option<(Vec<f32>, [usize; 2])>)> {
    let body = wire::encode(meta, tensor, dtype)?;
    let url = format!("{inference_url}/v1/prefill");
    let mut last_error = String::new();

    for attempt in 1..=CHUNK_ATTEMPTS {
        let sent = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(Duration::from_secs(600))
            .send()
            .await;

        match sent {
            Ok(r) if r.status().is_success() => {
                let bytes = r.bytes().await?;
                return wire::decode(&bytes);
            }
            Ok(r) => {
                let code = r.status();
                let text = r.text().await.unwrap_or_default();
                // A refusal is the peer's considered answer, not a hiccup;
                // repeating it would only waste the link.
                return Err(anyhow!("peer {node_id} recusou o pedaço {chunk_index}: {code} {text}"));
            }
            Err(e) => {
                last_error = e.to_string();
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
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &reqwest::Client,
) -> Result<(u32, String, bool)> {
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, [usize; 2])> = None;
    let mut meta = serde_json::Value::Null;

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let body_meta = if i == 0 {
            json!({ "session_id": session_id, "is_last": is_last, "token_id": token_id })
        } else {
            json!({ "session_id": session_id, "is_last": is_last, "scale": 1.0 })
        };

        let payload = tensor.take();
        let as_ref = payload.as_ref().map(|(v, s)| (v.as_slice(), *s));
        let body = wire::encode(&body_meta, as_ref, dtype)?;

        let url = format!("{}/v1/step", peer.inference_url);
        let resp = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| anyhow!("peer {} não respondeu: {e}", peer.node_id))?;

        if !resp.status().is_success() {
            let code = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("peer {} respondeu {code}: {text}", peer.node_id));
        }
        let bytes = resp.bytes().await?;
        let (m, t) = wire::decode(&bytes)?;
        meta = m;
        tensor = t;
    }

    let token = meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token"))? as u32;
    Ok((
        token,
        meta.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false),
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
}
