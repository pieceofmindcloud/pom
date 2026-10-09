//! HTTP/3 sobre QUIC para os hops de inferência.
//!
//! O endpoint é opcional no cliente, mas o servidor o deixa pronto quando a
//! identidade do nó existe. Isso permite ligar a política em tempo de execução
//! sem reiniciar e mantém o fallback TCP disponível durante uma atualização
//! parcial do cluster.

use anyhow::{anyhow, Context, Result};
use axum::body::{Body, Bytes};
use bytes::Buf;
use futures::{future::poll_fn, StreamExt};
use h3_quinn::Connection as H3Connection;
use http::{Request, StatusCode};
use quinn;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::lookup_host;
use tracing::{debug, info};

/// Offsets a partir do `p2p_port` do próprio nó para as portas UDP dos
/// endpoints HTTP/3.
///
/// A derivação **não** pode partir de `gateway_port`/`inference_port`: num
/// cluster co-locado (host networking, todos os nós no mesmo namespace) esses
/// números são consecutivos entre nós, então `porta + 1` cai exatamente na
/// `gateway_port` do nó vizinho. Foi o que quebrou a descoberta na ksia -
/// a porta QUIC de um nó ocupava a porta de gateway do seguinte.
///
/// O `p2p_port` já é único por nó co-locado (42777/42778/…). Os offsets são
/// altos e distintos para que as bandas de gateway-QUIC e inference-QUIC não
/// recolidam entre si nem com a faixa de `p2p_port`.
const GATEWAY_QUIC_OFFSET: u16 = 2000;
const INFERENCE_QUIC_OFFSET: u16 = 1000;

/// Porta UDP do HTTP/3 do gateway, derivada do `p2p_port` deste nó.
///
/// `None` quando `p2p_port` é 0 (bind efêmero pedido na config, ainda sem
/// número real) ou quando o offset estoura `u16`: nos dois casos o QUIC fica
/// desligado e o hop cai no TCP, em vez de arriscar uma colisão de porta.
pub fn gateway_quic_port(p2p_port: u16) -> Option<u16> {
    if p2p_port == 0 {
        return None;
    }
    p2p_port.checked_add(GATEWAY_QUIC_OFFSET)
}

/// Porta UDP do HTTP/3 da inferência, derivada do `p2p_port` deste nó.
/// Mesmas condições de `None` que [`gateway_quic_port`].
pub fn inference_quic_port(p2p_port: u16) -> Option<u16> {
    if p2p_port == 0 {
        return None;
    }
    p2p_port.checked_add(INFERENCE_QUIC_OFFSET)
}

/// Limite local do handshake. O orçamento global de estabelecimento também
/// cobre DNS e abertura do stream, então o fallback não espera a soma destes
/// limites.
pub(crate) const QUIC_ESTABLISH_TIMEOUT: Duration = Duration::from_millis(500);

/// Orçamento curto para descobrir que o endpoint QUIC não está disponível.
/// O envio do corpo e o processamento remoto usam o orçamento separado da
/// chamada. Endereços anunciados continuam sendo alternativas dentro deste
/// teto para que a queda para HTTP não espere uma sequência de timeouts.
pub(crate) const QUIC_FALLBACK_BUDGET: Duration = Duration::from_millis(300);

// Uma tentativa de transporte tem **um** prazo: o do frame, que o chamador
// passa como `timeout`. Não há um teto menor aninhado dentro dele - um teto
// fixo de 30 s matava prefill/decode legítimos mais longos no QUIC e no HTTP
// enquanto o canal persistente e o túnel esperavam o prazo inteiro. Peer que
// sumiu é detectado pela vida da conexão, em milissegundos: `QUIC_MAX_IDLE`
// aqui e o keep-alive TCP de `peer_client` no HTTP.

/// Uma falha de descoberta torna inútil repetir o mesmo handshake no próximo
/// token. O caminho HTTP continua disponível imediatamente, e o QUIC volta a
/// ser sondado depois desta janela para recuperar uma porta que foi reiniciada.
/// É a janela única de memória de falha (`peer_client::FAILURE_MEMORY`).
const QUIC_FAILURE_QUARANTINE: Duration = crate::peer_client::FAILURE_MEMORY;

/// Erro QUIC com certeza de entrega. Uma falha antes de abrir/enviar o corpo
/// pode seguir para outro transporte; depois de iniciar o corpo, repetir pode
/// executar o mesmo chunk duas vezes no KV remoto.
#[derive(Debug)]
pub struct QuicDeliveryError {
    pub before_body: bool,
    message: String,
}

impl QuicDeliveryError {
    fn new(error: impl std::fmt::Display, before_body: bool) -> Self {
        Self {
            before_body,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for QuicDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuicDeliveryError {}

/// Sem tráfego reconhecido por este período a conexão fecha com erro - é o
/// que detecta um caminho UDP morto em ~1 s em vez dos 600 s do frame. O
/// keep-alive mantém uma conexão viva-porém-lenta (o peer computando o
/// prefill/decode) sob esse teto, porque cada PING volta reconhecido.
pub(crate) const QUIC_MAX_IDLE: Duration = Duration::from_millis(1200);
pub(crate) const QUIC_KEEP_ALIVE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct QuicResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    /// Wall time awaiting request body send and stream finish. This is not
    /// the response-header wait or the QUIC path RTT.
    pub request_body_send_us: u64,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
    /// Smoothed RTT measured by QUIC for the path, independent of handler
    /// processing. Microseconds preserve sub-millisecond links such as the
    /// co-located ksia peers.
    pub transport_rtt_us: u64,
}

struct QuicSession {
    sender: tokio::sync::Mutex<h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>>,
    connection: quinn::Connection,
}

#[derive(Clone)]
pub struct ClientPool {
    endpoint: Arc<tokio::sync::OnceCell<quinn::Endpoint>>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<QuicSession>>>>,
    /// Motivo guardado junto do prazo: uma tentativa em quarentena reporta
    /// "em quarentena" sozinho, escondendo o erro original que quarentenou o
    /// endpoint (foi o caso do "invalid authority" na telemetria da ksia -
    /// só a primeira tentativa mostrava a causa real, as 113 seguintes só
    /// diziam "em quarentena").
    failed_until: Arc<tokio::sync::Mutex<HashMap<String, (Instant, String)>>>,
    /// Orçamento fixo de estabelecimento, usado pelas sondas do plano de
    /// rotas. Sem ele o orçamento sai do RTT medido do peer
    /// (`route_plan::RouteBook::establish_budget`).
    establish_budget: Option<Duration>,
}

impl ClientPool {
    pub fn new() -> Self {
        Self {
            endpoint: Arc::new(tokio::sync::OnceCell::const_new()),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            failed_until: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            establish_budget: None,
        }
    }

    /// Pool das sondas do plano de rotas. A sonda é quem mede o RTT, então não
    /// pode depender de um orçamento derivado dele: um peer ainda não medido
    /// com RTT alto nunca teria o QUIC medido.
    pub fn for_probes(budget: Duration) -> Self {
        Self {
            establish_budget: Some(budget),
            ..Self::new()
        }
    }

    async fn endpoint(&self) -> Result<&quinn::Endpoint> {
        self.endpoint
            .get_or_try_init(|| async {
                Ok::<_, anyhow::Error>(quinn::Endpoint::client("[::]:0".parse()?)?)
            })
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        peer_id: &str,
        base_url: &str,
        quic_addresses: &[String],
        path: &str,
        content_type: &str,
        authorization: Option<String>,
        session_id: Option<&str>,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<QuicResponse> {
        let mut last_error = "no QUIC address announced".to_string();
        let budget = self.establish_budget.unwrap_or_else(|| {
            crate::route_plan::book().establish_budget(peer_id, QUIC_FALLBACK_BUDGET)
        });
        let fallback_deadline = Instant::now() + budget;
        for advertised in quic_addresses {
            let key = format!("{peer_id}@{}", advertised.trim());
            if let Some(reason) = self.quic_quarantine_reason(&key).await {
                last_error = format!("{advertised}: QUIC endpoint in quarantine ({reason})");
                continue;
            }
            if remaining_budget(fallback_deadline).is_none() {
                last_error = "QUIC establishment budget exhausted".to_string();
                break;
            }
            match self
                .request_one(
                    &key,
                    peer_id,
                    advertised,
                    base_url,
                    path,
                    content_type,
                    authorization.clone(),
                    session_id,
                    body.clone(),
                    timeout,
                    fallback_deadline,
                )
                .await
            {
                Ok(response) => {
                    self.failed_until.lock().await.remove(&key);
                    return Ok(response);
                }
                Err(error) => {
                    if !error
                        .downcast_ref::<QuicDeliveryError>()
                        .is_some_and(|failure| failure.before_body)
                    {
                        return Err(error);
                    }
                    last_error = format!("{advertised}: {error}");
                    self.quarantine_quic(&key, error.to_string()).await;
                }
            }
        }
        Err(anyhow!(QuicDeliveryError::new(last_error, true)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_one(
        &self,
        key: &str,
        peer_id: &str,
        advertised: &str,
        base_url: &str,
        path: &str,
        content_type: &str,
        authorization: Option<String>,
        session_id: Option<&str>,
        body: Vec<u8>,
        timeout: Duration,
        fallback_deadline: Instant,
    ) -> Result<QuicResponse> {
        let request_deadline = Instant::now() + timeout;
        let mut retried_stale_session = false;
        loop {
            let session_timeout = remaining_budget(request_deadline).ok_or_else(|| {
                anyhow!(QuicDeliveryError::new(
                    "HTTP/3 transport deadline expired before the handshake",
                    true,
                ))
            })?;
            let (session, reused) = match tokio::time::timeout(
                session_timeout,
                self.session(key, peer_id, advertised, fallback_deadline),
            )
            .await
            {
                Ok(result) => {
                    result.map_err(|error| anyhow!(QuicDeliveryError::new(error, true)))?
                }
                Err(_) => {
                    return Err(anyhow!(QuicDeliveryError::new(
                        "HTTP/3 transport deadline expired during the handshake",
                        true,
                    )))
                }
            };
            let result = self
                .request_on_session(
                    &session,
                    base_url,
                    path,
                    content_type,
                    authorization.clone(),
                    session_id,
                    body.clone(),
                    timeout,
                    request_deadline,
                    fallback_deadline,
                )
                .await;
            if result.is_ok() {
                return result;
            }
            self.discard_session(key, &session).await;
            let before_body = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<QuicDeliveryError>())
                .is_some_and(|failure| failure.before_body);
            if reused && before_body && !retried_stale_session {
                // A peer restart invalidates the cached connection, not the
                // endpoint. Retry once immediately with a new handshake so a
                // transient reset never reaches the inference fallback.
                retried_stale_session = true;
                continue;
            }
            return result;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_on_session(
        &self,
        session: &Arc<QuicSession>,
        base_url: &str,
        path: &str,
        content_type: &str,
        authorization: Option<String>,
        session_id: Option<&str>,
        body: Vec<u8>,
        timeout: Duration,
        request_deadline: Instant,
        fallback_deadline: Instant,
    ) -> Result<QuicResponse> {
        let operation_timeout = remaining_budget(request_deadline).ok_or_else(|| {
            anyhow!(QuicDeliveryError::new(
                "HTTP/3 transport deadline expired before the stream",
                true,
            ))
        })?;
        let mut body_may_have_been_sent = false;
        let response = tokio::time::timeout(operation_timeout, async {
            let mut request_builder = Request::builder()
                .method("POST")
                .uri(format!("https://{}{}", authority(base_url)?, path))
                .header("content-type", content_type)
                .header("host", authority(base_url)?)
                .header("x-pom-internal-node", "1");
            if let Some(token) = authorization {
                request_builder =
                    request_builder.header("authorization", format!("Bearer {token}"));
            }
            if let Some(id) = session_id.filter(|id| !id.is_empty()) {
                request_builder = request_builder.header(crate::auth::INFERENCE_SESSION_HEADER, id);
            }
            let request = request_builder.body(())?;
            let mut sender = session.sender.lock().await.clone();
            // Abrir o stream é controle: um RTT num link vivo. Passou de
            // `QUIC_ESTABLISH_TIMEOUT` => caminho UDP morto, aborta já para o
            // chamador cair no próximo transporte sem penalidade de espera.
            let stream_timeout = remaining_budget(fallback_deadline)
                .ok_or_else(|| anyhow!("QUIC discovery budget exhausted before the stream"))?
                .min(QUIC_ESTABLISH_TIMEOUT);
            let mut stream = tokio::time::timeout(stream_timeout, sender.send_request(request))
                .await
                .context("HTTP/3 stream open did not complete within the RTT budget")??;
            // Marcar antes de iniciar `send_data`: em uma falha daqui em
            // diante o peer pode ter recebido parte ou todo o frame. O prazo
            // deriva do orçamento total restante, não de um limite fixo que
            // confunda upload de 5,2 MB com handshake.
            body_may_have_been_sent = true;
            let body_timeout = remaining_budget(request_deadline)
                .ok_or_else(|| anyhow!("deadline expired before sending the body"))?;
            let request_body_send_started = std::time::Instant::now();
            tokio::time::timeout(body_timeout, stream.send_data(Bytes::from(body)))
                .await
                .context("HTTP/3 body send stalled")?
                .map_err(|error| anyhow!("HTTP/3 body send failed: {error}"))?;
            let finish_timeout = remaining_budget(request_deadline)
                .ok_or_else(|| anyhow!("deadline expired while finishing the body"))?;
            tokio::time::timeout(finish_timeout, stream.finish())
                .await
                .context("HTTP/3 body finish stalled")?
                .map_err(|error| anyhow!("HTTP/3 body finish failed: {error}"))?;
            let request_body_send_us = request_body_send_started.elapsed().as_micros() as u64;
            let headers_started = std::time::Instant::now();
            // A espera da resposta cobre o processamento remoto (prefill/decode
            // legítimos passam de 1 s). Ela não tem timer próprio: o único
            // prazo é o `operation_timeout` que envolve a tentativa inteira, e
            // um peer que sumiu derruba a conexão por `QUIC_MAX_IDLE`.
            let response = stream
                .recv_response()
                .await
                .context("HTTP/3 response failed")?;
            let response_headers_ms = headers_started.elapsed().as_millis() as u64;
            let body_started = std::time::Instant::now();
            let mut output = Vec::new();
            while let Some(mut chunk) = stream.recv_data().await.context("HTTP/3 body failed")? {
                while chunk.has_remaining() {
                    let take = chunk.remaining();
                    output.extend_from_slice(&chunk.copy_to_bytes(take));
                }
            }
            Ok::<QuicResponse, anyhow::Error>(QuicResponse {
                status: response.status(),
                body: output,
                request_body_send_us,
                response_headers_ms,
                response_body_ms: body_started.elapsed().as_millis() as u64,
                transport_rtt_us: session.connection.rtt().as_micros() as u64,
            })
        })
        .await;
        match response {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(anyhow!(QuicDeliveryError::new(
                error,
                !body_may_have_been_sent,
            ))),
            Err(_) => Err(anyhow!(QuicDeliveryError::new(
                format!("HTTP/3 transport deadline expired after {:?}", timeout),
                !body_may_have_been_sent,
            ))),
        }
    }

    async fn discard_session(&self, key: &str, session: &Arc<QuicSession>) {
        session.connection.close(0u32.into(), b"transport failure");
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(key);
        }
    }

    async fn session(
        &self,
        key: &str,
        peer_id: &str,
        advertised: &str,
        fallback_deadline: Instant,
    ) -> Result<(Arc<QuicSession>, bool)> {
        if let Some(session) = self.sessions.lock().await.get(key).cloned() {
            return Ok((session, true));
        }
        let resolve_timeout = remaining_budget(fallback_deadline)
            .ok_or_else(|| anyhow!("QUIC discovery budget exhausted before DNS"))?;
        let addr = tokio::time::timeout(resolve_timeout, resolve_addr(advertised))
            .await
            .context("QUIC address resolution exceeded the budget")??;
        ensure_local_route(addr)?;
        let tls = crate::tls::quic_client_config(peer_id)?;
        let mut config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
        ));
        // Mesmas janelas largas do servidor: o lado que puxa a resposta do
        // prefill também recebe alguns MB de estado oculto.
        config.transport_config(Arc::new(inference_transport_config()));
        let endpoint = self.endpoint().await?;
        // O handshake QUIC é 1-RTT (0-RTT quando há ticket). Um orçamento de
        // sub-segundo é o que transforma "peer QUIC inalcançável" em um
        // fallback imediato para TCP, em vez de segurar o hop de inferência.
        let establish_timeout = remaining_budget(fallback_deadline)
            .ok_or_else(|| anyhow!("QUIC discovery budget exhausted before the handshake"))?;
        let connection = tokio::time::timeout(
            establish_timeout.min(QUIC_ESTABLISH_TIMEOUT),
            endpoint
                // The certificate verifier pins the full node_id. SNI only
                // needs a valid DNS label; a 64-character hex node_id can
                // exceed the single-label limit, so it is not used here.
                .connect_with(config, addr, &format!("peer.{}", crate::brand::slug()))?,
        )
        .await
        .context("QUIC handshake did not complete within the RTT budget")??;
        let connection_for_metrics = connection.clone();
        let (mut driver, sender) = h3::client::new(H3Connection::new(connection)).await?;
        tokio::spawn(async move {
            let _ = poll_fn(|cx| driver.poll_close(cx)).await;
        });
        let session = Arc::new(QuicSession {
            sender: tokio::sync::Mutex::new(sender),
            connection: connection_for_metrics,
        });
        self.sessions
            .lock()
            .await
            .insert(key.to_string(), session.clone());
        Ok((session, false))
    }

    /// `Some(motivo)` quando `key` está em quarentena - o motivo é o erro
    /// original que causou a quarentena, não o texto "em quarentena".
    async fn quic_quarantine_reason(&self, key: &str) -> Option<String> {
        let now = Instant::now();
        let mut failed_until = self.failed_until.lock().await;
        match failed_until.get(key) {
            Some((until, reason)) if *until > now => Some(reason.clone()),
            Some(_) => {
                failed_until.remove(key);
                None
            }
            None => None,
        }
    }

    async fn quarantine_quic(&self, key: &str, reason: String) {
        self.failed_until.lock().await.insert(
            key.to_owned(),
            (Instant::now() + QUIC_FAILURE_QUARANTINE, reason),
        );
    }
}

fn remaining_budget(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then_some(remaining)
}

pub fn server_config(id: &crate::identity::Identity) -> Result<Arc<quinn::ServerConfig>> {
    crate::tls::install_crypto_provider();
    let certificate = crate::tls::node_certificate(id)?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![certificate.cert], certificate.key)?;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
    ));
    config.transport = Arc::new(inference_transport_config());
    Ok(Arc::new(config))
}

/// Janelas de controle de fluxo largas o suficiente para o handoff de prefill.
///
/// O corpo desse hop carrega o estado oculto de todo o prompt (centenas de
/// posições x hidden_dim x 2 bytes = alguns MB). Com a janela padrão do quinn
/// (~1 MiB por stream) um prompt real fica gaguejando à espera de crédito de
/// janela, o que parecia "pendurado" mesmo antes de o timeout de `send_data`
/// existir. `receive_window` cobre a conexão inteira e `stream_receive_window`
/// cada stream; o `send_window` casa o lado que escreve.
pub(crate) fn inference_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(128u32.into());
    transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    transport.send_window(64 * 1024 * 1024);
    // O quinn deixa `max_idle_timeout` desligado por padrão: uma conexão que
    // vira buraco negro (peer sumiu, caminho UDP morto no meio de um handoff)
    // nunca erra e o hop fica preso até o timeout de 600 s do frame. Com um
    // idle timeout curto e keep-alive, um caminho morto falha em segundos e o
    // `send_frame_with_protocol` cai para TCP.
    transport.max_idle_timeout(Some(
        QUIC_MAX_IDLE
            .try_into()
            .expect("QUIC_MAX_IDLE fits in VarInt"),
    ));
    transport.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
    transport
}

pub async fn serve(
    port: u16,
    router: axum::Router,
    config: Arc<quinn::ServerConfig>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let socket = bind_quic_socket(format!("[::]:{port}").parse()?)?;
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow!("async QUIC runtime is not available"))?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(config.as_ref().clone()),
        socket,
        runtime,
    )?;
    info!(port, "endpoint QUIC HTTP/3 bound on [::]:{port}");
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                endpoint.close(0u32.into(), b"shutdown");
                endpoint.wait_idle().await;
                return Ok(());
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { return Ok(()); };
                let router = router.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            if let Err(error) = serve_connection(connection, router).await {
                                debug!("HTTP/3 connection closed: {error}");
                            }
                        }
                        Err(error) => debug!("QUIC handshake refused: {error}"),
                    }
                });
            }
        }
    }
}

async fn serve_connection(connection: quinn::Connection, router: axum::Router) -> Result<()> {
    let remote = connection.remote_address();
    let mut incoming = h3::server::builder()
        .build(H3Connection::new(connection))
        .await?;
    while let Some(resolver) = incoming.accept().await? {
        let router = router.clone();
        tokio::spawn(async move {
            let Ok((request, stream)) = resolver.resolve_request().await else {
                return;
            };
            let (send, recv) = stream.split();
            let (parts, ()) = request.into_parts();
            let body = Body::from_stream(futures::stream::unfold(recv, |mut stream| async move {
                match stream.recv_data().await {
                    Ok(Some(mut data)) => {
                        let bytes = data.copy_to_bytes(data.remaining());
                        Some((Ok::<Bytes, std::io::Error>(bytes), stream))
                    }
                    Ok(None) | Err(_) => None,
                }
            }));
            let mut request = http::Request::from_parts(parts, body);
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr(remote));
            request
                .extensions_mut()
                .insert(crate::serve::TransportSecurity(true));
            let mut router = router;
            let response = match tower::Service::call(&mut router, request).await {
                Ok(response) => response,
                Err(error) => {
                    let mut send = send;
                    let _ = send
                        .send_response(http::Response::builder().status(500).body(()).unwrap())
                        .await;
                    let _ = send.send_data(Bytes::from(error.to_string())).await;
                    let _ = send.finish().await;
                    return;
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let mut send = send;
            let mut head = http::Response::builder().status(status);
            *head.headers_mut().unwrap() = headers;
            if send.send_response(head.body(()).unwrap()).await.is_err() {
                return;
            }
            let mut body = response.into_body().into_data_stream();
            while let Some(Ok(chunk)) = body.next().await {
                if send.send_data(chunk).await.is_err() {
                    return;
                }
            }
            let _ = send.finish().await;
        });
    }
    Ok(())
}

fn authority(base_url: &str) -> Result<String> {
    let parsed = url::Url::parse(base_url)?;
    let host = parsed.host().ok_or_else(|| anyhow!("QUIC URL without a host"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("QUIC URL without a port"))?;
    // `Url::host_str()` já devolve hosts IPv6 colchetados (`[::1]`); só
    // `Url::host()` distingue a variante, então colchetar de novo em cima de
    // `host_str()` sempre que ele "contém `:`" duplicava os colchetes em
    // qualquer host IPv6 (`[[::1]]`) e o `http::Uri`/`Authority` rejeitava a
    // string resultante como "invalid authority" - o motivo dominante dos
    // fallbacks QUIC->HTTP num cluster IPv6. `url::Host` não tem essa
    // ambiguidade: `Ipv6` já é o endereço nu.
    Ok(match host {
        url::Host::Ipv6(addr) => format!("[{addr}]:{port}"),
        url::Host::Ipv4(addr) => format!("{addr}:{port}"),
        url::Host::Domain(domain) => format!("{domain}:{port}"),
    })
}

/// Falha na hora quando esta máquina não tem rota para a família do destino
/// (um container sem IPv6 discando um IPv6 anunciado). O quinn só registra o
/// `sendmsg` recusado e espera o orçamento de handshake inteiro; um `connect`
/// de UDP consulta a tabela de rotas sem mandar pacote nenhum.
fn ensure_local_route(addr: SocketAddr) -> Result<()> {
    let bind: SocketAddr = if addr.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = std::net::UdpSocket::bind(bind)
        .map_err(|error| anyhow!("no local socket for {addr}: {error}"))?;
    socket
        .connect(addr)
        .map_err(|error| anyhow!("no local route to {addr}: {error}"))
}

async fn resolve_addr(advertised: &str) -> Result<SocketAddr> {
    if advertised.contains("://") {
        return Err(anyhow!(
            "QUIC address must be host:port, not an HTTP URL"
        ));
    }
    if let Ok(addr) = advertised.parse::<SocketAddr>() {
        return quic_socket_addr(advertised, addr.port());
    }
    lookup_host(advertised)
        .await?
        .next()
        .ok_or_else(|| anyhow!("QUIC address did not resolve"))
}

fn quic_socket_addr(advertised_host: &str, quic_port: u16) -> Result<SocketAddr> {
    if advertised_host.contains("://") {
        return Err(anyhow!("announced address cannot contain a scheme"));
    }
    let host = advertised_host
        .parse::<SocketAddr>()
        .map(|addr| addr.ip())
        .or_else(|_| {
            advertised_host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
        })?;
    Ok(SocketAddr::new(host, quic_port))
}

/// Bind the server socket with IPv6 dual-stack explicitly enabled.
///
/// Quinn's client constructor already applies `IPV6_V6ONLY=0` for an IPv6
/// wildcard, but `Endpoint::server` delegates to `std::net::UdpSocket::bind`.
/// Windows leaves that socket IPv6-only by default, so an endpoint bound to
/// `[::]` would silently reject every IPv4 peer. Keeping the socket family as
/// IPv6 and clearing `only_v6` preserves both IPv6 and IPv4-mapped traffic on
/// every platform.
fn bind_quic_socket(addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use tokio::sync::oneshot;

    #[test]
    fn quic_ports_derive_from_the_p2p_port_and_do_not_overflow() {
        assert_eq!(gateway_quic_port(42777), Some(44777));
        assert_eq!(inference_quic_port(42777), Some(43777));
        // p2p efêmero ou offset que estoura u16 => QUIC desligado, não colisão.
        assert_eq!(gateway_quic_port(0), None);
        assert_eq!(inference_quic_port(0), None);
        assert_eq!(gateway_quic_port(u16::MAX - 10), None);
    }

    /// Cenário da ksia: 4 nós co-locados no mesmo namespace (host networking),
    /// `p2p_port` 42777..=42780, gateways 8080..=8083, inferências 8180..=8183.
    /// Nenhuma porta QUIC pode cair sobre a gateway_port/inference_port/p2p_port
    /// de outro nó - era isso que `gateway_port + 1` fazia.
    #[test]
    fn four_colocated_nodes_have_no_port_collision() {
        let mut all = Vec::new();
        for i in 0..4u16 {
            let p2p = 42777 + i;
            let gateway = 8080 + i;
            let inference = 8180 + i;
            all.push(("p2p", p2p));
            all.push(("gateway", gateway));
            all.push(("inference", inference));
            all.push(("gateway_quic", gateway_quic_port(p2p).unwrap()));
            all.push(("inference_quic", inference_quic_port(p2p).unwrap()));
        }
        for a in 0..all.len() {
            for b in (a + 1)..all.len() {
                assert_ne!(
                    all[a].1, all[b].1,
                    "port collision between {} and {} ({})",
                    all[a].0, all[b].0, all[a].1
                );
            }
        }
    }

    #[test]
    fn a_quic_endpoint_derives_the_socketaddr_from_the_announce_and_not_from_an_http_url() {
        assert_eq!(
            quic_socket_addr("[2001:db8::42]:42777", 8182).unwrap(),
            "[2001:db8::42]:8182".parse().unwrap()
        );
        assert_eq!(
            quic_socket_addr("2001:db8::42", 8182).unwrap(),
            "[2001:db8::42]:8182".parse().unwrap()
        );
        assert!(quic_socket_addr("http://[2001:db8::42]:8181", 8182).is_err());
    }

    /// Regressão do bug de produção salem->ksia: `Url::host_str()` já devolve
    /// hosts IPv6 colchetados, então colchetar de novo produzia
    /// `[[::]]:porta`, que o `http::Uri`/`Authority` rejeita como "invalid
    /// authority" - a causa raiz dos 114 fallbacks QUIC->HTTP na telemetria.
    #[test]
    fn authority_does_not_bracket_an_ipv6_host_twice() {
        let a = authority("https://[2804:d55:5579:1000:e6c8:310f:1a19:6e92]:43777").unwrap();
        assert_eq!(a, "[2804:d55:5579:1000:e6c8:310f:1a19:6e92]:43777");
        a.parse::<http::uri::Authority>()
            .expect("authority must parse as a valid Authority");

        // IPv4 e hostname DNS continuam funcionando (casos que já passavam).
        assert_eq!(
            authority("https://192.168.1.5:8181").unwrap(),
            "192.168.1.5:8181"
        );
        assert_eq!(
            authority("https://peer.example.com:8181").unwrap(),
            "peer.example.com:8181"
        );
        assert_eq!(
            authority("https://peer.example.com").unwrap(),
            "peer.example.com:443"
        );
    }

    #[test]
    fn an_ipv6_wildcard_server_socket_accepts_ipv4() {
        let socket = bind_quic_socket("[::]:0".parse().unwrap()).unwrap();
        assert!(
            !socket2::SockRef::from(&socket).only_v6().unwrap(),
            "the server QUIC socket must be dual-stack"
        );
    }

    #[test]
    fn quic_attempts_share_a_budget_before_the_tcp_fallback() {
        let mut remaining = QUIC_FALLBACK_BUDGET;
        let mut spent = Duration::ZERO;
        let mut attempts = 0;
        while let Some(timeout) =
            (!remaining.is_zero()).then_some(remaining.min(QUIC_ESTABLISH_TIMEOUT))
        {
            attempts += 1;
            spent += timeout;
            remaining = remaining.saturating_sub(timeout);
        }

        assert_eq!(
            attempts, 1,
            "a dead endpoint must not serialize timeouts"
        );
        assert_eq!(spent, QUIC_FALLBACK_BUDGET);
        assert!(spent < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn http3_client_and_server_talk_using_the_node_certificate() {
        let identity = crate::identity::Identity::generate();
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let router = Router::new().route(
            "/test",
            post(|body: Bytes| async move { (StatusCode::OK, body) }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let server_config = server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            serve(port, router, server_config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let pool = ClientPool::new();
        let response = pool
            .request(
                &identity.node_id.0,
                &format!("http://127.0.0.1:{port}"),
                &[format!("127.0.0.1:{port}")],
                "/test",
                "application/octet-stream",
                None,
                None,
                b"http3".to_vec(),
                Duration::from_secs(3),
            )
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"http3");
        assert!(
            response.transport_rtt_us < 100_000,
            "warm QUIC path should report low transport RTT: {} us",
            response.transport_rtt_us
        );
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_client_reconnects_quic_after_the_peer_restart() {
        let identity = crate::identity::Identity::generate();
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let router = Router::new().route(
            "/restart",
            post(|body: Bytes| async move { (StatusCode::OK, body) }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            serve(port, router, config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let pool = ClientPool::new();
        let peer_id = identity.node_id.0.clone();
        async fn request(pool: &ClientPool, peer_id: &str, port: u16) -> Result<QuicResponse> {
            pool.request(
                peer_id,
                &format!("http://127.0.0.1:{port}"),
                &[format!("127.0.0.1:{port}")],
                "/restart",
                "application/octet-stream",
                None,
                None,
                b"before-restart".to_vec(),
                Duration::from_secs(3),
            )
            .await
        }

        request(&pool, &peer_id, port).await.unwrap();
        stop_tx.send(()).unwrap();
        server.await.unwrap();

        let router = Router::new().route(
            "/restart",
            post(|body: Bytes| async move { (StatusCode::OK, body) }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let config = server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            serve(port, router, config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let result = request(&pool, &peer_id, port).await;
        stop_tx.send(()).unwrap();
        server.await.unwrap();

        let response = result.expect("a remote restart must force a new QUIC handshake");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"before-restart");
    }

    /// Um endpoint QUIC pode completar o handshake e aceitar o stream sem
    /// produzir headers. A tentativa termina no prazo do frame, com erro
    /// tipado de deadline de transporte, e não há outro timer aninhado nela.
    #[tokio::test]
    async fn a_reachable_unanswering_quic_endpoint_honors_the_transport_deadline() {
        let identity = crate::identity::Identity::generate();
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let router = Router::new().route(
            "/never",
            post(|_body: Bytes| async { std::future::pending::<(StatusCode, Bytes)>().await }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let server_config = server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            serve(port, router, server_config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let pool = ClientPool::new();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            pool.request(
                &identity.node_id.0,
                &format!("http://127.0.0.1:{port}"),
                &[format!("127.0.0.1:{port}")],
                "/never",
                "application/octet-stream",
                None,
                None,
                Vec::new(),
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("an unanswering HTTP/3 endpoint cannot exceed the frame deadline");
        let error = result.expect_err("an endpoint without headers must fail");
        assert!(
            error
                .to_string()
                .contains("HTTP/3 transport deadline expired"),
            "unexpected error: {error:#}"
        );

        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }
}
