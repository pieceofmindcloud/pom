//! Ligação persistente entre dois andares da pipeline (`transport/link-v1`).
//!
//! O commit dos lotes remotos tirou a travessia por ficha do trecho
//! **gateway → cabeça**: um control frame vai uma vez e os tokens voltam em
//! lotes por callback. O laço da cabeça também usa o mesmo canal para cada
//! ativação do hop seguinte quando a capability está presente - `POST /v1/step`
//! continua sendo a rota lógica, mas o request HTTP e o enquadramento vivem
//! em um fluxo persistente por aresta.
//!
//! A dependência causal do transformer proíbe adiantar o token seguinte de uma
//! sessão, mas não obriga a reconstruir a chamada. Aqui a aresta da pipeline
//! ganha **um canal**: um único `POST /v1/link` cujo corpo de ida é um fluxo de
//! quadros binários e cujo corpo de volta é outro fluxo de quadros binários. A
//! rede por baixo é a mesma de sempre - o cliente pinado por peer, a mesma
//! credencial, a mesma conexão TCP/TLS do pool. O que deixa de existir é a
//! abstração de requisição refeita por ativação.
//!
//! ## Enquadramento
//!
//! Cada quadro, nas duas direções:
//!
//! ```text
//! [4B BE tamanho do resto][2B BE status][2B BE tamanho do id][id][corpo]
//! ```
//!
//! * `status` é `0` numa ida e o código HTTP numa volta. Uma recusa do peer
//!   chega assim, com o mesmo significado que teria no caminho por requisição,
//!   e vira [`crate::relay::HttpRefusal`] no chamador.
//! * `id` é o `batch_id` do quadro `frame/v1-batch`, que é a identidade que o
//!   [`crate::relay::BatchCoordinator`] já usa. É ele quem correlaciona ida e
//!   volta, e é ele que permite **várias sessões no mesmo canal**: os quadros
//!   não voltam em ordem, voltam identificados.
//! * `corpo` é o `frame/v1-batch` inteiro. A rota vai no `kind` do próprio
//!   quadro, então não há caminho duplicado no envelope.
//!
//! ## Degradação
//!
//! Falha de transporte no canal **não mata a geração**: a chamada volta pelo
//! caminho de sempre (túnel libp2p, depois HTTPS por requisição) com
//! `transport_fallback=true`, e o canal morto sai do pool. Um destino que
//! recusou abrir fica em quarentena por [`OPEN_FAILURE_COOLDOWN`], para que a
//! tentativa de abrir não se pague por ficha num cluster onde o endereço
//! direto não serve.
//!
//! ## Limpeza
//!
//! Toda entrega pendente vive num registro por canal e sai dele no `Drop` do
//! guarda - em qualquer caminho de saída: resposta, erro, prazo, cancelamento
//! do chamador. Quando o canal morre, o leitor drena o registro inteiro com
//! erro antes de sumir, e nenhum chamador fica pendurado.

use crate::wire;
use anyhow::{anyhow, Result};
use axum::body::Bytes;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Rota do canal persistente. Fica na porta de inferência, sob a mesma
/// credencial das demais rotas `/v1`.
pub const LINK_PATH: &str = "/v1/link";

/// Cabeçalho fixo do envelope: status e tamanho do id.
const ENVELOPE_HEAD: usize = 4;

/// Teto de um quadro do canal. Sobra sobre o corpo máximo de inferência para o
/// envelope; um quadro maior que isto é erro de protocolo e derruba o canal.
const MAX_LINK_FRAME_BYTES: usize = wire::MAX_FRAME_PAYLOAD_BYTES + 4096;

/// Quantos quadros de ida cabem em voo antes de o remetente esperar. É a
/// contrapressão do canal: um hop lento segura quem o alimenta em vez de
/// deixar a memória crescer.
const OUTBOUND_QUEUE: usize = 64;

/// Prazo de uma chamada no canal, igual ao timeout do caminho por requisição.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Quanto tempo um destino fica sem nova tentativa de abrir canal depois de
/// uma falha de abertura. Deve acompanhar a memória de rotas: são a mesma
/// decisão de não pagar novamente uma URL que falhou.
const OPEN_FAILURE_COOLDOWN: Duration = crate::peer_client::ROUTE_FAILURE_COOLDOWN;

/// Só vale para receber os cabeçalhos da abertura. Depois que o canal abre,
/// o corpo continua vivo até `CHANNEL_LIFETIME`, sem transformar uma geração
/// longa em uma conexão interrompida prematuramente.
#[cfg(not(test))]
const LINK_OPEN_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
const LINK_OPEN_TIMEOUT: Duration = Duration::from_millis(50);

/// Teto de vida de um canal. O cliente HTTP do peer é construído com timeout
/// total, e o corpo em fluxo conta para ele: sem um teto explícito o canal
/// morreria no timeout comum de requisição. Quando este prazo vence, o canal
/// cai, a chamada em voo degrada para o caminho por requisição e o canal
/// seguinte é aberto na chamada seguinte.
const CHANNEL_LIFETIME: Duration = Duration::from_secs(12 * 3600);

/// O que uma travessia pelo canal devolve. Espelha o que o caminho por
/// requisição mede, menos o que não existe aqui: não há cabeçalhos de resposta
/// por ficha para cronometrar.
#[derive(Debug)]
pub struct LinkResponse {
    pub body: Vec<u8>,
    pub round_trip_ms: u64,
    pub bytes_sent: u64,
    pub profiling_spans: Vec<crate::prefill::TelemetrySpan>,
}

// ── Envelope ─────────────────────────────────────────────────────────────────

/// Monta `[tamanho][status][id][corpo]`.
pub fn encode_envelope(status: u16, id: &str, body: &[u8]) -> Result<Vec<u8>> {
    if id.len() > u16::MAX as usize {
        return Err(anyhow!("identificador de quadro longo demais"));
    }
    let rest = ENVELOPE_HEAD
        .checked_add(id.len())
        .and_then(|len| len.checked_add(body.len()))
        .ok_or_else(|| anyhow!("quadro do canal excede o limite numérico"))?;
    if rest > MAX_LINK_FRAME_BYTES {
        return Err(anyhow!(
            "quadro do canal excede o limite de {MAX_LINK_FRAME_BYTES} bytes"
        ));
    }
    let mut out = Vec::with_capacity(4 + rest);
    out.extend_from_slice(&(rest as u32).to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&(id.len() as u16).to_be_bytes());
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Um quadro já separado do fluxo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub status: u16,
    pub id: String,
    pub body: Vec<u8>,
}

/// Acumula bytes de um fluxo e devolve quadros inteiros conforme eles fecham.
///
/// Guardado à parte do transporte de propósito: é a única parte do canal com
/// aritmética de deslocamento, e é a que precisa de teste próprio contra
/// fragmentação arbitrária.
#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Próximo quadro inteiro, se já chegou. `Ok(None)` é "ainda não";
    /// `Err` é protocolo quebrado e o canal não continua.
    pub fn next(&mut self) -> Result<Option<Envelope>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let rest = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if rest > MAX_LINK_FRAME_BYTES {
            return Err(anyhow!(
                "quadro do canal anuncia {rest} bytes, acima do limite de {MAX_LINK_FRAME_BYTES}"
            ));
        }
        if rest < ENVELOPE_HEAD {
            return Err(anyhow!("quadro do canal curto demais: {rest} bytes"));
        }
        let total = 4 + rest;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let status = u16::from_be_bytes([self.buffer[4], self.buffer[5]]);
        let id_len = u16::from_be_bytes([self.buffer[6], self.buffer[7]]) as usize;
        let id_end = 8 + id_len;
        if id_end > total {
            return Err(anyhow!("identificador do quadro excede o quadro"));
        }
        let id = String::from_utf8(self.buffer[8..id_end].to_vec())
            .map_err(|_| anyhow!("identificador do quadro não é UTF-8"))?;
        let body = self.buffer[id_end..total].to_vec();
        self.buffer.drain(..total);
        Ok(Some(Envelope { status, id, body }))
    }
}

// ── Lado servidor ────────────────────────────────────────────────────────────

/// Serve um canal: lê quadros do corpo da requisição, despacha cada um pela
/// rota que o próprio quadro nomeia (`kind`) e devolve as respostas pelo corpo
/// da resposta, identificadas pelo `batch_id`.
///
/// Frames de inferência são despachados em tarefas próprias: um item lento de
/// uma sessão não bloqueia o quadro de outra. Callbacks `remote-stream` são
/// encaminhados por uma fila serial limitada, pois sua ordem é a ordem dos
/// tokens observados pelo gateway; isso preserva a causalidade sem transformar
/// o ACK em espera no produtor.
pub fn serve<D, F>(body: axum::body::Body, dispatch: D) -> axum::response::Response
where
    D: Fn(String, Bytes) -> F + Send + Sync + 'static,
    F: std::future::Future<Output = Result<axum::response::Response>> + Send + 'static,
{
    let dispatch = Arc::new(dispatch);
    let (out_tx, out_rx) = futures::channel::mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    let (callback_tx, mut callback_rx) =
        futures::channel::mpsc::channel::<Envelope>(OUTBOUND_QUEUE);
    let callback_dispatch = dispatch.clone();
    let mut callback_out_tx = out_tx.clone();
    tokio::spawn(async move {
        while let Some(envelope) = callback_rx.next().await {
            let reply = serve_one(callback_dispatch.as_ref(), &envelope).await;
            if callback_out_tx.send(Bytes::from(reply)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut stream = body.into_data_stream();
        let mut reader = FrameReader::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    debug!("canal de inferência encerrou a leitura: {error}");
                    break;
                }
            };
            reader.push(&chunk);
            loop {
                match reader.next() {
                    Ok(Some(envelope)) => {
                        if is_remote_stream_callback(&envelope.body) {
                            if callback_tx.clone().send(envelope).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let dispatch = dispatch.clone();
                        let mut out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let reply = serve_one(dispatch.as_ref(), &envelope).await;
                            let _ = out_tx.send(Bytes::from(reply)).await;
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        warn!("canal de inferência recebeu quadro inválido: {error}");
                        return;
                    }
                }
            }
        }
    });

    let stream = out_rx.map(Ok::<_, std::io::Error>);
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(axum::body::Body::from_stream(stream))
        .expect("resposta de canal sempre montável")
}

fn is_remote_stream_callback(body: &[u8]) -> bool {
    wire::peek_batch_header(body)
        .is_ok_and(|header| header.kind == crate::direct::REMOTE_STREAM_RESULT_PATH)
}

async fn serve_one<D, F>(dispatch: &D, envelope: &Envelope) -> Vec<u8>
where
    D: Fn(String, Bytes) -> F,
    F: std::future::Future<Output = Result<axum::response::Response>>,
{
    let path = match wire::peek_batch_header(&envelope.body) {
        Ok(header) => header.kind,
        Err(error) => return refusal(&envelope.id, 400, format!("quadro inválido: {error}")),
    };
    let response = match dispatch(path, Bytes::from(envelope.body.clone())).await {
        Ok(response) => response,
        Err(error) => return refusal(&envelope.id, 400, error.to_string()),
    };
    let status = response.status().as_u16();
    let bytes =
        match axum::body::to_bytes(response.into_body(), wire::MAX_FRAME_PAYLOAD_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return refusal(&envelope.id, 500, format!("resposta ilegível: {error}")),
        };
    match encode_envelope(status, &envelope.id, &bytes) {
        Ok(frame) => frame,
        Err(error) => refusal(&envelope.id, 500, error.to_string()),
    }
}

fn refusal(id: &str, status: u16, message: String) -> Vec<u8> {
    encode_envelope(status, id, message.as_bytes()).unwrap_or_else(|_| {
        // O identificador é o único campo de tamanho livre, e ele veio do fio
        // já limitado. Se ainda assim não couber, o quadro vazio mantém o canal
        // coerente em vez de emudecer a chamada.
        encode_envelope(status, "", b"").expect("envelope vazio sempre cabe")
    })
}

// ── Lado cliente ─────────────────────────────────────────────────────────────

struct Pending {
    reply: oneshot::Sender<std::result::Result<Envelope, String>>,
}

struct Channel {
    outbound: futures::channel::mpsc::Sender<Bytes>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    alive: Arc<AtomicBool>,
}

/// Remove a entrega pendente em **qualquer** caminho de saída: resposta,
/// erro, prazo, ou o chamador desistindo. Sem isto um cancelamento deixaria o
/// registro crescendo com tickets que ninguém mais espera.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

#[derive(Default)]
struct Pool {
    channels: tokio::sync::Mutex<HashMap<String, Arc<Channel>>>,
    /// Guarda somente a abertura de uma aresta. A rede nunca é aguardada com
    /// o mapa de canais segurado; chaves diferentes podem abrir em paralelo.
    opening: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    blocked: Mutex<HashMap<String, Instant>>,
    /// Aberturas por destino, não um total do processo: a bancada precisa
    /// provar que **aquela** aresta abriu uma ligação só, e um contador global
    /// contaria as arestas dos testes vizinhos.
    opened: Mutex<HashMap<String, u64>>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(Pool::default)
}

fn opening_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let Ok(mut opening) = pool().opening.lock() else {
        return Arc::new(tokio::sync::Mutex::new(()));
    };
    opening
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn target_key(node_id: &str, base_url: &str) -> String {
    format!("{node_id}@{}", base_url.trim_end_matches('/'))
}

/// Quantos canais esta aresta abriu. Existe para a bancada provar que a
/// ligação é **uma** para muitas fichas, e não uma por ficha. Em produção,
/// quem diz por onde a travessia passou é o `transport` do trace.
#[cfg(test)]
pub fn channels_opened(node_id: &str, base_url: &str) -> u64 {
    pool()
        .opened
        .lock()
        .ok()
        .and_then(|opened| opened.get(&target_key(node_id, base_url)).copied())
        .unwrap_or(0)
}

/// Se esta aresta tem canal vivo no pool.
#[cfg(test)]
pub async fn has_channel(node_id: &str, base_url: &str) -> bool {
    pool()
        .channels
        .lock()
        .await
        .contains_key(&target_key(node_id, base_url))
}

fn quarantined(key: &str) -> bool {
    let Ok(mut blocked) = pool().blocked.lock() else {
        return false;
    };
    match blocked.get(key) {
        Some(until) if *until > Instant::now() => true,
        Some(_) => {
            blocked.remove(key);
            false
        }
        None => false,
    }
}

fn quarantine(key: &str) {
    if let Ok(mut blocked) = pool().blocked.lock() {
        blocked.insert(key.to_string(), Instant::now() + OPEN_FAILURE_COOLDOWN);
    }
}

async fn forget(key: &str, channel: &Arc<Channel>) {
    let mut channels = pool().channels.lock().await;
    if channels
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, channel))
    {
        channels.remove(key);
    }
}

async fn channel_for(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Result<(Arc<Channel>, bool)> {
    let key = target_key(node_id, base_url);
    {
        let channels = pool().channels.lock().await;
        if let Some(channel) = channels.get(&key) {
            if channel.alive.load(Ordering::Relaxed) {
                return Ok((channel.clone(), false));
            }
        }
    }
    if quarantined(&key) {
        return Err(anyhow!("canal com {node_id} em quarentena após falha"));
    }

    // O guarda é por aresta. Duas travessias da mesma aresta compartilham a
    // abertura, mas uma tentativa lenta para outro destino não para este
    // processo inteiro.
    let opening = opening_lock(&key);
    let _opening = opening.lock().await;
    {
        let channels = pool().channels.lock().await;
        if let Some(channel) = channels.get(&key) {
            if channel.alive.load(Ordering::Relaxed) {
                return Ok((channel.clone(), false));
            }
        }
    }

    if quarantined(&key) {
        return Err(anyhow!("canal com {node_id} em quarentena após falha"));
    }

    match open(clients, node_id, base_url, session_id).await {
        Ok(channel) => {
            if let Ok(mut opened) = pool().opened.lock() {
                *opened.entry(key.clone()).or_default() += 1;
            }
            pool().channels.lock().await.insert(key, channel.clone());
            Ok((channel, true))
        }
        Err(error) => {
            quarantine(&key);
            Err(error)
        }
    }
}

async fn open(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Result<Arc<Channel>> {
    let http = clients.for_peer(node_id)?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), LINK_PATH);
    let (outbound, rx) = futures::channel::mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    let request = http
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        // O timeout do cliente vale para a requisição inteira, e a nossa dura
        // a geração toda. Ver `CHANNEL_LIFETIME`.
        .timeout(CHANNEL_LIFETIME)
        .body(reqwest::Body::wrap_stream(rx.map(Ok::<_, std::io::Error>)));
    let response = clients
        .authorize_inference_request_for_session(request, session_id)
        .await
        .send();
    let response = tokio::time::timeout(LINK_OPEN_TIMEOUT, response)
        .await
        .map_err(|_| anyhow!("canal com {node_id} não abriu antes do timeout de conexão"))?
        .map_err(|error| anyhow!("canal com {node_id} não abriu: {error}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "canal com {node_id} recusado com HTTP {}",
            response.status()
        ));
    }

    let pending: Arc<Mutex<HashMap<String, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let channel = Arc::new(Channel {
        outbound,
        pending: pending.clone(),
        alive: alive.clone(),
    });

    let key = target_key(node_id, base_url);
    let node = node_id.to_string();
    let reader_channel = channel.clone();
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut reader = FrameReader::new();
        let mut cause = String::from("canal encerrado pelo peer");
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    cause = format!("leitura do canal falhou: {error}");
                    break;
                }
            };
            reader.push(&chunk);
            loop {
                match reader.next() {
                    Ok(Some(envelope)) => {
                        let waiting = pending
                            .lock()
                            .ok()
                            .and_then(|mut map| map.remove(&envelope.id));
                        if let Some(waiting) = waiting {
                            let _ = waiting.reply.send(Ok(envelope));
                        } else {
                            debug!(
                                peer = %node,
                                id = %envelope.id,
                                "resposta do canal chegou depois da desistência"
                            );
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        cause = format!("quadro inválido no canal: {error}");
                        break 'outer;
                    }
                }
            }
        }
        alive.store(false, Ordering::Relaxed);
        forget(&key, &reader_channel).await;
        let waiting: Vec<Pending> = pending
            .lock()
            .map(|mut map| map.drain().map(|(_, value)| value).collect())
            .unwrap_or_default();
        for entry in waiting {
            let _ = entry.reply.send(Err(cause.clone()));
        }
    });

    debug!(peer = node_id, url = %url, "canal de inferência aberto");
    Ok(channel)
}

/// Envia um quadro `frame/v1-batch` inteiro pelo canal persistente e espera a
/// resposta correlacionada pelo `batch_id`.
///
/// Devolve erro em qualquer falha de transporte - é o chamador quem decide
/// degradar para o caminho por requisição. Uma recusa HTTP do peer volta como
/// [`crate::relay::HttpRefusal`], igual ao caminho por requisição, porque ela
/// é a resposta considerada do peer e trocar de transporte não a muda.
#[cfg(test)]
pub async fn send_batch(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
) -> Result<LinkResponse> {
    send_batch_with_candidates(clients, node_id, base_url, &[], batch_id, body, peer_label).await
}

/// Abre o canal persistente pela primeira rota saudável anunciada. A abertura
/// é a única fase em que o endereço ainda não está memorizado como saudável;
/// uma falha de conexão é registrada e a alternativa seguinte assume já nesta
/// chamada. Depois que um canal abre, `send_batch` mantém sua semântica de não
/// repetir um frame já enviado em outra rota.
#[allow(dead_code)]
pub async fn send_batch_with_candidates(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
) -> Result<LinkResponse> {
    send_batch_with_session(
        clients, node_id, base_url, candidates, batch_id, body, peer_label, None,
    )
    .await
}

pub async fn send_batch_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<LinkResponse> {
    send_batch_with_session_profile(
        clients, node_id, base_url, candidates, batch_id, body, peer_label, session_id, None,
    )
    .await
}

/// Variante usada pelo trace detalhado. O modo normal passa `None` e não
/// aloca spans nem faz trabalho adicional no caminho quente.
pub async fn send_batch_with_session_profile(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
    profiling: Option<crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
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
    let route_key = format!("inference:{node_id}");
    let ordered = clients.route_order(&route_key, &route_candidates);
    let mut last_error = String::new();
    for base in ordered {
        let setup_started = Instant::now();
        let (channel, opened) = match channel_for(clients, node_id, &base, session_id).await {
            Ok(channel) => channel,
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        match send_batch_on_channel(
            &channel,
            node_id,
            batch_id,
            body.clone(),
            peer_label,
            profiling.as_ref(),
        )
        .await
        {
            Ok(mut response) => {
                if opened {
                    if let Some(context) = profiling.as_ref() {
                        response.profiling_spans.push(context.span(
                            "https-link-setup",
                            setup_started,
                            Instant::now(),
                            None,
                            Some("https-link".into()),
                        ));
                    }
                }
                clients.remember_route_success(&route_key, &base);
                return Ok(response);
            }
            Err(error) if error.downcast_ref::<crate::relay::HttpRefusal>().is_some() => {
                return Err(error)
            }
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                return Err(error);
            }
        }
    }
    Err(anyhow!(
        "canal com {node_id} não abriu por nenhuma rota: {last_error}"
    ))
}

/// Envia um frame já enquadrado pelo canal persistente, sem exigir que a
/// resposta seja outro frame de inferência. O retorno remoto do
/// `remote-stream` usa exatamente esta variante: cada lote é um frame do
/// canal, mas o peer responde apenas com o ACK HTTP do callback.
pub async fn send_body_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    frame_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<LinkResponse> {
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
    let route_key = format!("reply:{node_id}");
    let mut last_error = String::new();
    for base in clients.route_order(&route_key, &route_candidates) {
        let (channel, _opened) = match channel_for(clients, node_id, &base, session_id).await {
            Ok(channel) => channel,
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        match send_frame_on_channel(
            &channel,
            node_id,
            frame_id,
            body.clone(),
            peer_label,
            false,
            None,
        )
        .await
        {
            Ok(response) => {
                clients.remember_route_success(&route_key, &base);
                return Ok(response);
            }
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                return Err(error);
            }
        }
    }
    Err(anyhow!(
        "canal de retorno com {node_id} não abriu por nenhuma rota: {last_error}"
    ))
}

/// Enfileira um frame de callback no canal persistente e não espera o ACK HTTP
/// correspondente. O próprio canal continua full-duplex: a tarefa leitora
/// drena as respostas e o `pending` acompanha falhas assíncronas. A fila de
/// saída limitada a [`OUTBOUND_QUEUE`] mantém contrapressão local sem impor
/// um round-trip por token ao produtor remoto.
pub async fn send_body_without_response_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    frame_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<()> {
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
    let route_key = format!("reply:{node_id}");
    let mut last_error = String::new();
    for base in clients.route_order(&route_key, &route_candidates) {
        let (channel, _opened) = match channel_for(clients, node_id, &base, session_id).await {
            Ok(channel) => channel,
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        let channel_key = target_key(node_id, &base);
        match send_frame_without_response(
            &channel,
            node_id,
            frame_id,
            body.clone(),
            peer_label,
            channel_key,
        )
        .await
        {
            Ok(()) => {
                clients.remember_route_success(&route_key, &base);
                return Ok(());
            }
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
            }
        }
    }
    Err(anyhow!(
        "canal de retorno com {node_id} não enfileirou por nenhuma rota: {last_error}"
    ))
}

async fn send_batch_on_channel(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    profiling: Option<&crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
    send_frame_on_channel(
        channel, node_id, batch_id, body, peer_label, true, profiling,
    )
    .await
}

async fn send_frame_on_channel(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    validate_response_batch: bool,
    profiling: Option<&crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
    let frame = encode_envelope(0, batch_id, &body)?;
    let bytes_sent = frame.len() as u64;

    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut map = channel
            .pending
            .lock()
            .map_err(|_| anyhow!("registro de entregas do canal corrompido"))?;
        if map.contains_key(batch_id) {
            return Err(anyhow!("batch_id repetido no canal: {batch_id}"));
        }
        map.insert(batch_id.to_string(), Pending { reply: reply_tx });
    }
    let _guard = PendingGuard {
        pending: channel.pending.clone(),
        id: batch_id.to_string(),
    };

    let started = Instant::now();
    let send_started = started;
    channel
        .outbound
        .clone()
        .send(Bytes::from(frame))
        .await
        .map_err(|_| anyhow!("canal com {node_id} fechou antes do envio"))?;
    let send_finished = Instant::now();

    let envelope = match tokio::time::timeout(CALL_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(envelope))) => envelope,
        Ok(Ok(Err(error))) => return Err(anyhow!("{peer_label}: {error}")),
        Ok(Err(_)) => return Err(anyhow!("{peer_label}: canal encerrou sem responder")),
        Err(_) => return Err(anyhow!("{peer_label}: canal não respondeu a tempo")),
    };
    let received = Instant::now();
    if !(200..300).contains(&envelope.status) {
        return Err(anyhow::Error::new(crate::relay::HttpRefusal {
            status: envelope.status,
            body: String::from_utf8_lossy(&envelope.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    if validate_response_batch {
        wire::validate_batch_bytes(&envelope.body)?;
    }
    let mut profiling_spans = Vec::new();
    if let Some(context) = profiling {
        profiling_spans.push(context.span(
            "frame-send",
            send_started,
            send_finished,
            None,
            Some("https-link".into()),
        ));
        profiling_spans.push(context.span(
            "transport-round-trip",
            started,
            received,
            None,
            Some("https-link".into()),
        ));
        profiling_spans.push(context.span(
            "frame-receive",
            received,
            received,
            None,
            Some("https-link".into()),
        ));
    }
    Ok(LinkResponse {
        body: envelope.body,
        round_trip_ms: started.elapsed().as_millis() as u64,
        bytes_sent,
        profiling_spans,
    })
}

/// Envia um frame de callback sem converter o ACK do receptor em uma barreira
/// no emissor. O ACK continua associado a um `Pending`, para que a tarefa
/// leitora observe HTTP 4xx/5xx e perda do canal, mas não é aguardado pelo
/// caminho de geração.
async fn send_frame_without_response(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    channel_key: String,
) -> Result<()> {
    if !channel.alive.load(Ordering::Acquire) {
        return Err(anyhow!("canal com {node_id} já está encerrado"));
    }
    let frame = encode_envelope(0, batch_id, &body)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut map = channel
            .pending
            .lock()
            .map_err(|_| anyhow!("registro de entregas do canal corrompido"))?;
        if map.contains_key(batch_id) {
            return Err(anyhow!("batch_id repetido no canal: {batch_id}"));
        }
        map.insert(batch_id.to_string(), Pending { reply: reply_tx });
    }
    let guard = PendingGuard {
        pending: channel.pending.clone(),
        id: batch_id.to_string(),
    };
    channel
        .outbound
        .clone()
        .send(Bytes::from(frame))
        .await
        .map_err(|_| anyhow!("{peer_label}: canal fechou antes do envio"))?;

    let pending = channel.pending.clone();
    let channel = channel.clone();
    let node_id = node_id.to_string();
    let batch_id = batch_id.to_string();
    tokio::spawn(async move {
        let failed = match tokio::time::timeout(CALL_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(envelope))) if (200..300).contains(&envelope.status) => None,
            Ok(Ok(Ok(envelope))) => Some(format!("peer respondeu HTTP {}", envelope.status)),
            Ok(Ok(Err(error))) => Some(error),
            Ok(Err(_)) => Some("canal encerrou sem responder".to_string()),
            Err(_) => Some("canal não respondeu ACK a tempo".to_string()),
        };
        drop(guard);
        if let Some(error) = failed {
            channel.alive.store(false, Ordering::Release);
            if let Ok(mut entries) = pending.lock() {
                entries.remove(&batch_id);
            }
            forget(&channel_key, &channel).await;
            warn!(peer = %node_id, batch = %batch_id, %error, "ACK assíncrono do callback falhou; canal descartado");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use serde_json::json;

    fn batch(kind: &str, batch_id: &str, item_id: &str) -> Vec<u8> {
        wire::encode_batch_items(
            kind,
            batch_id,
            &[wire::BatchPayloadItem {
                item_id: item_id.into(),
                session_id: "s".into(),
                meta: json!({"session_id": "s"}),
                tensor: Some((vec![0.5f32; 8], vec![1, 8])),
                dtype: wire::WireDtype::F16,
            }],
        )
        .expect("quadro válido")
    }

    #[test]
    fn o_leitor_remonta_quadros_partidos_em_qualquer_ponto() {
        let a = encode_envelope(0, "um", b"corpo-um").unwrap();
        let b = encode_envelope(503, "dois", b"corpo-dois").unwrap();
        let mut stream = a.clone();
        stream.extend_from_slice(&b);

        for cut in 1..stream.len() {
            let mut reader = FrameReader::new();
            reader.push(&stream[..cut]);
            let mut got = Vec::new();
            while let Some(envelope) = reader.next().unwrap() {
                got.push(envelope);
            }
            reader.push(&stream[cut..]);
            while let Some(envelope) = reader.next().unwrap() {
                got.push(envelope);
            }
            assert_eq!(got.len(), 2, "corte em {cut} perdeu um quadro");
            assert_eq!(got[0].id, "um");
            assert_eq!(got[0].status, 0);
            assert_eq!(got[0].body, b"corpo-um");
            assert_eq!(got[1].id, "dois");
            assert_eq!(got[1].status, 503);
            assert_eq!(got[1].body, b"corpo-dois");
        }
    }

    #[test]
    fn o_leitor_recusa_quadro_acima_do_teto() {
        let mut reader = FrameReader::new();
        reader.push(&(u32::MAX).to_be_bytes());
        assert!(reader.next().is_err());
    }

    async fn spawn_link_server(
        answer: impl Fn(String, Bytes) -> axum::response::Response + Clone + Send + Sync + 'static,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            LINK_PATH,
            post(move |body: axum::body::Body| {
                let answer = answer.clone();
                async move {
                    serve(body, move |path, bytes| {
                        let answer = answer.clone();
                        async move { Ok(answer(path, bytes)) }
                    })
                }
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        format!("http://{addr}")
    }

    /// Ecoa o quadro recebido como resposta, respondendo `atraso_ms` depois.
    fn echo_after(
        delay: std::collections::HashMap<String, u64>,
    ) -> impl Fn(String, Bytes) -> axum::response::Response + Clone + Send + Sync + 'static {
        move |path: String, body: Bytes| {
            let header = wire::peek_batch_header(&body).expect("quadro válido");
            let (_, items) = wire::decode_batch_items(&body).expect("itens válidos");
            let delay = delay.get(&header.batch_id).copied().unwrap_or(0);
            let out = wire::encode_batch_items(
                path,
                header.batch_id,
                &items
                    .into_iter()
                    .map(|item| wire::BatchPayloadItem {
                        item_id: item.item_id,
                        session_id: "s".into(),
                        meta: item.meta,
                        tensor: item.tensor,
                        dtype: item.dtype,
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("resposta válida");
            let response = axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(out))
                .unwrap();
            if delay == 0 {
                response
            } else {
                // Um handler síncrono não pode dormir; o atraso é simulado
                // devolvendo um corpo que só fecha depois.
                let (mut tx, rx) = futures::channel::mpsc::channel::<Bytes>(1);
                let (parts, body) = response.into_parts();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                    let _ = tx.send(bytes).await;
                });
                axum::response::Response::from_parts(
                    parts,
                    axum::body::Body::from_stream(rx.map(Ok::<_, std::io::Error>)),
                )
            }
        }
    }

    #[tokio::test]
    async fn muitas_travessias_compartilham_um_canal() {
        let url = spawn_link_server(echo_after(Default::default())).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        for index in 0..8 {
            let id = format!("batch-{index}");
            let body = batch("/v1/step", &id, "item-0");
            let response = send_batch(&clients, "hop", &url, &id, Bytes::from(body), "hop")
                .await
                .expect("travessia pelo canal");
            let header = wire::peek_batch_header(&response.body).unwrap();
            assert_eq!(header.batch_id, id);
        }
        assert_eq!(
            channels_opened("hop", &url),
            1,
            "oito travessias não podem abrir oito ligações"
        );
    }

    #[tokio::test]
    async fn callback_persistente_aceita_ack_que_nao_e_batch() {
        let url = spawn_link_server(|_path: String, _body: Bytes| {
            axum::response::Response::builder()
                .status(200)
                .body(axum::body::Body::from("callback aceito"))
                .unwrap()
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/remote-stream-result", "frame-0", "frame-0");

        let response = send_body_with_session(
            &clients,
            "gateway",
            &url,
            &[],
            "frame-0",
            Bytes::from(body),
            "remote-stream-return",
            None,
        )
        .await
        .expect("callback deve reutilizar o canal persistente");

        assert_eq!(response.body, b"callback aceito");
        assert_eq!(channels_opened("gateway", &url), 1);
    }

    #[tokio::test]
    async fn callbacks_persistentes_nao_esperam_ack_de_cada_frame() {
        let mut delays = std::collections::HashMap::new();
        for index in 0..8 {
            delays.insert(format!("frame-{index}"), 200u64);
        }
        let url = spawn_link_server(echo_after(delays)).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let started = Instant::now();

        for index in 0..8 {
            let id = format!("frame-{index}");
            send_body_without_response_with_session(
                &clients,
                "callback-full-duplex",
                &url,
                &[],
                &id,
                Bytes::from(batch("/v1/remote-stream-result", &id, &id)),
                "remote-stream-return",
                Some("session-full-duplex"),
            )
            .await
            .expect("o frame deve ser enfileirado sem esperar o ACK");
        }

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "o emissor esperou ACK por frame: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn sessoes_diferentes_nao_se_bloqueiam_no_mesmo_canal() {
        let mut delays = std::collections::HashMap::new();
        delays.insert("lenta".to_string(), 300u64);
        let url = spawn_link_server(echo_after(delays)).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));

        let slow_body = batch("/v1/step", "lenta", "item-0");
        let fast_body = batch("/v1/step", "rapida", "item-0");
        let slow_clients = clients.clone();
        let slow_url = url.clone();
        let slow = tokio::spawn(async move {
            send_batch(
                &slow_clients,
                "hop",
                &slow_url,
                "lenta",
                Bytes::from(slow_body),
                "hop",
            )
            .await
            .map(|response| response.round_trip_ms)
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        let fast = send_batch(
            &clients,
            "hop",
            &url,
            "rapida",
            Bytes::from(fast_body),
            "hop",
        )
        .await
        .expect("a sessão rápida atravessa");
        let fast_elapsed = started.elapsed();
        let slow_ms = slow.await.unwrap().expect("a sessão lenta atravessa");

        assert!(
            fast_elapsed < Duration::from_millis(200),
            "a sessão rápida esperou {fast_elapsed:?} pela lenta - o canal está bloqueando por cabeça de fila"
        );
        assert!(
            slow_ms >= 250,
            "a sessão lenta devia ter demorado, veio {slow_ms}ms"
        );
        let _ = fast;
    }

    #[tokio::test]
    async fn recusa_do_peer_volta_como_recusa_http() {
        let url = spawn_link_server(|_path: String, _body: Bytes| {
            axum::response::Response::builder()
                .status(400)
                .body(axum::body::Body::from("sessão desconhecida"))
                .unwrap()
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/step", "b", "item-0");
        let error = send_batch(&clients, "hop", &url, "b", Bytes::from(body), "hop")
            .await
            .expect_err("uma recusa não pode virar sucesso");
        let refusal = error
            .downcast_ref::<crate::relay::HttpRefusal>()
            .expect("recusa do peer preserva o status");
        assert_eq!(refusal.status, 400);
        assert!(refusal.body.contains("sessão desconhecida"));
    }

    /// Um quadro recusado pelo despachante - é assim que a credencial
    /// reconferida por quadro recusa - vira recusa **daquele quadro**, e o
    /// canal continua servindo os seguintes.
    #[tokio::test]
    async fn quadro_recusado_pelo_despachante_nao_derruba_o_canal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recusar = Arc::new(AtomicBool::new(true));
        let servidor = recusar.clone();
        let router = Router::new().route(
            LINK_PATH,
            post(move |body: axum::body::Body| {
                let servidor = servidor.clone();
                async move {
                    serve(body, move |path, bytes| {
                        let servidor = servidor.clone();
                        async move {
                            if servidor.swap(false, Ordering::Relaxed) {
                                return Err(anyhow!("credencial de inferência inválida"));
                            }
                            let header = wire::peek_batch_header(&bytes).unwrap();
                            let (_, items) = wire::decode_batch_items(&bytes).unwrap();
                            let out = wire::encode_batch_items(
                                path,
                                header.batch_id,
                                &items
                                    .into_iter()
                                    .map(|item| wire::BatchPayloadItem {
                                        item_id: item.item_id,
                                        session_id: "s".into(),
                                        meta: item.meta,
                                        tensor: item.tensor,
                                        dtype: item.dtype,
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            .unwrap();
                            Ok(axum::response::Response::builder()
                                .status(200)
                                .body(axum::body::Body::from(out))
                                .unwrap())
                        }
                    })
                }
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));

        let error = send_batch(
            &clients,
            "estrito",
            &url,
            "recusado",
            Bytes::from(batch("/v1/step", "recusado", "item-0")),
            "estrito",
        )
        .await
        .expect_err("o quadro sem credencial válida tem que ser recusado");
        let refusal = error
            .downcast_ref::<crate::relay::HttpRefusal>()
            .expect("recusa por quadro preserva o status");
        assert_eq!(refusal.status, 400);
        assert!(refusal.body.contains("credencial"));

        let aceito = send_batch(
            &clients,
            "estrito",
            &url,
            "aceito",
            Bytes::from(batch("/v1/step", "aceito", "item-0")),
            "estrito",
        )
        .await
        .expect("o quadro seguinte usa o mesmo canal, que continua vivo");
        assert_eq!(
            wire::peek_batch_header(&aceito.body).unwrap().batch_id,
            "aceito"
        );
        assert_eq!(channels_opened("estrito", &url), 1);
    }

    #[tokio::test]
    async fn abertura_de_aresta_morta_nao_bloqueia_outra() {
        use tokio::sync::Notify;

        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        let accepted = Arc::new(Notify::new());
        let accepted_task = accepted.clone();
        tokio::spawn(async move {
            let (stream, _) = blackhole.accept().await.expect("aceitou a abertura morta");
            accepted_task.notify_one();
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let healthy = spawn_link_server(echo_after(Default::default())).await;
        let dead = format!("http://{blackhole_addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let dead_clients = clients.clone();
        let dead_task = tokio::spawn(async move {
            send_batch(
                &dead_clients,
                "aresta-morta",
                &dead,
                "dead",
                Bytes::from(batch("/v1/step", "dead", "item-0")),
                "aresta-morta",
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), accepted.notified())
            .await
            .expect("a abertura morta começou");

        let started = Instant::now();
        let healthy_result = tokio::time::timeout(
            LINK_OPEN_TIMEOUT / 2,
            send_batch(
                &clients,
                "aresta-saudavel",
                &healthy,
                "healthy",
                Bytes::from(batch("/v1/step", "healthy", "item-0")),
                "aresta-saudavel",
            ),
        )
        .await
        .expect("a abertura de outra aresta não pode esperar o blackhole")
        .expect("a aresta saudável deve atravessar");

        assert_eq!(
            wire::peek_batch_header(&healthy_result.body)
                .expect("resposta válida")
                .batch_id,
            "healthy"
        );
        println!(
            "link_lab: aresta saudável respondeu enquanto a abertura morta estava em voo em {:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < LINK_OPEN_TIMEOUT / 2,
            "a aresta saudável ficou parada por {:?}",
            started.elapsed()
        );
        assert!(dead_task.await.unwrap().is_err());
        assert_eq!(channels_opened("aresta-saudavel", &healthy), 1);
    }

    #[tokio::test]
    async fn destino_inalcancavel_falha_rapido_e_fica_em_quarentena() {
        // Porta fechada: o canal não abre.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(2));
        let body = batch("/v1/step", "b", "item-0");
        assert!(send_batch(
            &clients,
            "morto",
            &url,
            "b",
            Bytes::from(body.clone()),
            "morto"
        )
        .await
        .is_err());
        let started = Instant::now();
        let error = send_batch(&clients, "morto", &url, "b2", Bytes::from(body), "morto")
            .await
            .expect_err("segunda tentativa também falha");
        assert!(
            error.to_string().contains("nenhuma rota"),
            "a segunda tentativa devia parar na memória de falha, veio: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "a quarentena existe para não pagar a conexão morta de novo"
        );
    }

    #[tokio::test]
    async fn uma_abertura_blackhole_cai_para_a_rota_saudavel_e_nao_se_repete() {
        let blackhole_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blackhole_hits_task = blackhole_hits.clone();
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = blackhole.accept().await {
                blackhole_hits_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                });
            }
        });

        let healthy = spawn_link_server(echo_after(std::collections::HashMap::new())).await;
        let bad = format!("http://{blackhole_addr}");
        let candidates = vec![healthy.clone()];
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let started = Instant::now();
        for batch_id in ["blackhole-1", "blackhole-2"] {
            let response = send_batch_with_candidates(
                &clients,
                "blackhole-fallback",
                &bad,
                &candidates,
                batch_id,
                Bytes::from(batch("/v1/step", batch_id, "item-0")),
                "blackhole-fallback",
            )
            .await
            .expect("a rota saudável deve assumir o canal");
            assert_eq!(
                wire::peek_batch_header(&response.body).unwrap().batch_id,
                batch_id
            );
        }
        assert_eq!(blackhole_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(channels_opened("blackhole-fallback", &healthy), 1);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a abertura morta voltou a ser paga: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_morte_do_canal_nao_deixa_entrega_pendente() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Servidor que aceita o canal e some sem responder.
        let router = Router::new().route(
            LINK_PATH,
            post(|_body: axum::body::Body| async move {
                let (tx, rx) = futures::channel::mpsc::channel::<Bytes>(1);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    drop(tx);
                });
                axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::from_stream(
                        rx.map(Ok::<_, std::io::Error>),
                    ))
                    .unwrap()
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/step", "b", "item-0");
        let error = send_batch(&clients, "mudo", &url, "b", Bytes::from(body), "mudo")
            .await
            .expect_err("um canal que morre tem que devolver erro, não pendurar");
        assert!(
            error.to_string().contains("mudo"),
            "o erro precisa nomear o peer: {error}"
        );
        assert!(
            !has_channel("mudo", &url).await,
            "o canal morto tem que sair do pool para a chamada seguinte reabrir"
        );
    }
}
