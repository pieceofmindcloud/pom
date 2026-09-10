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
use std::time::Duration;
use tokio::net::lookup_host;
use tracing::debug;

pub struct QuicResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub response_headers_ms: u64,
    pub response_body_ms: u64,
}

struct QuicSession {
    sender: tokio::sync::Mutex<h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>>,
}

#[derive(Clone)]
pub struct ClientPool {
    endpoint: Arc<tokio::sync::OnceCell<quinn::Endpoint>>,
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<QuicSession>>>>,
}

impl ClientPool {
    pub fn new() -> Self {
        Self {
            endpoint: Arc::new(tokio::sync::OnceCell::const_new()),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn endpoint(&self) -> Result<&quinn::Endpoint> {
        self.endpoint
            .get_or_try_init(|| async {
                Ok::<_, anyhow::Error>(quinn::Endpoint::client("[::]:0".parse()?)?)
            })
            .await
    }

    pub async fn request(
        &self,
        peer_id: &str,
        base_url: &str,
        path: &str,
        authorization: Option<String>,
        session_id: Option<&str>,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<QuicResponse> {
        let key = format!("{peer_id}@{}", base_url.trim_end_matches('/'));
        let session = match self.session(&key, peer_id, base_url, timeout).await {
            Ok(session) => session,
            Err(error) => {
                self.sessions.lock().await.remove(&key);
                return Err(error);
            }
        };
        let result: Result<QuicResponse> = async {
            let mut request_builder = Request::builder()
                .method("POST")
                .uri(format!("https://{}{}", authority(base_url)?, path))
                .header("content-type", "application/octet-stream")
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
            let mut stream = tokio::time::timeout(timeout, sender.send_request(request))
                .await
                .context("abertura do stream HTTP/3 expirou")??;
            stream
                .send_data(Bytes::from(body))
                .await
                .map_err(|error| anyhow!("envio do corpo HTTP/3 falhou: {error}"))?;
            stream
                .finish()
                .await
                .map_err(|error| anyhow!("finalização do corpo HTTP/3 falhou: {error}"))?;
            let headers_started = std::time::Instant::now();
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
            })
        }
        .await;
        if result.is_err() {
            self.sessions.lock().await.remove(&key);
        }
        result
    }

    async fn session(
        &self,
        key: &str,
        peer_id: &str,
        base_url: &str,
        timeout: Duration,
    ) -> Result<Arc<QuicSession>> {
        if let Some(session) = self.sessions.lock().await.get(key).cloned() {
            return Ok(session);
        }
        let addr = resolve_addr(base_url).await?;
        let tls = crate::tls::quic_client_config(peer_id)?;
        let config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
        ));
        let endpoint = self.endpoint().await?;
        let connection = tokio::time::timeout(
            timeout,
            endpoint
                // The certificate verifier pins the full node_id. SNI only
                // needs a valid DNS label; a 64-character hex node_id can
                // exceed the single-label limit, so it is not used here.
                .connect_with(config, addr, &format!("peer.{}", crate::brand::slug()))?,
        )
        .await
        .context("handshake QUIC expirou")??;
        let (mut driver, sender) = h3::client::new(H3Connection::new(connection)).await?;
        tokio::spawn(async move {
            let _ = poll_fn(|cx| driver.poll_close(cx)).await;
        });
        let session = Arc::new(QuicSession {
            sender: tokio::sync::Mutex::new(sender),
        });
        self.sessions
            .lock()
            .await
            .insert(key.to_string(), session.clone());
        Ok(session)
    }
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
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(128u32.into());
    config.transport = Arc::new(transport);
    Ok(Arc::new(config))
}

pub async fn serve(
    port: u16,
    router: axum::Router,
    config: Arc<quinn::ServerConfig>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let endpoint =
        quinn::Endpoint::server(config.as_ref().clone(), format!("[::]:{port}").parse()?)?;
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

async fn resolve_addr(base_url: &str) -> Result<SocketAddr> {
    let authority = authority(base_url)?;
    lookup_host(authority)
        .await?
        .next()
        .ok_or_else(|| anyhow!("endereço QUIC não resolveu"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Router};
    use tokio::sync::oneshot;

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
                "/test",
                None,
                None,
                b"http3".to_vec(),
                Duration::from_secs(3),
            )
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"http3");
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }
}
