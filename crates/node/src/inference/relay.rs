//! Shared "encode → POST with retry → decode" mechanics for one hop-to-hop
//! call.
//!
//! Used both by the gateway in hub mode (`prefill::send_chunk`, calling
//! every hop itself) and, once `PipelinePlan::supports_relay_chain()` is
//! true, by a relaying inference node in ring mode (`infer_server.rs`,
//! calling its own next hop directly instead of replying to the gateway).
//! Factoring this out means both call sites share one retry policy and one
//! encode/decode path instead of drifting apart.

use crate::infer_tunnel::{TunnelOutbound, TunnelOutboundSender, TunnelRequest};
use crate::wire::{self, FrameEncoding, WireDtype};
use anyhow::{anyhow, Result};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
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
    /// Se este salto foi cifrado.
    ///
    /// Também é o que denuncia uma virada pela metade, ou um ponto de chamada
    /// que ficou de fora da conversão para o cliente pinado.
    pub tls: bool,
    /// Custo do handshake TLS **medido no servidor**, onde ele é exato
    /// (`serve.rs` cerca o `TlsAcceptor::accept`).
    ///
    /// Vem preenchido no salto que abriu conexão nova e **zero nos demais**,
    /// porque as conexões ficam no pool do reqwest. Isso é a verdade, não falta
    /// de medição: é custo de estabelecimento de conexão, não custo por token,
    /// e a tela precisa dizer isso - ver `Chat.tsx`.
    pub tls_handshake_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BatchKey {
    node_id: String,
    inference_url: String,
    path: String,
    dtype: WireDtype,
    route: String,
    contract: String,
}

struct BatchCall {
    clients: crate::peer_client::PeerClients,
    step: crate::pipeline::PipelineStep,
    tunnel: Option<TunnelOutboundSender>,
    path: String,
    meta: Value,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    dtype: WireDtype,
    attempts: u32,
    peer_label: String,
    item_id: String,
    reply: oneshot::Sender<std::result::Result<SentFrame, String>>,
}

/// Coalesces independent requests that are about to cross the same pipeline
/// edge. The queue is process-local on purpose: each peer owns its own
/// transport and its own llama context. A short window is enough to combine
/// requests arriving in the same scheduler tick without making one request
/// wait behind an unrelated generation.
pub struct BatchCoordinator {
    pending: Mutex<HashMap<BatchKey, Vec<BatchCall>>>,
    window: Duration,
}

impl BatchCoordinator {
    fn global() -> &'static Arc<Self> {
        static INSTANCE: OnceLock<Arc<BatchCoordinator>> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            Arc::new(Self {
                pending: Mutex::new(HashMap::new()),
                window: Duration::from_millis(4),
            })
        })
    }

    fn key(
        step: &crate::pipeline::PipelineStep,
        path: &str,
        meta: &Value,
        dtype: WireDtype,
    ) -> BatchKey {
        let route = meta
            .get("next_hops")
            .map(|value| serde_json::to_string(value).unwrap_or_default())
            .unwrap_or_default();
        let contract = serde_json::to_string(&json!({
            "model": meta.get("model"),
            "deployment_id": meta.get("deployment_id"),
            "chunk_size": meta.get("chunk_size"),
            "is_last": meta.get("is_last"),
            "is_final_chunk": meta.get("is_final_chunk"),
            "speculative": meta.get("speculate").is_some()
                || meta.get("draft").is_some()
                || meta.get("mtp_k").is_some(),
        }))
        .unwrap_or_default();
        BatchKey {
            node_id: step.node_id.clone(),
            inference_url: step.inference_url.clone(),
            path: path.to_string(),
            dtype,
            route,
            contract,
        }
    }

    async fn submit(
        self: Arc<Self>,
        clients: &crate::peer_client::PeerClients,
        step: &crate::pipeline::PipelineStep,
        tunnel: Option<&TunnelOutboundSender>,
        path: &str,
        meta: &Value,
        tensor: Option<(&[f32], [usize; 2])>,
        dtype: WireDtype,
        attempts: u32,
        peer_label: &str,
    ) -> Result<SentFrame> {
        let item_id = meta
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut meta = meta.clone();
        meta["item_id"] = Value::from(item_id.clone());
        let key = Self::key(step, path, &meta, dtype);
        let (reply, receiver) = oneshot::channel();
        let call = BatchCall {
            clients: clients.clone(),
            step: step.clone(),
            tunnel: tunnel.cloned(),
            path: path.to_string(),
            meta,
            tensor: tensor.map(|(values, shape)| (values.to_vec(), shape)),
            dtype,
            attempts,
            peer_label: peer_label.to_string(),
            item_id,
            reply,
        };
        let should_flush = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow!("fila de lotes corrompida"))?;
            let entries = pending.entry(key.clone()).or_default();
            entries.push(call);
            entries.len() == 1
        };

        if should_flush {
            let coordinator = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(coordinator.window).await;
                coordinator.flush(key).await;
            });
        }

        receiver
            .await
            .map_err(|_| anyhow!("coordenador de lotes encerrou a chamada"))?
            .map_err(|error| anyhow!(error))
    }

    async fn flush(self: Arc<Self>, key: BatchKey) {
        let calls = match self.pending.lock() {
            Ok(mut pending) => pending.remove(&key).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if calls.is_empty() {
            return;
        }
        let batch_id = uuid::Uuid::new_v4().to_string();
        let items: Vec<wire::BatchPayloadItem> = calls
            .iter()
            .map(|call| wire::BatchPayloadItem {
                item_id: call.item_id.clone(),
                session_id: call
                    .meta
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("__session__")
                    .to_string(),
                meta: call.meta.clone(),
                tensor: call.tensor.clone(),
                dtype: call.dtype,
            })
            .collect();
        let result = async {
            let body = wire::encode_batch_items(&key.path, batch_id, &items)?;
            let first = calls.first().expect("non-empty calls");
            let response = send_batch_body(
                &first.clients,
                &first.step,
                first.tunnel.as_ref(),
                &first.path,
                body,
                first.attempts,
                &first.peer_label,
            )
            .await?;
            let (_, decoded) = wire::decode_batch_items(&response.body)?;
            let mut by_id = decoded
                .into_iter()
                .map(|item| (item.item_id.clone(), item))
                .collect::<HashMap<_, _>>();
            let mut output = Vec::with_capacity(calls.len());
            for call in &calls {
                let item = by_id
                    .remove(&call.item_id)
                    .ok_or_else(|| anyhow!("resposta batch não contém o item {}", call.item_id))?;
                let tls_handshake_ms = item
                    .meta
                    .get("tls_handshake_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                output.push(SentFrame {
                    meta: item.meta,
                    tensor: item.tensor,
                    round_trip_ms: response.round_trip_ms,
                    bytes_sent: response.bytes_sent / calls.len() as u64,
                    bytes_received: response.body.len() as u64 / calls.len() as u64,
                    client_encode_ms: response.client_encode_ms,
                    response_headers_ms: response.response_headers_ms,
                    response_body_ms: response.response_body_ms,
                    client_decode_ms: response.client_decode_ms,
                    tls: response.tls,
                    tls_handshake_ms,
                });
            }
            Ok::<_, anyhow::Error>(output)
        }
        .await;

        match result {
            Ok(outputs) => {
                for (call, output) in calls.into_iter().zip(outputs) {
                    let _ = call.reply.send(Ok(output));
                }
            }
            Err(error) => {
                let message = error.to_string();
                for call in calls {
                    let _ = call.reply.send(Err(message.clone()));
                }
            }
        }
    }
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
            "tls": self.tls,
            "tls_handshake_ms": self.tls_handshake_ms,
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
        write!(
            f,
            "peer {} recusou: {} {}",
            self.peer_label, self.status, self.body
        )
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
#[cfg(test)]
pub async fn send_frame(
    clients: &crate::peer_client::PeerClients,
    peer_id: &str,
    base_url: &str,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    attempts: u32,
    peer_label: &str,
    on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    send_frame_with_protocol(
        clients, peer_id, base_url, path, meta, tensor, dtype, encoding, false, attempts,
        peer_label, on_issue,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_frame_with_protocol(
    clients: &crate::peer_client::PeerClients,
    peer_id: &str,
    base_url: &str,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    use_batch_frame: bool,
    attempts: u32,
    peer_label: &str,
    mut on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    // O cliente sai do cache por peer: com o cluster em TLS ele carrega o
    // certificado esperado deste destino, e o pinning é o que distingue o peer
    // de quem se põe no meio dele.
    let http = &clients.for_peer(peer_id)?;
    let encode_started = Instant::now();
    let body = if use_batch_frame {
        wire::encode_batch_single(meta, tensor, dtype)?
    } else {
        match (tensor, dtype) {
            (Some(tensor), WireDtype::F16) => {
                wire::encode_with_encoding(meta, Some(tensor), encoding)?
            }
            (tensor, dtype) => wire::encode(meta, tensor, dtype)?,
        }
    };
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;
    let url = format!("{base_url}{path}");
    let attempts = attempts.max(1);
    let mut last_error = String::new();

    for attempt in 1..=attempts {
        let started = Instant::now();
        let request = http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(Duration::from_secs(600));
        let sent = clients
            .authorize_inference_request(request)
            .await
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
                // O handshake é medido no servidor e volta no meta da resposta:
                // é lá que o tempo é exato, porque é lá que o `accept` acontece.
                let tls_handshake_ms = meta
                    .get("tls_handshake_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                return Ok(SentFrame {
                    meta,
                    tensor,
                    tls: url.starts_with("https://"),
                    tls_handshake_ms,
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

struct BatchTransportResponse {
    body: Vec<u8>,
    round_trip_ms: u64,
    bytes_sent: u64,
    client_encode_ms: u64,
    response_headers_ms: u64,
    response_body_ms: u64,
    client_decode_ms: u64,
    tls: bool,
}

/// Send one already encoded batch. The batch coordinator uses this function
/// instead of recursively calling `send_frame_or_tunnel`, otherwise a flushed
/// batch would immediately be queued as another batch forever.
async fn send_batch_body(
    clients: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    path: &str,
    body: Vec<u8>,
    attempts: u32,
    peer_label: &str,
) -> Result<BatchTransportResponse> {
    let peer_id = step
        .libp2p_peer_id
        .as_deref()
        .and_then(|value| value.parse::<PeerId>().ok());
    let can_use_tunnel =
        step.supports(crate::pipeline::CAP_LIBP2P_TUNNEL) && peer_id.is_some() && tunnel.is_some();
    if let (true, Some(tunnel), Some(peer_id)) = (can_use_tunnel, tunnel, peer_id) {
        let auth = clients.inference_token().await;
        match send_batch_via_tunnel(tunnel, peer_id, path, body.clone(), peer_label, auth).await {
            Ok(response) => return Ok(response),
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) => {
                warn!("{peer_label}: libp2p batch falhou ({error}), tentando HTTPS direto")
            }
        }
    }

    let client = clients.for_peer(&step.node_id)?;
    let url = format!("{}{}", step.inference_url.trim_end_matches('/'), path);
    let attempts = attempts.max(1);
    let client_encode_ms = 0;
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        let started = Instant::now();
        let request = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(Duration::from_secs(600));
        let response = clients
            .authorize_inference_request(request)
            .await
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let response_headers_ms = started.elapsed().as_millis() as u64;
                let body_started = Instant::now();
                let bytes = response.bytes().await?.to_vec();
                let response_body_ms = body_started.elapsed().as_millis() as u64;
                let decode_started = Instant::now();
                wire::decode_batch(&bytes)?;
                let client_decode_ms = decode_started.elapsed().as_millis() as u64;
                return Ok(BatchTransportResponse {
                    body: bytes,
                    round_trip_ms: started.elapsed().as_millis() as u64,
                    bytes_sent: body.len() as u64,
                    client_encode_ms,
                    response_headers_ms,
                    response_body_ms,
                    client_decode_ms,
                    tls: url.starts_with("https://"),
                });
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(anyhow::Error::new(HttpRefusal {
                    status: status.as_u16(),
                    body: text,
                    peer_label: peer_label.to_string(),
                }));
            }
            Err(error) => {
                last_error = error.to_string();
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

async fn send_batch_via_tunnel(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    path: &str,
    body: Vec<u8>,
    peer_label: &str,
    auth_token: Option<String>,
) -> Result<BatchTransportResponse> {
    let mut headers = Vec::new();
    if let Some(token) = auth_token {
        headers.push((
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        ));
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    tunnel
        .send(TunnelOutbound {
            peer_id,
            request: TunnelRequest::post(path, body.clone(), headers),
            reply: reply_tx,
        })
        .await
        .map_err(|_| {
            anyhow!("{peer_label}: swarm de libp2p não está disponível (canal fechado)")
        })?;
    let started = Instant::now();
    let response = match tokio::time::timeout(TUNNEL_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(error))) => return Err(anyhow!("{peer_label}: falha no túnel libp2p: {error}")),
        Ok(Err(_)) => return Err(anyhow!("{peer_label}: túnel libp2p encerrou sem responder")),
        Err(_) => return Err(anyhow!("{peer_label}: túnel libp2p não respondeu a tempo")),
    };
    if !(200..300).contains(&response.status) {
        return Err(anyhow::Error::new(HttpRefusal {
            status: response.status,
            body: String::from_utf8_lossy(&response.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    wire::decode_batch(&response.body)?;
    Ok(BatchTransportResponse {
        body: response.body,
        round_trip_ms: started.elapsed().as_millis() as u64,
        bytes_sent: body.len() as u64,
        client_encode_ms: 0,
        response_headers_ms: started.elapsed().as_millis() as u64,
        response_body_ms: 0,
        client_decode_ms: 0,
        tls: false,
    })
}

/// Resposta crua de uma chamada HTTP encaminhada sem o enquadramento de uma
/// mensagem de inferência.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteTransport {
    Local,
    Direct,
    Tunnel,
}

pub struct RawPeerResponse {
    pub body: Vec<u8>,
    pub transport: ByteTransport,
    pub fallback: bool,
}

/// Envia bytes pelo túnel libp2p quando ele está disponível. Se o túnel falhar
/// por transporte ou timeout, repete o mesmo POST pelo HTTPS direto. O túnel
/// preserva caminho, corpo e cabeçalhos, pois o reduce TP carrega a barreira e
/// o rank fora do corpo.
#[allow(clippy::too_many_arguments)]
pub async fn send_bytes_or_tunnel(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    tunnel: Option<&TunnelOutboundSender>,
    peer_id: Option<PeerId>,
    supports_tunnel: bool,
    peer_label: &str,
) -> Result<RawPeerResponse> {
    let can_use_tunnel = tunnel.is_some() && supports_tunnel && peer_id.is_some();
    if let (Some(tunnel), Some(peer_id)) = (tunnel.filter(|_| supports_tunnel), peer_id) {
        match send_bytes_via_tunnel(
            tunnel,
            peer_id,
            path,
            body.clone(),
            headers.clone(),
            peer_label,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) => {
                warn!("{peer_label}: libp2p falhou ({error}), tentando HTTPS direto");
            }
        }
    }

    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut request = client
        .post(&url)
        .timeout(Duration::from_secs(600))
        .body(body.clone());
    for (name, value) in &headers {
        request = request.header(name, value);
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => Ok(RawPeerResponse {
            body: response.bytes().await?.to_vec(),
            transport: ByteTransport::Direct,
            fallback: can_use_tunnel,
        }),
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(anyhow::Error::new(HttpRefusal {
                status: status.as_u16(),
                body,
                peer_label: peer_label.to_string(),
            }))
        }
        Err(error) => Err(error.into()),
    }
}

async fn send_bytes_via_tunnel(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    path: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    peer_label: &str,
) -> Result<RawPeerResponse> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tunnel
        .send(TunnelOutbound {
            peer_id,
            request: TunnelRequest::post(path, body, headers),
            reply: reply_tx,
        })
        .await
        .map_err(|_| {
            anyhow!("{peer_label}: swarm de libp2p não está disponível (canal fechado)")
        })?;

    let response = match tokio::time::timeout(TUNNEL_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(error))) => return Err(anyhow!("{peer_label}: falha no túnel libp2p: {error}")),
        Ok(Err(_)) => return Err(anyhow!("{peer_label}: túnel libp2p encerrou sem responder")),
        Err(_) => return Err(anyhow!("{peer_label}: túnel libp2p não respondeu a tempo")),
    };

    if !(200..300).contains(&response.status) {
        return Err(anyhow::Error::new(HttpRefusal {
            status: response.status,
            body: String::from_utf8_lossy(&response.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    Ok(RawPeerResponse {
        body: response.body,
        transport: ByteTransport::Tunnel,
        fallback: false,
    })
}

/// Envia um frame pelo túnel libp2p quando o hop anuncia a capability, tem um
/// `libp2p_peer_id` conhecido e o swarm local está disponível. O túnel usa
/// Noise e leva a mesma credencial da porta de inferência. Se o transporte do
/// túnel falhar, a chamada segue pelo cliente direto pinado; uma recusa HTTP
/// do peer é final e não é repetida em outro transporte.
#[allow(clippy::too_many_arguments)]
pub async fn send_frame_or_tunnel(
    clients: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    attempts: u32,
    peer_label: &str,
    on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    let can_batch = step.supports(crate::pipeline::CAP_FRAME_BATCH)
        && !meta.get("reply_to").is_some_and(|value| !value.is_null())
        && meta.get("speculate").is_none()
        && meta.get("draft").is_none()
        && meta.get("mtp_k").is_none();
    if can_batch {
        let coordinator = BatchCoordinator::global().clone();
        let mut on_issue = on_issue;
        return match coordinator
            .submit(
                clients, step, tunnel, path, meta, tensor, dtype, attempts, peer_label,
            )
            .await
        {
            Ok(sent) => Ok(sent),
            Err(error) => {
                on_issue(SendIssue {
                    kind: SendIssueKind::Transport,
                    attempt: attempts.max(1),
                    message: error.to_string(),
                });
                Err(error)
            }
        };
    }
    let peer_id = step
        .libp2p_peer_id
        .as_deref()
        .and_then(|s| s.parse::<PeerId>().ok());

    let can_use_tunnel =
        step.supports(crate::pipeline::CAP_LIBP2P_TUNNEL) && peer_id.is_some() && tunnel.is_some();
    let use_batch_frame = step.supports(crate::pipeline::CAP_FRAME_BATCH);

    if let (true, Some(tunnel), Some(peer_id)) = (can_use_tunnel, tunnel, peer_id) {
        let auth = clients.inference_token().await;
        match send_frame_via_tunnel_with_auth(
            tunnel,
            peer_id,
            path,
            meta,
            tensor,
            dtype,
            encoding,
            use_batch_frame,
            peer_label,
            auth,
        )
        .await
        {
            Ok(sent) => return Ok(sent),
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) => warn!("{peer_label}: libp2p falhou ({error}), tentando HTTPS direto"),
        }
    }

    send_frame_with_protocol(
        clients,
        &step.node_id,
        &step.inference_url,
        path,
        meta,
        tensor,
        dtype,
        encoding,
        use_batch_frame,
        attempts,
        peer_label,
        on_issue,
    )
    .await
}

/// Predicado mantido para os testes e para consumidores que ainda classificam
/// uma falha antes de decidir o transporte. O caminho de produção atual tenta
/// o túnel primeiro e só usa essa classificação para a política de retry
/// legada.
///
/// `Http`-class issues are excluded on purpose: the peer *was* reached and
/// gave a considered refusal (bad request, model error, …) — retrying over a
/// different transport cannot fix that, it can only waste the tunnel's own
/// timeout finding the same answer again. Only `Timeout`/`Transport` — the
/// peer genuinely wasn't reachable — are worth a second attempt through a
/// path that doesn't share the same failure mode (a different route through
/// the swarm, not just the same TCP connection retried).
#[cfg(test)]
pub fn should_attempt_tunnel_fallback(
    last_issue_kind: SendIssueKind,
    step_supports_tunnel: bool,
    peer_id_known: bool,
    tunnel_available: bool,
) -> bool {
    step_supports_tunnel
        && peer_id_known
        && tunnel_available
        && matches!(
            last_issue_kind,
            SendIssueKind::Timeout | SendIssueKind::Transport
        )
}

/// Belt-and-suspenders ceiling on top of the tunnel protocol's own 60s
/// `request_response::Config` timeout (`infer_tunnel::infer_tunnel_behaviour`)
/// — that one should always fire first; this only guards against the swarm's
/// own task having died or wedged, which would otherwise hang the caller's
/// `oneshot::Receiver` forever instead of erroring.
const TUNNEL_REPLY_TIMEOUT: Duration = Duration::from_secs(75);

/// Uma chamada hop a hop enviada pelo swarm libp2p. O contrato de
/// encode/decode é o mesmo de `send_frame`, com uma única tentativa. Se o
/// túnel falhar por transporte, o chamador decide se deve usar o cliente
/// direto; repetir dentro do túnel duplicaria a latência de uma chamada já
/// lenta.
#[cfg(test)]
pub async fn send_frame_via_tunnel(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    peer_label: &str,
) -> Result<SentFrame> {
    send_frame_via_tunnel_with_auth(
        tunnel, peer_id, path, meta, tensor, dtype, encoding, false, peer_label, None,
    )
    .await
}

async fn send_frame_via_tunnel_with_auth(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], [usize; 2])>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    use_batch_frame: bool,
    peer_label: &str,
    auth_token: Option<String>,
) -> Result<SentFrame> {
    let encode_started = Instant::now();
    let body = if use_batch_frame {
        wire::encode_batch_single(meta, tensor, dtype)?
    } else {
        match (tensor, dtype) {
            (Some(tensor), WireDtype::F16) => {
                wire::encode_with_encoding(meta, Some(tensor), encoding)?
            }
            (tensor, dtype) => wire::encode(meta, tensor, dtype)?,
        }
    };
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;

    let (reply_tx, reply_rx) = oneshot::channel();
    let mut headers = Vec::new();
    if let Some(token) = auth_token {
        headers.push((
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        ));
    }
    let outbound = TunnelOutbound {
        peer_id,
        request: TunnelRequest::post(path, body.clone(), headers),
        reply: reply_tx,
    };
    tunnel.send(outbound).await.map_err(|_| {
        anyhow!("{peer_label}: swarm de libp2p não está disponível (canal fechado)")
    })?;

    let started = Instant::now();
    let response = match tokio::time::timeout(TUNNEL_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(e))) => return Err(anyhow!("{peer_label}: falha no túnel libp2p: {e}")),
        Ok(Err(_)) => return Err(anyhow!("{peer_label}: túnel libp2p encerrou sem responder")),
        Err(_) => return Err(anyhow!("{peer_label}: túnel libp2p não respondeu a tempo")),
    };
    let round_trip_ms = started.elapsed().as_millis() as u64;

    if !(200..300).contains(&response.status) {
        return Err(anyhow::Error::new(HttpRefusal {
            status: response.status,
            body: String::from_utf8_lossy(&response.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }

    let response_body_ms = 0;
    let client_decode_started = Instant::now();
    let (meta, tensor) = wire::decode(&response.body)?;
    let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;

    Ok(SentFrame {
        meta,
        tensor,
        // O túnel libp2p já é cifrado por Noise ponta a ponta, mas não é TLS:
        // dizer `true` aqui faria a tela alegar uma garantia que este salto não
        // deu. O handshake do túnel também não é o do TLS, e fica em zero.
        tls: false,
        tls_handshake_ms: 0,
        round_trip_ms,
        bytes_sent: body.len() as u64,
        bytes_received: response.body.len() as u64,
        client_encode_ms,
        response_headers_ms: round_trip_ms,
        response_body_ms,
        client_decode_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer_tunnel::TunnelResponse;

    #[tokio::test]
    async fn raw_peer_call_falls_back_to_tunnel_and_preserves_headers() {
        let peer_id = PeerId::random();
        let (tunnel, mut received) = mpsc::channel::<TunnelOutbound>(1);
        let expected_body = b"partial-reduce".to_vec();
        let expected_headers = vec![
            ("x-pom-tp-barrier".to_string(), "decode#0".to_string()),
            ("x-pom-tp-rank".to_string(), "1".to_string()),
        ];
        let receiver = tokio::spawn({
            let expected_body = expected_body.clone();
            let expected_headers = expected_headers.clone();
            async move {
                let outbound = received.recv().await.expect("tunnel request");
                assert_eq!(outbound.peer_id, peer_id);
                assert_eq!(outbound.request.path, "/v1/tp/reduce");
                assert_eq!(outbound.request.body, expected_body);
                assert_eq!(outbound.request.headers, expected_headers);
                outbound
                    .reply
                    .send(Ok(TunnelResponse {
                        status: 200,
                        body: b"reduced-by-tunnel".to_vec(),
                    }))
                    .expect("reply");
            }
        });

        let response = send_bytes_or_tunnel(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            "/v1/tp/reduce",
            expected_body,
            expected_headers,
            Some(&tunnel),
            Some(peer_id),
            true,
            "tp-primary",
        )
        .await
        .expect("tunnel fallback");

        assert_eq!(response.body, b"reduced-by-tunnel");
        receiver.await.expect("receiver");
    }

    fn frame(tls: bool, handshake: u64) -> SentFrame {
        SentFrame {
            meta: json!({ "processing_ms": 7 }),
            tensor: None,
            round_trip_ms: 12,
            bytes_sent: 100,
            bytes_received: 200,
            client_encode_ms: 1,
            response_headers_ms: 5,
            response_body_ms: 3,
            client_decode_ms: 1,
            tls,
            tls_handshake_ms: handshake,
        }
    }

    /// O handshake acontece **uma vez por conexão**, e as conexões ficam no pool
    /// do reqwest. Então zero na maioria dos saltos é a verdade, não falta de
    /// medição - e é por isso que a tela rotula o campo como custo de
    /// estabelecimento de conexão, nunca como custo por token.
    #[test]
    fn hop_timing_carrega_tls_e_handshake() {
        let e = frame(true, 4).as_hop_timing_entry();
        assert_eq!(e["tls"], true);
        assert_eq!(e["tls_handshake_ms"], 4);

        // Mesmo salto, conexão reaproveitada: cifrado e sem handshake.
        let e = frame(true, 0).as_hop_timing_entry();
        assert_eq!(e["tls"], true);
        assert_eq!(e["tls_handshake_ms"], 0);
    }

    /// Um salto em claro tem que ser visível como tal: é assim que uma virada
    /// pela metade aparece na tela em vez de passar despercebida.
    #[test]
    fn salto_em_claro_reporta_tls_falso() {
        let e = frame(false, 0).as_hop_timing_entry();
        assert_eq!(e["tls"], false);
        assert_eq!(e["tls_handshake_ms"], 0);
    }
    use tokio::sync::mpsc;

    #[test]
    fn only_timeout_and_transport_issues_ever_trigger_the_tunnel() {
        for kind in [SendIssueKind::Timeout, SendIssueKind::Transport] {
            assert!(should_attempt_tunnel_fallback(kind, true, true, true));
        }
        for kind in [SendIssueKind::Http, SendIssueKind::Protocol] {
            assert!(!should_attempt_tunnel_fallback(kind, true, true, true));
        }
    }

    #[test]
    fn every_precondition_must_hold_or_the_tunnel_is_skipped() {
        let kind = SendIssueKind::Timeout;
        assert!(
            !should_attempt_tunnel_fallback(kind, false, true, true),
            "step must advertise the capability"
        );
        assert!(
            !should_attempt_tunnel_fallback(kind, true, false, true),
            "the hop's PeerId must be known"
        );
        assert!(
            !should_attempt_tunnel_fallback(kind, true, true, false),
            "the local swarm must actually be up"
        );
    }

    /// `Result<SentFrame>::expect_err` would need `SentFrame: Debug`, which
    /// it deliberately doesn't derive — this reads the error out by hand
    /// instead of adding a derive purely for test convenience.
    fn expect_err(result: Result<SentFrame>, msg: &str) -> anyhow::Error {
        match result {
            Ok(_) => panic!("{msg}"),
            Err(e) => e,
        }
    }

    /// Stands in for `Discovery::run`'s outbound branch: drains one
    /// `TunnelOutbound` and answers it, without any real libp2p swarm — this
    /// exercises exactly the risk in `send_frame_via_tunnel` itself (framing,
    /// channel/oneshot correlation, status/error mapping), the same way
    /// `infer_server.rs`'s `relay_tests` fake a next hop with a real axum
    /// server instead of mocking `send_frame`.
    fn fake_swarm_answering_with(
        response: Result<crate::infer_tunnel::TunnelResponse, String>,
    ) -> TunnelOutboundSender {
        let (tx, mut rx) = mpsc::channel::<TunnelOutbound>(1);
        tokio::spawn(async move {
            if let Some(out) = rx.recv().await {
                let _ = out.reply.send(response);
            }
        });
        tx
    }

    #[tokio::test]
    async fn a_successful_tunnel_call_decodes_like_a_normal_frame() {
        let meta = json!({"done": true, "token_id": 7});
        let body = wire::encode(&meta, None, WireDtype::F16).unwrap();
        let tunnel = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body,
        }));

        let sent = send_frame_via_tunnel(
            &tunnel,
            PeerId::random(),
            "/v1/step",
            &json!({"session_id": "s1"}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            "peer-x",
        )
        .await
        .expect("tunnel call should succeed");

        assert_eq!(sent.meta["token_id"], 7);
        assert_eq!(sent.meta["done"], true);
    }

    #[tokio::test]
    async fn a_non_success_status_becomes_an_http_refusal() {
        let tunnel = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 500,
            body: b"kv cache corrupted".to_vec(),
        }));

        let result = send_frame_via_tunnel(
            &tunnel,
            PeerId::random(),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            "peer-x",
        )
        .await;
        let err = expect_err(result, "non-2xx status must surface as an error");

        let refusal = err
            .downcast_ref::<HttpRefusal>()
            .expect("expected HttpRefusal");
        assert_eq!(refusal.status, 500);
        assert!(refusal.body.contains("kv cache corrupted"));
    }

    #[tokio::test]
    async fn a_swarm_side_failure_surfaces_as_an_error() {
        let tunnel = fake_swarm_answering_with(Err("dial failed".to_string()));

        let result = send_frame_via_tunnel(
            &tunnel,
            PeerId::random(),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            "peer-x",
        )
        .await;
        let err = expect_err(result, "swarm failure must surface as an error");

        assert!(err.to_string().contains("dial failed"));
    }

    /// No `Discovery::run` task at all (libp2p disabled this run) — the
    /// channel itself has no live receiver, so `send` fails immediately
    /// instead of hanging until the reply timeout.
    #[tokio::test]
    async fn a_closed_channel_fails_fast_instead_of_hanging() {
        let (tx, rx) = mpsc::channel::<TunnelOutbound>(1);
        drop(rx);

        let result = send_frame_via_tunnel(
            &tx,
            PeerId::random(),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            "peer-x",
        )
        .await;
        let err = expect_err(result, "closed channel must fail immediately");

        assert!(err.to_string().contains("canal fechado"));
    }

    // ── send_frame_or_tunnel ─────────────────────────────────────────────────

    fn step_with(
        inference_url: &str,
        supports_tunnel: bool,
        peer_id: Option<String>,
    ) -> crate::pipeline::PipelineStep {
        crate::pipeline::PipelineStep {
            node_id: "peer-x".into(),
            inference_url: inference_url.into(),
            inference_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            inference_frame_capabilities: if supports_tunnel {
                vec![crate::pipeline::CAP_LIBP2P_TUNNEL.to_string()]
            } else {
                Vec::new()
            },
            libp2p_peer_id: peer_id,
        }
    }

    /// A hop that advertises the tunnel uses it before trying HTTP. The
    /// response is supplied by a fake swarm, so this test never waits for the
    /// 75-second tunnel timeout.
    #[tokio::test]
    async fn a_qualified_peer_prefers_the_tunnel() {
        let step = step_with(
            "http://127.0.0.1:1",
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_body = wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("qualified peer must succeed over libp2p");

        assert_eq!(sent.meta["done"], true);
    }

    /// An HTTP server that spawns per-test on loopback, for `send_frame_or_tunnel`
    /// coverage — same pattern as `infer_tunnel::tests::spawn_fake_inference_server`,
    /// duplicated locally since it lives in a different module.
    async fn spawn_fake_inference_server(
        handler: impl Fn(Vec<u8>) -> (u16, Vec<u8>) + Clone + Send + Sync + 'static,
    ) -> u16 {
        use axum::{body::Bytes, routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/step",
            post(move |body: Bytes| {
                let handler = handler.clone();
                async move {
                    let (status, body) = handler(body.to_vec());
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    /// The scenario the whole fallback exists for: the direct hop is
    /// unreachable, but it advertises the capability and a known `PeerId`, so
    /// the call still completes via the tunnel instead of failing outright.
    #[tokio::test]
    async fn an_unreachable_peer_recovers_via_the_tunnel_when_qualified() {
        let unreachable = step_with(
            "http://127.0.0.1:1",
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_body = wire::encode(&json!({"token_id": 99}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &unreachable,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("should recover via the tunnel");

        assert_eq!(sent.meta["token_id"], 99);
    }

    #[tokio::test]
    async fn a_peer_with_batch_capability_receives_the_versioned_single_item_frame() {
        let port = spawn_fake_inference_server(|body| {
            let batch = wire::decode_batch(&body).expect("capable peer must receive frame v3");
            assert_eq!(batch.version, wire::BATCH_FRAME_VERSION);
            assert_eq!(batch.items.len(), 1);
            assert_eq!(batch.items[0].session_id, "session-v3");
            (
                200,
                wire::encode_batch_items(
                    batch.kind,
                    batch.batch_id,
                    &[wire::BatchPayloadItem {
                        item_id: batch.items[0].item_id.clone(),
                        session_id: batch.items[0].session_id.clone(),
                        meta: json!({"done": true}),
                        tensor: None,
                        dtype: WireDtype::F16,
                    }],
                )
                .unwrap(),
            )
        })
        .await;
        let mut step = step_with(&format!("http://127.0.0.1:{port}"), false, None);
        step.inference_frame_capabilities
            .push(crate::pipeline::CAP_FRAME_BATCH.into());

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            None,
            "/v1/step",
            &json!({"session_id": "session-v3"}),
            Some((&[0.25_f32, -0.5], [1, 2])),
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("capable peer must accept the v3 adapter");

        assert_eq!(sent.meta["done"], true);
    }

    #[tokio::test]
    async fn concurrent_calls_to_the_same_hop_are_sent_as_one_batch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let request_count = Arc::new(AtomicUsize::new(0));
        let observed_sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let request_count_for_server = request_count.clone();
        let observed_sizes_for_server = observed_sizes.clone();
        let port = spawn_fake_inference_server(move |body| {
            request_count_for_server.fetch_add(1, Ordering::Relaxed);
            let batch = wire::decode_batch(&body).expect("batch obrigatório");
            observed_sizes_for_server
                .lock()
                .unwrap()
                .push(batch.items.len());
            let items = batch
                .items
                .iter()
                .map(|item| wire::BatchPayloadItem {
                    item_id: item.item_id.clone(),
                    session_id: item.session_id.clone(),
                    meta: json!({"done": true, "item_id": item.item_id}),
                    tensor: None,
                    dtype: WireDtype::F16,
                })
                .collect::<Vec<_>>();
            (
                200,
                wire::encode_batch_items(batch.kind, batch.batch_id, &items).unwrap(),
            )
        })
        .await;
        let mut step = step_with(&format!("http://127.0.0.1:{port}"), false, None);
        step.inference_frame_capabilities
            .push(crate::pipeline::CAP_FRAME_BATCH.into());
        let clients = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));
        let first_meta = json!({"session_id": "session-a"});
        let second_meta = json!({"session_id": "session-b"});

        let first = send_frame_or_tunnel(
            &clients,
            &step,
            None,
            "/v1/step",
            &first_meta,
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        );
        let second = send_frame_or_tunnel(
            &clients,
            &step,
            None,
            "/v1/step",
            &second_meta,
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        );
        let (first, second) = tokio::join!(first, second);
        assert!(first.is_ok(), "primeira chamada falhou");
        assert!(second.is_ok(), "segunda chamada falhou");
        assert_eq!(request_count.load(Ordering::Relaxed), 1);
        assert_eq!(*observed_sizes.lock().unwrap(), vec![2]);
        assert_eq!(first.unwrap().meta["done"], true);
        assert_eq!(second.unwrap().meta["done"], true);
    }

    /// Missing any one precondition (capability, known `PeerId`, or a live
    /// tunnel at all) must leave the original HTTP failure as the answer —
    /// never a silent success from a path that was never actually offered.
    #[tokio::test]
    async fn an_unreachable_peer_without_the_capability_stays_failed() {
        let unreachable = step_with(
            "http://127.0.0.1:1",
            false,
            Some(PeerId::random().to_string()),
        );
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: wire::encode(&json!({}), None, WireDtype::F16).unwrap(),
        }));

        let result = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &unreachable,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await;

        expect_err(
            result,
            "an unqualified hop must not fall back to the tunnel",
        );
    }

    /// A refusal generated by libp2p is final. HTTPS must not repeat the same
    /// frame after the peer explicitly rejected it.
    #[tokio::test]
    async fn a_tunnel_http_refusal_never_falls_back_to_https() {
        let port = spawn_fake_inference_server(|_body| (500, b"model crashed".to_vec())).await;
        let step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 500,
            body: b"model crashed in tunnel".to_vec(),
        }));

        let result = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await;

        let err = expect_err(result, "an HTTP refusal must surface as an error");
        assert!(err.downcast_ref::<HttpRefusal>().is_some());
        assert!(err.to_string().contains("model crashed in tunnel"));
    }

    /// A troca de transporte, contra uma máquina que está mesmo na rede.
    ///
    /// Os testes acima provam a decisão com um canal falso; este prova o que
    /// eles não podem: que o quadro sai daqui pelo enxame libp2p, chega ao
    /// servidor de inferência do outro lado e volta com a resposta *dele*. É a
    /// lacuna que o commit do túnel registrou por escrito ("only loopback and
    /// in-process fakes").
    ///
    /// ```sh
    /// POM_TUNNEL_PROBE=/ip6/<ip-do-peer>/tcp/<porta-p2p+10000>/p2p/<peer-id> \\
    ///   cargo test -p node --bin pom the_tunnel_carries_a_real_frame -- --ignored --nocapture
    /// ```
    ///
    /// Duas afirmações, nesta ordem:
    ///
    /// 1. **O desvio abre pelo motivo certo.** O HTTP direto aponta para
    ///    `192.0.2.1` (TEST-NET-1, RFC 5737, nunca roteável), então a primeira
    ///    perna falha por `Timeout`/`Transport` - o gatilho real - e não por
    ///    uma recusa considerada, que nunca deve desviar. Custa caro: o prazo
    ///    do `send_frame` é de 600 s por requisição (ele sobrepõe o do cliente),
    ///    então quem manda a conta é o `connect` do sistema - medido em 75 s no
    ///    macOS contra um endereço que engole os SYN. É esse o atraso que um
    ///    hop atrás de NAT impõe **antes** de o desvio começar.
    /// 2. **O quadro atravessa.** `/v1/session/release` é a rota mais barata do
    ///    servidor de inferência e não toca o modelo; ela lê JSON, não um
    ///    quadro, então recusa este com um status *dela*. Uma recusa HTTP vinda
    ///    do peer prova o caminho inteiro; um erro de transporte provaria o
    ///    contrário. `/v1/step` não serve aqui: com uma sessão que não existe
    ///    ele fica esperando, e a sonda mediria a espera em vez do caminho.
    #[tokio::test]
    #[ignore = "precisa de um peer vivo; ver o doc do teste"]
    async fn the_tunnel_carries_a_real_frame_to_a_live_peer() {
        let target = std::env::var("POM_TUNNEL_PROBE")
            .expect("defina POM_TUNNEL_PROBE com o multiaddr do peer");
        let addr: libp2p::Multiaddr = target.parse().expect("multiaddr inválido");
        let peer_id = addr
            .iter()
            .find_map(|p| match p {
                libp2p::multiaddr::Protocol::P2p(id) => Some(id),
                _ => None,
            })
            .expect("o multiaddr tem que terminar em /p2p/<peer-id>");

        let tunnel_tx = crate::infer_tunnel::live_probe_swarm(peer_id, addr);
        // Um endereço que nunca responde: é assim que se força o `Timeout` sem
        // mexer na máquina do outro lado.
        let step = step_with("http://192.0.2.1:8181", true, Some(peer_id.to_string()));

        let mut issues = Vec::new();
        let direto = std::time::Instant::now();
        let _ = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/session/release",
            &json!({"session_id": "sonda-do-tunel"}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-vivo",
            |issue| issues.push(issue.kind),
        )
        .await;

        println!(
            "primeira perna (HTTP direto): {issues:?} em {} s",
            direto.elapsed().as_secs()
        );
        assert!(
            issues
                .iter()
                .any(|k| matches!(k, SendIssueKind::Timeout | SendIssueKind::Transport)),
            "o HTTP direto tem que falhar por Timeout/Transport para o desvio abrir: {issues:?}"
        );

        let crossed = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            send_frame_via_tunnel(
                &tunnel_tx,
                peer_id,
                "/v1/session/release",
                &json!({"session_id": "sonda-do-tunel"}),
                None,
                WireDtype::F16,
                FrameEncoding::F16,
                "peer-vivo",
            ),
        )
        .await
        .expect("o peer não respondeu pelo túnel dentro do prazo");

        match crossed {
            Ok(sent) => println!("o peer respondeu pelo túnel: {:?}", sent.meta),
            Err(e) => {
                let refusal = e.downcast_ref::<HttpRefusal>().unwrap_or_else(|| {
                    panic!("o quadro não chegou ao peer - isto é falha de transporte: {e}")
                });
                println!(
                    "o peer recusou pelo túnel (o que já prova o caminho): {} {}",
                    refusal.status, refusal.body
                );
            }
        }
    }
}
