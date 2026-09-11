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

/// Estabelecer a conexão/stream QUIC é caminho de controle: num link vivo
/// leva um RTT. Se não fechou nesse orçamento, o caminho UDP está morto e o
/// hop precisa cair para o próximo transporte **sem esperar**.
pub(crate) const QUIC_ESTABLISH_TIMEOUT: Duration = Duration::from_millis(500);

/// O orçamento total para descobrir que o endpoint QUIC não está disponível.
/// Os endereços anunciados são alternativas, não tentativas independentes:
/// testar três deles não pode transformar um fallback TCP em três timeouts.
pub(crate) const QUIC_FALLBACK_BUDGET: Duration = Duration::from_millis(300);

/// Teto de uma tentativa completa de transporte, incluindo o handshake, o
/// envio do frame e a resposta. O timeout de processamento do frame pode ser
/// maior, mas não deve deixar um endpoint que aceitou a conexão e sumiu preso
/// por dez minutos.
#[cfg(not(test))]
pub(crate) const INFERENCE_TRANSPORT_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(test)]
pub(crate) const INFERENCE_TRANSPORT_DEADLINE: Duration = Duration::from_millis(500);

pub(crate) fn transport_deadline(timeout: Duration) -> Duration {
    timeout.min(INFERENCE_TRANSPORT_DEADLINE)
}

/// Uma falha de descoberta torna inútil repetir o mesmo handshake no próximo
/// token. O caminho HTTP continua disponível imediatamente, e o QUIC volta a
/// ser sondado depois desta janela para recuperar uma porta que foi reiniciada.
const QUIC_FAILURE_QUARANTINE: Duration = Duration::from_secs(5);

/// Empurrar o corpo (o estado oculto do prefill são alguns MB) é o único
/// trecho de envio que pode legitimamente demorar mais que um RTT. Ainda
/// assim, num link são drena em dezenas de ms; 1,5 s é teto de "algo travou".
pub(crate) const QUIC_SEND_TIMEOUT: Duration = Duration::from_millis(1500);

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
    failed_until: Arc<tokio::sync::Mutex<HashMap<String, Instant>>>,
}

impl ClientPool {
    pub fn new() -> Self {
        Self {
            endpoint: Arc::new(tokio::sync::OnceCell::const_new()),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            failed_until: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
        let mut last_error = anyhow!("nenhum endereço QUIC anunciado para {peer_id}");
        let fallback_deadline = Instant::now() + QUIC_FALLBACK_BUDGET;
        for advertised in quic_addresses {
            let key = format!("{peer_id}@{}", advertised.trim());
            if self.is_quic_quarantined(&key).await {
                last_error = anyhow!("{advertised}: endpoint QUIC em quarentena");
                continue;
            }
            if remaining_budget(fallback_deadline).is_none() {
                last_error = anyhow!("orçamento de descoberta QUIC esgotado");
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
                    last_error = anyhow!("{advertised}: {error}");
                    self.sessions.lock().await.remove(&key);
                    self.quarantine_quic(&key).await;
                }
            }
        }
        Err(last_error)
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
        let transport_timeout = transport_deadline(timeout);
        let request_deadline = Instant::now() + transport_timeout;
        let session_timeout = remaining_budget(request_deadline)
            .ok_or_else(|| anyhow!("deadline de transporte HTTP/3 expirou antes do handshake"))?;
        let session = match tokio::time::timeout(
            session_timeout,
            self.session(key, peer_id, advertised, fallback_deadline),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow!(
                    "deadline de transporte HTTP/3 expirou durante o handshake"
                ))
            }
        };
        let operation_timeout = remaining_budget(request_deadline)
            .ok_or_else(|| anyhow!("deadline de transporte HTTP/3 expirou antes do stream"))?;
        let result: Result<QuicResponse> = match tokio::time::timeout(operation_timeout, async {
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
                .ok_or_else(|| anyhow!("orçamento de descoberta QUIC esgotado antes do stream"))?;
            let mut stream = tokio::time::timeout(stream_timeout, sender.send_request(request))
                .await
                .context("abertura do stream HTTP/3 não fechou no orçamento de RTT")??;
            // `send_data`/`finish` empurram o corpo (o estado oculto do prefill
            // são alguns MB). Num link são drena em dezenas de ms; passou de
            // `QUIC_SEND_TIMEOUT` => travou, cai para TCP.
            tokio::time::timeout(QUIC_SEND_TIMEOUT, stream.send_data(Bytes::from(body)))
                .await
                .context("envio do corpo HTTP/3 travou")?
                .map_err(|error| anyhow!("envio do corpo HTTP/3 falhou: {error}"))?;
            tokio::time::timeout(QUIC_SEND_TIMEOUT, stream.finish())
                .await
                .context("finalização do corpo HTTP/3 travou")?
                .map_err(|error| anyhow!("finalização do corpo HTTP/3 falhou: {error}"))?;
            let headers_started = std::time::Instant::now();
            // A espera da resposta cobre o processamento remoto (prefill/decode
            // legítimos passam de 1 s), mas a tentativa inteira já está dentro
            // de `INFERENCE_TRANSPORT_DEADLINE`. Assim, um endpoint que aceitou
            // o stream e não produz headers não fica preso ao timeout de 600 s
            // do frame.
            let response = tokio::time::timeout(timeout, stream.recv_response())
                .await
                .context("resposta HTTP/3 expirou")??;
            let response_headers_ms = headers_started.elapsed().as_millis() as u64;
            let body_started = std::time::Instant::now();
            let mut output = Vec::new();
            while let Some(mut chunk) = tokio::time::timeout(timeout, stream.recv_data())
                .await
                .context("corpo HTTP/3 expirou")??
            {
                while chunk.has_remaining() {
                    let take = chunk.remaining();
                    output.extend_from_slice(&chunk.copy_to_bytes(take));
                }
            }
            Ok(QuicResponse {
                status: response.status(),
                body: output,
                response_headers_ms,
                response_body_ms: body_started.elapsed().as_millis() as u64,
                transport_rtt_us: session.connection.rtt().as_micros() as u64,
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!(
                "deadline de transporte HTTP/3 expirou após {:?}",
                transport_timeout
            )),
        };
        if result.is_err() {
            self.sessions.lock().await.remove(key);
        }
        result
    }

    async fn session(
        &self,
        key: &str,
        peer_id: &str,
        advertised: &str,
        fallback_deadline: Instant,
    ) -> Result<Arc<QuicSession>> {
        if let Some(session) = self.sessions.lock().await.get(key).cloned() {
            return Ok(session);
        }
        let resolve_timeout = remaining_budget(fallback_deadline)
            .ok_or_else(|| anyhow!("orçamento de descoberta QUIC esgotado antes do DNS"))?;
        let addr = tokio::time::timeout(resolve_timeout, resolve_addr(advertised))
            .await
            .context("resolução do endereço QUIC excedeu o orçamento")??;
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
            .ok_or_else(|| anyhow!("orçamento de descoberta QUIC esgotado antes do handshake"))?;
        let connection = tokio::time::timeout(
            establish_timeout.min(QUIC_ESTABLISH_TIMEOUT),
            endpoint
                // The certificate verifier pins the full node_id. SNI only
                // needs a valid DNS label; a 64-character hex node_id can
                // exceed the single-label limit, so it is not used here.
                .connect_with(config, addr, &format!("peer.{}", crate::brand::slug()))?,
        )
        .await
        .context("handshake QUIC não fechou no orçamento de RTT")??;
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
        Ok(session)
    }

    async fn is_quic_quarantined(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut failed_until = self.failed_until.lock().await;
        match failed_until.get(key).copied() {
            Some(until) if until > now => true,
            Some(_) => {
                failed_until.remove(key);
                false
            }
            None => false,
        }
    }

    async fn quarantine_quic(&self, key: &str) {
        self.failed_until
            .lock()
            .await
            .insert(key.to_owned(), Instant::now() + QUIC_FAILURE_QUARANTINE);
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
            .expect("QUIC_MAX_IDLE cabe em VarInt"),
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
    let endpoint =
        quinn::Endpoint::server(config.as_ref().clone(), format!("[::]:{port}").parse()?)?;
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
                                debug!("conexão HTTP/3 encerrada: {error}");
                            }
                        }
                        Err(error) => debug!("handshake QUIC recusado: {error}"),
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
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL QUIC sem host"))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("URL QUIC sem porta"))?;
    Ok(format!(
        "{}:{port}",
        if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        }
    ))
}

async fn resolve_addr(advertised: &str) -> Result<SocketAddr> {
    if advertised.contains("://") {
        return Err(anyhow!(
            "endereço QUIC precisa ser host:porta, não uma URL HTTP"
        ));
    }
    if let Ok(addr) = advertised.parse::<SocketAddr>() {
        return quic_socket_addr(advertised, addr.port());
    }
    lookup_host(advertised)
        .await?
        .next()
        .ok_or_else(|| anyhow!("endereço QUIC não resolveu"))
}

fn quic_socket_addr(advertised_host: &str, quic_port: u16) -> Result<SocketAddr> {
    if advertised_host.contains("://") {
        return Err(anyhow!("endereço anunciado não pode conter scheme"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use tokio::sync::oneshot;

    #[test]
    fn portas_quic_derivam_do_p2p_port_e_nao_estouram() {
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
    fn quatro_nos_colocados_nao_tem_colisao_de_porta() {
        let mut todas = Vec::new();
        for i in 0..4u16 {
            let p2p = 42777 + i;
            let gateway = 8080 + i;
            let inference = 8180 + i;
            todas.push(("p2p", p2p));
            todas.push(("gateway", gateway));
            todas.push(("inference", inference));
            todas.push(("gateway_quic", gateway_quic_port(p2p).unwrap()));
            todas.push(("inference_quic", inference_quic_port(p2p).unwrap()));
        }
        for a in 0..todas.len() {
            for b in (a + 1)..todas.len() {
                assert_ne!(
                    todas[a].1, todas[b].1,
                    "colisão de porta entre {} e {} ({})",
                    todas[a].0, todas[b].0, todas[a].1
                );
            }
        }
    }

    #[test]
    fn endpoint_quic_deriva_socketaddr_do_anuncio_e_nao_de_url_http() {
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

    #[test]
    fn tentativas_quic_compartilham_orcamento_antes_do_fallback_tcp() {
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
            "um endpoint morto não deve serializar timeouts"
        );
        assert_eq!(spent, QUIC_FALLBACK_BUDGET);
        assert!(spent < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn cliente_e_servidor_http3_falam_com_o_certificado_do_no() {
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

    /// Um endpoint QUIC pode completar o handshake e aceitar o stream sem
    /// produzir headers. Isso precisa ser uma falha de transporte limitada,
    /// não uma espera pelo timeout de processamento do frame.
    #[tokio::test]
    async fn endpoint_quic_alcancavel_sem_resposta_respeita_deadline_de_transporte() {
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
                Duration::from_secs(600),
            ),
        )
        .await
        .expect("endpoint HTTP/3 sem resposta não pode exceder o deadline");
        let error = result.expect_err("endpoint sem headers deve falhar");
        assert!(
            error
                .to_string()
                .contains("deadline de transporte HTTP/3 expirou"),
            "erro inesperado: {error:#}"
        );

        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }
}
