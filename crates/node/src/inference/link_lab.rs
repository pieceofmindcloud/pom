//! Bancada do custo de rede por ficha no **salto interno**.
//!
//! O que ela mede não é o motor nem a latência: é quantos bytes uma ficha
//! custa **no soquete**, por travessia de aresta da pipeline, e quantas
//! ligações o caminho abre para produzir N fichas. Por isso cada hop falso é
//! servido por cima de um `TcpStream` embrulhado num contador
//! ([`Counted`]) - o número não sai de estimativa nem de `Content-Length`, sai
//! dos bytes que realmente passaram.
//!
//! O motor é falso de propósito: um hop que decodifica de verdade mediria a
//! GPU, não o transporte. O que é real aqui é tudo que o transporte usa -
//! [`crate::wire`], o [`crate::relay::BatchCoordinator`], o
//! [`crate::relay::send_frame_or_tunnel`] e o canal de [`crate::link`].
//!
//! As duas configurações comparadas são a **mesma bancada com um bit de
//! diferença**: o hop anuncia ou não `transport/link-v1`. Sem ele, cada ficha
//! reconstrói um `POST /v1/step`; com ele, as fichas sucessivas viajam como
//! quadros num único corpo em fluxo.

#![cfg(test)]

use crate::pipeline::{PipelineStep, CAP_FRAME_BATCH, CAP_LINK_STREAM, CAP_RELAY_CHAIN};
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tower::Service;

/// Igual ao Qwen3.8-27B do cluster do captain, como em `direct_lab`.
const HIDDEN_DIM: usize = 5120;
const DECODE_TOKENS: usize = 40;

// ── Contador de bytes no soquete ─────────────────────────────────────────────

#[derive(Default)]
struct Counters {
    read: AtomicU64,
    written: AtomicU64,
    connections: AtomicU64,
}

struct Counted<S> {
    inner: S,
    counters: Arc<Counters>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if polled.is_ready() {
            let delta = buf.filled().len().saturating_sub(before);
            self.counters
                .read
                .fetch_add(delta as u64, Ordering::Relaxed);
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &polled {
            self.counters
                .written
                .fetch_add(*written as u64, Ordering::Relaxed);
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ── O hop falso ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Hop {
    clients: crate::peer_client::PeerClients,
}

/// Responde um quadro de lote com um item por item recebido, repassando ao
/// próximo hop quando o quadro carrega `next_hops` - exatamente a forma do
/// `relay_and_bubble` de produção, com o motor trocado por nada.
async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    let (header, items) = match wire::decode_batch_items(&body) {
        Ok(decoded) => decoded,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let mut replies = Vec::with_capacity(items.len());
    for item in items {
        let next_hops: Vec<PipelineStep> = item
            .meta
            .get("next_hops")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        let session_id = item
            .meta
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("s")
            .to_string();

        let meta = if next_hops.is_empty() {
            json!({
                "token_id": 1234,
                "text": "x",
                "done": false,
                "processing_ms": 0,
                "node_timings": {},
            })
        } else {
            let (values, shape) = item
                .tensor
                .clone()
                .unwrap_or_else(|| (vec![0.0; HIDDEN_DIM], vec![1, HIDDEN_DIM]));
            match forward(&hop.clients, &next_hops, &session_id, (&values, shape)).await {
                Ok(meta) => meta,
                Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
            }
        };
        replies.push(wire::BatchPayloadItem {
            item_id: item.item_id,
            session_id,
            meta,
            tensor: None,
            dtype: item.dtype,
        });
    }
    match wire::encode_batch_items(header.kind, header.batch_id, &replies) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn forward(
    clients: &crate::peer_client::PeerClients,
    hops: &[PipelineStep],
    session_id: &str,
    tensor: (&[f32], TensorShape),
) -> anyhow::Result<Value> {
    let next = &hops[0];
    let meta = json!({
        "session_id": session_id,
        "is_last": hops.len() == 1,
        "scale": 1.0,
        "next_hops": &hops[1..],
        "exec_start": next.exec_start,
        "exec_end": next.exec_end,
    });
    let sent = crate::relay::send_frame_or_tunnel(
        clients,
        next,
        None,
        "/v1/step",
        &meta,
        Some(tensor),
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        &next.node_id,
        |_| {},
    )
    .await?;
    Ok(sent.meta)
}

async fn handle_link(State(hop): State<Hop>, body: axum::body::Body) -> Response {
    crate::link::serve(body, move |path, bytes| {
        let hop = hop.clone();
        async move {
            if path != "/v1/step" {
                return Err(anyhow::anyhow!("rota inesperada no canal: {path}"));
            }
            Ok(fake_step(State(hop), bytes).await)
        }
    })
}

/// Sobe um hop e devolve `(url, contadores)`. O `serve_connection` é chamado
/// à mão porque é o único jeito de pôr o contador **debaixo** do HTTP.
async fn spawn_hop(hop: Hop) -> (String, Arc<Counters>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counters = Arc::new(Counters::default());
    let app = Router::new()
        .route("/v1/step", post(fake_step))
        .route(
            crate::link::LINK_PATH,
            post(handle_link).layer(DefaultBodyLimit::disable()),
        )
        .with_state(hop);
    let serving = counters.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let counters = serving.clone();
            counters.connections.fetch_add(1, Ordering::Relaxed);
            let app = app.clone();
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let counted: Counted<TcpStream> = Counted {
                    inner: stream,
                    counters,
                };
                let service = hyper::service::service_fn(move |request| {
                    let mut app = app.clone();
                    async move { Service::call(&mut app, request).await }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(counted), service)
                    .await;
            });
        }
    });
    (format!("http://{addr}"), counters)
}

fn step_for(node_id: &str, url: &str, layer: u32, link: bool) -> PipelineStep {
    let mut caps = vec![CAP_RELAY_CHAIN.to_string(), CAP_FRAME_BATCH.to_string()];
    if link {
        caps.push(CAP_LINK_STREAM.to_string());
    }
    PipelineStep {
        node_id: node_id.into(),
        inference_url: url.into(),
        inference_candidates: vec![],
        start_layer: layer,
        end_layer: layer,
        exec_start: layer,
        exec_end: layer,
        inference_frame_capabilities: caps,
        libp2p_peer_id: None,
        libp2p_addr_candidates: Vec::new(),
    }
}

// ── A medição ────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Measurement {
    /// Bytes lidos e escritos no soquete do primeiro hop, por ficha.
    bytes_per_token_first_hop: f64,
    /// Idem, somando todos os hops da cadeia.
    bytes_per_token_all_hops: f64,
    /// Conexões TCP abertas contra o primeiro hop.
    connections_first_hop: u64,
    /// Tamanho do quadro de aplicação enviado na primeira travessia, e quanto
    /// dele é o estado oculto. É o que separa o custo do transporte do custo
    /// irredutível do tensor.
    frame_bytes: u64,
    hidden_state_bytes: u64,
    ms_per_token: f64,
    transport: &'static str,
}

async fn measure(hops: usize, link: bool, tokens: usize) -> Measurement {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(30));
    let mut urls = Vec::new();
    let mut counters = Vec::new();
    for _ in 0..hops {
        let (url, counter) = spawn_hop(Hop {
            clients: clients.clone(),
        })
        .await;
        urls.push(url);
        counters.push(counter);
    }
    let steps: Vec<PipelineStep> = urls
        .iter()
        .enumerate()
        .map(|(index, url)| step_for(&format!("lab-hop{index}"), url, index as u32, link))
        .collect();

    let session = uuid::Uuid::new_v4().to_string();
    let residual = vec![0.25f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    // Aquecimento: a primeira travessia abre a conexão (e o canal), e o custo
    // de estabelecimento não é custo por ficha.
    let mut transport = "";
    let warm = forward(&clients, &steps, &session, (&residual, shape.clone()))
        .await
        .expect("aquecimento");
    let _ = warm;
    for counter in &counters {
        counter.read.store(0, Ordering::Relaxed);
        counter.written.store(0, Ordering::Relaxed);
    }
    let connections_before = counters[0].connections.load(Ordering::Relaxed);

    let mut frame_bytes = 0;
    let mut hidden_state_bytes = 0;
    let started = Instant::now();
    for _ in 0..tokens {
        let next = &steps[0];
        let meta = json!({
            "session_id": session,
            "is_last": steps.len() == 1,
            "scale": 1.0,
            "next_hops": &steps[1..],
            "exec_start": next.exec_start,
            "exec_end": next.exec_end,
        });
        let sent = crate::relay::send_frame_or_tunnel(
            &clients,
            next,
            None,
            "/v1/step",
            &meta,
            Some((&residual, shape.clone())),
            WireDtype::F16,
            WireDtype::F16.frame_encoding(),
            1,
            &next.node_id,
            |_| {},
        )
        .await
        .expect("travessia");
        transport = match sent.transport {
            crate::relay::InferenceTransport::HttpsLink => "https-link",
            crate::relay::InferenceTransport::Https => "https",
            other => {
                panic!("transporte inesperado na bancada: {}", other.as_str())
            }
        };
        assert_eq!(sent.meta["token_id"], json!(1234));
        frame_bytes = sent.bytes_sent;
        hidden_state_bytes = sent.hidden_state_bytes_sent;
    }
    let elapsed = started.elapsed();

    let first =
        counters[0].read.load(Ordering::Relaxed) + counters[0].written.load(Ordering::Relaxed);
    let all: u64 = counters
        .iter()
        .map(|counter| {
            counter.read.load(Ordering::Relaxed) + counter.written.load(Ordering::Relaxed)
        })
        .sum();
    Measurement {
        bytes_per_token_first_hop: first as f64 / tokens as f64,
        bytes_per_token_all_hops: all as f64 / tokens as f64,
        connections_first_hop: counters[0].connections.load(Ordering::Relaxed) - connections_before,
        frame_bytes,
        hidden_state_bytes,
        ms_per_token: elapsed.as_secs_f64() * 1000.0 / tokens as f64,
        transport,
    }
}

/// A lei desta bancada: **a ligação persistente não pode custar mais bytes por
/// ficha que a requisição por ficha**, e tem que atravessar sem abrir conexão
/// nova por ficha.
///
/// O número absoluto é registrado por `println!` para entrar no commit e na
/// documentação; o que o teste guarda é a comparação, que não depende da
/// máquina.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ligacao_persistente_custa_menos_bytes_por_ficha() {
    for hops in [1usize, 2, 3] {
        let before = measure(hops, false, DECODE_TOKENS).await;
        let after = measure(hops, true, DECODE_TOKENS).await;
        for (rotulo, m) in [("antes ", &before), ("depois", &after)] {
            println!(
                "hops={hops} {rotulo} transporte={:<10} bytes/ficha(1º hop)={:.1} \
                 bytes/ficha(cadeia)={:.1} quadro={} estado_oculto={} conexões={} ms/ficha={:.3}",
                m.transport,
                m.bytes_per_token_first_hop,
                m.bytes_per_token_all_hops,
                m.frame_bytes,
                m.hidden_state_bytes,
                m.connections_first_hop,
                m.ms_per_token
            );
        }
        assert_eq!(before.transport, "https");
        assert_eq!(after.transport, "https-link");
        assert_eq!(
            after.connections_first_hop, 0,
            "o canal persistente não pode abrir conexão por ficha"
        );
        assert!(
            after.bytes_per_token_first_hop < before.bytes_per_token_first_hop,
            "hops={hops}: o canal gastou {:.1} B/ficha contra {:.1} B/ficha da requisição por ficha",
            after.bytes_per_token_first_hop,
            before.bytes_per_token_first_hop
        );
        assert!(
            after.bytes_per_token_all_hops < before.bytes_per_token_all_hops,
            "hops={hops}: a cadeia inteira não economizou"
        );
    }
}

/// Um hop que anuncia o canal mas não o serve não pode matar a geração: a
/// travessia degrada para o caminho por requisição e a telemetria diz isso.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canal_indisponivel_degrada_para_o_caminho_por_requisicao() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Só `/v1/step`: `/v1/link` responde 404 e o canal não abre.
    let app = Router::new()
        .route("/v1/step", post(fake_step))
        .with_state(Hop {
            clients: clients.clone(),
        });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).tcp_nodelay(true).await;
    });
    let url = format!("http://{addr}");
    let step = step_for("lab-sem-canal", &url, 0, true);

    let residual = vec![0.25f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];
    let meta = json!({
        "session_id": "sessao",
        "is_last": true,
        "scale": 1.0,
        "next_hops": [],
        "exec_start": 0,
        "exec_end": 0,
    });
    let sent = crate::relay::send_frame_or_tunnel(
        &clients,
        &step,
        None,
        "/v1/step",
        &meta,
        Some((&residual, shape)),
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        &step.node_id,
        |_| {},
    )
    .await
    .expect("a geração continua pelo caminho de sempre");
    assert_eq!(sent.transport, crate::relay::InferenceTransport::Https);
    assert!(
        sent.transport_fallback,
        "a telemetria tem que denunciar que o canal não serviu"
    );
    assert_eq!(sent.meta["token_id"], json!(1234));
    assert!(
        !crate::link::has_channel("lab-sem-canal", &url).await,
        "um canal que não abriu não pode ficar no pool"
    );
}

/// Duas sessões atravessando a mesma aresta compartilham **uma** ligação, e o
/// `batch_id` é o que as mantém separadas.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessoes_concorrentes_compartilham_a_ligacao() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(30));
    let (url, counters) = spawn_hop(Hop {
        clients: clients.clone(),
    })
    .await;
    let step = step_for("lab-multiplexado", &url, 0, true);
    let residual = Arc::new(vec![0.25f32; HIDDEN_DIM]);

    let mut sessions = Vec::new();
    for index in 0..6 {
        let clients = clients.clone();
        let step = step.clone();
        let residual = residual.clone();
        sessions.push(tokio::spawn(async move {
            for _ in 0..10 {
                let meta = json!({
                    "session_id": format!("sessao-{index}"),
                    "is_last": true,
                    "scale": 1.0,
                    "next_hops": [],
                    "exec_start": 0,
                    "exec_end": 0,
                });
                let sent = crate::relay::send_frame_or_tunnel(
                    &clients,
                    &step,
                    None,
                    "/v1/step",
                    &meta,
                    Some((residual.as_slice(), vec![1, HIDDEN_DIM])),
                    WireDtype::F16,
                    WireDtype::F16.frame_encoding(),
                    1,
                    &step.node_id,
                    |_| {},
                )
                .await
                .expect("travessia");
                assert_eq!(sent.transport, crate::relay::InferenceTransport::HttpsLink);
                assert_eq!(sent.meta["token_id"], json!(1234));
            }
        }));
    }
    for session in sessions {
        session.await.expect("sessão");
    }
    assert_eq!(
        counters.connections.load(Ordering::Relaxed),
        1,
        "seis sessões e sessenta travessias têm que caber numa ligação"
    );
    assert_eq!(crate::link::channels_opened("lab-multiplexado", &url), 1);
}
