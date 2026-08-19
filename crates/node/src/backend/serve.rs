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
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tracing::{debug, warn};

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
    const SO_DE_PEER: [&str; 4] = [
        "/api/transfer/",
        "/v1/relay-result",
        "/v1/prefill",
        "/v1/step",
    ];
    if SO_DE_PEER.iter().any(|p| path.starts_with(p)) {
        PlainAction::Refuse
    } else {
        PlainAction::Redirect
    }
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
    pub fn so_de_peers(&self) -> Self {
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
    let mut nomes: Vec<String> = vec!["127.0.0.1".into(), "::1".into()];
    if let Some(p) = pinned {
        nomes.push(crate::network::ip_host(p).trim_matches(['[', ']']).to_string());
    }
    for addr in crate::addrs::filter_candidates(crate::addrs::local_addrs(), None) {
        nomes.push(crate::network::ip_host(&addr).trim_matches(['[', ']']).to_string());
    }
    if let Ok(h) = hostname_local() {
        nomes.push(h);
    }
    nomes.retain(|n| !n.is_empty());
    nomes.sort();
    nomes.dedup();
    nomes
}

fn hostname_local() -> Result<String> {
    let saida = std::process::Command::new("hostname").output()?;
    Ok(String::from_utf8_lossy(&saida.stdout).trim().to_string())
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
pub async fn serve(listener: TcpListener, app: Router, tls: Option<TlsServer>) -> Result<()> {
    serve_with_shutdown(listener, app, tls, std::future::pending::<()>()).await
}

/// Quanto tempo uma conexão viva tem para terminar sozinha depois do sinal de
/// desligamento, antes de ser abortada.
const DRAIN_DEADLINE: Duration = Duration::from_secs(3);

/// Como `serve`, mas para de aceitar quando `shutdown` resolve e **encerra as
/// conexões que sobraram** em vez de deixá-las soltas.
///
/// A diferença importa para memória, não para higiene: cada conexão fica numa
/// task que carrega uma cópia do `Router`, e o `Router` do servidor de
/// inferência carrega os pesos do modelo. Antes disto as tasks eram
/// desgarradas (`tokio::spawn` sem dono), então uma conexão em keep-alive
/// parada — o pool do gateway deixa várias — segurava o motor vivo depois do
/// `undeploy` e a VRAM nunca voltava. Ver `LlmEngine::unload`.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let acceptor = tls
        .as_ref()
        .map(|t| tokio_rustls::TlsAcceptor::from(t.config.clone()));

    let mut conns = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    let motivo = loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break "desligamento pedido",
            // Conexões já encerradas saem do conjunto aqui; sem isto o
            // `JoinSet` cresceria por toda a vida do servidor.
            Some(_) = conns.join_next(), if !conns.is_empty() => continue,
            aceita = listener.accept() => match aceita {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept falhou: {e}");
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
                debug!("conexão de {peer} encerrada: {e}");
            }
        });
    };

    // Prazo para quem está no meio de uma resposta; depois dele, aborta. Quem
    // está parada em keep-alive não termina sozinha nunca, e é justamente ela
    // que segurava a memória.
    debug!("{motivo}: drenando {} conexões", conns.len());
    let drenagem = async {
        while conns.join_next().await.is_some() {}
    };
    if tokio::time::timeout(DRAIN_DEADLINE, drenagem).await.is_err() {
        warn!("conexões ainda vivas depois de {DRAIN_DEADLINE:?}; encerrando");
        conns.shutdown().await;
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
        let politica = tls.as_ref().map(|t| t.plain).unwrap_or(PlainPolicy::Refuse);
        if politica == PlainPolicy::Refuse {
            warn!("recusada conexão em texto claro de {peer}: o cluster está em TLS");
            return Ok(());
        }
        // Porta que também serve gente: em vez de fechar na cara do operador,
        // manda o navegador refazer em https. Ver `plain_action` para o que
        // continua sendo recusado.
        debug!("redirecionando para https a conexão em claro de {peer}");
        let svc = hyper::service::service_fn(redirecionar_para_https);
        return Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), svc)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }

    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut app = app.clone();
        async move { Service::call(&mut app, req).await }
    });

    match (is_tls, acceptor) {
        (true, Some(acceptor)) => {
            let comecou = Instant::now();
            let tls_stream = acceptor.accept(stream).await?;
            if let Some(t) = &tls {
                t.last_handshake_ms
                    .store(comecou.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tls_stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        (true, None) => {
            // Cliente falou TLS com um nó que não tem certificado. Não há o que
            // servir, e deixar a conexão pendurada esconderia o motivo.
            warn!("{peer} tentou TLS mas este nó não tem TLS configurado");
        }
        (false, _) => {
            Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), svc)
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
    let caminho = path_and_query.split('?').next().unwrap_or("/");
    if plain_action(caminho) == PlainAction::Refuse {
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
async fn redirecionar_para_https(
    req: hyper::Request<hyper::body::Incoming>,
) -> std::result::Result<hyper::Response<String>, std::convert::Infallible> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let alvo = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let (status, location) = plain_reply(&host, &alvo);
    let mut resposta = hyper::Response::builder().status(status);
    if let Some(l) = &location {
        resposta = resposta.header(hyper::header::LOCATION, l);
    }
    let corpo = if location.is_some() {
        String::new()
    } else {
        "este cluster exige TLS entre nós; refaça a chamada em https\n".to_string()
    };
    Ok(resposta.body(corpo).expect("resposta é construível"))
}

#[cfg(test)]
mod integracao {
    use super::*;
    use crate::identity::Identity;
    use axum::{routing::get, Router};
    use std::time::Duration;

    async fn subir(id: &Identity, strict: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let tls = TlsServer::from_identity(id, strict, &["127.0.0.1".to_string()]).unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        addr
    }

    fn clientes() -> crate::peer_client::PeerClients {
        let c = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        c.set_tls(true);
        c
    }

    /// O caminho feliz inteiro: certificado derivado da identidade, cliente que
    /// fixa o node_id, handshake, resposta.
    #[tokio::test]
    async fn cliente_pinado_fala_com_o_servidor() {
        let id = Identity::generate();
        let addr = subir(&id, false).await;
        let http = clientes().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("o handshake tinha que fechar");
        assert!(r.status().is_success());
    }

    /// E o ataque que o pinning existe para impedir: quem espera outro node_id
    /// não completa o handshake, mesmo o certificado sendo válido como
    /// auto assinado.
    #[tokio::test]
    async fn cliente_que_espera_outro_no_e_recusado() {
        let servidor = Identity::generate();
        let outro = Identity::generate();
        let addr = subir(&servidor, false).await;
        let http = clientes().for_peer(&outro.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await;
        assert!(r.is_err(), "o handshake tinha que falhar, veio {r:?}");
    }

    /// A virada não é simultânea: o mesmo servidor atende texto claro enquanto
    /// peers ainda não souberam da política nova.
    #[tokio::test]
    async fn o_mesmo_servidor_atende_texto_claro() {
        let id = Identity::generate();
        let addr = subir(&id, false).await;
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
    async fn no_modo_estrito_o_loopback_ainda_fala_claro() {
        let id = Identity::generate();
        let addr = subir(&id, true).await;
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
    async fn um_navegador_consegue_abrir_a_interface_por_tls() {
        let id = Identity::generate();
        let addr = subir(&id, true).await;

        // Como um navegador: sem `pom/1`, e sem confiar em CA nenhuma (é auto
        // assinado; o navegador de verdade mostra o aviso e o operador segue).
        let navegador = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let r = navegador
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("o navegador tinha que fechar o handshake");
        assert!(r.status().is_success());
    }

    /// E o peer continua recebendo o certificado do pinning na mesma porta.
    #[tokio::test]
    async fn na_mesma_porta_o_peer_recebe_o_certificado_do_pinning() {
        let id = Identity::generate();
        let addr = subir(&id, true).await;
        let http = clientes().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("o peer tinha que fechar o handshake com o cert do pinning");
        assert!(r.status().is_success());
    }

    /// E TLS continua funcionando no modo estrito, que é o ponto dele.
    #[tokio::test]
    async fn no_modo_estrito_o_tls_continua_passando() {
        let id = Identity::generate();
        let addr = subir(&id, true).await;
        let http = clientes().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("TLS tem que passar no modo estrito");
        assert!(r.status().is_success());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// `0x16` é `ContentType::Handshake` do TLS. Todo método HTTP começa com
    /// letra ASCII maiúscula (`G`, `P`, `O`...), então um byte só separa as
    /// duas falas sem ambiguidade.
    #[test]
    fn primeiro_byte_separa_tls_de_http() {
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
    fn loopback_fala_claro_mesmo_no_modo_estrito() {
        let local: SocketAddr = "127.0.0.1:5050".parse().unwrap();
        let local6: SocketAddr = "[::1]:5050".parse().unwrap();
        let mapeado: SocketAddr = "[::ffff:127.0.0.1]:5050".parse().unwrap();
        assert!(plain_allowed(local, true));
        assert!(plain_allowed(local6, true));
        // O socket dual-stack entrega IPv4 como `::ffff:a.b.c.d`, então sem
        // desmapear o loopback local seria recusado pelo próprio nó.
        assert!(plain_allowed(mapeado, true));
    }

    /// A UI e as rotas de gente são redirecionadas; as de peer, não.
    #[test]
    fn so_rota_de_peer_deixa_de_ser_redirecionada() {
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
    fn o_redirecionamento_preserva_o_caminho_e_a_query() {
        assert_eq!(
            plain_reply("ksia:8080", "/config/rede?aba=tls"),
            (301, Some("https://ksia:8080/config/rede?aba=tls".to_string()))
        );
        // A porta vai junto porque é a mesma: o servidor atende as duas falas.
        assert_eq!(
            plain_reply("192.168.100.138:8080", "/"),
            (301, Some("https://192.168.100.138:8080/".to_string()))
        );
    }

    #[test]
    fn rota_de_peer_recebe_403_em_vez_de_redirecionamento() {
        assert_eq!(plain_reply("ksia:8080", "/api/transfer/download?path=x"), (403, None));
        assert_eq!(plain_reply("ksia:8080", "/v1/relay-result"), (403, None));
    }

    /// Sem `Host` não há para onde mandar, e um `Location` quebrado faria o
    /// cliente falhar longe daqui, com um erro que não aponta para cá.
    #[test]
    fn sem_host_nao_se_inventa_destino() {
        assert_eq!(plain_reply("", "/"), (403, None));
    }

    /// De fora, o modo estrito recusa texto claro - é o que faz a flag valer.
    #[test]
    fn remoto_em_claro_e_recusado_no_modo_estrito() {
        let remoto: SocketAddr = "192.168.100.138:5050".parse().unwrap();
        assert!(!plain_allowed(remoto, true));
        assert!(plain_allowed(remoto, false));
    }

    /// O bug do `undeploy`: uma conexão em keep-alive parada segurava a task
    /// que a atendia, a task segurava uma cópia do `Router`, e o `Router` do
    /// servidor de inferência segura os pesos do modelo — a VRAM nunca voltava
    /// e o próximo modelo não cabia. O `serve` tem de encerrar mesmo com uma
    /// conexão dessas aberta, e é o que este teste prende.
    #[tokio::test]
    async fn conexao_parada_nao_segura_o_desligamento() {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (parar, desligar) = tokio::sync::oneshot::channel::<()>();

        let app = Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let servidor = tokio::spawn(async move {
            serve_with_shutdown(listener, app, None, async {
                let _ = desligar.await;
            })
            .await
        });

        // Uma requisição de verdade, para a conexão existir do lado do
        // servidor, e depois nada: é o keep-alive parado do pool do gateway.
        let mut cliente = TcpStream::connect(addr).await.unwrap();
        cliente
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resposta = [0u8; 12];
        tokio::io::AsyncReadExt::read(&mut cliente, &mut resposta)
            .await
            .unwrap();

        let _ = parar.send(());
        // Com folga sobre o `DRAIN_DEADLINE`, e muito abaixo do "para sempre"
        // que era o comportamento antigo.
        let saida = tokio::time::timeout(DRAIN_DEADLINE * 3, servidor).await;
        assert!(saida.is_ok(), "o serve não encerrou com uma conexão parada");
        assert!(saida.unwrap().unwrap().is_ok());
    }
}
