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
use crate::wire::{self, FrameEncoding, TensorShape, WireDtype};
use anyhow::{anyhow, Result};
use axum::body::Bytes;
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, warn};

#[async_trait::async_trait]
pub(crate) trait InProcessInference: Send + Sync {
    async fn dispatch(&self, path: &str, body: Bytes) -> Result<axum::response::Response>;
}

pub(crate) type LocalInference = Arc<dyn InProcessInference>;

type LocalInferenceSlot = Arc<tokio::sync::RwLock<Option<(u64, LocalInference)>>>;

fn local_inference_slot() -> &'static LocalInferenceSlot {
    static SLOT: OnceLock<LocalInferenceSlot> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::RwLock::new(None)))
}

pub(crate) async fn install_local_inference(generation: u64, local: LocalInference) {
    *local_inference_slot().write().await = Some((generation, local));
}

pub(crate) async fn clear_local_inference(generation: u64) {
    let mut slot = local_inference_slot().write().await;
    if slot
        .as_ref()
        .is_some_and(|(current, _)| *current == generation)
    {
        *slot = None;
    }
}

async fn current_local_inference() -> Option<LocalInference> {
    local_inference_slot()
        .read()
        .await
        .as_ref()
        .map(|(_, local)| local.clone())
}

#[cfg(test)]
pub(crate) async fn set_test_local_inference(local: Option<LocalInference>) {
    *local_inference_slot().write().await = local.map(|local| (u64::MAX, local));
}

/// Tentativas do mesmo chunk quando todos os caminhos provam que não
/// entregaram o corpo. Uma falha depois do início do envio é ambígua e nunca é
/// repetida automaticamente, porque o peer pode já ter avançado o KV.
pub const CHUNK_ATTEMPTS: u32 = 3;

#[derive(Debug)]
struct FrameNotDelivered {
    message: String,
}

impl std::fmt::Display for FrameNotDelivered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FrameNotDelivered {}

/// `true` apenas quando a camada de transporte confirma que o frame não
/// chegou ao handler remoto. Timeout de resposta e erro durante envio ficam
/// deliberadamente fora desta classe.
pub fn is_pre_delivery_error(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<FrameNotDelivered>().is_some() {
        return true;
    }
    if error
        .downcast_ref::<crate::quic::QuicDeliveryError>()
        .is_some_and(|failure| failure.before_body)
    {
        return true;
    }
    if error
        .downcast_ref::<crate::infer_tunnel::TunnelDeliveryError>()
        .is_some_and(|failure| failure.before_request)
    {
        return true;
    }
    if error
        .downcast_ref::<crate::link::LinkDeliveryError>()
        .is_some_and(|failure| failure.before_frame)
    {
        return true;
    }
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|failure| failure.is_connect() || failure.is_builder())
}

/// `true` quando o transporte pode ter entregue o corpo antes de perder a
/// resposta. O gateway só deve reconstruir a sessão inteira nesses casos; uma
/// falha de conexão comprovadamente anterior ao corpo pode retomar o chunk.
pub fn is_delivery_ambiguous(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<FrameNotDelivered>().is_some()
        || error.downcast_ref::<HttpRefusal>().is_some()
    {
        return false;
    }
    if let Some(failure) = error.downcast_ref::<crate::quic::QuicDeliveryError>() {
        return !failure.before_body;
    }
    if let Some(failure) = error.downcast_ref::<crate::infer_tunnel::TunnelDeliveryError>() {
        return !failure.before_request;
    }
    if let Some(failure) = error.downcast_ref::<crate::link::LinkDeliveryError>() {
        return !failure.before_frame;
    }
    if error.downcast_ref::<TransportDeadline>().is_some() {
        return true;
    }
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|failure| !failure.is_connect() && !failure.is_builder())
}

fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_millis(100u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(3)))
}

#[cfg(not(test))]
const DIRECT_FRAME_TIMEOUT: Duration = Duration::from_secs(600);

#[cfg(test)]
const DIRECT_FRAME_TIMEOUT: Duration = Duration::from_millis(250);

/// Transporte que realmente carregou um frame de inferência.
///
/// `tls` continua separado porque o túnel libp2p é cifrado por Noise, mas não
/// é TLS. O nome do transporte é a evidência necessária para comparar o
/// caminho de inferência com o caminho HTTP de arquivos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InferenceTransport {
    Local,
    Libp2pTunnel,
    Http,
    Https,
    Quic,
    /// The remote decode stream's callback transport. It carries batches
    /// rather than one request/response per generated token.
    RemoteStream,
    /// A ligação persistente entre andares (`transport/link-v1`): o mesmo
    /// HTTP ou HTTPS pinado, mas um corpo em fluxo que carrega quadros
    /// sucessivos em vez de uma requisição por ativação.
    HttpsLink,
    Unknown,
}

impl InferenceTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Libp2pTunnel => "libp2p-tunnel",
            Self::Http => "http",
            Self::Https => "https",
            Self::Quic => "quic",
            Self::RemoteStream => "remote-stream",
            Self::HttpsLink => "https-link",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(value: &str) -> Self {
        match value {
            "local" => Self::Local,
            "libp2p-tunnel" => Self::Libp2pTunnel,
            "http" => Self::Http,
            "https" => Self::Https,
            "quic" => Self::Quic,
            "remote-stream" => Self::RemoteStream,
            "https-link" => Self::HttpsLink,
            _ => Self::Unknown,
        }
    }
}

fn tcp_transport_for_url(url: &str) -> InferenceTransport {
    if url.starts_with("https://") {
        InferenceTransport::Https
    } else {
        InferenceTransport::Http
    }
}

/// One hop's response, plus the client-observable timing a caller needs to
/// build its own bookkeeping from (a `RequestTrace` hop entry in
/// `prefill.rs`, or a `hop_timings` array entry in `infer_server.rs`).
pub struct SentFrame {
    pub meta: Value,
    pub tensor: Option<(Vec<f32>, TensorShape)>,
    pub round_trip_ms: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Tensor bytes sent on this crossing, excluding the frame header.
    pub hidden_state_bytes_sent: u64,
    /// Tensor bytes returned by this crossing, excluding the frame header.
    pub hidden_state_bytes_received: u64,
    /// Wire precision selected for this crossing.
    pub hidden_state_dtype: WireDtype,
    pub client_encode_ms: u64,
    /// For QUIC, wall time awaiting request body send and finish. Batch
    /// responses attribute the shared upload once to avoid duplicate totals.
    pub request_body_send_us: Option<u64>,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    pub client_decode_ms: u64,
    /// Estado observável do cliente/pool reqwest usado nesta travessia.
    pub connection_state: crate::peer_client::ConnectionState,
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
    /// RTT suavizado medido pela pilha QUIC, sem o processamento do handler
    /// remoto. TCP não expõe esse valor pelo reqwest e usa a diferença entre
    /// wall-clock e `processing_ms` como medição de transporte.
    pub transport_rtt_us: Option<u64>,
    /// Transporte escolhido para esta travessia, sem agregar os hops
    /// seguintes.
    pub transport: InferenceTransport,
    /// True quando o transporte preferido falhou e o frame usou uma reserva.
    pub transport_fallback: bool,
    /// Motivo da queda para HTTPS, preservado para a UI e para o relatório de
    /// diagnóstico. `None` no caminho normal ou quando não houve fallback.
    pub transport_fallback_reason: Option<String>,
    /// Eventos de transporte detalhados, somente quando o profiling está
    /// ativo. O modo normal não cria esta coleção.
    pub profiling_spans: Vec<crate::prefill::TelemetrySpan>,
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
    local: Option<LocalInference>,
    path: String,
    meta: Value,
    tensor: Option<(Vec<f32>, TensorShape)>,
    dtype: WireDtype,
    attempts: u32,
    peer_label: String,
    item_id: String,
    reply: oneshot::Sender<std::result::Result<SentFrame, String>>,
}

/// Coalesces independent requests that are about to cross the same pipeline
/// edge. The queue is process-local on purpose: each peer owns its own
/// transport and its own llama context. The flush is scheduled immediately,
/// so calls already concurrent in this executor turn can still share a frame
/// without imposing a fixed delay on a lone request.
pub struct BatchCoordinator {
    pending: Mutex<HashMap<BatchKey, Vec<BatchCall>>>,
}

impl BatchCoordinator {
    fn global() -> &'static Arc<Self> {
        static INSTANCE: OnceLock<Arc<BatchCoordinator>> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            Arc::new(Self {
                pending: Mutex::new(HashMap::new()),
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
        local: Option<LocalInference>,
        path: &str,
        meta: &Value,
        tensor: Option<(&[f32], TensorShape)>,
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
            local,
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
        let payloads = calls
            .iter()
            .map(|call| {
                wire::tensor_payload_len(
                    call.tensor
                        .as_ref()
                        .map(|(values, shape)| (values.as_slice(), shape.clone())),
                    call.dtype,
                )
                .unwrap_or(wire::MAX_BATCH_PAYLOAD_BYTES.saturating_add(1))
            })
            .collect::<Vec<_>>();
        let groups = partition_batch_payloads(&payloads);
        let mut slots = calls.into_iter().map(Some).collect::<Vec<_>>();
        let groups = groups
            .into_iter()
            .map(|indices| {
                indices
                    .into_iter()
                    .map(|index| slots[index].take().expect("batch call assigned once"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        futures::future::join_all(
            groups
                .into_iter()
                .map(|calls| flush_batch_group(key.path.clone(), calls)),
        )
        .await;
    }
}

async fn flush_batch_group(path: String, calls: Vec<BatchCall>) {
    let batch_id = uuid::Uuid::new_v4().to_string();
    let items = calls
        .iter()
        .map(|call| {
            let session_id = call
                .meta
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|session_id| !session_id.trim().is_empty())
                .ok_or_else(|| anyhow!("chamada batch {} sem session_id", call.item_id))?;
            Ok(wire::BatchPayloadItem {
                item_id: call.item_id.clone(),
                session_id: session_id.to_string(),
                meta: call.meta.clone(),
                tensor: call.tensor.clone(),
                dtype: call.dtype,
            })
        })
        .collect::<Result<Vec<_>>>();
    let result = match items {
        Err(error) => Err(error),
        Ok(items) => {
            async {
                let body = wire::encode_batch_items(&path, batch_id.clone(), &items)?;
                let first = calls.first().expect("non-empty calls");
                let profiling = crate::prefill::TelemetryContext::from_meta(
                    &first.meta,
                    first.peer_label.clone(),
                    if path == "/v1/prefill" {
                        crate::prefill::Phase::Prefill
                    } else {
                        crate::prefill::Phase::Decode
                    },
                    Instant::now(),
                    None,
                );
                let response = send_batch_body(
                    &first.clients,
                    &first.step,
                    first.tunnel.as_ref(),
                    first.local.clone(),
                    &first.path,
                    &batch_id,
                    Bytes::from(body),
                    first.attempts,
                    &first.peer_label,
                    first.meta.get("session_id").and_then(Value::as_str),
                    profiling,
                )
                .await?;
                let (_, decoded) = wire::decode_batch_items(&response.body)?;
                let mut by_id = decoded
                    .into_iter()
                    .map(|item| (item.item_id.clone(), item))
                    .collect::<HashMap<_, _>>();
                let mut output = Vec::with_capacity(calls.len());
                for call in &calls {
                    let item = by_id.remove(&call.item_id).ok_or_else(|| {
                        anyhow!("resposta batch não contém o item {}", call.item_id)
                    })?;
                    let hidden_state_bytes_sent = hidden_state_bytes(
                        call.tensor
                            .as_ref()
                            .map(|(values, shape)| (values.as_slice(), shape.clone())),
                        call.dtype,
                    )?;
                    let hidden_state_bytes_received = received_hidden_state_bytes(
                        &item.meta,
                        item.tensor
                            .as_ref()
                            .map(|(values, shape)| (values.as_slice(), shape.clone())),
                        call.dtype,
                        &call.peer_label,
                    )?;
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
                        hidden_state_bytes_sent,
                        hidden_state_bytes_received,
                        hidden_state_dtype: call.dtype,
                        client_encode_ms: response.client_encode_ms,
                        request_body_send_us: if output.is_empty() {
                            response.request_body_send_us
                        } else {
                            None
                        },
                        response_headers_ms: response.response_headers_ms,
                        response_body_ms: response.response_body_ms,
                        client_decode_ms: response.client_decode_ms,
                        connection_state: response.connection_state,
                        tls: response.tls,
                        tls_handshake_ms,
                        transport_rtt_us: response.transport_rtt_us,
                        transport: response.transport,
                        transport_fallback: response.transport_fallback,
                        transport_fallback_reason: response.transport_fallback_reason.clone(),
                        profiling_spans: response.profiling_spans.clone(),
                    });
                }
                Ok::<_, anyhow::Error>(output)
            }
            .await
        }
    };

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

impl SentFrame {
    /// O canal persistente é uma otimização do hop HTTP, não um transporte
    /// observável separado. A telemetria pública informa a camada que cifrou
    /// o tráfego, mantendo `HttpsLink` apenas para a bancada interna.
    pub fn telemetry_transport(&self) -> InferenceTransport {
        match self.transport {
            InferenceTransport::HttpsLink if self.tls => InferenceTransport::Https,
            InferenceTransport::HttpsLink => InferenceTransport::Http,
            transport => transport,
        }
    }

    /// This hop's own contribution to a bubbled-back `hop_timings` array:
    /// what the *caller* observed of the round trip (`self`'s fields)
    /// combined with what the *callee* reported about itself in its own
    /// reply meta (`processing_ms`, `node_timings` — already present in
    /// every reply today, hub or ring). One entry, for the peer just called.
    pub fn as_hop_timing_entry(&self) -> Value {
        let transport = self.telemetry_transport().as_str();
        json!({
            "round_trip_ms": self.round_trip_ms,
            "processing_ms": self.meta.get("processing_ms").cloned().unwrap_or(json!(0)),
            "bytes_sent": self.bytes_sent,
            "bytes_received": self.bytes_received,
            "hidden_state_bytes_sent": self.hidden_state_bytes_sent,
            "hidden_state_bytes_received": self.hidden_state_bytes_received,
            "hidden_state_dtype": self.hidden_state_dtype.as_str(),
            "client_encode_ms": self.client_encode_ms,
            "request_body_send_us": self.request_body_send_us,
            "response_headers_ms": self.response_headers_ms,
            "response_body_ms": self.response_body_ms,
            "client_decode_ms": self.client_decode_ms,
            "connection_state": self.connection_state.as_str(),
            "tls": self.tls,
            "tls_handshake_ms": self.tls_handshake_ms,
            "transport_rtt_us": self.transport_rtt_us,
            "transport": transport,
            "transport_fallback": self.transport_fallback,
            "transport_fallback_reason": self.transport_fallback_reason,
            "profiling_spans": self.profiling_spans,
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

/// A frame-size refusal is deterministic for the current chunk. Retrying the
/// same prefix, or repeating it cold, cannot change its encoded dimensions.
pub fn is_frame_size_error(error: &anyhow::Error) -> bool {
    if error
        .downcast_ref::<HttpRefusal>()
        .is_some_and(|refusal| refusal.status == 413)
    {
        return true;
    }
    let text = error.to_string();
    text.contains("payload do batch excede limite")
        || text.contains("payload do tensor excede limite")
        || text.contains("corpo do frame excede limite")
}

/// Identifica a recusa que o gateway consegue reparar reconstruindo o prefill
/// desde o prompt completo. A mensagem pode vir diretamente de `HttpRefusal`
/// ou dentro de um `RelayFailure` serializado pelo anel/resposta direta, mas
/// `anyhow::Error::to_string` preserva o texto nos dois casos.
pub fn is_unknown_session_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string().to_lowercase();
        text.contains("sessão") && text.contains("desconhecida")
    })
}

/// Erros que provam divergência do KV remoto. Diferente de uma falha de
/// transporte antes da entrega, eles pedem que a sessão inteira seja limpa.
pub fn is_prefill_state_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("chunk fora de ordem")
        || text.contains("sem start_pos absoluto")
        || text.contains("não aceita descartar posições do meio do cache")
}

/// Uma falha de repasse entre hops causada pelo **transporte** - caminho
/// morto, timeout, ou recusa de autenticação 401/403 - e não pelo conteúdo do
/// pedido. Um gap transitório na propagação da política de rede (TLS/QUIC
/// ainda não confirmados num peer, credencial interna ainda não adotada) cai
/// aqui e some sozinho, então vale um único prefill frio antes de desistir.
///
/// Um `400`/`413`/erro de modelo do peer **não** casa: repetir não muda nada.
/// A mensagem chega achatada por `anyhow::Error::to_string`, venha de um
/// `HttpRefusal`, de um `RelayFailure` serializado pelo anel/resposta direta
/// (`"pipeline falhou em <no>: <kind> ..."`) ou do texto do próprio frame.
pub fn is_transport_relay_failure(error: &anyhow::Error) -> bool {
    if let Some(failure) = extract_relay_failure(error) {
        let kind = failure.kind.trim().to_ascii_lowercase();
        if kind == "transport" || kind == "timeout" {
            return true;
        }
    }
    let text = error.to_string().to_lowercase();
    if text.contains("missing authorization header") {
        return true;
    }
    if text.contains("pipeline falhou em")
        && (text.contains(": transport") || text.contains(": timeout"))
    {
        return true;
    }
    text.contains("recusou: 401")
        || text.contains("recusou: 403")
        || text.contains("http 401:")
        || text.contains("http 403:")
}

/// The request payload does not reveal the size of a hidden-state response
/// produced by a head hop. Coalescing such calls could create a response batch
/// larger than the wire limit even when every request was tiny. Ring frames
/// already carry their downstream route and return metadata, so they remain
/// safe to coalesce; hub frames without a tensor are sent one at a time.
fn can_coalesce_frame(path: &str, meta: &Value, tensor: Option<(&[f32], TensorShape)>) -> bool {
    let hidden_response = tensor.is_none()
        && matches!(path, "/v1/prefill" | "/v1/step")
        && meta.get("is_last").and_then(Value::as_bool) == Some(false)
        && !meta
            .get("next_hops")
            .and_then(Value::as_array)
            .is_some_and(|hops| !hops.is_empty());
    !hidden_response
}

fn can_use_batch_transport(
    step: &crate::pipeline::PipelineStep,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
) -> bool {
    can_coalesce_frame(path, meta, tensor)
        && step.supports(crate::pipeline::CAP_FRAME_BATCH)
        && !meta.get("reply_to").is_some_and(|value| !value.is_null())
        && meta.get("speculate").is_none()
}

/// Partition item indexes so every coalesced tensor payload leaves room for
/// the batch header. An item larger than the budget remains alone and fails
/// with the precise single-frame error during encoding.
fn partition_batch_payloads(payloads: &[usize]) -> Vec<Vec<usize>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0usize;

    for (index, bytes) in payloads.iter().copied().enumerate() {
        if !current.is_empty()
            && current_bytes.saturating_add(bytes) > wire::MAX_BATCH_PAYLOAD_BYTES
        {
            groups.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(index);
        current_bytes = current_bytes.saturating_add(bytes);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// If `send_frame` failed because a relaying peer reported (via
/// `RelayFailure`) that *its* next hop was the real problem, return that —
/// otherwise `None`, meaning the failure originated at the peer we called
/// directly.
pub fn extract_relay_failure(e: &anyhow::Error) -> Option<RelayFailure> {
    let refusal = e.downcast_ref::<HttpRefusal>()?;
    serde_json::from_str(&refusal.body).ok()
}

/// POST one wire frame to `{base_url}{path}`, retrying up to `attempts`
/// times on failures proven to precede delivery, with a short millisecond
/// backoff. An explicit HTTP error response is never
/// retried — it's the peer's considered refusal, not a hiccup. `attempts=1`
/// means exactly one try, no retry at all (the decode/`/v1/step` path has
/// never retried, since, unlike prefill chunks, a step has no KV-rollback
/// idempotency protection against being redecoded).
fn encode_frame_body(
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    _encoding: FrameEncoding,
    use_batch_frame: bool,
) -> Result<Vec<u8>> {
    if use_batch_frame {
        wire::encode_batch_single(meta, tensor, dtype)
    } else {
        wire::encode(meta, tensor, dtype)
    }
}

fn can_use_single_link(path: &str) -> bool {
    matches!(path, "/v1/prefill" | "/v1/step")
}

fn encode_link_single(
    path: &str,
    batch_id: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
) -> Result<Vec<u8>> {
    let session_id = meta
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("quadro persistente sem session_id"))?;
    let item_id = meta
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(batch_id);
    wire::encode_batch_items(
        path,
        batch_id,
        &[wire::BatchPayloadItem {
            item_id: item_id.to_owned(),
            session_id: session_id.to_owned(),
            meta: meta.clone(),
            tensor: tensor.map(|(values, shape)| (values.to_vec(), shape)),
            dtype,
        }],
    )
}

fn hidden_state_bytes(tensor: Option<(&[f32], TensorShape)>, dtype: WireDtype) -> Result<u64> {
    Ok(wire::tensor_payload_len(tensor, dtype)? as u64)
}

fn received_hidden_state_bytes(
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    expected_dtype: WireDtype,
    peer_label: &str,
) -> Result<u64> {
    let Some(tensor) = tensor else {
        return Ok(0);
    };
    let actual_dtype = WireDtype::parse(
        meta.get("dtype")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{peer_label}: resposta com estado oculto sem dtype"))?,
    )?;
    if actual_dtype != expected_dtype {
        return Err(anyhow!(
            "{peer_label}: resposta de estado oculto usa {}, mas o formato negociado é {}",
            actual_dtype.as_str(),
            expected_dtype.as_str()
        ));
    }
    hidden_state_bytes(Some(tensor), actual_dtype)
}

async fn send_frame_in_process(
    local: LocalInference,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    use_batch_frame: bool,
    peer_label: &str,
) -> Result<SentFrame> {
    debug!(peer = peer_label, path, "inferência em processo");
    let hidden_state_bytes_sent = hidden_state_bytes(tensor.clone(), dtype)?;
    let encode_started = Instant::now();
    let body = encode_frame_body(meta, tensor, dtype, encoding, use_batch_frame)?;
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;
    let started = Instant::now();
    let bytes_sent = body.len() as u64;
    let response = local.dispatch(path, Bytes::from(body)).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), wire::MAX_FRAME_PAYLOAD_BYTES).await?;
    if !status.is_success() {
        return Err(anyhow::Error::new(HttpRefusal {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    let client_decode_started = Instant::now();
    let (meta, tensor) = wire::decode(&bytes)?;
    let hidden_state_bytes_received = received_hidden_state_bytes(
        &meta,
        tensor
            .as_ref()
            .map(|(values, shape)| (values.as_slice(), shape.clone())),
        dtype,
        peer_label,
    )?;
    let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
    let tls_handshake_ms = meta
        .get("tls_handshake_ms")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    Ok(SentFrame {
        meta,
        tensor,
        round_trip_ms: started.elapsed().as_millis() as u64,
        bytes_sent,
        bytes_received: bytes.len() as u64,
        hidden_state_bytes_sent,
        hidden_state_bytes_received,
        hidden_state_dtype: dtype,
        client_encode_ms,
        request_body_send_us: None,
        response_headers_ms: 0,
        response_body_ms: 0,
        client_decode_ms,
        connection_state: crate::peer_client::ConnectionState::Unknown,
        tls: false,
        tls_handshake_ms,
        transport_rtt_us: None,
        transport: InferenceTransport::Local,
        transport_fallback: false,
        transport_fallback_reason: None,
        profiling_spans: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub async fn send_frame(
    clients: &crate::peer_client::PeerClients,
    peer_id: &str,
    base_url: &str,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    attempts: u32,
    peer_label: &str,
    on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    send_frame_with_protocol(
        clients,
        peer_id,
        base_url,
        &[],
        &[],
        path,
        meta,
        tensor,
        dtype,
        encoding,
        false,
        attempts,
        peer_label,
        false,
        None,
        on_issue,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_frame_with_protocol(
    clients: &crate::peer_client::PeerClients,
    peer_id: &str,
    base_url: &str,
    candidates: &[String],
    quic_addresses: &[String],
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    use_batch_frame: bool,
    attempts: u32,
    peer_label: &str,
    mut transport_fallback: bool,
    mut transport_fallback_reason: Option<String>,
    mut on_issue: impl FnMut(SendIssue),
    session_id: Option<&str>,
) -> Result<SentFrame> {
    // O cliente sai do cache por peer: com o cluster em TLS ele carrega o
    // certificado esperado deste destino, e o pinning é o que distingue o peer
    // de quem se põe no meio dele.
    let (http, connection_state) = clients.for_peer_with_state(peer_id)?;
    let hidden_state_bytes_sent = hidden_state_bytes(tensor.clone(), dtype)?;
    let encode_started = Instant::now();
    let body = encode_frame_body(meta, tensor, dtype, encoding, use_batch_frame)?;
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;
    let attempts = attempts.max(1);
    let mut last_error = String::new();

    for attempt in 1..=attempts {
        let mut route_candidates = Vec::with_capacity(candidates.len() + 1);
        if !base_url.is_empty() {
            route_candidates.push(base_url.to_string());
        }
        let additional: Vec<String> = candidates
            .iter()
            .filter(|candidate| !route_candidates.contains(candidate))
            .cloned()
            .collect();
        route_candidates.extend(additional);
        let route_key = format!("inference:{peer_id}");
        let ordered = clients.route_order(&route_key, &route_candidates);
        if ordered.is_empty() {
            break;
        }

        for base in ordered {
            let url = format!("{}{path}", base.trim_end_matches('/'));
            let started = Instant::now();
            if clients.effective_quic_enabled() {
                match clients
                    .quic_post(
                        peer_id,
                        &base,
                        quic_addresses,
                        path,
                        body.clone(),
                        session_id,
                        DIRECT_FRAME_TIMEOUT,
                    )
                    .await
                {
                    Ok(response) if response.status.is_success() => {
                        let client_decode_started = Instant::now();
                        let (meta, tensor) = wire::decode(&response.body)?;
                        let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
                        let hidden_state_bytes_received = received_hidden_state_bytes(
                            &meta,
                            tensor
                                .as_ref()
                                .map(|(values, shape)| (values.as_slice(), shape.clone())),
                            dtype,
                            peer_label,
                        )?;
                        let received_at = Instant::now();
                        let profiling_spans = profile_transport_spans(
                            profile_context_for_path(&meta, peer_label, path, started).as_ref(),
                            "quic",
                            started,
                            started,
                            received_at,
                            received_at,
                        );
                        clients.remember_route_success(&route_key, &base);
                        return Ok(SentFrame {
                            meta,
                            tensor,
                            // HTTP/3 always carries TLS 1.3 inside QUIC, even
                            // when the separate TCP TLS preference is off.
                            tls: true,
                            tls_handshake_ms: 0,
                            round_trip_ms: started.elapsed().as_millis() as u64,
                            bytes_sent: body.len() as u64,
                            bytes_received: response.body.len() as u64,
                            hidden_state_bytes_sent,
                            hidden_state_bytes_received,
                            hidden_state_dtype: dtype,
                            client_encode_ms,
                            request_body_send_us: Some(response.request_body_send_us),
                            response_headers_ms: response.response_headers_ms,
                            response_body_ms: response.response_body_ms,
                            client_decode_ms,
                            connection_state: crate::peer_client::ConnectionState::Unknown,
                            transport_rtt_us: Some(response.transport_rtt_us),
                            transport: InferenceTransport::Quic,
                            transport_fallback,
                            transport_fallback_reason,
                            profiling_spans,
                        });
                    }
                    Ok(response) if matches!(response.status.as_u16(), 401 | 403 | 404 | 405) => {
                        // 404/405: peer sem a rota HTTP/3. 401/403: transporte
                        // sem credencial aceita para este peer (rede pública em
                        // texto claro, política ainda não propagada) - não é uma
                        // recusa considerada do pedido, então cai para o próximo
                        // transporte (TCP e, por fim, o túnel) em vez de morrer.
                        transport_fallback = true;
                        transport_fallback_reason = Some(match transport_fallback_reason.take() {
                            Some(previous) => format!("{previous}; quic: HTTP {}", response.status),
                            None => format!("quic: HTTP {}", response.status),
                        });
                        on_issue(SendIssue {
                            kind: SendIssueKind::Transport,
                            attempt,
                            message: format!("QUIC HTTP/3 indisponível: {}", response.status),
                        });
                        warn!(
                            "peer {peer_label} não aceitou a rota HTTP/3 ({}), usando TCP",
                            response.status
                        );
                    }
                    Ok(response) => {
                        return Err(anyhow::Error::new(HttpRefusal {
                            status: response.status.as_u16(),
                            body: String::from_utf8_lossy(&response.body).into_owned(),
                            peer_label: peer_label.to_string(),
                        }));
                    }
                    Err(error) => {
                        if !is_pre_delivery_error(&error) {
                            on_issue(SendIssue {
                                kind: SendIssueKind::Transport,
                                attempt,
                                message: format!("QUIC: {error}"),
                            });
                            return Err(error);
                        }
                        transport_fallback = true;
                        transport_fallback_reason = Some(match transport_fallback_reason.take() {
                            Some(previous) => format!("{previous}; quic: {error}"),
                            None => format!("quic: {error}"),
                        });
                        on_issue(SendIssue {
                            kind: SendIssueKind::Transport,
                            attempt,
                            message: format!("QUIC: {error}"),
                        });
                        warn!(
                            "chamada QUIC para {peer_label} em {base} falhou ({error}), usando TCP"
                        );
                    }
                }
            }
            let request = http
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body.clone())
                .timeout(DIRECT_FRAME_TIMEOUT);
            let sent =
                send_http_request_with_deadline(clients, request, session_id, DIRECT_FRAME_TIMEOUT)
                    .await;

            match sent {
                Ok(r) if r.status.is_success() => {
                    let response_headers_at = r.response_headers_at;
                    let response_headers_ms = response_headers_at
                        .saturating_duration_since(started)
                        .as_millis() as u64;
                    let bytes = r.body;
                    let response_body_ms = r.response_body_ms;
                    let response_received_at = Instant::now();
                    let transport = tcp_transport_for_url(&url);
                    let profiling_spans = profile_transport_spans(
                        profile_context_for_path(meta, peer_label, path, started).as_ref(),
                        transport.as_str(),
                        started,
                        started,
                        response_headers_at,
                        response_received_at,
                    );
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
                    let hidden_state_bytes_received = received_hidden_state_bytes(
                        &meta,
                        tensor
                            .as_ref()
                            .map(|(values, shape)| (values.as_slice(), shape.clone())),
                        dtype,
                        peer_label,
                    )?;
                    // O handshake é medido no servidor e volta no meta da resposta:
                    // é lá que o tempo é exato, porque é lá que o accept acontece.
                    let tls_handshake_ms = meta
                        .get("tls_handshake_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    clients.remember_route_success(&route_key, &base);
                    return Ok(SentFrame {
                        meta,
                        tensor,
                        tls: url.starts_with("https://"),
                        tls_handshake_ms,
                        round_trip_ms: started.elapsed().as_millis() as u64,
                        bytes_sent: body.len() as u64,
                        bytes_received: bytes.len() as u64,
                        hidden_state_bytes_sent,
                        hidden_state_bytes_received,
                        hidden_state_dtype: dtype,
                        client_encode_ms,
                        request_body_send_us: None,
                        response_headers_ms,
                        response_body_ms,
                        client_decode_ms,
                        connection_state,
                        transport_rtt_us: None,
                        transport,
                        transport_fallback,
                        transport_fallback_reason,
                        profiling_spans,
                    });
                }
                Ok(r) if matches!(r.status.as_u16(), 401 | 403) => {
                    // Rejeição de autenticação do transporte direto: numa rede
                    // pública sem TLS não há credencial que o peer aceite por
                    // HTTP em claro. Não é recusa do pedido - deixa a escada
                    // seguir para o túnel libp2p (Noise), que autentica.
                    let code = r.status;
                    let text = String::from_utf8_lossy(&r.body).into_owned();
                    last_error = format!("HTTP {code}: {text}");
                    transport_fallback = true;
                    transport_fallback_reason = Some(match transport_fallback_reason.take() {
                        Some(previous) => format!("{previous}; http: {last_error}"),
                        None => format!("http: {last_error}"),
                    });
                    clients.remember_route_failure(&route_key, &base);
                    on_issue(SendIssue {
                        kind: SendIssueKind::Transport,
                        attempt,
                        message: last_error.clone(),
                    });
                    warn!("peer {peer_label} recusou o HTTP direto ({code}), tentando o túnel");
                }
                Ok(r) => {
                    let code = r.status;
                    let text = String::from_utf8_lossy(&r.body).into_owned();
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
                    clients.remember_route_failure(&route_key, &base);
                    on_issue(SendIssue {
                        kind: if is_transport_timeout(&e) {
                            SendIssueKind::Timeout
                        } else {
                            SendIssueKind::Transport
                        },
                        attempt,
                        message: last_error.clone(),
                    });
                    warn!(
                        "chamada para {peer_label} em {base} falhou (tentativa {attempt}/{attempts}): {last_error}"
                    );
                    if !is_pre_delivery_error(&e) {
                        return Err(e);
                    }
                }
            }
        }
        if attempt < attempts {
            tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
        }
    }

    Err(anyhow::Error::new(FrameNotDelivered {
        message: format!(
            "{peer_label} não recebeu o frame após {attempts} tentativa(s): {last_error}"
        ),
    }))
}

#[derive(Debug)]
struct HttpTransportResponse {
    status: reqwest::StatusCode,
    body: Vec<u8>,
    response_headers_at: Instant,
    response_body_ms: u64,
}

#[derive(Debug)]
struct TransportDeadline {
    protocol: &'static str,
    timeout: Duration,
}

impl std::fmt::Display for TransportDeadline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "deadline de transporte {} expirou após {:?}",
            self.protocol, self.timeout
        )
    }
}

impl std::error::Error for TransportDeadline {}

fn is_transport_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TransportDeadline>().is_some()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
}

/// Limita a tentativa HTTP inteira, incluindo os headers e o corpo. O
/// timeout longo do frame continua sendo o limite interno do reqwest, mas o
/// transporte direto nunca deve deixar uma inferência presa quando o peer
/// aceitou a conexão e parou de responder.
async fn send_http_request_with_deadline(
    clients: &crate::peer_client::PeerClients,
    request: reqwest::RequestBuilder,
    session_id: Option<&str>,
    timeout: Duration,
) -> Result<HttpTransportResponse> {
    let transport_timeout = crate::quic::transport_deadline(timeout);
    let response = tokio::time::timeout(transport_timeout, async {
        let response = clients
            .authorize_inference_request_for_session(request, session_id)
            .await
            .send()
            .await?;
        let status = response.status();
        let response_headers_at = Instant::now();
        let body_started = Instant::now();
        let body = response.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>((status, body, response_headers_at, body_started.elapsed()))
    })
    .await;

    match response {
        Ok(Ok((status, body, response_headers_at, response_body_time))) => {
            Ok(HttpTransportResponse {
                status,
                body,
                response_headers_at,
                response_body_ms: response_body_time.as_millis() as u64,
            })
        }
        Ok(Err(error)) => Err(anyhow::Error::new(error)),
        Err(_) => Err(anyhow::Error::new(TransportDeadline {
            protocol: "HTTP",
            timeout: transport_timeout,
        })),
    }
}

struct BatchTransportResponse {
    body: Vec<u8>,
    round_trip_ms: u64,
    bytes_sent: u64,
    client_encode_ms: u64,
    request_body_send_us: Option<u64>,
    response_headers_ms: u64,
    response_body_ms: u64,
    client_decode_ms: u64,
    connection_state: crate::peer_client::ConnectionState,
    transport_rtt_us: Option<u64>,
    tls: bool,
    transport: InferenceTransport,
    transport_fallback: bool,
    transport_fallback_reason: Option<String>,
    profiling_spans: Vec<crate::prefill::TelemetrySpan>,
}

fn profile_transport_spans(
    context: Option<&crate::prefill::TelemetryContext>,
    transport: &str,
    roundtrip_started: Instant,
    frame_started: Instant,
    frame_finished: Instant,
    received: Instant,
) -> Vec<crate::prefill::TelemetrySpan> {
    let Some(context) = context else {
        return Vec::new();
    };
    let transport = Some(transport.to_owned());
    vec![
        context.span(
            "frame-send",
            frame_started,
            frame_finished,
            None,
            transport.clone(),
        ),
        context.span(
            "transport-round-trip",
            roundtrip_started,
            received,
            None,
            transport.clone(),
        ),
        context.span("frame-receive", received, received, None, transport),
    ]
}

fn profile_context_for_path(
    meta: &Value,
    peer_label: &str,
    path: &str,
    origin: Instant,
) -> Option<crate::prefill::TelemetryContext> {
    crate::prefill::TelemetryContext::from_meta(
        meta,
        peer_label.to_owned(),
        if path == "/v1/prefill" {
            crate::prefill::Phase::Prefill
        } else {
            crate::prefill::Phase::Decode
        },
        origin,
        None,
    )
}

/// Send one already encoded batch. The batch coordinator uses this function
/// instead of recursively calling `send_frame_or_tunnel`, otherwise a flushed
/// batch would immediately be queued as another batch forever.
#[allow(clippy::too_many_arguments)]
async fn send_batch_body(
    clients: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    local: Option<LocalInference>,
    path: &str,
    batch_id: &str,
    body: Bytes,
    attempts: u32,
    peer_label: &str,
    session_id: Option<&str>,
    profiling: Option<crate::prefill::TelemetryContext>,
) -> Result<BatchTransportResponse> {
    if let Some(local) = local {
        debug!(peer = peer_label, path, "inferência em processo em batch");
        let started = Instant::now();
        let bytes_sent = body.len() as u64;
        let response = local.dispatch(path, body).await?;
        let status = response.status();
        let bytes =
            axum::body::to_bytes(response.into_body(), wire::MAX_FRAME_PAYLOAD_BYTES).await?;
        if !status.is_success() {
            return Err(anyhow::Error::new(HttpRefusal {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).into_owned(),
                peer_label: peer_label.to_string(),
            }));
        }
        return Ok(BatchTransportResponse {
            body: bytes.to_vec(),
            round_trip_ms: started.elapsed().as_millis() as u64,
            bytes_sent,
            client_encode_ms: 0,
            request_body_send_us: None,
            response_headers_ms: 0,
            response_body_ms: 0,
            client_decode_ms: 0,
            connection_state: crate::peer_client::ConnectionState::Unknown,
            transport_rtt_us: None,
            tls: false,
            transport: InferenceTransport::Local,
            transport_fallback: false,
            transport_fallback_reason: None,
            profiling_spans: Vec::new(),
        });
    }

    let peer_id = step
        .libp2p_peer_id
        .as_deref()
        .and_then(|value| value.parse::<PeerId>().ok());
    // The batch path follows the same ladder as the single-frame path:
    // persistent HTTP link when selected, then QUIC/TCP direct, and only then
    // the libp2p tunnel. Keeping this order here is essential because
    // `/v1/step` commonly enters the coordinator with a hidden-state tensor.
    let can_use_tunnel = tunnel_is_allowed_for_peer(
        step.supports(crate::pipeline::CAP_LIBP2P_TUNNEL),
        peer_id.is_some(),
        tunnel.is_some(),
        crate::infer_tunnel::is_local_node(&step.node_id),
    );
    let mut transport_fallback_reason = None;
    let mut transport_fallback = false;

    // A ligação persistente vem antes porque é a única que não reconstrói a
    // abstração de chamada por ativação. Quando ela não serve - peer sem a
    // capability, endereço direto inalcançável, canal caído - a chamada segue
    // pela escada de sempre e o `transport_fallback` diz que isso aconteceu.
    if step.supports(crate::pipeline::CAP_LINK_STREAM)
        && !clients.effective_quic_enabled()
        && !crate::infer_tunnel::is_local_node(&step.node_id)
    {
        match crate::link::send_batch_with_session_profile(
            clients,
            &step.node_id,
            &step.inference_url,
            &step.inference_candidates,
            batch_id,
            body.clone(),
            peer_label,
            session_id,
            profiling.clone(),
        )
        .await
        {
            Ok(response) => {
                return Ok(BatchTransportResponse {
                    body: response.body,
                    round_trip_ms: response.round_trip_ms,
                    bytes_sent: response.bytes_sent,
                    client_encode_ms: 0,
                    request_body_send_us: None,
                    response_headers_ms: 0,
                    response_body_ms: 0,
                    client_decode_ms: 0,
                    connection_state: crate::peer_client::ConnectionState::Unknown,
                    transport_rtt_us: None,
                    tls: response.tls,
                    // Preserve the internal distinction for the link lab;
                    // `as_hop_timing_entry` exposes http/https to operators.
                    transport: InferenceTransport::HttpsLink,
                    transport_fallback: false,
                    transport_fallback_reason: None,
                    profiling_spans: response.profiling_spans,
                });
            }
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) if !is_pre_delivery_error(&error) => return Err(error),
            Err(error) => {
                transport_fallback = true;
                transport_fallback_reason = Some(format!("https-link: {error}"));
                warn!("{peer_label}: canal persistente falhou ({error}), usando o caminho por requisição")
            }
        }
    }

    let (client, connection_state) = clients.for_peer_with_state(&step.node_id)?;
    let mut route_candidates = Vec::with_capacity(step.inference_candidates.len() + 1);
    if !step.inference_url.is_empty() {
        route_candidates.push(step.inference_url.clone());
    }
    let additional: Vec<String> = step
        .inference_candidates
        .iter()
        .filter(|candidate| !route_candidates.contains(candidate))
        .cloned()
        .collect();
    route_candidates.extend(additional);
    let route_key = format!("inference:{}", step.node_id);
    let attempts = attempts.max(1);
    let client_encode_ms = 0;
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        last_error.clear();
        let ordered = clients.route_order(&route_key, &route_candidates);
        if ordered.is_empty() {
            last_error = "nenhuma rota HTTP de inferência disponível".to_string();
        }
        for base in ordered {
            let url = format!("{}{path}", base.trim_end_matches('/'));
            let started = Instant::now();
            if clients.effective_quic_enabled() {
                match clients
                    .quic_post(
                        &step.node_id,
                        &base,
                        &step.quic_addr_candidates,
                        path,
                        body.to_vec(),
                        session_id,
                        DIRECT_FRAME_TIMEOUT,
                    )
                    .await
                {
                    Ok(response) if response.status.is_success() => {
                        wire::validate_batch_bytes(&response.body)?;
                        let received_at = Instant::now();
                        let profiling_spans = profile_transport_spans(
                            profiling.as_ref(),
                            "quic",
                            started,
                            started,
                            received_at,
                            received_at,
                        );
                        clients.remember_route_success(&route_key, &base);
                        return Ok(BatchTransportResponse {
                            body: response.body,
                            round_trip_ms: started.elapsed().as_millis() as u64,
                            bytes_sent: body.len() as u64,
                            client_encode_ms,
                            request_body_send_us: Some(response.request_body_send_us),
                            response_headers_ms: response.response_headers_ms,
                            response_body_ms: response.response_body_ms,
                            client_decode_ms: 0,
                            connection_state: crate::peer_client::ConnectionState::Unknown,
                            transport_rtt_us: Some(response.transport_rtt_us),
                            tls: true,
                            transport: InferenceTransport::Quic,
                            transport_fallback,
                            transport_fallback_reason,
                            profiling_spans,
                        });
                    }
                    Ok(response) if matches!(response.status.as_u16(), 401 | 403 | 404 | 405) => {
                        // 404/405: peer sem HTTP/3. 401/403: transporte sem
                        // credencial aceita - cai para TCP e, por fim, o túnel.
                        transport_fallback = true;
                        transport_fallback_reason = Some(match transport_fallback_reason.take() {
                            Some(previous) => format!("{previous}; quic: HTTP {}", response.status),
                            None => format!("quic: HTTP {}", response.status),
                        });
                        warn!(
                            "peer {peer_label} não aceitou a rota HTTP/3 do batch ({}), usando TCP",
                            response.status
                        );
                    }
                    Ok(response) => {
                        return Err(anyhow::Error::new(HttpRefusal {
                            status: response.status.as_u16(),
                            body: String::from_utf8_lossy(&response.body).into_owned(),
                            peer_label: peer_label.to_string(),
                        }));
                    }
                    Err(error) => {
                        if !is_pre_delivery_error(&error) {
                            return Err(error);
                        }
                        transport_fallback = true;
                        transport_fallback_reason = Some(match transport_fallback_reason.take() {
                            Some(previous) => format!("{previous}; quic: {error}"),
                            None => format!("quic: {error}"),
                        });
                        warn!("{peer_label}: QUIC batch falhou ({error}), tentando TCP direto");
                    }
                }
            }
            let request = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body.clone())
                .timeout(DIRECT_FRAME_TIMEOUT);
            let response =
                send_http_request_with_deadline(clients, request, session_id, DIRECT_FRAME_TIMEOUT)
                    .await;
            match response {
                Ok(response) if response.status.is_success() => {
                    let response_headers_at = response.response_headers_at;
                    let response_headers_ms = response_headers_at
                        .saturating_duration_since(started)
                        .as_millis() as u64;
                    let bytes = response.body;
                    let response_body_ms = response.response_body_ms;
                    let response_received_at = Instant::now();
                    let transport = tcp_transport_for_url(&url);
                    let decode_started = Instant::now();
                    wire::validate_batch_bytes(&bytes)?;
                    let client_decode_ms = decode_started.elapsed().as_millis() as u64;
                    clients.remember_route_success(&route_key, &base);
                    return Ok(BatchTransportResponse {
                        body: bytes,
                        round_trip_ms: started.elapsed().as_millis() as u64,
                        bytes_sent: body.len() as u64,
                        client_encode_ms,
                        request_body_send_us: None,
                        response_headers_ms,
                        response_body_ms,
                        client_decode_ms,
                        connection_state,
                        transport_rtt_us: None,
                        tls: url.starts_with("https://"),
                        transport,
                        transport_fallback,
                        transport_fallback_reason,
                        profiling_spans: profile_transport_spans(
                            profiling.as_ref(),
                            transport.as_str(),
                            started,
                            started,
                            response_headers_at,
                            response_received_at,
                        ),
                    });
                }
                Ok(response) if matches!(response.status.as_u16(), 401 | 403) => {
                    let status = response.status;
                    let text = String::from_utf8_lossy(&response.body).into_owned();
                    last_error = format!("HTTP {status}: {text}");
                    transport_fallback = true;
                    transport_fallback_reason = Some(match transport_fallback_reason.take() {
                        Some(previous) => format!("{previous}; http: {last_error}"),
                        None => format!("http: {last_error}"),
                    });
                    clients.remember_route_failure(&route_key, &base);
                    warn!("peer {peer_label} recusou o HTTP direto do batch ({status}), tentando o túnel");
                }
                Ok(response) => {
                    let status = response.status;
                    let text = String::from_utf8_lossy(&response.body).into_owned();
                    return Err(anyhow::Error::new(HttpRefusal {
                        status: status.as_u16(),
                        body: text,
                        peer_label: peer_label.to_string(),
                    }));
                }
                Err(error) => {
                    last_error = error.to_string();
                    clients.remember_route_failure(&route_key, &base);
                    if !is_pre_delivery_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        let direct_error = anyhow::Error::new(FrameNotDelivered {
            message: format!("{peer_label} não recebeu o batch: {last_error}"),
        });

        // Cada tentativa percorre a escada completa antes do recuo: QUIC,
        // HTTP e túnel. Falhar em abrir o transporte direto não pode consumir
        // todas as tentativas antes de experimentar o caminho independente.
        if let (true, Some(tunnel), Some(peer_id)) = (can_use_tunnel, tunnel, peer_id) {
            let auth = clients.internal_node_token().await;
            match send_batch_via_tunnel(
                tunnel,
                peer_id,
                &step.libp2p_addr_candidates,
                path,
                body.to_vec(),
                peer_label,
                auth,
                session_id,
                profiling.as_ref(),
            )
            .await
            {
                Ok(mut response) => {
                    response.transport_fallback = true;
                    response.transport_fallback_reason = Some(match transport_fallback_reason {
                        Some(previous) => format!("{previous}; http: {direct_error}"),
                        None => format!("http: {direct_error}"),
                    });
                    return Ok(response);
                }
                Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
                Err(tunnel_error) if is_pre_delivery_error(&tunnel_error) => {
                    last_error = format!("{direct_error}; libp2p-tunnel: {tunnel_error}");
                }
                Err(tunnel_error) => {
                    return Err(tunnel_error.context(format!("{direct_error}")));
                }
            }
        } else {
            last_error = direct_error.to_string();
        }

        if attempt < attempts {
            tokio::time::sleep(retry_backoff(attempt)).await;
        }
    }

    Err(anyhow::Error::new(FrameNotDelivered {
        message: format!(
            "{peer_label} não recebeu o batch após {attempts} tentativa(s): {last_error}"
        ),
    }))
}

async fn send_batch_via_tunnel(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    addr_candidates: &[String],
    path: &str,
    body: Vec<u8>,
    peer_label: &str,
    auth_token: Option<String>,
    session_id: Option<&str>,
    profiling: Option<&crate::prefill::TelemetryContext>,
) -> Result<BatchTransportResponse> {
    let mut headers = Vec::new();
    if let Some(token) = auth_token {
        headers.push((
            crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
            "1".to_string(),
        ));
        headers.push((
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        ));
    }
    if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
        headers.push((
            crate::auth::INFERENCE_SESSION_HEADER.to_string(),
            session_id.to_string(),
        ));
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let send_started = Instant::now();
    tunnel
        .send(TunnelOutbound {
            peer_id,
            addr_candidates: addr_candidates.to_vec(),
            request: TunnelRequest::post(path, body.clone(), headers),
            reply: reply_tx,
        })
        .await
        .map_err(|_| {
            anyhow::Error::new(crate::infer_tunnel::TunnelDeliveryError::before_request(
                format!("{peer_label}: swarm de libp2p não está disponível (canal fechado)"),
            ))
        })?;
    let send_finished = Instant::now();
    // Include queueing the request into the swarm in the measured round-trip.
    // Starting after `send` would report only the reply wait and undercount the
    // same request-response operation used by `infer_tunnel::measure_rtt`.
    let started = send_started;
    let response = match tokio::time::timeout(TUNNEL_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(error))) => {
            let message = format!("{peer_label}: falha no túnel libp2p: {error}");
            return Err(anyhow::Error::new(error).context(message));
        }
        Ok(Err(_)) => {
            return Err(anyhow::Error::new(
                crate::infer_tunnel::TunnelDeliveryError::after_dispatch(format!(
                    "{peer_label}: túnel libp2p encerrou sem responder"
                )),
            ))
        }
        Err(_) => {
            return Err(anyhow::Error::new(
                crate::infer_tunnel::TunnelDeliveryError::after_dispatch(format!(
                    "{peer_label}: túnel libp2p não respondeu a tempo"
                )),
            ))
        }
    };
    if !(200..300).contains(&response.status) {
        return Err(anyhow::Error::new(HttpRefusal {
            status: response.status,
            body: String::from_utf8_lossy(&response.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    let received = Instant::now();
    wire::validate_batch_bytes(&response.body)?;
    let profiling_spans = profile_transport_spans(
        profiling,
        "libp2p-tunnel",
        send_started,
        send_started,
        send_finished,
        received,
    );
    Ok(BatchTransportResponse {
        body: response.body,
        round_trip_ms: started.elapsed().as_millis() as u64,
        bytes_sent: body.len() as u64,
        client_encode_ms: 0,
        request_body_send_us: None,
        response_headers_ms: started.elapsed().as_millis() as u64,
        response_body_ms: 0,
        client_decode_ms: 0,
        connection_state: crate::peer_client::ConnectionState::Unknown,
        transport_rtt_us: None,
        tls: false,
        transport: InferenceTransport::Libp2pTunnel,
        transport_fallback: false,
        transport_fallback_reason: None,
        profiling_spans,
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
    /// True when the direct HTTPS candidates failed and the tunnel completed
    /// the request.
    pub fallback: bool,
}

/// Envia bytes pelo HTTP(S) direto pinado e, se todos os endereços anunciados
/// falharem por transporte, tenta o túnel libp2p. O caminho direto vem
/// primeiro porque a coletiva TP espera a resposta HTTP dentro da barreira e
/// um túnel indisponível não deve impor seu timeout antes de usar um endereço
/// HTTPS comprovadamente alcançável. O túnel preserva caminho, corpo e
/// cabeçalhos, pois o reduce TP carrega a barreira e o rank fora do corpo.
#[cfg(test)]
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
    let direct_urls = [base_url.to_string()];
    send_bytes_or_tunnel_with_candidates(
        client,
        base_url,
        &direct_urls,
        path,
        body,
        headers,
        &[],
        tunnel,
        peer_id,
        supports_tunnel,
        peer_label,
    )
    .await
}

/// Variante do POST bruto que tenta todos os endereços HTTPS anunciados pelo
/// peer antes de recorrer ao túnel libp2p. O `base_url` continua sendo o
/// endereço histórico e o primeiro candidato, para manter o contrato dos
/// chamadores atuais.
#[allow(clippy::too_many_arguments)]
pub async fn send_bytes_or_tunnel_with_candidates(
    client: &reqwest::Client,
    base_url: &str,
    direct_urls: &[String],
    path: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    tunnel_addr_candidates: &[String],
    tunnel: Option<&TunnelOutboundSender>,
    peer_id: Option<PeerId>,
    supports_tunnel: bool,
    peer_label: &str,
) -> Result<RawPeerResponse> {
    let candidates: Vec<&str> = if direct_urls.is_empty() {
        vec![base_url]
    } else {
        direct_urls.iter().map(String::as_str).collect()
    };
    let mut last_error = None;
    for candidate in candidates {
        let url = format!("{}{}", candidate.trim_end_matches('/'), path);
        let mut request = client
            .post(&url)
            // A dead advertised address must not hold the TP barrier for the
            // full 10-minute request timeout. A real TP broadcast/reduce is
            // expected to finish well within this bound; another candidate is
            // tried when this one does not answer.
            .timeout(DIRECT_BYTES_TIMEOUT)
            .body(body.clone());
        for (name, value) in &headers {
            request = request.header(name, value);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                return Ok(RawPeerResponse {
                    body: response.bytes().await?.to_vec(),
                    transport: ByteTransport::Direct,
                    fallback: false,
                });
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow::Error::new(HttpRefusal {
                    status: status.as_u16(),
                    body,
                    peer_label: peer_label.to_string(),
                }));
            }
            Err(error) => {
                let detail = error.to_string();
                warn!("{peer_label}: HTTPS direto não alcançou {candidate}: {detail}");
                last_error = Some(detail);
            }
        }
    }

    let direct_error = anyhow!(
        "{peer_label}: nenhum endereço HTTPS respondeu: {}",
        last_error.unwrap_or_else(|| "sem candidatos anunciados".to_string())
    );

    if let (Some(tunnel), Some(peer_id)) = (tunnel.filter(|_| supports_tunnel), peer_id) {
        match send_bytes_via_tunnel(
            tunnel,
            peer_id,
            tunnel_addr_candidates,
            path,
            body,
            headers,
            peer_label,
        )
        .await
        {
            Ok(mut response) => {
                response.fallback = true;
                return Ok(response);
            }
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) => {
                warn!("{peer_label}: HTTPS direto e libp2p falharam ({error})");
                return Err(anyhow!("{direct_error}; túnel: {error}"));
            }
        }
    }

    Err(direct_error)
}

async fn send_bytes_via_tunnel(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    addr_candidates: &[String],
    path: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    peer_label: &str,
) -> Result<RawPeerResponse> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tunnel
        .send(TunnelOutbound {
            peer_id,
            addr_candidates: addr_candidates.to_vec(),
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

/// Seleciona o transporte de um frame na ordem QUIC, HTTP/HTTPS direto e,
/// somente como último recurso, túnel libp2p. O túnel exige a capability, um
/// `libp2p_peer_id` conhecido e um swarm local disponível. Ele usa Noise e a
/// mesma credencial da porta de inferência. Uma recusa HTTP é final e não é
/// repetida em outro transporte.
#[allow(clippy::too_many_arguments)]
pub async fn send_frame_or_tunnel(
    clients: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    attempts: u32,
    peer_label: &str,
    on_issue: impl FnMut(SendIssue),
) -> Result<SentFrame> {
    let local = if crate::infer_tunnel::is_local_node(&step.node_id) {
        Some(current_local_inference().await.ok_or_else(|| {
            anyhow!(
                "inferência em processo indisponível para o próprio nó {}",
                step.node_id
            )
        })?)
    } else {
        None
    };
    let can_batch = can_use_batch_transport(step, path, meta, tensor.clone());
    if can_batch {
        let coordinator = BatchCoordinator::global().clone();
        let mut on_issue = on_issue;
        return match coordinator
            .submit(
                clients,
                step,
                tunnel,
                local.clone(),
                path,
                meta,
                tensor,
                dtype,
                attempts,
                peer_label,
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

    if let Some(local) = local {
        return send_frame_in_process(
            local,
            path,
            meta,
            tensor.clone(),
            dtype,
            encoding,
            step.supports(crate::pipeline::CAP_FRAME_BATCH),
            peer_label,
        )
        .await;
    }
    let mut on_issue = on_issue;

    let peer_id = step
        .libp2p_peer_id
        .as_deref()
        .and_then(|s| s.parse::<PeerId>().ok());

    // Keep the transport selector single-valued: QUIC, when preferred, takes
    // precedence over direct TCP; HTTP/HTTPS remains the first fallback and
    // the libp2p tunnel is the last resort. A tunnel must never hide a direct
    // endpoint that is reachable, because that would turn a healthy
    // `/v1/step` or `/v1/prefill` hop into the slower NAT path.
    let can_use_tunnel = tunnel_is_allowed_for_peer(
        step.supports(crate::pipeline::CAP_LIBP2P_TUNNEL),
        peer_id.is_some(),
        tunnel.is_some(),
        crate::infer_tunnel::is_local_node(&step.node_id),
    );
    let use_batch_frame = step.supports(crate::pipeline::CAP_FRAME_BATCH);
    let session_id = meta.get("session_id").and_then(Value::as_str);
    let mut transport_fallback = false;
    let mut transport_fallback_reason: Option<String> = None;

    // Direct prefill carries `reply_to`, so it deliberately cannot enter the
    // process-local BatchCoordinator. It must still use the same persistent
    // channel as ordinary batch frames; otherwise this path silently falls
    // back to the libp2p tunnel while decode uses HTTP/HTTPS/QUIC.
    if step.supports(crate::pipeline::CAP_LINK_STREAM)
        && can_use_single_link(path)
        && !clients.effective_quic_enabled()
    {
        let batch_id = uuid::Uuid::new_v4().to_string();
        let encode_started = Instant::now();
        let body = encode_link_single(path, &batch_id, meta, tensor.clone(), dtype)?;
        let client_encode_ms = encode_started.elapsed().as_millis() as u64;
        let hidden_state_bytes_sent = hidden_state_bytes(tensor.clone(), dtype)?;
        let profiling = profile_context_for_path(meta, peer_label, path, encode_started);
        match crate::link::send_batch_with_session_profile(
            clients,
            &step.node_id,
            &step.inference_url,
            &step.inference_candidates,
            &batch_id,
            Bytes::from(body),
            peer_label,
            session_id,
            profiling,
        )
        .await
        {
            Ok(response) => {
                let client_decode_started = Instant::now();
                let (meta, tensor) = wire::decode_batch_single(&response.body)?;
                let hidden_state_bytes_received = received_hidden_state_bytes(
                    &meta,
                    tensor
                        .as_ref()
                        .map(|(values, shape)| (values.as_slice(), shape.clone())),
                    dtype,
                    peer_label,
                )?;
                let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
                let tls_handshake_ms = meta
                    .get("tls_handshake_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                return Ok(SentFrame {
                    meta,
                    tensor,
                    round_trip_ms: response.round_trip_ms,
                    bytes_sent: response.bytes_sent,
                    bytes_received: response.body.len() as u64,
                    hidden_state_bytes_sent,
                    hidden_state_bytes_received,
                    hidden_state_dtype: dtype,
                    client_encode_ms,
                    request_body_send_us: None,
                    response_headers_ms: 0,
                    response_body_ms: 0,
                    client_decode_ms,
                    connection_state: crate::peer_client::ConnectionState::Unknown,
                    tls: response.tls,
                    tls_handshake_ms,
                    transport_rtt_us: None,
                    transport: InferenceTransport::HttpsLink,
                    transport_fallback: false,
                    transport_fallback_reason: None,
                    profiling_spans: response.profiling_spans,
                });
            }
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) if is_pre_delivery_error(&error) => {
                transport_fallback = true;
                transport_fallback_reason = Some(format!("https-link: {error}"));
                warn!("{peer_label}: canal persistente falhou ({error}), tentando transporte de reserva");
            }
            Err(error) => return Err(error),
        }
    }

    let max_attempts = attempts.max(1);
    let mut last_error = String::new();
    for attempt in 1..=max_attempts {
        let direct = send_frame_with_protocol(
            clients,
            &step.node_id,
            &step.inference_url,
            &step.inference_candidates,
            &step.quic_addr_candidates,
            path,
            meta,
            tensor.clone(),
            dtype,
            encoding,
            use_batch_frame,
            1,
            peer_label,
            transport_fallback,
            transport_fallback_reason.clone(),
            |issue| on_issue(issue),
            session_id,
        )
        .await;

        let direct_error = match direct {
            Ok(sent) => return Ok(sent),
            Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
            Err(error) if is_pre_delivery_error(&error) => error,
            Err(error) => return Err(error),
        };
        let direct_stage = if clients.effective_quic_enabled() {
            format!("quic/http: {direct_error}")
        } else {
            format!("http: {direct_error}")
        };
        let direct_reason = match &transport_fallback_reason {
            Some(previous) => format!("{previous}; {direct_stage}"),
            None => direct_stage,
        };
        last_error = direct_reason.clone();

        if let (true, Some(tunnel), Some(peer_id)) = (can_use_tunnel, tunnel, peer_id) {
            let auth = clients.internal_node_token().await;
            match send_frame_via_tunnel_with_auth(
                tunnel,
                peer_id,
                &step.libp2p_addr_candidates,
                path,
                meta,
                tensor.clone(),
                dtype,
                encoding,
                use_batch_frame,
                peer_label,
                auth,
                session_id,
            )
            .await
            {
                Ok(mut sent) => {
                    sent.transport_fallback = true;
                    sent.transport_fallback_reason = Some(direct_reason);
                    return Ok(sent);
                }
                Err(error) if error.downcast_ref::<HttpRefusal>().is_some() => return Err(error),
                Err(error) if is_pre_delivery_error(&error) => {
                    last_error = format!("{direct_reason}; libp2p-tunnel: {error}");
                }
                Err(error) => {
                    return Err(anyhow!("{direct_reason}; libp2p-tunnel: {error}"));
                }
            }
        }

        if attempt < max_attempts {
            tokio::time::sleep(retry_backoff(attempt)).await;
        }
    }

    Err(anyhow::Error::new(FrameNotDelivered {
        message: format!("{peer_label} não recebeu o frame: {last_error}"),
    }))
}

/// A local pipeline step is dispatched directly to this process's inference
/// handler. Remote steps may use the tunnel before the direct HTTP fallback.
fn tunnel_is_allowed_for_peer(
    step_supports_tunnel: bool,
    peer_id_known: bool,
    tunnel_available: bool,
    target_is_local: bool,
) -> bool {
    !target_is_local && step_supports_tunnel && peer_id_known && tunnel_available
}

/// Predicado mantido para os testes e para consumidores que ainda classificam
/// uma falha antes de decidir o transporte. A seleção de produção já tenta
/// HTTP/HTTPS antes do túnel; esta função continua descrevendo quais falhas de
/// transporte podem justificar a última reserva.
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

const DIRECT_BYTES_TIMEOUT: Duration = Duration::from_secs(30);

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
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    peer_label: &str,
) -> Result<SentFrame> {
    send_frame_via_tunnel_with_auth(
        tunnel,
        peer_id,
        &[],
        path,
        meta,
        tensor,
        dtype,
        encoding,
        false,
        peer_label,
        None,
        None,
    )
    .await
}

async fn send_frame_via_tunnel_with_auth(
    tunnel: &TunnelOutboundSender,
    peer_id: PeerId,
    addr_candidates: &[String],
    path: &str,
    meta: &Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    _encoding: FrameEncoding,
    use_batch_frame: bool,
    peer_label: &str,
    auth_token: Option<String>,
    session_id: Option<&str>,
) -> Result<SentFrame> {
    let hidden_state_bytes_sent = hidden_state_bytes(tensor.clone(), dtype)?;
    let encode_started = Instant::now();
    let body = if use_batch_frame {
        wire::encode_batch_single(meta, tensor, dtype)?
    } else {
        wire::encode(meta, tensor, dtype)?
    };
    let client_encode_ms = encode_started.elapsed().as_millis() as u64;

    let (reply_tx, reply_rx) = oneshot::channel();
    let call_started = Instant::now();
    let mut headers = Vec::new();
    if let Some(token) = auth_token {
        headers.push((
            crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
            "1".to_string(),
        ));
        headers.push((
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        ));
    }
    if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
        headers.push((
            crate::auth::INFERENCE_SESSION_HEADER.to_string(),
            session_id.to_string(),
        ));
    }
    let outbound = TunnelOutbound {
        peer_id,
        addr_candidates: addr_candidates.to_vec(),
        request: TunnelRequest::post(path, body.clone(), headers),
        reply: reply_tx,
    };
    tunnel.send(outbound).await.map_err(|_| {
        anyhow::Error::new(crate::infer_tunnel::TunnelDeliveryError::before_request(
            format!("{peer_label}: swarm de libp2p não está disponível (canal fechado)"),
        ))
    })?;
    let frame_sent_at = Instant::now();

    // Keep the measurement boundary consistent with the direct HTTP path:
    // enqueueing the tunnel request and waiting for its response are one
    // application round-trip.
    let started = call_started;
    let response = match tokio::time::timeout(TUNNEL_REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(error))) => {
            let message = format!("{peer_label}: falha no túnel libp2p: {error}");
            return Err(anyhow::Error::new(error).context(message));
        }
        Ok(Err(_)) => {
            return Err(anyhow::Error::new(
                crate::infer_tunnel::TunnelDeliveryError::after_dispatch(format!(
                    "{peer_label}: túnel libp2p encerrou sem responder"
                )),
            ))
        }
        Err(_) => {
            return Err(anyhow::Error::new(
                crate::infer_tunnel::TunnelDeliveryError::after_dispatch(format!(
                    "{peer_label}: túnel libp2p não respondeu a tempo"
                )),
            ))
        }
    };
    let response_received_at = Instant::now();
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
    let hidden_state_bytes_received = received_hidden_state_bytes(
        &meta,
        tensor
            .as_ref()
            .map(|(values, shape)| (values.as_slice(), shape.clone())),
        dtype,
        peer_label,
    )?;
    let client_decode_ms = client_decode_started.elapsed().as_millis() as u64;
    let profiling_spans = profile_transport_spans(
        profile_context_for_path(&meta, peer_label, path, call_started).as_ref(),
        "libp2p-tunnel",
        call_started,
        call_started,
        frame_sent_at,
        response_received_at,
    );

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
        hidden_state_bytes_sent,
        hidden_state_bytes_received,
        hidden_state_dtype: dtype,
        client_encode_ms,
        request_body_send_us: None,
        response_headers_ms: round_trip_ms,
        response_body_ms,
        client_decode_ms,
        connection_state: crate::peer_client::ConnectionState::Unknown,
        transport_rtt_us: None,
        transport: InferenceTransport::Libp2pTunnel,
        transport_fallback: false,
        transport_fallback_reason: None,
        profiling_spans,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer_tunnel::TunnelResponse;

    #[test]
    fn outbound_hidden_responses_are_not_coalesced_without_a_size_signal() {
        assert!(!can_coalesce_frame(
            "/v1/prefill",
            &json!({"is_last": false}),
            None,
        ));
        assert!(can_coalesce_frame(
            "/v1/prefill",
            &json!({"is_last": true}),
            None,
        ));
        assert!(can_coalesce_frame(
            "/v1/prefill",
            &json!({"is_last": false, "next_hops": ["tail"]}),
            None,
        ));
    }

    #[test]
    fn mtp_handoff_can_use_persistent_link_batch_transport() {
        let mut step = step_with("https://peer.example", false, None);
        step.inference_frame_capabilities = vec![
            crate::pipeline::CAP_FRAME_BATCH.to_string(),
            crate::pipeline::CAP_LINK_STREAM.to_string(),
        ];

        assert!(can_use_batch_transport(
            &step,
            "/v1/step",
            &json!({
                "session_id": "session-mtp",
                "is_last": true,
                "tokens": [10, 11, 12],
                "draft": [11, 12],
                "mtp_k": 3
            }),
            None,
        ));
    }

    #[test]
    fn remote_stream_control_stays_out_of_persistent_link_batch_transport() {
        let mut step = step_with("https://peer.example", false, None);
        step.inference_frame_capabilities = vec![
            crate::pipeline::CAP_FRAME_BATCH.to_string(),
            crate::pipeline::CAP_LINK_STREAM.to_string(),
        ];

        assert!(!can_use_batch_transport(
            &step,
            "/v1/step-stream",
            &json!({
                "session_id": "session-mtp",
                "reply_to": "gateway-ticket",
                "speculate": true,
                "draft": [11, 12],
                "mtp_k": 3
            }),
            None,
        ));
    }

    #[test]
    fn tensor_batches_are_split_before_the_frame_payload_limit() {
        let item = wire::MAX_BATCH_PAYLOAD_BYTES / 2 + 1;
        let groups = partition_batch_payloads(&[item, item, 1]);

        assert_eq!(groups, vec![vec![0], vec![1, 2]]);
    }

    #[test]
    fn frame_size_refusals_are_not_retried_as_a_cold_prefix() {
        let error = anyhow::Error::new(HttpRefusal {
            status: 500,
            body: "payload do batch excede limite de 67108864 bytes".into(),
            peer_label: "peer-x".into(),
        });

        assert!(is_frame_size_error(&error));
    }

    #[test]
    fn a_response_with_another_hidden_state_dtype_is_refused() {
        let values = vec![1.0_f32; 2];
        let error = received_hidden_state_bytes(
            &json!({"dtype": "f16"}),
            Some((&values, vec![1, 2])),
            WireDtype::F8,
            "old-peer",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("old-peer"), "erro inesperado: {error}");
        assert!(error.contains("f16"), "erro inesperado: {error}");
        assert!(error.contains("f8"), "erro inesperado: {error}");
    }

    #[test]
    fn unknown_session_refusal_is_classified() {
        let unknown = anyhow::Error::new(HttpRefusal {
            status: 400,
            body: "sessão 's' desconhecida - envie reset=true antes".into(),
            peer_label: "peer-x".into(),
        });
        let other = anyhow::Error::new(HttpRefusal {
            status: 400,
            body: "chunk fora de ordem".into(),
            peer_label: "peer-x".into(),
        });

        assert!(is_unknown_session_error(&unknown));
        assert!(is_unknown_session_error(
            &unknown.context("telemetria parcial do prefill")
        ));
        assert!(!is_unknown_session_error(&other));
    }

    /// Regressão: um 401/403 ou falha de transporte no repasse head->tail
    /// (que `run_direct` trata como terminal no chunk 0) tem que ser
    /// reconhecível para o gateway conceder **um** prefill frio. Um
    /// 400/erro de modelo não pode casar - repetir não muda nada.
    #[test]
    fn transport_relay_failure_is_classified() {
        // O achatado que o gateway vê pela resposta direta (kind "http",
        // corpo do 401 do peer), build pré-fallback.
        let direct_http_401 = anyhow!(
            "pipeline falhou em 02eb8c40: http — peer recusou: 401 {{\"error\":\"missing Authorization header\"}}"
        );
        // O mesmo caminho já com a classificação de transporte nova.
        let direct_transport =
            anyhow!("pipeline falhou em 02eb8c40: transport — quic/http: caminho morto");
        // HttpRefusal direto de um hop 403.
        let refusal_403 = anyhow::Error::new(HttpRefusal {
            status: 403,
            body: "peer-y recusou: 403 forbidden".into(),
            peer_label: "peer-y".into(),
        });
        // Não é falha de transporte: pedido malformado / erro de modelo.
        let bad_request = anyhow::Error::new(HttpRefusal {
            status: 400,
            body: "chunk fora de ordem".into(),
            peer_label: "peer-x".into(),
        });
        let unknown_session = anyhow!("sessão 's' desconhecida");

        assert!(is_transport_relay_failure(&direct_http_401));
        assert!(is_transport_relay_failure(&direct_transport));
        assert!(is_transport_relay_failure(&refusal_403));
        assert!(!is_transport_relay_failure(&bad_request));
        assert!(!is_transport_relay_failure(&unknown_session));
    }

    #[tokio::test]
    async fn direct_bytes_fallback_tries_the_next_advertised_address() {
        use axum::{body::Bytes, routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/v1/tp/reduce", post(|body: Bytes| async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = send_bytes_or_tunnel_with_candidates(
            &reqwest::Client::new(),
            "http://127.0.0.1:1",
            &[
                "http://127.0.0.1:1".into(),
                format!("http://127.0.0.1:{port}"),
            ],
            "/v1/tp/reduce",
            b"partial".to_vec(),
            Vec::new(),
            &[],
            None,
            None,
            false,
            "tp-worker",
        )
        .await
        .expect("segundo endereço direto");

        assert_eq!(response.body, b"partial");
        assert_eq!(response.transport, ByteTransport::Direct);
        assert!(!response.fallback);
    }

    #[tokio::test]
    async fn prefill_retries_a_connection_refused_before_the_chunk_reaches_the_peer() {
        use axum::{body::Bytes, routing::post, Router};

        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);

        let server_address = address;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let listener = tokio::net::TcpListener::bind(server_address).await.unwrap();
            let response =
                Bytes::from(wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap());
            let app = Router::new().route(
                "/v1/prefill",
                post(move || {
                    let response = response.clone();
                    async move { response }
                }),
            );
            let _ = axum::serve(listener, app).await;
        });

        let step = step_with(&format!("http://{address}"), false, None);
        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            None,
            "/v1/prefill",
            &json!({"session_id": "resume-after-connect-refused"}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            CHUNK_ATTEMPTS,
            "peer-x",
            |_| {},
        )
        .await
        .expect("o mesmo pedaço deve ser reenviado após uma recusa de conexão");

        assert_eq!(sent.meta["done"], true);
    }

    #[tokio::test]
    async fn prefill_does_not_retry_after_the_peer_may_have_processed_the_chunk() {
        use axum::{body::Bytes, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let processed = Arc::new(AtomicUsize::new(0));
        let server_processed = processed.clone();
        let response =
            Bytes::from(wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap());
        let app = Router::new().route(
            "/v1/prefill",
            post(move || {
                let processed = server_processed.clone();
                let response = response.clone();
                async move {
                    processed.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(350)).await;
                    response
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let step = step_with(&format!("http://{address}"), false, None);
        let result = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            None,
            "/v1/prefill",
            &json!({"session_id": "ambiguous-delivery"}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            CHUNK_ATTEMPTS,
            "peer-x",
            |_| {},
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("uma resposta atrasada não prova que o frame ficou sem efeito"),
        };

        assert!(is_delivery_ambiguous(&error));
        assert_eq!(processed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn raw_peer_call_prefers_direct_before_the_tunnel() {
        use axum::{body::Bytes, routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/v1/tp/reduce",
            post(|_body: Bytes| async move { Bytes::from_static(b"reduced-directly") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let peer_id = PeerId::random();
        let (tunnel, mut received) = mpsc::channel::<TunnelOutbound>(1);
        let receiver = tokio::spawn(async move {
            if let Some(outbound) = received.recv().await {
                outbound
                    .reply
                    .send(Ok(TunnelResponse {
                        status: 200,
                        body: b"reduced-by-tunnel".to_vec(),
                    }))
                    .expect("reply");
            }
        });

        let response = send_bytes_or_tunnel_with_candidates(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}"),
            &[format!("http://127.0.0.1:{port}")],
            "/v1/tp/reduce",
            b"partial".to_vec(),
            Vec::new(),
            &[],
            Some(&tunnel),
            Some(peer_id),
            true,
            "tp-worker",
        )
        .await
        .expect("HTTPS direto");

        assert_eq!(response.body, b"reduced-directly");
        assert_eq!(response.transport, ByteTransport::Direct);
        assert!(!response.fallback);
        drop(tunnel);
        receiver.await.expect("receiver");
    }

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
        assert!(response.fallback);
        receiver.await.expect("receiver");
    }

    fn frame(tls: bool, handshake: u64) -> SentFrame {
        SentFrame {
            meta: json!({ "processing_ms": 7 }),
            tensor: None,
            round_trip_ms: 12,
            bytes_sent: 100,
            bytes_received: 200,
            hidden_state_bytes_sent: 0,
            hidden_state_bytes_received: 0,
            hidden_state_dtype: WireDtype::F16,
            client_encode_ms: 1,
            request_body_send_us: None,
            response_headers_ms: 5,
            response_body_ms: 3,
            client_decode_ms: 1,
            connection_state: crate::peer_client::ConnectionState::Unknown,
            tls,
            tls_handshake_ms: handshake,
            transport_rtt_us: None,
            transport: if tls {
                InferenceTransport::Https
            } else {
                InferenceTransport::Unknown
            },
            transport_fallback: false,
            transport_fallback_reason: None,
            profiling_spans: Vec::new(),
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

    #[test]
    fn hop_timing_expone_transporte_e_medidas_da_travessia() {
        let e = SentFrame {
            transport: InferenceTransport::Https,
            transport_fallback: true,
            ..frame(false, 0)
        }
        .as_hop_timing_entry();

        assert_eq!(e["transport"], "https");
        assert_eq!(e["transport_fallback"], true);
        assert_eq!(e["round_trip_ms"], 12);
        assert_eq!(e["bytes_sent"], 100);
        assert_eq!(e["bytes_received"], 200);
    }
    use tokio::sync::mpsc;

    static LOCAL_IN_PROCESS_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

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

    #[test]
    fn the_local_peer_is_never_sent_through_the_tunnel() {
        assert!(!tunnel_is_allowed_for_peer(true, true, true, true));
        assert!(tunnel_is_allowed_for_peer(true, true, true, false));
    }

    #[derive(Clone)]
    struct FakeInProcessInference {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl InProcessInference for FakeInProcessInference {
        async fn dispatch(
            &self,
            _path: &str,
            body: axum::body::Bytes,
        ) -> Result<axum::response::Response> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let response = if let Ok(batch) = wire::decode_batch(&body) {
                wire::encode_batch_items(
                    batch.kind,
                    batch.batch_id,
                    &batch
                        .items
                        .iter()
                        .map(|item| wire::BatchPayloadItem {
                            item_id: item.item_id.clone(),
                            session_id: item.session_id.clone(),
                            meta: json!({"done": true}),
                            tensor: None,
                            dtype: WireDtype::F16,
                        })
                        .collect::<Vec<_>>(),
                )?
            } else {
                wire::encode(&json!({"done": true}), None, WireDtype::F16)?
            };
            Ok(axum::response::Response::new(axum::body::Body::from(
                response,
            )))
        }
    }

    fn local_in_process_probe() -> (Arc<std::sync::atomic::AtomicUsize>, LocalInference) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle: LocalInference = Arc::new(FakeInProcessInference {
            calls: calls.clone(),
        });
        (calls, handle)
    }

    #[tokio::test]
    async fn a_local_step_is_dispatched_in_process_without_opening_a_socket() {
        let _guard = LOCAL_IN_PROCESS_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        crate::infer_tunnel::set_local_node_id("in-process-local".into());
        let (calls, local) = local_in_process_probe();
        set_test_local_inference(Some(local)).await;
        let step = step_with_node("in-process-local", "http://127.0.0.1:1", false, None);

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            None,
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "in-process-local",
            |_| {},
        )
        .await
        .expect("local inference must not use the invalid HTTP endpoint");

        assert_eq!(sent.meta["done"], true);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        set_test_local_inference(None).await;
    }

    #[tokio::test]
    async fn a_local_batch_is_dispatched_in_process_without_opening_a_socket() {
        let _guard = LOCAL_IN_PROCESS_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        crate::infer_tunnel::set_local_node_id("in-process-local".into());
        let (calls, local) = local_in_process_probe();
        set_test_local_inference(Some(local)).await;
        let mut step = step_with_node("in-process-local", "http://127.0.0.1:1", false, None);
        step.inference_frame_capabilities
            .push(crate::pipeline::CAP_FRAME_BATCH.into());

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            &step,
            None,
            "/v1/step",
            &json!({"session_id": "local-batch"}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "in-process-local",
            |_| {},
        )
        .await
        .expect("local batch must not use the invalid HTTP endpoint");

        assert_eq!(sent.meta["done"], true);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        set_test_local_inference(None).await;
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
        response: Result<
            crate::infer_tunnel::TunnelResponse,
            crate::infer_tunnel::TunnelDeliveryError,
        >,
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
        let tunnel = fake_swarm_answering_with(Err(
            crate::infer_tunnel::TunnelDeliveryError::before_request("dial failed"),
        ));

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
        step_with_node("peer-x", inference_url, supports_tunnel, peer_id)
    }

    fn step_with_node(
        node_id: &str,
        inference_url: &str,
        supports_tunnel: bool,
        peer_id: Option<String>,
    ) -> crate::pipeline::PipelineStep {
        crate::pipeline::PipelineStep {
            node_id: node_id.into(),
            inference_url: inference_url.into(),
            inference_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            exec_start: 0,
            exec_end: 1,
            inference_frame_capabilities: if supports_tunnel {
                vec![crate::pipeline::CAP_LIBP2P_TUNNEL.to_string()]
            } else {
                Vec::new()
            },
            libp2p_peer_id: peer_id,
            libp2p_addr_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
        }
    }

    /// A hop that advertises the tunnel uses it only after direct HTTP fails.
    /// The response is supplied by a fake swarm, so this test never waits for
    /// the 75-second tunnel timeout.
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

    #[tokio::test]
    async fn a_remote_peer_uses_http_before_tunnel_fallback() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let port = spawn_fake_inference_server(|_body| {
            (
                200,
                wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
            )
        })
        .await;
        let step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_swarm = tunnel_attempts.clone();
        let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<TunnelOutbound>(1);
        tokio::spawn(async move {
            if let Some(outbound) = tunnel_rx.recv().await {
                attempts_for_swarm.fetch_add(1, Ordering::Relaxed);
                let _ = outbound.reply.send(Err(
                    crate::infer_tunnel::TunnelDeliveryError::before_request("dial failed"),
                ));
            }
        });

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
        .expect("remote HTTP fallback must succeed after tunnel failure");

        assert_eq!(tunnel_attempts.load(Ordering::Relaxed), 0);
        assert_eq!(sent.meta["done"], true);
    }

    #[tokio::test]
    async fn prefill_uses_direct_http_before_the_tunnel() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let port = spawn_fake_inference_server(|_body| {
            (
                200,
                wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
            )
        })
        .await;
        let step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_swarm = tunnel_attempts.clone();
        let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<TunnelOutbound>(1);
        tokio::spawn(async move {
            if let Some(outbound) = tunnel_rx.recv().await {
                attempts_for_swarm.fetch_add(1, Ordering::Relaxed);
                let body = wire::encode(&json!({"done": false}), None, WireDtype::F16).unwrap();
                let _ = outbound.reply.send(Ok(crate::infer_tunnel::TunnelResponse {
                    status: 200,
                    body,
                }));
            }
        });

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/prefill",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("prefill deve usar o endpoint direto alcançável");

        assert_eq!(sent.transport, InferenceTransport::Http);
        assert_eq!(tunnel_attempts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn quic_falls_back_to_http_and_marks_the_real_transport() {
        let port = spawn_fake_inference_server(|_body| {
            (
                200,
                wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
            )
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
        let step = step_with(&format!("http://127.0.0.1:{port}"), false, None);

        let sent = send_frame_or_tunnel(
            &clients,
            &step,
            None,
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
        .expect("HTTP deve assumir quando o peer não fala QUIC");

        assert_eq!(sent.transport, InferenceTransport::Http);
        assert!(sent.transport_fallback);
        assert_eq!(sent.as_hop_timing_entry()["transport"], "http");
    }

    #[tokio::test]
    async fn quic_precedes_the_tunnel_and_falls_back_to_http_when_unavailable() {
        let port = spawn_fake_inference_server(|_body| {
            (
                200,
                wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
            )
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
        let step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_body = wire::encode(&json!({"done": false}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let sent = send_frame_or_tunnel(
            &clients,
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
        .expect("HTTP deve assumir quando QUIC não está disponível");

        assert_eq!(sent.meta["done"], true);
        assert_eq!(sent.transport, InferenceTransport::Http);
        assert!(sent.transport_fallback);
        assert!(sent
            .transport_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("quic")));
    }

    #[tokio::test]
    async fn uma_sessao_quic_morta_ainda_cai_para_tcp_direto() {
        let identity = crate::identity::Identity::generate();
        let http_body = wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap();
        let http_port = spawn_fake_inference_server({
            let http_body = http_body.clone();
            move |_body| (200, http_body.clone())
        })
        .await;

        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let quic_port = probe.local_addr().unwrap().port();
        drop(probe);
        let quic_body = http_body.clone();
        let router = axum::Router::new().route(
            "/v1/step",
            axum::routing::post(move |_body: Bytes| {
                let body = quic_body.clone();
                async move { (axum::http::StatusCode::OK, body) }
            }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = crate::quic::server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            crate::quic::serve(quic_port, router, config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
        let node_id = identity.node_id.0.clone();
        let mut step = step_with_node(
            &node_id,
            &format!("http://127.0.0.1:{http_port}"),
            false,
            None,
        );
        step.quic_addr_candidates = vec![format!("127.0.0.1:{quic_port}")];

        let first = send_frame_or_tunnel(
            &clients,
            &step,
            None,
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            &node_id,
            |_| {},
        )
        .await
        .expect("a primeira chamada deve aquecer a sessão QUIC");
        assert_eq!(first.transport, InferenceTransport::Quic);

        stop_tx.send(()).unwrap();
        server.await.unwrap();

        let second = send_frame_or_tunnel(
            &clients,
            &step,
            None,
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            &node_id,
            |_| {},
        )
        .await
        .expect("o HTTP direto deve sobreviver à queda da sessão QUIC");
        assert_eq!(second.transport, InferenceTransport::Http);
        assert!(second.transport_fallback);
        assert!(second
            .transport_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("quic")));
    }

    #[tokio::test]
    async fn quic_enabled_http_failure_uses_tunnel_only_as_last_fallback() {
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
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
            &clients,
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
        .expect("o túnel deve ser a última reserva quando HTTP e QUIC falham");

        assert_eq!(sent.transport, InferenceTransport::Libp2pTunnel);
        assert!(sent.transport_fallback);
        assert!(sent
            .transport_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("quic/http") && reason.contains("http")));
    }

    /// Um 401/403 do HTTP direto não é recusa considerada do pedido: numa rede
    /// pública em texto claro o peer não tem credencial para aceitar o canal
    /// HTTP. A escada tem que cair para o túnel libp2p (Noise), não devolver
    /// erro terminal - era isso que pendurava o prefill.
    #[tokio::test]
    async fn a_direct_http_401_falls_back_to_the_tunnel() {
        let port =
            spawn_fake_inference_server(|_body| (401, b"missing Authorization header".to_vec()))
                .await;
        let step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        let tunnel_body = wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/prefill",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("um 401 do HTTP direto deve cair para o túnel, não falhar");

        assert_eq!(sent.transport, InferenceTransport::Libp2pTunnel);
        assert!(sent.transport_fallback);
        assert!(sent
            .transport_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("401")));
    }

    /// Um endereço QUIC anunciado que não responde (porta UDP fechada, caminho
    /// morto) não pode segurar o hop: o handshake tem orçamento de RTT e o
    /// chamador cai para o próximo transporte em bem menos de 1 s.
    #[tokio::test]
    async fn a_dead_quic_address_fails_over_fast() {
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
        let mut step = step_with(
            "http://127.0.0.1:1",
            true,
            Some(PeerId::random().to_string()),
        );
        // 127.0.0.1:9 (discard) não tem endpoint QUIC: o handshake nunca fecha.
        step.quic_addr_candidates = vec!["127.0.0.1:9".to_string()];
        let tunnel_body = wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let started = Instant::now();
        let sent = send_frame_or_tunnel(
            &clients,
            &step,
            Some(&tunnel_tx),
            "/v1/prefill",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("QUIC morto deve cair para o túnel, não pendurar");

        assert_eq!(sent.transport, InferenceTransport::Libp2pTunnel);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "fallback demorou {:?}, deveria ser sub-segundo por transporte",
            started.elapsed()
        );
    }

    /// O fallback TCP também pode aceitar a conexão e nunca devolver headers.
    /// O orçamento do frame é deliberadamente longo para representar a
    /// configuração de produção; a tentativa de transporte deve aplicar o
    /// deadline curto antes dele.
    #[tokio::test]
    async fn fallback_http_sem_resposta_respeita_deadline_de_transporte() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                std::future::pending::<()>().await;
            }
        });

        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let request = clients
            .plain()
            .post(format!("http://{address}/v1/step"))
            .body(Vec::new())
            .timeout(Duration::from_secs(600));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            send_http_request_with_deadline(&clients, request, None, Duration::from_secs(600)),
        )
        .await
        .expect("fallback HTTP sem resposta não pode exceder o deadline");
        let error = result.expect_err("endpoint HTTP sem headers deve falhar");
        assert!(
            error
                .to_string()
                .contains("deadline de transporte HTTP expirou"),
            "erro inesperado: {error:#}"
        );
    }

    #[tokio::test]
    async fn quic_precedes_the_tunnel_when_the_endpoint_is_available() {
        use axum::{body::Bytes, http::StatusCode, routing::post, Router};
        use tokio::sync::oneshot;

        let identity = crate::identity::Identity::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/v1/step",
            post(|_body: Bytes| async {
                (
                    StatusCode::OK,
                    wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
                )
            }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let quic = crate::serve::quic_server_config(&identity).unwrap();
        let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let quic_port = udp_probe.local_addr().unwrap().port();
        drop(udp_probe);
        let server = tokio::spawn(async move {
            crate::serve::serve_with_quic(
                listener,
                app,
                None,
                Some(quic),
                Some(quic_port),
                async move {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap();
        });

        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        clients.set_quic(true);
        let mut step = step_with_node(
            &identity.node_id.0,
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        step.quic_addr_candidates = vec![format!("127.0.0.1:{quic_port}")];
        let tunnel_body = wire::encode(&json!({"done": false}), None, WireDtype::F16).unwrap();
        let tunnel_tx = fake_swarm_answering_with(Ok(crate::infer_tunnel::TunnelResponse {
            status: 200,
            body: tunnel_body,
        }));

        let sent = send_frame_or_tunnel(
            &clients,
            &step,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({}),
            None,
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "quic-peer",
            |_| {},
        )
        .await
        .expect("QUIC deve vencer o túnel quando o endpoint existe");

        assert_eq!(sent.meta["done"], true);
        assert_eq!(sent.transport, InferenceTransport::Quic);
        assert!(!sent.transport_fallback);
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_inference_address_is_tried_once_before_using_the_healthy_route() {
        let unreachable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unreachable_addr = unreachable.local_addr().unwrap();
        drop(unreachable);
        let healthy_port = spawn_fake_inference_server(|_body| {
            (
                200,
                wire::encode(&json!({"done": true}), None, WireDtype::F16).unwrap(),
            )
        })
        .await;
        let unreachable_url = format!("http://{unreachable_addr}");
        let healthy_url = format!("http://127.0.0.1:{healthy_port}");
        let mut step = step_with(&unreachable_url, false, None);
        step.inference_candidates = vec![unreachable_url, healthy_url];
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let started = std::time::Instant::now();

        for _ in 0..2 {
            let sent = send_frame_or_tunnel(
                &clients,
                &step,
                None,
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
            .expect("o endereço saudável deve assumir a sessão");
            assert_eq!(sent.meta["done"], true);
        }

        assert!(
            started.elapsed() < DIRECT_FRAME_TIMEOUT * 2,
            "o endereço morto voltou a ser pago por quadro: {:?}",
            started.elapsed()
        );
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
        let step_handler = handler.clone();
        let prefill_handler = handler;
        let router = Router::new()
            .route(
                "/v1/step",
                post(move |body: Bytes| {
                    let handler = step_handler.clone();
                    async move {
                        let (status, body) = handler(body.to_vec());
                        (axum::http::StatusCode::from_u16(status).unwrap(), body)
                    }
                }),
            )
            .route(
                "/v1/prefill",
                post(move |body: Bytes| {
                    let handler = prefill_handler.clone();
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
            Some((&[0.25_f32, -0.5], vec![1, 2])),
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
    async fn batched_step_uses_direct_http_before_the_tunnel() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let port = spawn_fake_inference_server(|body| {
            let batch = wire::decode_batch(&body).expect("batch válido");
            let item = &batch.items[0];
            (
                200,
                wire::encode_batch_items(
                    batch.kind,
                    batch.batch_id,
                    &[wire::BatchPayloadItem {
                        item_id: item.item_id.clone(),
                        session_id: item.session_id.clone(),
                        meta: json!({"done": true}),
                        tensor: None,
                        dtype: WireDtype::F16,
                    }],
                )
                .unwrap(),
            )
        })
        .await;
        let mut step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        step.inference_frame_capabilities
            .push(crate::pipeline::CAP_FRAME_BATCH.into());
        let tunnel_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_swarm = tunnel_attempts.clone();
        let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<TunnelOutbound>(1);
        tokio::spawn(async move {
            if let Some(outbound) = tunnel_rx.recv().await {
                attempts_for_swarm.fetch_add(1, Ordering::Relaxed);
                let batch = wire::decode_batch(&outbound.request.body).unwrap();
                let item = &batch.items[0];
                let body = wire::encode_batch_items(
                    batch.kind,
                    batch.batch_id,
                    &[wire::BatchPayloadItem {
                        item_id: item.item_id.clone(),
                        session_id: item.session_id.clone(),
                        meta: json!({"done": false}),
                        tensor: None,
                        dtype: WireDtype::F16,
                    }],
                )
                .unwrap();
                let _ = outbound.reply.send(Ok(crate::infer_tunnel::TunnelResponse {
                    status: 200,
                    body,
                }));
            }
        });

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({"session_id": "batch-session", "is_last": true}),
            Some((&[0.25_f32, -0.5], vec![1, 2])),
            WireDtype::F16,
            FrameEncoding::F16,
            1,
            "peer-x",
            |_| {},
        )
        .await
        .expect("step em batch deve usar o endpoint direto alcançável");

        assert_eq!(sent.transport, InferenceTransport::Http);
        assert_eq!(tunnel_attempts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn batched_step_falls_back_to_the_tunnel_before_retrying_the_http_path() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut step = step_with(
            &format!("http://127.0.0.1:{port}"),
            true,
            Some(PeerId::random().to_string()),
        );
        step.inference_frame_capabilities
            .push(crate::pipeline::CAP_FRAME_BATCH.into());

        let tunnel_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_swarm = tunnel_attempts.clone();
        let (tunnel_tx, mut tunnel_rx) = mpsc::channel::<TunnelOutbound>(2);
        tokio::spawn(async move {
            while let Some(outbound) = tunnel_rx.recv().await {
                let attempt = attempts_for_swarm.fetch_add(1, Ordering::Relaxed);
                if attempt == 0 {
                    let _ = outbound.reply.send(Err(
                        crate::infer_tunnel::TunnelDeliveryError::before_request(
                            "primeiro dial falhou",
                        ),
                    ));
                    continue;
                }
                let batch = wire::decode_batch(&outbound.request.body).unwrap();
                let item = &batch.items[0];
                let body = wire::encode_batch_items(
                    batch.kind,
                    batch.batch_id,
                    &[wire::BatchPayloadItem {
                        item_id: item.item_id.clone(),
                        session_id: item.session_id.clone(),
                        meta: json!({"done": true}),
                        tensor: None,
                        dtype: WireDtype::F16,
                    }],
                )
                .unwrap();
                let _ = outbound.reply.send(Ok(crate::infer_tunnel::TunnelResponse {
                    status: 200,
                    body,
                }));
                break;
            }
        });

        let sent = send_frame_or_tunnel(
            &crate::peer_client::PeerClients::new(Duration::from_secs(600)),
            &step,
            Some(&tunnel_tx),
            "/v1/step",
            &json!({"session_id": "batch-fallback", "is_last": true}),
            Some((&[0.25_f32, -0.5], vec![1, 2])),
            WireDtype::F16,
            FrameEncoding::F16,
            2,
            "peer-x",
            |_| {},
        )
        .await
        .expect("após um dial falhar, a repetição deve percorrer a escada de transportes");

        assert_eq!(sent.meta["done"], true);
        assert_eq!(sent.transport, InferenceTransport::Libp2pTunnel);
        assert_eq!(tunnel_attempts.load(Ordering::Relaxed), 2);
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

    /// A refusal generated by direct HTTP is final. The tunnel must not repeat
    /// the same frame after the peer explicitly rejected it.
    #[tokio::test]
    async fn a_direct_http_refusal_never_falls_back_to_the_tunnel() {
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
        assert!(err.to_string().contains("model crashed"));
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
    ///    uma recusa considerada, que nunca deve desviar. O prazo de 600 s do
    ///    `send_frame` continua reservado para processamento longo, mas o
    ///    cliente tem teto próprio de 2 s para abrir TCP. Assim um endereço que
    ///    engole SYN não decide a duração da chamada antes de o desvio começar.
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
