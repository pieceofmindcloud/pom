//! Proxy reverso das interfaces web que um plugin serve:
//! `/api/ui/plugins/:code/proxy/*`.
//!
//! Um plugin pode rodar um servidor HTTP próprio em loopback (por exemplo uma
//! aplicação web completa) e expô-lo na interface do POM sem abrir outra porta
//! na rede. O plugin informa onde ele está pela operação `ui.upstream` do
//! `query` genérico: `{"status":"ready","port":N,"token":"..."}`. O nó encaminha
//! para `127.0.0.1:N` - HTTP com corpo em streaming e upgrades (WebSocket) -,
//! anexando o `token` em [`PROXY_TOKEN_HEADER`] para que só ele alcance o
//! servidor do plugin, e [`FORWARDED_PREFIX_HEADER`] para que o plugin saiba
//! sob qual caminho foi montado.
//!
//! Quem entra: a rota está sob `/api/ui`, então passa pela superfície
//! administrativa (loopback em `localhost`, sessão por senha em `network`) e é
//! AdminOnly na tabela de perfis. Como scripts, WebSockets e `EventSource` não
//! carregam o `Authorization` da interface, a credencial aqui é a da
//! superfície, e a falsificação entre sites é barrada por [`same_origin`].
//! Chamadas entre nós nunca passam: o proxy é só para o navegador.
//!
//! O que sai: o cookie de sessão administrativa, o `Authorization` e o token de
//! acesso nunca chegam ao plugin, e o plugin não grava cookies na origem do POM.
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Cabeçalho com o segredo que o plugin publicou em `ui.upstream`.
pub(crate) const PROXY_TOKEN_HEADER: &str = "x-pom-plugin-token";
/// Caminho público sob o qual o nó montou o servidor do plugin.
pub(crate) const FORWARDED_PREFIX_HEADER: &str = "x-forwarded-prefix";
/// Teto do cabeçalho de resposta de um upgrade lido à mão.
const MAX_UPGRADE_HEAD: usize = 64 * 1024;

/// Cabeçalhos que não atravessam um proxy (RFC 9110 §7.6.1), mais os que só
/// valem para a sessão do POM.
const DROPPED_REQUEST_HEADERS: [&str; 13] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "cookie",
    "authorization",
    "x-pom-access-token",
    PROXY_TOKEN_HEADER,
];
const DROPPED_RESPONSE_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "set-cookie",
];
/// Marcadores de chamada entre nós; o proxy existe só para o navegador.
const INTERNAL_HEADERS: [&str; 2] = ["x-pom-internal-node", "x-pom-control-signature"];

/// Onde o servidor do plugin está, segundo `ui.upstream`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Upstream {
    pub port: u16,
    pub token: String,
}

/// Lê a resposta de `ui.upstream`. `Err` traz o motivo mostrado ao navegador.
pub(crate) fn parse_upstream(value: &Value) -> Result<Upstream, String> {
    match value.get("status").and_then(Value::as_str) {
        Some("ready") => {}
        Some("starting") => return Err("o plugin ainda está iniciando".into()),
        _ => {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("o plugin não informou um servidor")
                .to_owned())
        }
    }
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or("porta do plugin inválida")?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| {
            (16..=512).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or("token do plugin inválido")?;
    Ok(Upstream {
        port,
        token: token.to_owned(),
    })
}

pub(crate) fn proxy_prefix(code: &str) -> String {
    format!("/api/ui/plugins/{code}/proxy")
}

/// Barreira contra falsificação entre sites. O navegador marca a origem de
/// toda requisição que um script ou página pode forjar (`Sec-Fetch-Site`, e
/// `Origin` em POST e WebSocket); só a própria origem do POM passa.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        if !matches!(site.to_str(), Ok("same-origin" | "none")) {
            return false;
        }
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(HeaderValue::to_str),
    ) else {
        return false;
    };
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    authority.is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

/// O alvo no servidor do plugin: o caminho depois do prefixo, byte a byte, e a query.
pub(crate) fn upstream_target(prefix: &str, uri: &axum::http::Uri) -> String {
    let rest = uri.path().strip_prefix(prefix).unwrap_or_default();
    let path = if rest.starts_with('/') { rest } else { "/" };
    match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    }
}

/// Cabeçalhos que seguem para o plugin: os do navegador menos os da sessão do
/// POM, mais o token e o contexto de montagem.
pub(crate) fn upstream_headers(
    incoming: &HeaderMap,
    upstream: &Upstream,
    prefix: &str,
    secure: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in incoming {
        if !DROPPED_REQUEST_HEADERS.contains(&name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    if let Some(host) = incoming.get(header::HOST) {
        headers.insert(HeaderName::from_static("x-forwarded-host"), host.clone());
    }
    headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static(if secure { "https" } else { "http" }),
    );
    if let Ok(prefix) = HeaderValue::from_str(prefix) {
        headers.insert(HeaderName::from_static(FORWARDED_PREFIX_HEADER), prefix);
    }
    if let Ok(token) = HeaderValue::from_str(&upstream.token) {
        headers.insert(HeaderName::from_static(PROXY_TOKEN_HEADER), token);
    }
    headers
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("cliente HTTP do proxy de plugins")
    })
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// Encaminha uma requisição do navegador para o servidor do plugin.
pub(crate) async fn forward(upstream: &Upstream, prefix: &str, request: Request) -> Response {
    let secure = request
        .extensions()
        .get::<crate::serve::TransportSecurity>()
        .is_some_and(|transport| transport.0);
    let target = upstream_target(prefix, request.uri());
    let headers = upstream_headers(request.headers(), upstream, prefix, secure);
    if is_upgrade(request.headers()) {
        return forward_upgrade(upstream, &target, headers, request).await;
    }
    let method = request.method().clone();
    let url = format!("http://127.0.0.1:{}{target}", upstream.port);
    let body = reqwest::Body::wrap_stream(request.into_body().into_data_stream());
    let reply = match client()
        .request(method, url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(reply) => reply,
        Err(error) => {
            tracing::warn!(%error, "servidor do plugin não respondeu ao proxy");
            return refusal(
                StatusCode::BAD_GATEWAY,
                "o servidor do plugin não respondeu",
            );
        }
    };
    let mut response = Response::builder().status(reply.status());
    if let Some(out) = response.headers_mut() {
        for (name, value) in reply.headers() {
            if !DROPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
                out.append(name.clone(), value.clone());
            }
        }
    }
    response
        .body(Body::from_stream(reply.bytes_stream()))
        .unwrap_or_else(|_| refusal(StatusCode::BAD_GATEWAY, "resposta inválida do plugin"))
}

/// Um upgrade (WebSocket) não cabe no cliente HTTP: o cabeçalho vai à mão pela
/// conexão TCP, e depois do `101` os dois lados só copiam bytes.
async fn forward_upgrade(
    upstream: &Upstream,
    target: &str,
    mut headers: HeaderMap,
    mut request: Request,
) -> Response {
    let Some(protocol) = request.headers().get(header::UPGRADE).cloned() else {
        return refusal(StatusCode::BAD_REQUEST, "upgrade sem protocolo");
    };
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, protocol);
    let host = format!("127.0.0.1:{}", upstream.port);
    let mut head = format!("{} {target} HTTP/1.1\r\nhost: {host}\r\n", request.method());
    for (name, value) in &headers {
        let Ok(value) = value.to_str() else {
            continue;
        };
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    let mut stream = match tokio::net::TcpStream::connect(&host).await {
        Ok(stream) => stream,
        Err(_) => {
            return refusal(
                StatusCode::BAD_GATEWAY,
                "o servidor do plugin não respondeu",
            )
        }
    };
    if stream.write_all(head.as_bytes()).await.is_err() {
        return refusal(
            StatusCode::BAD_GATEWAY,
            "o servidor do plugin fechou a conexão",
        );
    }
    let (status, reply_headers, leftover) = match read_response_head(&mut stream).await {
        Ok(reply) => reply,
        Err(error) => {
            tracing::warn!(%error, "upgrade recusado pelo servidor do plugin");
            return refusal(StatusCode::BAD_GATEWAY, "resposta inválida do plugin");
        }
    };
    if status != StatusCode::SWITCHING_PROTOCOLS {
        return refusal(status, "o servidor do plugin recusou o upgrade");
    }

    let on_upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(upgraded) => upgraded,
            Err(error) => {
                tracing::debug!(%error, "navegador desistiu do upgrade");
                return;
            }
        };
        let mut browser = hyper_util::rt::TokioIo::new(upgraded);
        if !leftover.is_empty() && browser.write_all(&leftover).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut browser, &mut stream).await;
    });

    let mut response = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Some(out) = response.headers_mut() {
        for (name, value) in reply_headers.iter() {
            if name != "set-cookie" {
                out.append(name.clone(), value.clone());
            }
        }
    }
    response
        .body(Body::empty())
        .unwrap_or_else(|_| refusal(StatusCode::BAD_GATEWAY, "resposta inválida do plugin"))
}

/// Lê o cabeçalho de resposta HTTP/1.1 e devolve os bytes que vieram depois dele.
async fn read_response_head(
    stream: &mut tokio::net::TcpStream,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), String> {
    let mut buffer = Vec::with_capacity(1024);
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > MAX_UPGRADE_HEAD {
            return Err("cabeçalho grande demais".into());
        }
        let mut chunk = [0u8; 4096];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("conexão fechada antes do cabeçalho".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffer[..end]).map_err(|_| "cabeçalho não é UTF-8")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .ok_or("linha de status inválida")?;
    let mut headers = HeaderMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value.trim()),
        ) {
            headers.append(name, value);
        }
    }
    Ok((status, headers, buffer[end + 4..].to_vec()))
}

/// `ANY /api/ui/plugins/:code/proxy` e `/api/ui/plugins/:code/proxy/*path`.
pub(crate) async fn plugin_proxy(
    State(state): State<crate::gateway::GatewayState>,
    Path(params): Path<Vec<(String, String)>>,
    request: Request,
) -> Response {
    let Some(code) = params
        .iter()
        .find_map(|(name, value)| (name == "code").then(|| value.clone()))
    else {
        return refusal(StatusCode::NOT_FOUND, "plugin não encontrado");
    };
    if INTERNAL_HEADERS
        .iter()
        .any(|name| request.headers().contains_key(*name))
    {
        return refusal(
            StatusCode::FORBIDDEN,
            "o proxy de plugins atende só o navegador",
        );
    }
    if !same_origin(request.headers()) {
        return refusal(
            StatusCode::FORBIDDEN,
            "origem recusada pelo proxy de plugins",
        );
    }
    let Some(reply) = state
        .plugins
        .query_audit(&code, "ui.upstream", json!({}))
        .await
    else {
        return refusal(StatusCode::NOT_FOUND, "plugin inativo ou sem servidor");
    };
    let upstream = match parse_upstream(&reply) {
        Ok(upstream) => upstream,
        Err(error) => return refusal(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    let prefix = proxy_prefix(&code);
    if request.method() == Method::CONNECT {
        return refusal(StatusCode::METHOD_NOT_ALLOWED, "método não suportado");
    }
    forward(&upstream, &prefix, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn upstream(port: u16) -> Upstream {
        Upstream {
            port,
            token: "t".repeat(32),
        }
    }

    #[test]
    fn upstream_reply_must_be_ready_with_a_port_and_a_token() {
        let token = "a".repeat(32);
        assert_eq!(
            parse_upstream(&json!({"status":"ready","port":4100,"token":token})),
            Ok(Upstream {
                port: 4100,
                token: token.clone()
            })
        );
        assert!(parse_upstream(&json!({"status":"starting"})).is_err());
        assert_eq!(
            parse_upstream(&json!({"status":"error","error":"boom"})),
            Err("boom".into())
        );
        assert!(parse_upstream(&json!({"status":"ready","port":0,"token":token})).is_err());
        assert!(parse_upstream(&json!({"status":"ready","port":70000,"token":token})).is_err());
        assert!(parse_upstream(&json!({"status":"ready","port":4100,"token":"short"})).is_err());
        assert!(parse_upstream(
            &json!({"status":"ready","port":4100,"token":"has space and more"})
        )
        .is_err());
    }

    #[test]
    fn only_the_pom_origin_passes_the_cross_site_fence() {
        assert!(same_origin(&headers(&[("host", "salem:8080")])));
        assert!(same_origin(&headers(&[
            ("host", "salem:8080"),
            ("origin", "http://salem:8080"),
            ("sec-fetch-site", "same-origin"),
        ])));
        assert!(same_origin(&headers(&[
            ("host", "salem:8080"),
            ("origin", "https://SALEM:8080"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "salem:8080"),
            ("origin", "http://evil.example"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "salem:8080"),
            ("origin", "http://salem:9090"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "salem:8080"),
            ("sec-fetch-site", "cross-site"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "salem:8080"),
            ("sec-fetch-site", "same-site"),
        ])));
        assert!(!same_origin(&headers(&[("origin", "http://salem:8080")])));
    }

    #[test]
    fn target_keeps_the_path_after_the_prefix_and_the_raw_query() {
        let prefix = proxy_prefix("demo");
        let uri: Uri = "/api/ui/plugins/demo/proxy/plugins/??@a/b.js,@c/d.js&rev=1"
            .parse()
            .unwrap();
        assert_eq!(
            upstream_target(&prefix, &uri),
            "/plugins/??@a/b.js,@c/d.js&rev=1"
        );
        let root: Uri = "/api/ui/plugins/demo/proxy".parse().unwrap();
        assert_eq!(upstream_target(&prefix, &root), "/");
        let encoded: Uri = "/api/ui/plugins/demo/proxy/a%2Fb".parse().unwrap();
        assert_eq!(upstream_target(&prefix, &encoded), "/a%2Fb");
    }

    #[test]
    fn pom_credentials_never_reach_the_plugin() {
        let incoming = headers(&[
            ("host", "salem:8080"),
            ("cookie", "pom_admin_session=secret"),
            ("authorization", "Bearer connection-key"),
            ("x-pom-access-token", "password"),
            ("x-pom-plugin-token", "forged"),
            ("accept", "text/html"),
            ("connection", "keep-alive"),
        ]);
        let out = upstream_headers(
            &incoming,
            &upstream(4100),
            "/api/ui/plugins/demo/proxy",
            true,
        );
        for name in [
            "cookie",
            "authorization",
            "x-pom-access-token",
            "connection",
            "host",
        ] {
            assert!(!out.contains_key(name), "{name} vazou");
        }
        assert_eq!(out["accept"], "text/html");
        assert_eq!(out[PROXY_TOKEN_HEADER], "t".repeat(32).as_str());
        assert_eq!(out[FORWARDED_PREFIX_HEADER], "/api/ui/plugins/demo/proxy");
        assert_eq!(out["x-forwarded-host"], "salem:8080");
        assert_eq!(out["x-forwarded-proto"], "https");
    }

    /// Servidor HTTP mínimo que devolve o que recebeu, e faz eco depois de um upgrade.
    async fn echo_upstream() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let end = loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end;
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                    if head.to_ascii_lowercase().contains("upgrade: websocket") {
                        socket
                            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nset-cookie: nope=1\r\n\r\n")
                            .await
                            .unwrap();
                        let (mut reader, mut writer) = socket.split();
                        let _ = tokio::io::copy(&mut reader, &mut writer).await;
                        return;
                    }
                    let body = serde_json::to_vec(&json!({ "head": head })).unwrap();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nset-cookie: plugin=1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn http_requests_reach_the_plugin_with_its_token_and_without_pom_cookies() {
        let port = echo_upstream().await;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/ui/plugins/demo/proxy/api/call?x=1")
            .header("host", "salem:8080")
            .header("cookie", "pom_admin_session=secret")
            .body(Body::from("{}"))
            .unwrap();
        let response = forward(&upstream(port), "/api/ui/plugins/demo/proxy", request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("set-cookie"));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let head = serde_json::from_slice::<Value>(&body).unwrap()["head"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(head.starts_with("post /api/call?x=1 http/1.1"));
        assert!(head.contains(&format!("x-pom-plugin-token: {}", "t".repeat(32))));
        assert!(head.contains("x-forwarded-prefix: /api/ui/plugins/demo/proxy"));
        assert!(!head.contains("pom_admin_session"));
    }

    #[tokio::test]
    async fn websocket_upgrades_are_relayed_end_to_end() {
        let upstream_port = echo_upstream().await;
        let app = axum::Router::new().route(
            "/api/ui/plugins/demo/proxy/*rest",
            axum::routing::any(move |request: Request| async move {
                forward(
                    &upstream(upstream_port),
                    "/api/ui/plugins/demo/proxy",
                    request,
                )
                .await
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let app = app.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let mut app = app.clone();
                        async move { tower::Service::call(&mut app, request).await }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
                });
            }
        });

        let mut socket = tokio::net::TcpStream::connect(gateway).await.unwrap();
        socket
            .write_all(
                b"GET /api/ui/plugins/demo/proxy/api/remote.mux HTTP/1.1\r\nhost: salem:8080\r\nconnection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap();
        let (status, reply_headers, leftover) = read_response_head(&mut socket).await.unwrap();
        assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
        assert!(!reply_headers.contains_key("set-cookie"));
        assert!(leftover.is_empty());
        socket.write_all(b"ping-frame").await.unwrap();
        let mut echoed = [0u8; 10];
        socket.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping-frame");
    }
}
