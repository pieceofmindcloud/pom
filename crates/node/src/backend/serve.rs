//! Um listener, duas falas.
//!
//! O servidor espia o primeiro byte da conexão sem consumi-lo (`peek`) e
//! decide: `0x16` é handshake TLS, qualquer outra coisa é HTTP em claro. As
//! duas caem no mesmo router.
//!
//! Por que uma porta só, em vez de uma porta TLS ao lado: nenhum anúncio muda,
//! nenhum campo novo entra no `PeerInfo` (que já está no limite da MTU do
//! `PeerList`), nenhuma regra de firewall nova - e, o que importa mais, **a
//! virada não precisa ser simultânea**. Enquanto uns peers já falam TLS e
//! outros ainda não, este servidor atende os dois.

use anyhow::Result;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tracing::{debug, warn};

/// Endereço real do socket que trouxe a requisição. A autenticação
/// administrativa usa isto para manter o modo localhost fora da rede, sem
/// confiar em cabeçalhos que o cliente pode forjar.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// Origem que vale para a autenticação: o pedido que o túnel libp2p entrega
/// pelo loopback, com a marca deste processo, passa a contar como remoto.
///
/// Sem isso, qualquer par do enxame que alcançasse uma rota pelo túnel seria
/// "localhost" para `AdminAuth` no modo localhost - administração sem senha.
/// O cabeçalho sai do pedido sempre, marcado ou não, para nenhuma rota vê-lo.
pub(crate) fn request_origin<B>(peer: SocketAddr, req: &mut hyper::Request<B>) -> SocketAddr {
    let Some(mark) = req
        .headers_mut()
        .remove(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
    else {
        return peer;
    };
    if crate::auth::is_loopback_addr(peer) && crate::infer_tunnel::is_tunnel_mark(mark.as_bytes()) {
        TUNNELED_ORIGIN
    } else {
        peer
    }
}

/// Endereço com que um pedido vindo do túnel aparece em `PeerAddr`: não é
/// loopback, e é tudo que a autenticação consulta.
pub(crate) const TUNNELED_ORIGIN: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    std::net::Ipv4Addr::UNSPECIFIED,
    0,
));

/// Whether the current request came through the TLS branch of the listener.
/// The admin session middleware uses the real transport state to decide if a
/// Secure cookie is safe, never a client-controlled header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransportSecurity(pub bool);

/// O que fazer com uma conexão em texto claro vinda de fora, com a política
/// ligada.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlainPolicy {
    /// Porta que também serve gente: responde 301 para `https://` na mesma
    /// porta, para o operador não ficar trancado para fora do painel.
    Redirect,
    /// Porta exclusivamente de peers (o servidor de inferência): a conexão é
    /// fechada. Não há navegador aqui para acomodar, e redirecionar tráfego de
    /// peer só trocaria uma falha clara por uma confusa.
    Refuse,
}

/// O que responder a uma requisição que chegou em claro quando o cluster exige
/// TLS.
#[derive(Debug, PartialEq)]
pub enum PlainAction {
    /// Manda o cliente refazer em `https://`.
    Redirect,
    /// Rota que só peer usa: nada a redirecionar, e o `reqwest` segue
    /// redirecionamento por padrão - devolver 301 aqui faria um peer antigo
    /// tentar TLS sem saber verificar nada e falhar com um erro que não diz o
    /// motivo. O 403 diz.
    Refuse,
}

/// Rotas que só existem entre peers, e que por isso nunca são redirecionadas.
pub fn plain_action(path: &str) -> PlainAction {
    const PEER_ONLY: [&str; 4] = [
        "/api/transfer/",
        "/v1/relay-result",
        "/v1/prefill",
        "/v1/step",
    ];
    if PEER_ONLY.iter().any(|p| path.starts_with(p)) {
        PlainAction::Refuse
    } else {
        PlainAction::Redirect
    }
}

/// Configuração do listener HTTP/3, independente da política TLS sobre TCP.
pub type QuicServerConfig = Arc<h3_quinn::quinn::ServerConfig>;

/// Deriva a configuração autoassinada do endpoint QUIC a partir da identidade
/// do nó. QUIC sempre leva TLS 1.3 próprio, mesmo quando o TCP fala HTTP puro.
pub fn quic_server_config(id: &crate::identity::Identity) -> Result<QuicServerConfig> {
    crate::quic::server_config(id)
}

/// O que o servidor precisa para falar TLS, e o estado que ele publica de volta.
#[derive(Clone)]
pub struct TlsServer {
    pub config: Arc<rustls::ServerConfig>,
    /// O que fazer com texto claro remoto quando a política está ligada.
    pub plain: PlainPolicy,
    /// Com `true`, texto claro de fora do loopback é recusado.
    pub strict: Arc<AtomicBool>,
    /// Duração do último handshake aceito, em ms. É daqui que sai o
    /// `tls_handshake_ms` da telemetria de inferência.
    pub last_handshake_ms: Arc<AtomicU64>,
}

impl TlsServer {
    /// Monta o estado do servidor a partir da identidade do nó.
    ///
    /// `sans` são os endereços por onde este nó é alcançado, e entram no
    /// certificado que o **navegador** recebe - sem eles o aviso do navegador
    /// seria "host errado", que faz o operador desconfiar da máquina em vez do
    /// certificado. Ver `tls::browser_certificate`.
    pub fn from_identity(
        id: &crate::identity::Identity,
        strict: bool,
        sans: &[String],
    ) -> Result<Self> {
        Ok(Self {
            config: crate::tls::server_config(id, sans)?,
            plain: PlainPolicy::Redirect,
            strict: Arc::new(AtomicBool::new(strict)),
            last_handshake_ms: Arc::new(AtomicU64::new(0)),
        })
    }

    /// A mesma configuração, para uma porta que só peers usam.
    pub fn peers_only(&self) -> Self {
        Self {
            plain: PlainPolicy::Refuse,
            ..self.clone()
        }
    }

    /// Liga ou desliga a recusa de texto claro remoto.
    pub fn set_strict(&self, on: bool) {
        self.strict.store(on, Ordering::Relaxed);
    }

    /// Duração do último handshake, para a telemetria.
    pub fn last_handshake_ms(&self) -> u64 {
        self.last_handshake_ms.load(Ordering::Relaxed)
    }
}

/// Endereços que entram no certificado do navegador.
///
/// Os locais da máquina, o endereço fixado pelo operador (se houver) e o
/// hostname. Loopback entra porque abrir o painel na própria máquina é o caso
/// mais comum de todos.
pub fn browser_sans(pinned: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = vec!["127.0.0.1".into(), "::1".into()];
    if let Some(p) = pinned {
        names.push(
            crate::network::ip_host(p)
                .trim_matches(['[', ']'])
                .to_string(),
        );
    }
    for addr in crate::addrs::filter_candidates(crate::addrs::local_addrs(), None) {
        names.push(
            crate::network::ip_host(&addr)
                .trim_matches(['[', ']'])
                .to_string(),
        );
    }
    if let Ok(h) = hostname_local() {
        names.push(h);
    }
    names.retain(|n| !n.is_empty());
    names.sort();
    names.dedup();
    names
}

fn hostname_local() -> Result<String> {
    let output = std::process::Command::new("hostname").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `0x16` é `ContentType::Handshake` do TLS; nenhum método HTTP começa assim.
pub fn wants_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

/// Texto claro é sempre aceito do loopback, e só de fora dele quando a flag
/// está desligada.
pub fn plain_allowed(peer: SocketAddr, strict: bool) -> bool {
    if !strict {
        return true;
    }
    match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// Serve `app` no `listener`, aceitando TLS e texto claro conforme a política.
///
/// `tls: None` é o comportamento anterior a esta feature, e é o que o `testkit`
/// e os servidores de teste em `127.0.0.1:0` usam.
#[cfg(test)]
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
) -> Result<()> {
    serve_with_shutdown(listener, app, tls, std::future::pending::<()>()).await
}

/// Quanto tempo uma conexão viva tem para terminar sozinha depois do sinal de
/// desligamento, antes de ser abortada.
const DRAIN_DEADLINE: Duration = Duration::from_secs(3);

/// O endpoint QUIC pode estar esperando uma conexão que nunca termina. O
/// supervisor não pode transformar esse caso em uma espera sem fim durante o
/// reload; depois do prazo, abortar a task derruba o endpoint e permite que a
/// geração nova seja criada. O valor é pouco maior que o idle timeout QUIC de
/// 1,2 s, com margem para o fechamento do endpoint.
const QUIC_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// Como `serve`, mas para de aceitar quando `shutdown` resolve e **encerra as
/// conexões que sobraram** em vez de deixá-las soltas.
///
/// A diferença importa para memória, não para higiene: cada conexão fica numa
/// task que carrega uma cópia do `Router`, e o `Router` do servidor de
/// inferência carrega os pesos do modelo. Antes disto as tasks eram
/// desgarradas (`tokio::spawn` sem dono), então uma conexão em keep-alive
/// parada — o pool do gateway deixa várias — segurava o motor vivo depois do
/// `undeploy` e a VRAM nunca voltava. Ver `LlmEngine::unload`.
#[cfg(test)]
pub(crate) async fn serve_with_shutdown(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_with_quic(listener, app, tls, None, None, shutdown).await
}

/// Como `serve_with_shutdown`, mas com um endpoint HTTP/3 independente da
/// configuração TLS sobre TCP. A porta de inferência usa esta variante para
/// que `tls_required = false` não impeça o QUIC.
pub async fn serve_with_quic(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
    quic: Option<QuicServerConfig>,
    quic_port: Option<u16>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // This is a one-shot lifecycle signal, not a broadcast event. Unlike
    // `Notify::notify_waiters`, a oneshot keeps the value when the QUIC task
    // has not reached its first poll yet.
    let (quic_shutdown_tx, quic_shutdown_rx) = tokio::sync::oneshot::channel();
    let quic_task = quic.and_then(|config| {
        let port = quic_port?;
        let router = app.clone();
        Some(tokio::spawn(async move {
            let result = crate::quic::serve(port, router, config, async move {
                let _ = quic_shutdown_rx.await;
            })
            .await;
            if let Err(error) = &result {
                warn!(
                    port,
                    "HTTP/3 endpoint unavailable, keeping the TCP fallback: {error}"
                );
            }
            result
        }))
    });
    let acceptor = tls
        .as_ref()
        .map(|t| tokio_rustls::TlsAcceptor::from(t.config.clone()));

    let mut conns = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    let reason = loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break "shutdown requested",
            // Conexões já encerradas saem do conjunto aqui; sem isto o
            // `JoinSet` cresceria por toda a vida do servidor.
            Some(_) = conns.join_next(), if !conns.is_empty() => continue,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept failed: {e}");
                    continue;
                }
            },
        };
        // Nagle segura a escrita pequena até o ACK anterior; no SSE cada token é
        // uma escrita pequena. Mesma razão do `tcp_nodelay(true)` que estava no
        // `axum::serve`.
        let _ = stream.set_nodelay(true);

        let app = app.clone();
        let acceptor = acceptor.clone();
        let tls = tls.clone();

        conns.spawn(async move {
            if let Err(e) = handle_conn(stream, peer, app, acceptor, tls).await {
                debug!("connection from {peer} closed: {e}");
            }
        });
    };

    // Prazo para quem está no meio de uma resposta; depois dele, aborta. Quem
    // está parada em keep-alive não termina sozinha nunca, e é justamente ela
    // que segurava a memória.
    debug!("{reason}: draining {} connections", conns.len());
    let drain = async { while conns.join_next().await.is_some() {} };
    if tokio::time::timeout(DRAIN_DEADLINE, drain)
        .await
        .is_err()
    {
        warn!("connections still alive after {DRAIN_DEADLINE:?}; closing");
        conns.shutdown().await;
    }
    let _ = quic_shutdown_tx.send(());
    if let Some(mut task) = quic_task {
        match tokio::time::timeout(QUIC_SHUTDOWN_DEADLINE, &mut task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                warn!("HTTP/3 endpoint unavailable, keeping the TCP fallback: {error}")
            }
            Ok(Err(error)) => warn!("HTTP/3 task ended unexpectedly: {error}"),
            Err(_) => {
                warn!("HTTP/3 endpoint did not close in {QUIC_SHUTDOWN_DEADLINE:?}; aborting the task");
                task.abort();
                let _ = task.await;
            }
        }
    }
    Ok(())
}

async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    app: Router,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    tls: Option<TlsServer>,
) -> Result<()> {
    let mut first = [0u8; 1];
    // `peek` não consome: quem for servir a conexão ainda vê o byte.
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    let is_tls = wants_tls(first[0]);
    let strict = tls
        .as_ref()
        .map(|t| t.strict.load(Ordering::Relaxed))
        .unwrap_or(false);

    if !is_tls && !plain_allowed(peer, strict) {
        let policy = tls.as_ref().map(|t| t.plain).unwrap_or(PlainPolicy::Refuse);
        if policy == PlainPolicy::Refuse {
            warn!("refused plain text connection from {peer}: the cluster is on TLS");
            return Ok(());
        }
        // Porta que também serve gente: em vez de fechar na cara do operador,
        // manda o navegador refazer em https. Ver `plain_action` para o que
        // continua sendo recusado.
        debug!("redirecting the plain connection from {peer} to https");
        let svc = hyper::service::service_fn(redirect_to_https);
        return Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), svc)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }

    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut app = app.clone();
        async move {
            let mut req = req;
            let origin = request_origin(peer, &mut req);
            req.extensions_mut().insert(PeerAddr(origin));
            req.extensions_mut().insert(TransportSecurity(is_tls));
            Service::call(&mut app, req).await
        }
    });

    match (is_tls, acceptor) {
        (true, Some(acceptor)) => {
            let started = Instant::now();
            let tls_stream = acceptor.accept(stream).await?;
            if let Some(t) = &tls {
                t.last_handshake_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls_stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        (true, None) => {
            // Cliente falou TLS com um nó que não tem certificado. Não há o que
            // servir, e deixar a conexão pendurada esconderia o motivo.
            warn!("{peer} tried TLS but this node has no TLS configured");
        }
        (false, _) => {
            Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(())
}

/// A resposta para quem falou em claro numa porta que exige TLS, decidida sem
/// tocar em socket nenhum: `(status, Location)`.
///
/// O destino é a **mesma porta** - o servidor atende as duas falas nela. O host
/// vem do cabeçalho porque é o nome que o operador digitou; reconstruí-lo a
/// partir do socket daria o IP, e trocaria o nome dele por um endereço que pode
/// nem estar no certificado.
pub fn plain_reply(host: &str, path_and_query: &str) -> (u16, Option<String>) {
    let path = path_and_query.split('?').next().unwrap_or("/");
    if plain_action(path) == PlainAction::Refuse {
        return (403, None);
    }
    if host.is_empty() {
        // Sem `Host` não há para onde mandar, e um `Location` quebrado é pior
        // que uma recusa: o cliente segue e falha longe daqui.
        return (403, None);
    }
    (301, Some(format!("https://{host}{path_and_query}")))
}

/// Resposta para quem falou em claro numa porta que exige TLS.
async fn redirect_to_https(
    req: hyper::Request<hyper::body::Incoming>,
) -> std::result::Result<hyper::Response<String>, std::convert::Infallible> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let (status, location) = plain_reply(&host, &target);
    let mut response = hyper::Response::builder().status(status);
    if let Some(l) = &location {
        response = response.header(hyper::header::LOCATION, l);
    }
    let body = if location.is_some() {
        String::new()
    } else {
        "this cluster requires TLS between nodes; repeat the call over https\n".to_string()
    };
    Ok(response.body(body).expect("response is constructible"))
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::identity::Identity;
    use axum::{
        body::Bytes,
        extract::State,
        http::StatusCode,
        routing::{get, post},
        Router,
    };
    use std::time::Duration;

    async fn start_server(id: &Identity, strict: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let tls = TlsServer::from_identity(id, strict, &["127.0.0.1".to_string()]).unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        addr
    }

    fn clients() -> crate::peer_client::PeerClients {
        let c = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        c.set_tls(true);
        c
    }

    /// O caminho feliz inteiro: certificado derivado da identidade, cliente que
    /// fixa o node_id, handshake, resposta.
    #[tokio::test]
    async fn a_pinned_client_talks_to_the_server() {
        let id = Identity::generate();
        let addr = start_server(&id, false).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the handshake had to complete");
        assert!(r.status().is_success());
    }

    /// HTTP/3 precisa continuar disponível quando o TCP da inferência está em
    /// HTTP puro. O endpoint recebe sua própria configuração TLS 1.3 e não
    /// pode depender do `TlsServer` nem de `tls_required`.
    #[tokio::test]
    async fn quic_starts_without_a_tcp_tls_configuration() {
        let identity = Identity::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/test",
            post(|body: Bytes| async move { (StatusCode::OK, body) }),
        );
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let quic = quic_server_config(&identity).unwrap();
        let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let quic_port = udp_probe.local_addr().unwrap().port();
        drop(udp_probe);
        let server = tokio::spawn(async move {
            serve_with_quic(
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

        let response = crate::quic::ClientPool::new()
            .request(
                &identity.node_id.0,
                &format!("http://127.0.0.1:{port}"),
                &[format!("127.0.0.1:{quic_port}")],
                "/test",
                "application/octet-stream",
                None,
                None,
                b"http3-without-tcp-tls".to_vec(),
                Duration::from_secs(3),
            )
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"http3-without-tcp-tls");
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    /// Dois peers podem estar ociosos e receber o wake-up ao mesmo tempo. Uma
    /// requisição QUIC pendurada em cada um não pode transformar o shutdown da
    /// geração anterior em uma espera sem fim.
    #[tokio::test]
    async fn two_idle_peers_can_close_quic_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct RequestsInFlight {
            started: Arc<AtomicUsize>,
            all_started: Arc<Notify>,
        }

        async fn hold_request(
            State(state): State<RequestsInFlight>,
            _body: Bytes,
        ) -> std::convert::Infallible {
            if state.started.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                state.all_started.notify_one();
            }
            std::future::pending::<()>().await;
            unreachable!()
        }

        async fn spawn_peer(
            identity: Identity,
            state: RequestsInFlight,
        ) -> (
            tokio::sync::oneshot::Sender<()>,
            tokio::task::JoinHandle<Result<()>>,
            u16,
            u16,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_port = listener.local_addr().unwrap().port();
            let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let quic_port = udp_probe.local_addr().unwrap().port();
            drop(udp_probe);
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let quic = quic_server_config(&identity).unwrap();
            let app = Router::new()
                .route("/hold", post(hold_request))
                .with_state(state);
            let server = tokio::spawn(async move {
                serve_with_quic(
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
            });
            (stop_tx, server, tcp_port, quic_port)
        }

        let barrier = RequestsInFlight {
            started: Arc::new(AtomicUsize::new(0)),
            all_started: Arc::new(Notify::new()),
        };
        let first_identity = Identity::generate();
        let second_identity = Identity::generate();
        let first_node_id = first_identity.node_id.0.clone();
        let second_node_id = second_identity.node_id.0.clone();
        let (first_stop, first_server, first_tcp, first_quic) =
            spawn_peer(first_identity, barrier.clone()).await;
        let (second_stop, second_server, second_tcp, second_quic) =
            spawn_peer(second_identity, barrier.clone()).await;

        let clients = crate::quic::ClientPool::new();
        let first_request = tokio::spawn({
            let clients = clients.clone();
            let node_id = first_node_id;
            async move {
                clients
                    .request(
                        &node_id,
                        &format!("http://127.0.0.1:{first_tcp}"),
                        &[format!("127.0.0.1:{first_quic}")],
                        "/hold",
                        "application/octet-stream",
                        None,
                        None,
                        Vec::new(),
                        Duration::from_secs(600),
                    )
                    .await
            }
        });
        let second_request = tokio::spawn({
            let clients = clients.clone();
            let node_id = second_node_id;
            async move {
                clients
                    .request(
                        &node_id,
                        &format!("http://127.0.0.1:{second_tcp}"),
                        &[format!("127.0.0.1:{second_quic}")],
                        "/hold",
                        "application/octet-stream",
                        None,
                        None,
                        Vec::new(),
                        Duration::from_secs(600),
                    )
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(2), barrier.all_started.notified())
            .await
            .expect("both peers should accept the QUIC request");

        let shutdown = tokio::time::timeout(Duration::from_secs(5), async {
            first_stop.send(()).unwrap();
            second_stop.send(()).unwrap();
            first_server.await.unwrap().unwrap();
            second_server.await.unwrap().unwrap();
        })
        .await;

        first_request.abort();
        second_request.abort();
        assert!(
            shutdown.is_ok(),
            "idle peers cannot wait indefinitely for the QUIC shutdown"
        );
    }

    /// O reload pode pedir o encerramento antes de a task QUIC ter sido
    /// escalonada pela primeira vez. O sinal precisa sobreviver a essa
    /// corrida em cada peer, ou o supervisor fica esperando para sempre.
    #[tokio::test(flavor = "current_thread")]
    async fn two_peers_do_not_lose_the_quic_shutdown_signal_before_the_first_poll() {
        async fn peer_parts(
            identity: Identity,
        ) -> (tokio::net::TcpListener, QuicServerConfig, u16) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let quic_port = udp_probe.local_addr().unwrap().port();
            drop(udp_probe);
            let quic = quic_server_config(&identity).unwrap();
            (listener, quic, quic_port)
        }

        // Prepare both sockets before spawning either server. This leaves the
        // already-ready shutdown futures queued together with the two peers,
        // making the lost `notify_waiters` wake-up deterministic.
        let (first_listener, first_quic, first_quic_port) = peer_parts(Identity::generate()).await;
        let (second_listener, second_quic, second_quic_port) =
            peer_parts(Identity::generate()).await;
        let mut first = tokio::spawn(serve_with_quic(
            first_listener,
            Router::new(),
            None,
            Some(first_quic),
            Some(first_quic_port),
            async {},
        ));
        let mut second = tokio::spawn(serve_with_quic(
            second_listener,
            Router::new(),
            None,
            Some(second_quic),
            Some(second_quic_port),
            async {},
        ));
        tokio::task::yield_now().await;
        let finished = tokio::time::timeout(Duration::from_secs(1), async {
            (&mut first).await.unwrap().unwrap();
            (&mut second).await.unwrap().unwrap();
        })
        .await;

        if finished.is_err() {
            first.abort();
            second.abort();
            let _ = first.await;
            let _ = second.await;
        }
        assert!(
            finished.is_ok(),
            "the simultaneous shutdown of both peers cannot lose the QUIC signal"
        );
    }

    /// E o ataque que o pinning existe para impedir: quem espera outro node_id
    /// não completa o handshake, mesmo o certificado sendo válido como
    /// auto assinado.
    #[tokio::test]
    async fn a_client_expecting_another_node_is_refused() {
        let server = Identity::generate();
        let other = Identity::generate();
        let addr = start_server(&server, false).await;
        let http = clients().for_peer(&other.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await;
        assert!(r.is_err(), "the handshake had to fail, got {r:?}");
    }

    /// A virada não é simultânea: o mesmo servidor atende texto claro enquanto
    /// peers ainda não souberam da política nova.
    #[tokio::test]
    async fn the_same_server_serves_plain_text() {
        let id = Identity::generate();
        let addr = start_server(&id, false).await;
        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    /// No modo estrito o loopback continua passando em claro - é requisito, não
    /// descuido: `infer_tunnel::serve_loopback`, a sonda do `runtime` e o
    /// `testkit` batem em 127.0.0.1 sem par de chaves.
    #[tokio::test]
    async fn in_strict_mode_loopback_still_speaks_plain() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    /// O navegador tem que conseguir abrir o painel com o cluster em TLS.
    ///
    /// Este teste existe por uma falha real: o certificado do pinning é
    /// Ed25519, que navegador não verifica - medido com o curl/LibreSSL do
    /// macOS, `sslv3 alert handshake failure`. Somado ao modo estrito, ligar o
    /// TLS deixava a interface inalcançável de qualquer outra máquina. Um
    /// cliente que **não** oferece o ALPN `pom/1` recebe o certificado ECDSA,
    /// que ele sabe verificar.
    #[tokio::test]
    async fn a_browser_can_open_the_interface_over_tls() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;

        // Como um navegador: sem `pom/1`, e sem confiar em CA nenhuma (é auto
        // assinado; o navegador de verdade mostra o aviso e o operador segue).
        let browser = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let r = browser
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the browser had to complete the handshake");
        assert!(r.status().is_success());
    }

    /// E o peer continua recebendo o certificado do pinning na mesma porta.
    #[tokio::test]
    async fn on_the_same_port_the_peer_gets_the_pinning_certificate() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the peer had to complete the handshake with the pinning cert");
        assert!(r.status().is_success());
    }

    /// E TLS continua funcionando no modo estrito, que é o ponto dele.
    #[tokio::test]
    async fn in_strict_mode_tls_still_goes_through() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("TLS must go through in strict mode");
        assert!(r.status().is_success());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_origin_header(value: &str) -> hyper::Request<()> {
        let mut req = hyper::Request::new(());
        req.headers_mut().insert(
            crate::infer_tunnel::TUNNEL_ORIGIN_HEADER,
            value.parse().unwrap(),
        );
        req
    }

    /// O que o túnel entrega pelo loopback, com a marca deste processo, não é
    /// local: é isso que impede um par do enxame de virar administrador no
    /// modo localhost.
    #[test]
    fn a_tunneled_request_is_never_loopback() {
        let loopback: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let mut req = with_origin_header(crate::infer_tunnel::tunnel_mark());
        let origin = request_origin(loopback, &mut req);
        assert!(!crate::auth::is_loopback_addr(origin));
        assert!(req
            .headers()
            .get(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
            .is_none());
    }

    /// Marca errada, ou vinda de fora do loopback, não muda a origem - e o
    /// cabeçalho sai do pedido de qualquer jeito.
    #[test]
    fn a_forged_tunnel_mark_changes_nothing() {
        let loopback: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let mut req = with_origin_header("forjada");
        assert_eq!(request_origin(loopback, &mut req), loopback);
        assert!(req
            .headers()
            .get(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
            .is_none());

        let remote: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let mut req = with_origin_header(crate::infer_tunnel::tunnel_mark());
        assert_eq!(request_origin(remote, &mut req), remote);
    }
    use std::net::SocketAddr;

    /// `0x16` é `ContentType::Handshake` do TLS. Todo método HTTP começa com
    /// letra ASCII maiúscula (`G`, `P`, `O`...), então um byte só separa as
    /// duas falas sem ambiguidade.
    #[test]
    fn the_first_byte_separates_tls_from_http() {
        assert!(wants_tls(0x16));
        assert!(!wants_tls(b'G')); // GET
        assert!(!wants_tls(b'P')); // POST
        assert!(!wants_tls(b'O')); // OPTIONS
    }

    /// Loopback fala claro sempre: `infer_tunnel::serve_loopback`, a sonda de
    /// `runtime.rs` e o `gateway::testkit` batem em 127.0.0.1 e não têm par de
    /// chaves a apresentar. Tráfego que não sai da máquina não tem o que
    /// proteger.
    #[test]
    fn loopback_speaks_plain_even_in_strict_mode() {
        let local: SocketAddr = "127.0.0.1:5050".parse().unwrap();
        let local6: SocketAddr = "[::1]:5050".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:5050".parse().unwrap();
        assert!(plain_allowed(local, true));
        assert!(plain_allowed(local6, true));
        // O socket dual-stack entrega IPv4 como `::ffff:a.b.c.d`, então sem
        // desmapear o loopback local seria recusado pelo próprio nó.
        assert!(plain_allowed(mapped, true));
    }

    /// A UI e as rotas de gente são redirecionadas; as de peer, não.
    #[test]
    fn only_a_peer_route_stops_being_redirected() {
        assert_eq!(plain_action("/"), PlainAction::Redirect);
        assert_eq!(plain_action("/assets/app.js"), PlainAction::Redirect);
        assert_eq!(plain_action("/api/ui/node"), PlainAction::Redirect);
        assert_eq!(plain_action("/admin/nodes"), PlainAction::Redirect);

        // Estas o `reqwest` de um peer seguiria em silêncio, e a falha
        // resultante não diria o motivo. O 403 diz.
        assert_eq!(plain_action("/api/transfer/download"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/relay-result"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/prefill"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/step"), PlainAction::Refuse);
    }

    /// O painel é redirecionado preservando caminho e query - um 301 que joga
    /// o operador na raiz perde o link que ele abriu.
    #[test]
    fn the_redirect_preserves_the_path_and_the_query() {
        assert_eq!(
            plain_reply("ksia:8080", "/config/network?tab=tls"),
            (
                301,
                Some("https://ksia:8080/config/network?tab=tls".to_string())
            )
        );
        // A porta vai junto porque é a mesma: o servidor atende as duas falas.
        assert_eq!(
            plain_reply("192.168.100.138:8080", "/"),
            (301, Some("https://192.168.100.138:8080/".to_string()))
        );
    }

    #[test]
    fn a_peer_route_gets_403_instead_of_a_redirect() {
        assert_eq!(
            plain_reply("ksia:8080", "/api/transfer/download?path=x"),
            (403, None)
        );
        assert_eq!(plain_reply("ksia:8080", "/v1/relay-result"), (403, None));
    }

    /// Sem `Host` não há para onde mandar, e um `Location` quebrado faria o
    /// cliente falhar longe daqui, com um erro que não aponta para cá.
    #[test]
    fn without_a_host_no_destination_is_invented() {
        assert_eq!(plain_reply("", "/"), (403, None));
    }

    /// De fora, o modo estrito recusa texto claro - é o que faz a flag valer.
    #[test]
    fn a_plain_remote_is_refused_in_strict_mode() {
        let remote: SocketAddr = "192.168.100.138:5050".parse().unwrap();
        assert!(!plain_allowed(remote, true));
        assert!(plain_allowed(remote, false));
    }

    /// O bug do `undeploy`: uma conexão em keep-alive parada segurava a task
    /// que a atendia, a task segurava uma cópia do `Router`, e o `Router` do
    /// servidor de inferência segura os pesos do modelo — a VRAM nunca voltava
    /// e o próximo modelo não cabia. O `serve` tem de encerrar mesmo com uma
    /// conexão dessas aberta, e é o que este teste prende.
    #[tokio::test]
    async fn a_stalled_connection_does_not_hold_the_shutdown() {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();

        let app = Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let server = tokio::spawn(async move {
            serve_with_shutdown(listener, app, None, async {
                let _ = shutdown.await;
            })
            .await
        });

        // Uma requisição de verdade, para a conexão existir do lado do
        // servidor, e depois nada: é o keep-alive parado do pool do gateway.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0u8; 12];
        tokio::io::AsyncReadExt::read(&mut client, &mut response)
            .await
            .unwrap();

        let _ = stop.send(());
        // Com folga sobre o `DRAIN_DEADLINE`, e muito abaixo do "para sempre"
        // que era o comportamento antigo.
        let output = tokio::time::timeout(DRAIN_DEADLINE * 3, server).await;
        assert!(output.is_ok(), "serve did not stop with a stalled connection");
        assert!(output.unwrap().unwrap().is_ok());
    }
}
