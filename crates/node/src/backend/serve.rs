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
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tracing::{debug, warn};

/// O que o servidor precisa para falar TLS, e o estado que ele publica de volta.
#[derive(Clone)]
pub struct TlsServer {
    pub config: Arc<rustls::ServerConfig>,
    /// Com `true`, texto claro de fora do loopback é recusado.
    pub strict: Arc<AtomicBool>,
    /// Duração do último handshake aceito, em ms. É daqui que sai o
    /// `tls_handshake_ms` da telemetria de inferência.
    pub last_handshake_ms: Arc<AtomicU64>,
}

impl TlsServer {
    /// Monta o estado do servidor a partir da identidade do nó.
    pub fn from_identity(id: &crate::identity::Identity, strict: bool) -> Result<Self> {
        Ok(Self {
            config: crate::tls::server_config(id)?,
            strict: Arc::new(AtomicBool::new(strict)),
            last_handshake_ms: Arc::new(AtomicU64::new(0)),
        })
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
    let acceptor = tls
        .as_ref()
        .map(|t| tokio_rustls::TlsAcceptor::from(t.config.clone()));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("accept falhou: {e}");
                continue;
            }
        };
        // Nagle segura a escrita pequena até o ACK anterior; no SSE cada token é
        // uma escrita pequena. Mesma razão do `tcp_nodelay(true)` que estava no
        // `axum::serve`.
        let _ = stream.set_nodelay(true);

        let app = app.clone();
        let acceptor = acceptor.clone();
        let tls = tls.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, peer, app, acceptor, tls).await {
                debug!("conexão de {peer} encerrada: {e}");
            }
        });
    }
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
        warn!("recusada conexão em texto claro de {peer}: o cluster está em TLS");
        return Ok(());
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
        let tls = TlsServer::from_identity(id, strict).unwrap();
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

    /// De fora, o modo estrito recusa texto claro - é o que faz a flag valer.
    #[test]
    fn remoto_em_claro_e_recusado_no_modo_estrito() {
        let remoto: SocketAddr = "192.168.100.138:5050".parse().unwrap();
        assert!(!plain_allowed(remoto, true));
        assert!(plain_allowed(remoto, false));
    }
}
