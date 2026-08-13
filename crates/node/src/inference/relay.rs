//! Shared "encode → POST with retry → decode" mechanics for one hop-to-hop
//! call.
//!
//! Used both by the gateway in hub mode (`prefill::send_chunk`, calling
//! every hop itself) and, once `PipelinePlan::supports_relay_chain()` is
//! true, by a relaying inference node in ring mode (`infer_server.rs`,
//! calling its own next hop directly instead of replying to the gateway).
//! Factoring this out means both call sites share one retry policy and one
//! encode/decode path instead of drifting apart.

use crate::wire::{self, FrameEncoding, WireDtype};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tracing::warn;

/// Attempts per prefill chunk before giving up on it — safe to retry because
/// the receiving peer rewinds its KV cache to the chunk's position first
/// (`infer_server.rs`'s rollback/order check). A decode step has no such
/// protection, so callers relaying a step must pass `1` instead — see each
/// call site's own comment for which applies.
pub const CHUNK_ATTEMPTS: u32 = 3;

/// One hop's response, plus the client-observable timing a caller needs to
/// build its own bookkeeping from (a `RequestTrace` hop entry in
/// `prefill.rs`, or a `hop_timings` array entry in `infer_server.rs`).
pub struct SentFrame {
    pub meta: Value,
    pub tensor: Option<(Vec<f32>, [usize; 2])>,
    pub round_trip_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub client_encode_ms: u64,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    pub client_decode_ms: u64,
}

impl SentFrame {
    /// This hop's own contribution to a bubbled-back `hop_timings` array:
    /// what the *caller* observed of the round trip (`self`'s fields)
    /// combined with what the *callee* reported about itself in its own
    /// reply meta (`processing_ms`, `node_timings` — already present in
    /// every reply today, hub or ring). One entry, for the peer just called.
    pub fn as_hop_timing_entry(&self) -> Value {
        json!({
            "round_trip_ms": self.round_trip_ms,
            "processing_ms": self.meta.get("processing_ms").cloned().unwrap_or(json!(0)),
            "bytes_sent": self.bytes_sent,
            "bytes_received": self.bytes_received,
            "client_encode_ms": self.client_encode_ms,
            "response_headers_ms": self.response_headers_ms,
            "response_body_ms": self.response_body_ms,
            "client_decode_ms": self.client_decode_ms,
            "node_timings": self.meta.get("node_timings").cloned().unwrap_or(json!({})),
        })
    }
}

/// Prepend this hop's own timing entry onto whatever `hop_timings` array
/// bubbled up inside its reply (representing every hop further downstream,
/// already ordered head-to-tail by the same prepending done one level down),
/// yielding the full ordered array from the hop just called through to the
/// tail. The caller either relays this further up (embedding it in its own
/// reply meta) or, at the top of the chain, folds it straight into a
/// `RequestTrace` via `record_hop_from_json` per entry.
pub fn bubble_hop_timings(sent: &SentFrame) -> Vec<Value> {
    let mut rest: Vec<Value> = sent
        .meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rest.insert(0, sent.as_hop_timing_entry());
    rest
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendIssueKind {
    Timeout,
    Transport,
    Http,
    Protocol,
}

/// One failed attempt. Reported through a callback rather than returned
/// directly, so a caller with a `RequestTrace` (the gateway) can record a
/// `TraceIssue` per attempt, while a caller without one (a relaying node)
/// can just log — this function itself stays ignorant of both.
pub struct SendIssue {
    pub kind: SendIssueKind,
    pub attempt: u32,
    pub message: String,
}

/// A relaying hop's own outbound call to *its* next hop failed after
/// exhausting retries, or came back with a non-success status. Reported to
/// whoever called this hop as a `502` with this body, so the failure can be
/// attributed to the peer that actually died rather than whichever hop
/// happened to be closest to the caller. Propagated unchanged (re-serialized
/// as-is) if a hop relaying on our behalf already built one further down the
/// chain — see `extract_relay_failure`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFailure {
    pub failed_node_id: String,
    pub kind: String,
    pub message: String,
}

/// An HTTP error response from a peer, carrying the raw body so a caller can
/// attempt to parse it as a `RelayFailure` (see `extract_relay_failure`)
/// instead of only getting a flattened string.
#[derive(Debug)]
pub struct HttpRefusal {
    pub status: u16,
    pub body: String,
    pub peer_label: String,
}

impl std::fmt::Display for HttpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer {} recusou: {} {}", self.peer_label, self.status, self.body)
    }
}

impl std::error::Error for HttpRefusal {}

/// If `send_frame` failed because a relaying peer reported (via
/// `RelayFailure`) that *its* next hop was the real problem, return that —
/// otherwise `None`, meaning the failure originated at the peer we called
/// directly.
pub fn extract_relay_failure(e: &anyhow::Error) -> Option<RelayFailure> {
    let refusal = e.downcast_ref::<HttpRefusal>()?;
    serde_json::from_str(&refusal.body).ok()
}

/// POST one wire frame to `{base_url}{path}`, retrying up to `attempts`
/// times on transport/timeout failure with the same 400ms×attempt backoff
/// this project has always used. An explicit HTTP error response is never
/// retried — it's the peer's considered refusal, not a hiccup. `attempts=1`
/// means exactly one try, no retry at all (the decode/`/v1/step` path has
/// never retried, since — unlike prefill chunks — a step has no KV-rollback
/// idempotency protection against being redecoded).
#[allow(clippy::too_many_arguments)]
pub async fn send_frame(
    http: &reqwest::Client,
    base_url: &str,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    attempts: u32,
    peer_label: &str,
    mut on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    let encode_started = Instant::now();
    let body = match (tensor, dtype) {
        (Some(tensor), WireDtype::F16) => wire::encode_with_encoding(meta, Some(tensor), encoding)?,
        (tensor, dtype) => wire::encode(meta, tensor, dtype)?,
    };
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;
    let url = format!("{base_url}{path}");
    let attempts = attempts.max(1);
    let mut last_error = String::new();

    for attempt in 1..=attempts {
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
                let (meta, tensor) = match wire::decode(&bytes) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        on_issue(SendIssue {
                            kind: SendIssueKind::Protocol,
                            attempt,
                            message: error.to_string(),
                        });
                        return Err(error);
                    }
                };
                let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
                return Ok(SentFrame {
                    meta,
                    tensor,
                    round_trip_ms: started.elapsed().as_millis() as u64,
                    bytes_sent: body.len() as u64,
                    bytes_received: bytes.len() as u64,
                    client_encode_ms,
                    response_headers_ms,
                    response_body_ms,
                    client_decode_ms,
                });
            }
            Ok(r) => {
                let code = r.status();
                let text = r.text().await.unwrap_or_default();
                on_issue(SendIssue {
                    kind: SendIssueKind::Http,
                    attempt,
                    message: format!("HTTP {code}: {text}"),
                });
                return Err(anyhow::Error::new(HttpRefusal {
                    status: code.as_u16(),
                    body: text,
                    peer_label: peer_label.to_string(),
                }));
            }
            Err(e) => {
                last_error = e.to_string();
                on_issue(SendIssue {
                    kind: if e.is_timeout() {
                        SendIssueKind::Timeout
                    } else {
                        SendIssueKind::Transport
                    },
                    attempt,
                    message: last_error.clone(),
                });
                warn!(
                    "chamada para {peer_label} falhou (tentativa {attempt}/{attempts}): {last_error}"
                );
                if attempt < attempts {
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
            }
        }
    }

    Err(anyhow!(
        "{peer_label} não respondeu após {attempts} tentativa(s): {last_error}"
    ))
}
