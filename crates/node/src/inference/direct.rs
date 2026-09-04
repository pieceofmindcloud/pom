//! Resposta direta: a cauda entrega o resultado a quem pediu.
//!
//! O modo anel (`relay/v1-chain`) encadeia chamadas HTTP aninhadas — cada hop
//! segura a própria resposta aberta enquanto chama o hop seguinte, e o
//! resultado volta refazendo o caminho da ida. São `2N` travessias de rede por
//! token para uma cadeia de `N` hops, e a conta piora justamente quando a
//! cabeça não está na máquina que recebeu a requisição: aí a ida e a volta
//! atravessam o enlace duas vezes cada. Medido em 2026-08-14 entre os dois
//! sites do captain (RTT 30 ms): 25 tok/s com a cabeça local contra 14 tok/s
//! com ela remota, exatamente um RTT a mais por token.
//!
//! Aqui a cadeia deixa de voltar. O gateway manda junto do quadro um
//! [`DirectReply::reply_to`] — os endereços por onde ele mesmo pode ser
//! alcançado, mais um *ticket* aleatório — e cada hop repassa esse campo
//! adiante sem tocar nele. Quem termina (a cauda, ou o hop que falhou ao
//! repassar) faz **um** POST em `/v1/relay-result` direto ao gateway. São
//! `N + 1` travessias, e o custo deixa de depender de onde está a cabeça.
//!
//! O que cada hop devolve ao seu chamador vira um *ack* imediato: o hop já
//! aceitou a responsabilidade pelo quadro, e nada mais viaja por ali. O ack
//! sai do caminho crítico — quem espera é o gateway, no ticket.
//!
//! ## Falha, cancelamento e prazo
//!
//! * **Hop do meio morre antes de aceitar** — quem o chamou recebe erro de
//!   transporte e entrega um [`crate::relay::RelayFailure`] direto ao gateway,
//!   nomeando o nó que realmente morreu.
//! * **Cauda morre** — idem, denunciada pelo hop anterior.
//! * **Hop morre depois de aceitar** (processo caiu, máquina sumiu) — não há
//!   quem denuncie; quem fecha a requisição é o prazo de [`DIRECT_REPLY_DEADLINE`].
//! * **Resposta direta perdida** (gateway inalcançável a partir da cauda) —
//!   mesmo prazo. A cauda tenta cada URL de `reply_to` antes de desistir.
//! * **Resposta que chega depois da desistência** — o [`Ticket`] já saiu do
//!   inbox (é removido no `Drop`, em qualquer caminho de saída), e o resultado
//!   atrasado leva `410 Gone`. Como o ticket é único por quadro, ele nunca
//!   pode ser confundido com o do token seguinte.
//!
//! A capacidade é negociada como todas as outras: só entra quando **todos** os
//! passos anunciam `relay/v2-direct` (ver `PipelinePlan::supports_direct_reply`)
//! e o gateway sabe pelo menos um endereço de si mesmo. Um cluster em
//! atualização progressiva cai no anel de sempre, sem nada a configurar.

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Rota em que o gateway recebe o resultado que a cauda entrega.
pub const RELAY_RESULT_PATH: &str = "/v1/relay-result";
/// Callback used by the remote decode transport for each opportunistic batch.
pub const REMOTE_STREAM_RESULT_PATH: &str = "/v1/remote-stream-result";

/// Quanto o gateway espera pelo resultado direto antes de desistir do quadro.
///
/// Mesmo teto que `relay::send_frame` já usa por chamada: um hop que estoura
/// isso não está lento, está morto, e a sessão precisa ser liberada.
pub const DIRECT_REPLY_DEADLINE: Duration = Duration::from_secs(600);

/// Tempo máximo para a telemetria das travessias chegar depois do resultado.
/// É deliberadamente curto e independente do prazo da inferência: faltar uma
/// amostra não pode prender um pedido por dez minutos.
const DIRECT_TRACE_GRACE: Duration = Duration::from_secs(2);

/// Tentativas de entrega do resultado direto, por URL candidata.
#[cfg(not(test))]
const DELIVERY_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(test)]
const DELIVERY_TIMEOUT: Duration = Duration::from_millis(50);

// ── Inbox ─────────────────────────────────────────────────────────────────────

/// Os quadros que este nó despachou e ainda espera resultado.
///
/// Um mapa de ticket → canal. Vive no gateway (`GatewayState`) porque é o
/// gateway que abre a requisição; o handler de `/v1/relay-result` só procura o
/// ticket aqui.
#[derive(Clone, Default)]
pub struct ReplyInbox(Arc<Mutex<InboxState>>);

#[derive(Default)]
struct InboxState {
    replies: HashMap<String, ReplyState>,
    streams: HashMap<String, StreamState>,
}

struct ReplyState {
    sender: oneshot::Sender<Value>,
    expected_transport: usize,
    transport: Vec<Option<Value>>,
    pending_result: Option<Value>,
    transport_trace_expired: bool,
}

/// Bounded on purpose: a slow gateway must apply backpressure to the remote
/// emitter and, through it, to the inference producer.
const REMOTE_STREAM_QUEUE_CAPACITY: usize = 16;

struct StreamState {
    sender: mpsc::Sender<Value>,
}

impl ReplyInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Abre um ticket. O guarda devolvido remove-o do inbox quando cai, em
    /// qualquer caminho de saída — é o que impede um ticket órfão de segurar
    /// memória depois de um erro ou de um prazo estourado.
    pub fn register(&self) -> Ticket {
        self.register_with_expected_transport(0)
    }

    /// Abre um ticket que também espera uma evidência por travessia.
    ///
    /// A resposta direta chega por um POST separado dos acks. O resultado é
    /// retido por uma janela curta para que a telemetria não seja publicada
    /// antes da última travessia, mas o prazo é finito: uma falha de medição
    /// aparece como `transport_trace_complete=false`, nunca como um pedido
    /// pendurado indefinidamente.
    pub fn register_with_expected_transport(&self, expected_transport: usize) -> Ticket {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().replies.insert(
            id.clone(),
            ReplyState {
                sender: tx,
                expected_transport,
                transport: vec![None; expected_transport],
                pending_result: None,
                transport_trace_expired: false,
            },
        );
        Ticket {
            id,
            inbox: self.clone(),
            rx,
        }
    }

    /// Opens a ticket that can receive several remote decode batches.
    pub fn register_stream(&self) -> StreamTicket {
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = mpsc::channel(REMOTE_STREAM_QUEUE_CAPACITY);
        self.0
            .lock()
            .unwrap()
            .streams
            .insert(id.clone(), StreamState { sender });
        StreamTicket {
            id,
            inbox: self.clone(),
            receiver,
        }
    }

    /// Entrega um resultado. `false` quando o ticket não existe mais — o
    /// gateway já desistiu, e quem entregou precisa saber que chegou tarde.
    pub fn deliver(&self, ticket: &str, meta: Value) -> bool {
        let mut state = self.0.lock().unwrap();
        let Some(entry) = state.replies.get_mut(ticket) else {
            return false;
        };
        if entry.expected_transport > 0
            && !entry.transport_trace_expired
            && entry.transport.iter().any(Option::is_none)
        {
            let schedule_expiry = entry.pending_result.is_none();
            entry.pending_result = Some(meta);
            drop(state);
            if schedule_expiry {
                let inbox = self.clone();
                let expiry_id = ticket.to_owned();
                tokio::spawn(async move {
                    tokio::time::sleep(DIRECT_TRACE_GRACE).await;
                    inbox.expire_transport_trace(&expiry_id);
                });
            }
            return true;
        }
        let entry = state
            .replies
            .remove(ticket)
            .expect("ticket ainda está no inbox");
        let meta = complete_transport_trace(&entry, meta);
        meta_sender_send(meta, entry.sender)
    }

    /// Registra a resposta de uma travessia sem transformá-la em resultado.
    /// Retorna `false` quando o ticket já expirou ou foi removido.
    pub fn record_transport(&self, ticket: &str, hop_index: usize, meta: Value) -> bool {
        let mut state = self.0.lock().unwrap();
        let Some(entry) = state.replies.get_mut(ticket) else {
            return false;
        };
        if hop_index >= entry.expected_transport {
            return false;
        }
        entry.transport[hop_index] = Some(meta);
        let ready = entry.pending_result.is_some()
            && (entry.transport_trace_expired || entry.transport.iter().all(Option::is_some));
        if !ready {
            return true;
        }
        let entry = state
            .replies
            .remove(ticket)
            .expect("ticket ainda está no inbox");
        let meta = entry
            .pending_result
            .clone()
            .map(|meta| complete_transport_trace(&entry, meta));
        match meta {
            Some(meta) => meta_sender_send(meta, entry.sender),
            None => false,
        }
    }

    fn expire_transport_trace(&self, ticket: &str) {
        let mut state = self.0.lock().unwrap();
        let Some(entry) = state.replies.get_mut(ticket) else {
            return;
        };
        entry.transport_trace_expired = true;
        if entry.pending_result.is_none() {
            return;
        }
        let entry = state
            .replies
            .remove(ticket)
            .expect("ticket ainda está no inbox");
        if let Some(meta) = entry
            .pending_result
            .clone()
            .map(|meta| complete_transport_trace(&entry, meta))
        {
            let _ = meta_sender_send(meta, entry.sender);
        }
    }

    fn cancel(&self, ticket: &str) {
        let mut state = self.0.lock().unwrap();
        state.replies.remove(ticket);
        state.streams.remove(ticket);
    }

    /// Quantos quadros estão em voo. Só para teste e diagnóstico.
    pub fn outstanding(&self) -> usize {
        self.0.lock().unwrap().replies.len()
    }

    #[cfg(test)]
    fn stream_outstanding(&self) -> usize {
        self.0.lock().unwrap().streams.len()
    }

    /// Delivers a batch without holding the inbox lock across the await. A
    /// terminal batch removes the stream after its value has entered the
    /// consumer queue.
    pub async fn deliver_stream(&self, ticket: &str, meta: Value) -> bool {
        let terminal = meta.get("done").and_then(Value::as_bool).unwrap_or(false)
            || meta.get("error").and_then(Value::as_str).is_some();
        let sender = {
            let state = self.0.lock().unwrap();
            state
                .streams
                .get(ticket)
                .map(|stream| stream.sender.clone())
        };
        let Some(sender) = sender else {
            return false;
        };
        if sender.send(meta).await.is_err() {
            self.0.lock().unwrap().streams.remove(ticket);
            return false;
        }
        if terminal {
            self.0.lock().unwrap().streams.remove(ticket);
        }
        true
    }
}

fn complete_transport_trace(entry: &ReplyState, mut meta: Value) -> Value {
    if entry.expected_transport == 0 {
        return meta;
    }
    let traversals: Vec<Value> = entry.transport.iter().flatten().cloned().collect();
    meta["transport_traversals"] = json!(traversals);
    meta["transport_trace_complete"] = json!(
        entry.expected_transport == meta["transport_traversals"].as_array().map_or(0, Vec::len)
    );
    meta
}

fn meta_sender_send(meta: Value, sender: oneshot::Sender<Value>) -> bool {
    sender.send(meta).is_ok()
}

/// Um quadro em voo. Removido do inbox no `Drop`.
pub struct Ticket {
    id: String,
    inbox: ReplyInbox,
    rx: oneshot::Receiver<Value>,
}

impl Ticket {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.inbox.cancel(&self.id);
    }
}

/// Consumer side of a bounded remote decode stream.
pub struct StreamTicket {
    id: String,
    inbox: ReplyInbox,
    receiver: mpsc::Receiver<Value>,
}

impl StreamTicket {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }
}

impl Drop for StreamTicket {
    fn drop(&mut self) {
        self.inbox.cancel(&self.id);
    }
}

// ── O que o gateway manda junto do quadro ─────────────────────────────────────

/// Endereços por onde a cauda alcança este gateway, mais o inbox onde o
/// resultado será procurado.
#[derive(Clone)]
pub struct DirectReply {
    inbox: ReplyInbox,
    urls: Vec<String>,
    /// node_id de quem vai receber a resposta.
    ///
    /// Viaja no `reply_to` porque quem entrega precisa dele para fixar o
    /// certificado do gateway: com o cluster em TLS, uma URL sem identidade não
    /// dá para verificar. Peer em binário antigo não manda o campo, e aí a
    /// entrega cai no cliente sem pinning - ver `deliver_result`.
    node_id: String,
}

impl DirectReply {
    /// `None` quando este nó não sabe nenhum endereço de si mesmo: sem isso a
    /// cauda não teria para onde responder, e o certo é cair no anel de sempre
    /// em vez de despachar um quadro que nunca volta.
    pub fn new(inbox: ReplyInbox, urls: Vec<String>, node_id: String) -> Option<Self> {
        if urls.is_empty() {
            return None;
        }
        Some(Self {
            inbox,
            urls,
            node_id,
        })
    }

    pub fn ticket_for_hops(&self, expected_hops: usize) -> Ticket {
        self.inbox.register_with_expected_transport(expected_hops)
    }

    pub fn reply_to(&self, ticket: &str) -> Value {
        json!({ "urls": self.urls, "ticket": ticket, "node_id": self.node_id })
    }

    pub fn stream_ticket(&self) -> StreamTicket {
        self.inbox.register_stream()
    }

    pub fn stream_reply_to(&self, ticket: &str) -> Value {
        self.reply_to(ticket)
    }

    pub fn record_transport(
        &self,
        ticket: &str,
        hop_index: usize,
        sent: &crate::relay::SentFrame,
    ) -> bool {
        self.inbox
            .record_transport(ticket, hop_index, transport_receipt(hop_index, sent))
    }
}

fn transport_receipt(hop_index: usize, sent: &crate::relay::SentFrame) -> Value {
    json!({
        "hop_index": hop_index,
        "transport": sent.transport.as_str(),
        "transport_fallback": sent.transport_fallback,
        "round_trip_ms": sent.round_trip_ms,
        "bytes_sent": sent.bytes_sent,
        "bytes_received": sent.bytes_received,
        "tls": sent.tls,
        "tls_handshake_ms": sent.tls_handshake_ms,
        "client_encode_ms": sent.client_encode_ms,
        "response_headers_ms": sent.response_headers_ms,
        "response_body_ms": sent.response_body_ms,
        "client_decode_ms": sent.client_decode_ms,
    })
}

/// URLs de gateway deste nó, na ordem em que um peer deve tentá-las.
///
/// Mesma regra de preferência de `gateway::self_dial_host`: o endereço fixado
/// pelo operador vale mais que o da LAN, porque entre sites um `192.168.x.y` é
/// tão inútil quanto o loopback. Os candidatos enumerados das interfaces vêm
/// depois, e daí o loopback não entra: a cauda quase nunca é esta máquina.
///
/// `pinned` é o endereço que o operador fixou (`advertise_addr`), e só ele pode
/// ser loopback: numa máquina só, apontar para si mesmo é a configuração certa
/// e foi escolha de quem configurou. O que não pode entrar é o loopback que
/// aparece sozinho, do fallback `127.0.0.1` de `gateway::self_dial_host` ou da
/// enumeração de interfaces: aí a cauda em outra máquina entrega ao loopback
/// *dela* e a resposta se perde, com prazo de 600 s. Sem nenhuma URL utilizável
/// a lista fica vazia e o plano cai no anel, que funciona.
pub fn self_reply_urls(
    pinned: Option<String>,
    hosts: impl IntoIterator<Item = String>,
    gateway_port: u16,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    let fixado = pinned.into_iter().map(|h| (h, true));
    for (host, fixado_pelo_operador) in fixado.chain(hosts.into_iter().map(|h| (h, false))) {
        let host = crate::network::ip_host(&host).to_string();
        if host.is_empty() || (!fixado_pelo_operador && is_unroutable_host(&host)) {
            continue;
        }
        // O esquema segue a política do cluster: estas URLs são as que a cauda
        // vai discar de volta, e num cluster em TLS o `/relay/result` deste nó
        // recusa texto claro vindo de fora.
        let url =
            crate::peer_client::peer_base_url(&host, gateway_port, crate::peer_client::tls_ativo());
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// Endereço que não serve para outra máquina nos alcançar: o curinga de bind e
/// o loopback, em v4 e v6. O loopback é o que mais engana, porque
/// `gateway::self_dial_host` cai em `127.0.0.1` quando não acha endereço
/// melhor: a lista fica não-vazia, o modo direto liga, e a cauda em outra
/// máquina entrega o resultado ao loopback *dela*, onde ninguém espera.
fn is_unroutable_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        // Nome que não é IP (um `--public-addr` com domínio, por exemplo)
        // passa: resolvê-lo aqui seria adivinhar.
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

// ── Lado de quem recebe o resultado (gateway) ─────────────────────────────────

/// Handler de `/v1/relay-result`: procura o ticket e resolve o canal.
///
/// A autenticação é o próprio ticket, um UUID v4 sorteado por quadro e
/// conhecido apenas pela cadeia. Um resultado sem ticket vivo é recusado com
/// `410`, que é o que diz a quem entregou que o gateway já desistiu.
pub fn handle_relay_result(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (mut meta, _tensor) = match crate::wire::decode(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("quadro inválido: {e}")).into_response()
        }
    };
    let Some(ticket) = meta
        .get("ticket")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "resultado sem ticket").into_response();
    };
    if meta
        .get("transport_update")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let Some(hop_index) = meta.get("hop_index").and_then(Value::as_u64) else {
            return (StatusCode::BAD_REQUEST, "telemetria sem índice de hop").into_response();
        };
        if inbox.record_transport(&ticket, hop_index as usize, meta) {
            return (StatusCode::OK, "ok").into_response();
        }
        return (StatusCode::GONE, "ticket desconhecido").into_response();
    }
    // Os bytes que atravessaram a rede na volta. Só quem recebeu sabe.
    meta["result_bytes"] = json!(body.len());
    if inbox.deliver(&ticket, meta) {
        (StatusCode::OK, "ok").into_response()
    } else {
        debug!("resultado direto para ticket desconhecido (o gateway já desistiu)");
        (StatusCode::GONE, "ticket desconhecido").into_response()
    }
}

/// Receives one batch from a remote decode producer. Delivery awaits the
/// bounded inbox queue, so a slow client propagates backpressure all the way
/// to the remote emitter.
pub async fn handle_remote_stream_result(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (mut meta, _tensor) = match crate::wire::decode(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("quadro inválido: {e}")).into_response()
        }
    };
    let Some(ticket) = meta
        .get("ticket")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "lote sem ticket").into_response();
    };
    meta["stream_batch_bytes"] = json!(body.len());
    if inbox.deliver_stream(&ticket, meta).await {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::GONE, "stream desconhecido").into_response()
    }
}

/// Espera o resultado e executa uma ação síncrona quando o ack da cabeça
/// chega. O ack é observado mesmo se o POST final vencer a corrida, para que
/// a travessia cabeça→primeiro hop não desapareça da telemetria.
pub async fn await_result_with_ack<F, C>(
    ack: F,
    ticket: &mut Ticket,
    head_label: &str,
    mut on_ack: C,
) -> Result<Value>
where
    F: Future<Output = Result<crate::relay::SentFrame>>,
    C: FnMut(&crate::relay::SentFrame),
{
    tokio::pin!(ack);
    let deadline = tokio::time::sleep(DIRECT_REPLY_DEADLINE);
    tokio::pin!(deadline);
    let mut acked = false;

    loop {
        tokio::select! {
            biased;
            got = &mut ticket.rx => {
                let result = got.map_err(|_| anyhow!("resultado direto perdido antes de chegar"))?;
                if !acked {
                    if let Ok(Ok(frame)) = tokio::time::timeout(DIRECT_TRACE_GRACE, &mut ack).await {
                        on_ack(&frame);
                    }
                }
                return Ok(result);
            }
            sent = &mut ack, if !acked => {
                acked = true;
                match sent {
                    // Peer antigo: em vez de aceitar e repassar, borbulhou a
                    // resposta inteira. Vale como resultado — é o que mantém
                    // um cluster meio atualizado funcionando mesmo se a
                    // negociação de capacidade escapar.
                    Ok(frame) if frame.meta.get("token_id").is_some() => {
                        on_ack(&frame);
                        return Ok(frame.meta)
                    }
                    Ok(frame) => on_ack(&frame),
                    Err(e) => return Err(e),
                }
            }
            _ = &mut deadline => {
                return Err(anyhow!(
                    "nenhum hop entregou o resultado em {}s (cadeia despachada em {head_label})",
                    DIRECT_REPLY_DEADLINE.as_secs()
                ));
            }
        }
    }
}

/// O erro que um hop entregou no lugar do resultado, se foi isso que chegou.
pub fn result_failure(meta: &Value) -> Option<crate::relay::RelayFailure> {
    serde_json::from_value(meta.get("error")?.clone()).ok()
}

// ── Lado de quem entrega (cauda, ou hop que falhou) ───────────────────────────

/// Entrega um resultado ao gateway, tentando cada URL de `reply_to` em ordem.
///
/// Uma máquina pode ser alcançável por vários endereços e nenhum deles é
/// certo de fora: o gateway anuncia todos os que conhece de si e quem entrega
/// descobre qual funciona. Falhou em todos, o gateway fecha no prazo — não há
/// terceira via, e inventar uma seria pior que a espera.
pub async fn deliver_result(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    meta: Value,
) -> bool {
    deliver_meta_at(http, reply_to, RELAY_RESULT_PATH, meta).await
}

/// Delivers one remote decode batch to the stream inbox. The HTTP response is
/// intentionally awaited: the gateway's bounded queue is the backpressure
/// signal for the emitter.
pub async fn deliver_stream_batch(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    meta: Value,
) -> bool {
    deliver_meta_at(http, reply_to, REMOTE_STREAM_RESULT_PATH, meta).await
}

/// Entrega uma evidência de transporte pelo mesmo retorno autenticado do
/// resultado. O POST é pequeno e fica fora dos bytes da travessia medida.
pub async fn deliver_transport(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    hop_index: usize,
    sent: &crate::relay::SentFrame,
) -> bool {
    let mut meta = transport_receipt(hop_index, sent);
    meta["transport_update"] = json!(true);
    deliver_meta_at(http, reply_to, RELAY_RESULT_PATH, meta).await
}

async fn deliver_meta_at(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    path: &str,
    mut meta: Value,
) -> bool {
    let Some(ticket) = reply_to.get("ticket").and_then(|v| v.as_str()) else {
        warn!("reply_to sem ticket — resultado direto descartado");
        return false;
    };
    meta["ticket"] = json!(ticket);

    let body = match crate::wire::encode(&meta, None, crate::wire::WireDtype::F16) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("resultado direto não pôde ser codificado: {e}");
            return false;
        }
    };

    let urls = reply_to
        .get("urls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let urls: Vec<String> = urls
        .iter()
        .filter_map(|url| url.as_str().map(str::to_owned))
        .collect();
    let route_key = reply_to
        .get("node_id")
        .and_then(|value| value.as_str())
        .map(|node_id| format!("reply:{node_id}"))
        .unwrap_or_else(|| format!("reply:urls:{}", urls.join("\u{1f}")));
    // Sem node_id no `reply_to` o remetente roda binário antigo: não há o que
    // fixar, e a entrega vai pelo cliente compartilhado. Num cluster estrito
    // ela será recusada do outro lado, que é o resultado correto - o que não
    // pode acontecer é entregar em texto claro fingindo que foi verificado.
    let cliente = match reply_to.get("node_id").and_then(|v| v.as_str()) {
        Some(id) => match http.for_peer(id) {
            Ok(c) => c,
            Err(e) => {
                warn!("sem cliente para entregar o resultado a {id}: {e}");
                return false;
            }
        },
        None => http.plain(),
    };
    let mut last = String::new();
    for url in http.route_order(&route_key, &urls) {
        let target = format!("{url}{path}");
        match cliente
            .post(&target)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(DELIVERY_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                http.remember_route_success(&route_key, &url);
                return true;
            }
            // `410` é resposta, não falha de rota: o gateway desistiu deste
            // quadro. Tentar o próximo endereço só acharia o mesmo inbox.
            Ok(r) if r.status() == StatusCode::GONE => {
                debug!("gateway já havia desistido do quadro entregue em {target}");
                return false;
            }
            Ok(r) => last = format!("{target} respondeu {}", r.status()),
            Err(e) => {
                http.remember_route_failure(&route_key, &url);
                last = format!("{target}: {e}");
            }
        }
    }
    warn!("resultado direto não chegou ao gateway ({last})");
    false
}

/// Entrega uma falha no lugar do resultado, para que o gateway nomeie o nó que
/// morreu em vez de esperar o prazo inteiro.
pub async fn deliver_failure(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    failure: crate::relay::RelayFailure,
) {
    let meta = json!({ "error": failure });
    deliver_result(http, reply_to, meta).await;
}

// ── Telemetria acumulada na ida ───────────────────────────────────────────────

/// A contribuição de um hop para o `hop_timings` que viaja **na ida**.
///
/// No anel borbulhado cada entrada era montada por quem chamou, na volta. Sem
/// volta, cada hop descreve a si mesmo e empurra a lista adiante; a cauda
/// entrega o conjunto junto do resultado, na mesma ordem cabeça→cauda que
/// `RequestTrace::record_hop_from_json` já espera.
///
/// `bytes_sent` é o quadro que **entrou** neste hop, que é exatamente o que o
/// anterior pôs no fio. A chamada que originou a entrada envia uma evidência
/// posterior com o relógio e o transporte observados no cliente.
pub fn hop_entry(processing_ms: &Value, node_timings: &Value, inbound_bytes: usize) -> Value {
    json!({
        "round_trip_ms": processing_ms.clone(),
        "processing_ms": processing_ms.clone(),
        "bytes_sent": inbound_bytes,
        "bytes_received": 0,
        "client_encode_ms": 0,
        "response_headers_ms": 0,
        "response_body_ms": 0,
        "client_decode_ms": 0,
        "node_timings": node_timings.clone(),
    })
}

/// Variant used by inference hops after the selected wire dtype is known.
/// `inbound_bytes` remains the complete frame size for transport telemetry;
/// the hidden-state counters isolate the tensor payload on either side of the
/// crossing. Direct mode records both sides because no caller waits for a
/// response frame from each hop.
pub fn hop_entry_with_state(
    processing_ms: &Value,
    node_timings: &Value,
    inbound_bytes: usize,
    hidden_state_bytes_received: usize,
    hidden_state_bytes_sent: usize,
    dtype: crate::wire::WireDtype,
) -> Value {
    let mut entry = hop_entry(processing_ms, node_timings, inbound_bytes);
    entry["hidden_state_bytes_sent"] = json!(hidden_state_bytes_sent);
    entry["hidden_state_bytes_received"] = json!(hidden_state_bytes_received);
    entry["hidden_state_dtype"] = json!(dtype.as_str());
    entry
}

/// Acrescenta a entrada deste hop à lista que veio na ida.
pub fn accumulate(inbound_meta: &Value, entry: Value) -> Vec<Value> {
    let mut hops: Vec<Value> = inbound_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    hops.push(entry);
    hops
}

/// Substitui a estimativa de um hop pela medição da chamada que o originou.
/// O índice é a posição do destino na cadeia, por isso nenhum tempo de uma
/// travessia pode ser lançado no primeiro hop por conveniência.
// Mantido para a prova unitária do mapeamento de índice; o caminho direto em
// produção registra a medição no inbox com `record_transport`.
#[cfg(test)]
pub fn attach_transport(hops: &mut [Value], destination: usize, sent: &crate::relay::SentFrame) {
    let Some(hop) = hops.get_mut(destination) else {
        return;
    };
    hop["round_trip_ms"] = json!(sent.round_trip_ms);
    hop["bytes_sent"] = json!(sent.bytes_sent);
    hop["bytes_received"] = json!(sent.bytes_received);
    hop["client_encode_ms"] = json!(sent.client_encode_ms);
    hop["response_headers_ms"] = json!(sent.response_headers_ms);
    hop["response_body_ms"] = json!(sent.response_body_ms);
    hop["client_decode_ms"] = json!(sent.client_decode_ms);
    hop["tls"] = json!(sent.tls);
    hop["tls_handshake_ms"] = json!(sent.tls_handshake_ms);
    hop["transport"] = json!(sent.transport.as_str());
    hop["transport_fallback"] = json!(sent.transport_fallback);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_blackholed_reply_address_is_tried_only_once_across_stream_batches() {
        use axum::{body::Bytes, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        let blackhole_hits = Arc::new(AtomicUsize::new(0));
        let blackhole_hits_task = blackhole_hits.clone();
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = blackhole.accept().await else {
                    break;
                };
                blackhole_hits_task.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                });
            }
        });

        let healthy_hits = Arc::new(AtomicUsize::new(0));
        let healthy_hits_task = healthy_hits.clone();
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_addr = healthy_listener.local_addr().unwrap();
        let healthy = Router::new().route(
            REMOTE_STREAM_RESULT_PATH,
            post(move |_body: Bytes| {
                let healthy_hits = healthy_hits_task.clone();
                async move {
                    healthy_hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "ok")
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(healthy_listener, healthy).await;
        });

        let reply_to = json!({
            "ticket": "blackhole-ticket",
            "node_id": "gateway-blackhole-test",
            "urls": [
                format!("http://{blackhole_addr}"),
                format!("http://{healthy_addr}"),
            ],
        });
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let started = Instant::now();

        assert!(deliver_stream_batch(&http, &reply_to, json!({"index": 1})).await);
        assert!(deliver_stream_batch(&http, &reply_to, json!({"index": 2})).await);

        assert_eq!(blackhole_hits.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a rota morta voltou a ser paga: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_delivered_result_reaches_the_ticket_that_asked_for_it() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register();
        assert_eq!(inbox.outstanding(), 1);

        assert!(inbox.deliver(&ticket.id().to_string(), json!({"token_id": 7})));
        assert_eq!(inbox.outstanding(), 0, "entregar consome o ticket");

        let got = (&mut ticket.rx).await.expect("resultado");
        assert_eq!(got["token_id"], 7);
    }

    #[tokio::test]
    async fn a_result_waits_for_each_transport_receipt_and_marks_the_hop() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_with_expected_transport(2);
        let id = ticket.id().to_string();

        assert!(inbox.deliver(&id, json!({"token_id": 7})));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut ticket.rx)
                .await
                .is_err()
        );
        assert!(inbox.record_transport(
            &id,
            1,
            json!({"hop_index": 1, "transport": "https", "round_trip_ms": 23})
        ));
        assert!(inbox.record_transport(
            &id,
            0,
            json!({"hop_index": 0, "transport": "libp2p-tunnel", "round_trip_ms": 11})
        ));

        let got = (&mut ticket.rx).await.expect("resultado");
        assert_eq!(got["transport_trace_complete"], true);
        assert_eq!(got["transport_traversals"][0]["hop_index"], 0);
        assert_eq!(got["transport_traversals"][1]["hop_index"], 1);
    }

    #[tokio::test]
    async fn a_remote_stream_is_bounded_and_applies_backpressure() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let id = ticket.id().to_string();
        for index in 0..REMOTE_STREAM_QUEUE_CAPACITY {
            assert!(inbox.deliver_stream(&id, json!({"index": index})).await);
        }
        assert_eq!(inbox.stream_outstanding(), 1);

        let mut pending = Box::pin(inbox.deliver_stream(&id, json!({"index": 99})));
        tokio::select! {
            result = &mut pending => panic!("a fila cheia não pode aceitar outro lote: {result}"),
            got = ticket.recv() => assert_eq!(got.expect("primeiro lote")["index"], 0),
        }
        assert!(pending.await);
    }

    #[tokio::test]
    async fn a_terminal_remote_stream_batch_removes_the_ticket_after_delivery() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let id = ticket.id().to_string();

        assert!(inbox.deliver_stream(&id, json!({"done": true})).await);
        assert_eq!(inbox.stream_outstanding(), 0);
        assert_eq!(ticket.recv().await.expect("lote terminal")["done"], true);
    }

    #[tokio::test]
    async fn a_cancelled_remote_stream_refuses_late_batches() {
        let inbox = ReplyInbox::new();
        let id = {
            let ticket = inbox.register_stream();
            ticket.id().to_string()
        };

        assert!(!inbox.deliver_stream(&id, json!({"accepted": []})).await);
        assert_eq!(inbox.stream_outstanding(), 0);
    }

    #[tokio::test]
    async fn the_remote_stream_callback_delivers_the_wire_batch_to_its_ticket() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let body = crate::wire::encode(
            &json!({
                "ticket": ticket.id(),
                "accepted": [{"token_id": 4, "text": "ok"}],
                "done": true,
            }),
            None,
            crate::wire::WireDtype::F16,
        )
        .expect("batch codificável");

        let response = handle_remote_stream_result(&inbox, Bytes::from(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let received = ticket.recv().await.expect("batch recebido");
        assert_eq!(received["accepted"][0]["token_id"], 4);
        assert_eq!(
            received["stream_batch_bytes"].as_u64().unwrap_or(0) > 0,
            true
        );
    }

    /// O guarda tem que limpar o inbox sozinho: o caminho de erro e o de prazo
    /// estourado saem por `return`, e um ticket esquecido segura memória por
    /// requisição perdida.
    #[test]
    fn a_dropped_ticket_leaves_no_entry_behind() {
        let inbox = ReplyInbox::new();
        {
            let _ticket = inbox.register();
            assert_eq!(inbox.outstanding(), 1);
        }
        assert_eq!(inbox.outstanding(), 0);
    }

    /// Um hop que responde depois de o gateway ter desistido não pode ser
    /// confundido com o quadro seguinte, nem derrubar nada.
    #[test]
    fn a_late_result_is_refused_instead_of_landing_on_another_frame() {
        let inbox = ReplyInbox::new();
        let atrasado = {
            let ticket = inbox.register();
            ticket.id().to_string()
        };
        let _seguinte = inbox.register();

        assert!(!inbox.deliver(&atrasado, json!({"token_id": 1})));
        assert_eq!(inbox.outstanding(), 1, "o ticket seguinte fica intacto");
    }

    #[test]
    fn a_reply_url_never_points_at_the_wildcard_address() {
        let urls = self_reply_urls(
            None,
            vec![
                "0.0.0.0:42777".into(),
                "192.168.1.9:42777".into(),
                "[2804:f30::1]:42777".into(),
                "192.168.1.9:42777".into(),
            ],
            8080,
        );
        assert_eq!(
            urls,
            vec![
                "http://192.168.1.9:8080".to_string(),
                "http://[2804:f30::1]:8080".to_string(),
            ]
        );
    }

    /// Loopback é endereço da máquina de quem responde, não desta. Uma lista
    /// que só tem loopback tem que ficar vazia para `DirectReply::new` recusar
    /// o modo direto e o plano cair no anel, que funciona: ligado, o quadro
    /// sairia para nunca voltar e a requisição só terminaria no prazo de 600 s.
    #[test]
    fn a_loopback_only_host_list_falls_back_to_the_ring() {
        let urls = self_reply_urls(
            None,
            vec![
                "127.0.0.1:42777".into(),
                "127.0.0.53:42777".into(),
                "[::1]:42777".into(),
                "localhost:42777".into(),
            ],
            8080,
        );
        assert!(
            urls.is_empty(),
            "loopback não alcança outra máquina: {urls:?}"
        );
        assert!(
            DirectReply::new(ReplyInbox::new(), urls, "n1".into()).is_none(),
            "sem endereço utilizável o plano tem que cair no anel"
        );

        // O loopback que o operador fixou é outra história: numa máquina só,
        // apontar para si mesmo é a configuração certa e foi escolha dele.
        assert_eq!(
            self_reply_urls(
                Some("127.0.0.1:42777".into()),
                vec!["127.0.0.1:42777".into()],
                8080
            ),
            vec!["http://127.0.0.1:8080".to_string()]
        );
    }

    #[test]
    fn a_direct_transport_receipt_updates_only_its_destination_hop() {
        let mut hops = vec![
            hop_entry(&json!(10), &json!({}), 1_000),
            hop_entry(&json!(20), &json!({}), 2_000),
        ];
        let sent = crate::relay::SentFrame {
            meta: json!({ "processing_ms": 20, "node_timings": {} }),
            tensor: None,
            round_trip_ms: 47,
            bytes_sent: 2_000,
            bytes_received: 123,
            hidden_state_bytes_sent: 0,
            hidden_state_bytes_received: 0,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            client_encode_ms: 3,
            response_headers_ms: 4,
            response_body_ms: 5,
            client_decode_ms: 6,
            tls: false,
            tls_handshake_ms: 0,
            transport: crate::relay::InferenceTransport::Libp2pTunnel,
            transport_fallback: false,
        };

        attach_transport(&mut hops, 1, &sent);

        assert_eq!(hops[0]["round_trip_ms"], 10);
        assert_eq!(hops[1]["round_trip_ms"], 47);
        assert_eq!(hops[1]["bytes_sent"], 2_000);
        assert_eq!(hops[1]["bytes_received"], 123);
        assert_eq!(hops[1]["transport"], "libp2p-tunnel");
    }
}
