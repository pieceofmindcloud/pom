//! Bench of the network cost per token on the **internal hop**.
//!
//! What it measures is not the engine nor the latency: it is how many bytes a
//! token costs **on the socket**, per pipeline-edge traversal, and how many
//! connections the path opens to produce N tokens. That is why each fake hop is
//! served on top of a `TcpStream` wrapped in a counter
//! ([`Counted`]) - the number comes from neither an estimate nor `Content-Length`, it comes
//! from the bytes that really passed.
//!
//! The engine is fake on purpose: a hop that really decodes would measure the
//! GPU, not the transport. What is real here is everything the transport uses -
//! [`crate::wire`], the [`crate::relay::BatchCoordinator`], the
//! [`crate::relay::send_frame_or_tunnel`] and the [`crate::link`] channel.
//!
//! The two configurations compared are the **same bench with a one-bit
//! difference**: the hop advertises `transport/link-v1` or it does not. Without it, each token
//! rebuilds a `POST /v1/step`; with it, successive tokens travel as
//! frames in a single streamed body.

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

/// Same as the captain cluster's Qwen3.8-27B, as in `direct_lab`.
const HIDDEN_DIM: usize = 5120;
const DECODE_TOKENS: usize = 40;

// ── Byte counter on the socket ───────────────────────────────────────────────

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

/// Answers a batch frame with one item per item received, forwarding to the
/// next hop when the frame carries `next_hops` - exactly the shape of
/// production's `relay_and_bubble`, with the engine swapped for nothing.
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
            if !matches!(path.as_str(), "/v1/step" | "/v1/prefill") {
                return Err(anyhow::anyhow!("unexpected route on the channel: {path}"));
            }
            Ok(fake_step(State(hop), bytes).await)
        }
    })
}

/// Brings up a hop and returns `(url, counters)`. `serve_connection` is called
/// by hand because it is the only way to put the counter **under** the HTTP.
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
        quic_addr_candidates: vec![],
        start_layer: layer,
        end_layer: layer,
        exec_start: layer,
        exec_end: layer,
        inference_frame_capabilities: caps,
        endpoint_id: None,
        endpoint_addr_candidates: Vec::new(),
    }
}

// ── The measurement ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct Measurement {
    /// Bytes read and written on the first hop's socket, per token.
    bytes_per_token_first_hop: f64,
    /// Same, summing all the hops of the chain.
    bytes_per_token_all_hops: f64,
    /// TCP connections opened against the first hop.
    connections_first_hop: u64,
    /// Size of the application frame sent on the first traversal, and how much
    /// of it is the hidden state. It is what separates the transport cost from the
    /// irreducible cost of the tensor.
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

    // Warm-up: the first traversal opens the connection (and the channel), and the
    // setup cost is not a per-token cost.
    let mut transport = "";
    let warm = forward(&clients, &steps, &session, (&residual, shape.clone()))
        .await
        .expect("warmup");
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
        .expect("traversal");
        transport = match sent.transport {
            crate::relay::InferenceTransport::HttpsLink => "https-link",
            crate::relay::InferenceTransport::Https => "https",
            crate::relay::InferenceTransport::Http => "http",
            other => {
                panic!("unexpected transport on the bench: {}", other.as_str())
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

/// The law of this bench: **the persistent link must not cost more bytes per
/// token than the per-token request**, and it must cross without opening a new
/// connection per token.
///
/// The absolute number is recorded by `println!` to go into the commit and the
/// documentation; what the test guards is the comparison, which does not depend
/// on the machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_persistent_link_costs_fewer_bytes_per_token() {
    for hops in [1usize, 2, 3] {
        let before = measure(hops, false, DECODE_TOKENS).await;
        let after = measure(hops, true, DECODE_TOKENS).await;
        for (label, m) in [("antes ", &before), ("depois", &after)] {
            println!(
                "hops={hops} {label} transport={:<10} bytes/token(1st hop)={:.1} bytes/token(chain)={:.1} frame={} hidden_state={} connections={} ms/token={:.3}",
                m.transport,
                m.bytes_per_token_first_hop,
                m.bytes_per_token_all_hops,
                m.frame_bytes,
                m.hidden_state_bytes,
                m.connections_first_hop,
                m.ms_per_token
            );
        }
        assert_eq!(before.transport, "http");
        assert_eq!(after.transport, "https-link");
        assert_eq!(
            after.connections_first_hop, 0,
            "the persistent channel cannot open a connection per token"
        );
        assert!(
            after.bytes_per_token_first_hop < before.bytes_per_token_first_hop,
            "hops={hops}: the channel spent {:.1} B/token against {:.1} B/token of the per-token request",
            after.bytes_per_token_first_hop,
            before.bytes_per_token_first_hop
        );
        assert!(
            after.bytes_per_token_all_hops < before.bytes_per_token_all_hops,
            "hops={hops}: the whole chain saved nothing"
        );
    }
}

/// The MTP handoff has extra metadata, but it is still a `/v1/step` frame
/// compatible with the persistent channel's envelope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtp_handoff_uses_the_persistent_link() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));
    let (url, _counter) = spawn_hop(Hop {
        clients: clients.clone(),
    })
    .await;
    let step = step_for("mtp-tail", &url, 0, true);
    let residual = vec![0.25f32; HIDDEN_DIM];
    let sent = crate::relay::send_frame_or_tunnel(
        &clients,
        &step,
        None,
        "/v1/step",
        &json!({
            "session_id": "mtp-session",
            "is_last": true,
            "tokens": [41, 42, 43],
            "draft": [42, 43],
            "mtp_k": 3
        }),
        Some((&residual, vec![1, HIDDEN_DIM])),
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        "mtp-tail",
        |_| {},
    )
    .await
    .expect("handoff MTP");

    assert_eq!(sent.transport, crate::relay::InferenceTransport::HttpsLink);
}

/// The direct prefill carries `reply_to`, so it cannot enter the
/// `BatchCoordinator`. It still needs to use the same persistent link as the
/// decode, or the first handoff would keep paying p2p per chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_prefill_handoff_uses_the_persistent_link() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));
    let (url, _counter) = spawn_hop(Hop {
        clients: clients.clone(),
    })
    .await;
    let node_id = format!("prefill-link-{}", uuid::Uuid::new_v4());
    let step = step_for(&node_id, &url, 0, true);
    let residual = vec![0.25f32; HIDDEN_DIM];
    let sent = crate::relay::send_frame_or_tunnel(
        &clients,
        &step,
        None,
        "/v1/prefill",
        &json!({
            "session_id": "prefill-session",
            "reply_to": {"ticket": "gateway-ticket"},
            "is_last": true,
            "is_final_chunk": true,
        }),
        Some((&residual, vec![1, HIDDEN_DIM])),
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        &node_id,
        |_| {},
    )
    .await
    .expect("prefill handoff");

    assert_eq!(sent.transport, crate::relay::InferenceTransport::HttpsLink);
    assert_eq!(crate::link::channels_opened(&node_id, &url), 1);
}

/// A hop that advertises the channel but does not serve it must not kill the generation: the
/// traversal degrades to the per-request path and the telemetry says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unavailable_channel_degrades_to_the_per_request_path() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Only `/v1/step`: `/v1/link` answers 404 and the channel does not open.
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
    .expect("the generation continues through the usual path");
    assert_eq!(sent.transport, crate::relay::InferenceTransport::Http);
    assert!(
        sent.transport_fallback,
        "the telemetry must report that the channel did not work"
    );
    assert_eq!(sent.meta["token_id"], json!(1234));
    assert!(
        !crate::link::has_channel("lab-sem-canal", &url).await,
        "a channel that did not open cannot stay in the pool"
    );
}

/// The internal hop of a `remote-stream` is still autoregressive, but it does not
/// need to rebuild a POST on every activation. The link capability must
/// also hold when the `remote_stream` flag prevents coalescing between
/// sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_internal_remote_stream_reuses_the_persistent_link() {
    let clients = crate::peer_client::PeerClients::new(Duration::from_secs(30));
    let (url, counters) = spawn_hop(Hop {
        clients: clients.clone(),
    })
    .await;
    let step = step_for("lab-remote-stream-link", &url, 0, true);
    let residual = vec![0.25f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    for token_id in 0..4 {
        let meta = json!({
            "session_id": "sessao-remote-stream",
            "remote_stream": true,
            "token_id": token_id,
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
            Some((&residual, shape.clone())),
            WireDtype::F16,
            WireDtype::F16.frame_encoding(),
            1,
            &step.node_id,
            |_| {},
        )
        .await
        .expect("internal traversal");
        assert_eq!(
            sent.transport,
            crate::relay::InferenceTransport::HttpsLink,
            "remote-stream must use the persistent channel when announced"
        );
        assert_eq!(sent.meta["token_id"], json!(1234));
    }

    assert_eq!(
        counters.connections.load(Ordering::Relaxed),
        1,
        "four internal activations must share one connection"
    );
    assert_eq!(
        crate::link::channels_opened("lab-remote-stream-link", &url),
        1
    );
}

/// Two sessions crossing the same edge share **one** link, and the
/// `batch_id` is what keeps them separate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_share_the_link() {
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
                .expect("traversal");
                assert_eq!(sent.transport, crate::relay::InferenceTransport::HttpsLink);
                assert_eq!(sent.meta["token_id"], json!(1234));
            }
        }));
    }
    for session in sessions {
        session.await.expect("session");
    }
    assert_eq!(
        counters.connections.load(Ordering::Relaxed),
        1,
        "six sessions and sixty traversals must fit in one link"
    );
    assert_eq!(crate::link::channels_opened("lab-multiplexado", &url), 1);
}
