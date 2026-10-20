//! Law bench: **the cost per token cannot depend on where the
//! pipeline's head is**.
//!
//! It brings up fake hops that speak the real protocol ([`crate::wire`],
//! [`crate::relay::send_frame`], the `next_hops` chaining) and are driven
//! by the real orchestration code ([`crate::prefill::run`] and
//! [`crate::prefill::step`]). The relay and the delivery of the direct response are the
//! production functions in person
//! ([`crate::infer_server::forward_direct`], [`crate::direct::deliver_result`]),
//! and the fake gateway serves the production handler of `/v1/relay-result`. The
//! only pretence is the engine: each hop sleeps instead of decoding.
//!
//! The network is emulated by *site*: each hop and the gateway live at a site, and every
//! message between different sites costs [`LINK_MS`] of **one-way**
//! latency - charged on arrival and again on return, because both directions
//! cross the link. That is what makes the bench measure traversals instead of
//! measuring the machine it runs on: with the computation at zero, the time per token is the
//! number of network legs times `LINK_MS`.
//!
//! It inherits from the investigation of 2026-08-14 (`labsse.rs`), which proved the asymmetry
//! and refuted the SSE hypothesis by counterfactual. Here the bench stopped being a
//! loose experiment and came to guard the fix.

#![cfg(test)]

use crate::pipeline::{PipelinePlan, PipelineStep};
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Same as the Qwen3.8-27B of the captain's cluster.
const HIDDEN_DIM: usize = 5120;
/// One-way latency between two sites.
const LINK_MS: u64 = 50;
/// Prompt tokens in the fake. Larger than `CHUNK` on purpose: a prompt of more
/// than one piece is what exposes a badly relayed `prompt_progress`.
const PROMPT_TOKENS: u32 = 3;
const CHUNK: u32 = 2;
/// Decode tokens measured per scenario.
const DECODE_TOKENS: u32 = 6;

// ── The fake hop ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Hop {
    /// Where this hop lives.
    site: usize,
    /// Where whoever calls it lives. The chain is fixed, so this is known when
    /// assembling the scenario: whoever calls hop `i` is hop `i-1`, and hop 0 is
    /// called by the gateway.
    caller_site: usize,
    /// This hop tokenized the prompt (it is the head).
    is_head: bool,
    /// Positions received by the head. The bench refuses the absence of an absolute
    /// position in the following chunks, as `expected_prefill_pos` does in
    /// production, and the test compares the values with the published progression.
    received_starts: Arc<Mutex<Vec<(u32, Option<u32>, u32)>>>,
    auth: Option<AuthProbe>,
}

/// Observes the authentication boundary without touching the real network. Each hop must
/// receive only the `connection_key`; the regression made the first hop use the
/// session key and left the next hop without a credential.
#[derive(Clone)]
struct AuthProbe {
    accepted_tokens: Vec<String>,
    missing: Arc<AtomicU64>,
    invalid: Arc<AtomicU64>,
}

fn auth_failure(headers: &HeaderMap, probe: Option<&AuthProbe>) -> Option<Response> {
    let probe = probe?;
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        });
    let internal = headers
        .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some("1");
    match token {
        None => {
            probe.missing.fetch_add(1, Ordering::Relaxed);
            Some((StatusCode::UNAUTHORIZED, "missing inference credential").into_response())
        }
        Some(token)
            if internal
                && probe
                    .accepted_tokens
                    .iter()
                    .any(|accepted| accepted == token) =>
        {
            None
        }
        Some(_) => {
            probe.invalid.fetch_add(1, Ordering::Relaxed);
            Some((StatusCode::FORBIDDEN, "invalid inference API key").into_response())
        }
    }
}

impl Hop {
    /// Arrival of a message: pays the link if it came from another site.
    async fn arrive(&self) {
        if self.site != self.caller_site {
            tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
        }
    }

    /// Return of the response over the same link. On the ring this is on the critical
    /// path (the response is the result); in direct mode it is only the accept, and it leaves
    /// the path on its own - nobody waits for it.
    async fn depart(&self) {
        if self.site != self.caller_site {
            tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
        }
    }
}

fn frame(meta: &Value, tensor: Option<(&[f32], TensorShape)>) -> Response {
    match wire::encode(meta, tensor, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn fake_health() -> impl IntoResponse {
    Json(json!({"status": "ok", "model_loaded": true, "hidden_dim": HIDDEN_DIM}))
}

async fn fake_release() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

/// What every handler needs to know about the frame that arrived, read with the same
/// rules as the real `infer_server`.
struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    next_hops: Vec<PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
}

fn read(body: &Bytes) -> Incoming {
    let (meta, _tensor) = wire::decode(body).expect("valid frame");
    let declared: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = declared.as_ref().map(|h| h.is_empty()).unwrap_or_else(|| {
        meta.get("is_last")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    let reply_to = crate::infer_server::direct_reply_to(&meta);
    Incoming {
        inbound_bytes: body.len(),
        next_hops: declared.unwrap_or_default(),
        is_last,
        reply_to,
        meta,
    }
}

/// Mirrors `infer_server::relay_and_bubble` - the old path, which serves as the
/// baseline against which the fix is measured.
async fn bubble(
    hops: &[PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], TensorShape),
    local_processing_ms: u64,
) -> Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);

    let sent = crate::relay::send_frame(
        &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
        &next.node_id,
        &next.inference_url,
        path,
        &base_meta,
        Some(tensor),
        WireDtype::F16,
        next.frame_encoding(),
        1,
        &next.node_id,
        |_| {},
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(local_processing_ms);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.arrive().await;
    let inc = read(&body);
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(CHUNK.into()) as u32;
    let is_final_chunk = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);

    // Only chunk 0 may omit `start_pos`: the session is the reference in that
    // case. Every later chunk needs to carry the absolute position the
    // head just published - deriving it from the index would hide exactly the
    // defect this bench is meant to catch.
    let supplied_start = inc
        .meta
        .get("start_pos")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let cached_start =
        if hop.is_head && chunk_index == 0 && inc.meta["reuse_prefix"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        };
    let start_pos = match (supplied_start, chunk_index) {
        (Some(actual), _) => actual,
        (None, 0) => cached_start,
        (None, _) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("chunk {chunk_index} without an absolute start_pos"),
            )
                .into_response();
        }
    };
    let n_tokens = PROMPT_TOKENS
        .saturating_sub(start_pos)
        .min(chunk_size)
        .max(1) as usize;
    let next_pos = start_pos + n_tokens as u32;
    if hop.is_head {
        hop.received_starts
            .lock()
            .unwrap()
            .push((chunk_index, supplied_start, start_pos));
    }
    let more = hop.is_head && next_pos < PROMPT_TOKENS;

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    local["start_pos"] = json!(start_pos);
    local["next_pos"] = json!(next_pos);
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(PROMPT_TOKENS);
        local["reuse_prefix"] = inc.meta["reuse_prefix"].clone();
        local["cached_tokens"] = json!(cached_start);
    }

    if inc.is_last {
        local["done"] = json!(false);
        if is_final_chunk {
            local["token_id"] = json!(1234);
            local["text"] = json!("x");
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];
    let progress = hop
        .is_head
        .then(|| crate::infer_server::prompt_progress_for_forward(&inc.meta, &local));

    if let Some(reply_to) = inc.reply_to {
        let hop_timings = crate::direct::accumulate(
            &inc.meta,
            crate::direct::hop_entry(
                &local["processing_ms"],
                &local["node_timings"],
                inc.inbound_bytes,
            ),
        );
        if inc.next_hops.is_empty() {
            let mut result = local;
            crate::infer_server::apply_prompt_progress(&inc.meta, &mut result);
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(
                    &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
                    &reply_to,
                    result,
                )
                .await;
            });
        } else {
            let forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "reuse_prefix": inc.meta["reuse_prefix"].clone(),
                "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
                "start_pos": local["start_pos"].clone(),
                "next_pos": local["next_pos"].clone(),
                "prompt_progress": crate::infer_server::prompt_progress_for_forward(&inc.meta, &local),
                "hop_timings": hop_timings,
            });
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/prefill",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
            ));
        }
        hop.depart().await;
        return crate::infer_server::direct_ack_with_progress(progress);
    }

    // ── Old path (bubbled ring or hub), for comparison ──────────────────────
    let response = if inc.next_hops.is_empty() {
        if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        }
    } else {
        let base = json!({
            "session_id": inc.meta["session_id"].clone(),
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": inc.meta["reuse_prefix"].clone(),
            "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
            "start_pos": local["start_pos"].clone(),
            "next_pos": local["next_pos"].clone(),
        });
        match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape), 0).await {
            Ok(mut bubbled) => {
                crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
                frame(&bubbled, None)
            }
            Err(resp) => resp,
        }
    };
    hop.depart().await;
    response
}

async fn auth_prefill(State(hop): State<Hop>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = auth_failure(&headers, hop.auth.as_ref()) {
        return response;
    }
    fake_prefill(State(hop), body).await
}

async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.arrive().await;
    let inc = read(&body);
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        local["token_id"] = json!(1234);
        local["text"] = json!("x");
        local["done"] = json!(false);
    }
    let residual = vec![0.0f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    if let Some(reply_to) = inc.reply_to {
        let hop_timings = crate::direct::accumulate(
            &inc.meta,
            crate::direct::hop_entry(
                &local["processing_ms"],
                &local["node_timings"],
                inc.inbound_bytes,
            ),
        );
        if inc.next_hops.is_empty() {
            let mut result = local;
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(
                    &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
                    &reply_to,
                    result,
                )
                .await;
            });
        } else {
            let forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "hop_timings": hop_timings,
            });
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/step",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
            ));
        }
        hop.depart().await;
        return crate::infer_server::direct_ack();
    }

    let response = if inc.next_hops.is_empty() {
        if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        }
    } else {
        let base = json!({"session_id": inc.meta["session_id"].clone()});
        match bubble(&inc.next_hops, "/v1/step", base, (&residual, shape), 0).await {
            Ok(bubbled) => frame(&bubbled, None),
            Err(resp) => resp,
        }
    };
    hop.depart().await;
    response
}

async fn spawn_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(fake_prefill))
        .route("/v1/step", post(fake_step))
        .route("/v1/session/release", post(fake_release))
        .route("/v1/session/rollback", post(fake_release))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

async fn spawn_auth_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(auth_prefill))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── The fake gateway ──────────────────────────────────────────────────────────

/// Serves the production handler of `/v1/relay-result`, paying the link between the
/// tail and the gateway's site - the new leg, the one that replaces the climb through the
/// nested stack.
#[derive(Clone)]
struct GatewaySite {
    inbox: crate::direct::ReplyInbox,
    site: usize,
    tail_site: usize,
    deliveries: Arc<AtomicU64>,
    prefill_deliveries: Arc<AtomicU64>,
    auth: Option<AuthProbe>,
}

async fn fake_relay_result(
    State(gw): State<GatewaySite>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = auth_failure(&headers, gw.auth.as_ref()) {
        return response;
    }
    let (meta, _) = wire::decode(&body).expect("valid direct result");
    let is_transport_update = meta
        .get("transport_update")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_transport_update && gw.site != gw.tail_site {
        tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
    }
    if !is_transport_update {
        gw.deliveries.fetch_add(1, Ordering::Relaxed);
        if meta.get("next_pos").is_some() {
            gw.prefill_deliveries.fetch_add(1, Ordering::Relaxed);
        }
    }
    crate::direct::handle_relay_result(&gw.inbox, body)
}

async fn spawn_gateway(gw: GatewaySite) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route(crate::direct::RELAY_RESULT_PATH, post(fake_relay_result))
        .with_state(gw);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── Scenario ──────────────────────────────────────────────────────────────────

fn step_for(node_id: &str, url: &str, start: u32, end: u32, direct: bool) -> PipelineStep {
    // The fake endpoint below implements the single-frame wire contract. Do
    // not advertise batching unless the fixture also implements the batch
    // request and response envelope.
    let mut caps = vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()];
    if direct {
        caps.push(crate::pipeline::CAP_RELAY_DIRECT.to_string());
    }
    PipelineStep {
        node_id: node_id.into(),
        inference_url: url.into(),
        inference_candidates: vec![],
        quic_addr_candidates: vec![],
        start_layer: start,
        end_layer: end,
        exec_start: start,
        exec_end: end,
        inference_frame_capabilities: caps,
        endpoint_id: None,
        endpoint_addr_candidates: Vec::new(),
    }
}

struct Outcome {
    ms_per_token: f64,
    deliveries: u64,
    prefill_deliveries: u64,
    prefill_chunks: u32,
    head_starts: Vec<(u32, Option<u32>, u32)>,
}

/// Runs `n_hops` hops, each in its own site, with the gateway at the site of hop
/// `entry`. `direct` chooses between the fix and the bubbled ring.
async fn run_scenario(n_hops: usize, entry: usize, direct: bool, reuse_prefix: bool) -> Outcome {
    let gateway_site = entry;
    let deliveries = Arc::new(AtomicU64::new(0));
    let prefill_deliveries = Arc::new(AtomicU64::new(0));

    let inbox = crate::direct::ReplyInbox::new();
    let gateway_url = spawn_gateway(GatewaySite {
        inbox: inbox.clone(),
        site: gateway_site,
        tail_site: n_hops - 1,
        deliveries: deliveries.clone(),
        prefill_deliveries: prefill_deliveries.clone(),
        auth: None,
    })
    .await;

    let mut steps = Vec::new();
    let mut head_starts = Arc::new(Mutex::new(Vec::new()));
    for i in 0..n_hops {
        // Whoever calls hop `i` is hop `i-1`; hop 0 is called by the gateway.
        let caller_site = if i == 0 { gateway_site } else { i - 1 };
        let received_starts = Arc::new(Mutex::new(Vec::new()));
        if i == 0 {
            head_starts = received_starts.clone();
        }
        let url = spawn_hop(Hop {
            site: i,
            caller_site,
            is_head: i == 0,
            received_starts,
            auth: None,
        })
        .await;
        steps.push(step_for(
            &format!("hop{i}"),
            &url,
            i as u32,
            i as u32,
            direct,
        ));
    }

    let plan = PipelinePlan {
        steps,
        edges: vec![],
    };
    let session = uuid::Uuid::new_v4().to_string();
    let reply = crate::direct::DirectReply::new(inbox, vec![gateway_url], "gateway".into());
    let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));
    let routes = crate::route::RouteTable::new();

    let first = crate::prefill::run_with_options(
        &plan,
        "lab",
        &session,
        "um prompt qualquer",
        &[],
        true,
        true,
        CHUNK,
        WireDtype::F16,
        &http,
        &routes,
        reply.as_ref(),
        None,
        &crate::prefill::Cancellation::new(),
        reuse_prefix,
        crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS,
        None,
        None,
        None,
    )
    .await
    .expect("prefill");
    assert_eq!(
        first.prompt_tokens, PROMPT_TOKENS,
        "the prompt count must arrive from the head to the gateway"
    );
    assert_eq!(
        first.chunks,
        if reuse_prefix {
            PROMPT_TOKENS.saturating_sub(1).div_ceil(CHUNK)
        } else {
            PROMPT_TOKENS.div_ceil(CHUNK)
        },
        "the prompt must cross in all the chunks — a lost `prompt_progress` truncates at the first"
    );

    let mut next = first.token_id;
    let started = Instant::now();
    for _ in 1..DECODE_TOKENS {
        let step = crate::prefill::step(
            &plan,
            "lab",
            &session,
            next,
            WireDtype::F16,
            &http,
            &routes,
            reply.as_ref(),
            None,
            None,
            None,
        )
        .await
        .expect("step");
        next = step.accepted[0].0;
    }

    let head_starts = head_starts.lock().unwrap().clone();
    Outcome {
        ms_per_token: started.elapsed().as_millis() as f64 / (DECODE_TOKENS - 1) as f64,
        deliveries: deliveries.load(Ordering::Relaxed),
        prefill_deliveries: prefill_deliveries.load(Ordering::Relaxed),
        prefill_chunks: first.chunks,
        head_starts,
    }
}

/// Reproduces the regression of `f9d5632`: the gateway authenticates the first hop with
/// the session key, but the intermediate hop does not receive a credential to talk
/// to the next one. When reporting the failure, the tail also reaches the callback without an
/// API key and hits the second 401. The scenario stays end to end to
/// guarantee that the frame reaches the tail and the result is delivered to the inbox.
#[tokio::test(flavor = "multi_thread")]
async fn direct_pipeline_reproduces_both_missing_credentials() {
    let session_key = format!("sk-session-{}", uuid::Uuid::new_v4().simple());
    let connection_key = "cluster-secret".to_string();
    let tail_missing = Arc::new(AtomicU64::new(0));
    let accepted_tokens = vec![connection_key.clone()];

    let inbox = crate::direct::ReplyInbox::new();
    let gateway_url = spawn_gateway(GatewaySite {
        inbox: inbox.clone(),
        site: 1,
        tail_site: 1,
        deliveries: Arc::new(AtomicU64::new(0)),
        prefill_deliveries: Arc::new(AtomicU64::new(0)),
        // The callback is authenticated by the per-frame Ticket in the body.
        auth: None,
    })
    .await;

    let head_missing = Arc::new(AtomicU64::new(0));
    let head_url = spawn_auth_hop(Hop {
        site: 0,
        caller_site: 1,
        is_head: true,
        received_starts: Arc::new(Mutex::new(Vec::new())),
        auth: Some(AuthProbe {
            accepted_tokens: accepted_tokens.clone(),
            missing: head_missing.clone(),
            invalid: Arc::new(AtomicU64::new(0)),
        }),
    })
    .await;
    let tail_url = spawn_auth_hop(Hop {
        site: 1,
        caller_site: 0,
        is_head: false,
        received_starts: Arc::new(Mutex::new(Vec::new())),
        auth: Some(AuthProbe {
            accepted_tokens,
            missing: tail_missing.clone(),
            invalid: Arc::new(AtomicU64::new(0)),
        }),
    })
    .await;

    let steps = vec![
        step_for("head", &head_url, 0, 0, true),
        step_for("tail", &tail_url, 1, 1, true),
    ];
    let session = uuid::Uuid::new_v4().to_string();
    let reply = crate::direct::DirectReply::new(inbox.clone(), vec![gateway_url], "gateway".into())
        .expect("test gateway must be reachable");
    let ticket = reply.ticket_for_hops(steps.len());
    let meta = json!({
        "session_id": session,
        "chunk_index": 0,
        "chunk_size": CHUNK,
        "is_final_chunk": true,
        "next_hops": [steps[1].clone()],
        "reply_to": reply.reply_to(ticket.id()),
    });

    let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
    {
        let mut config = state.auth.write().await;
        config.connection_key = connection_key;
        config.api_keys.push(crate::config::ApiKey {
            id: "session-key".into(),
            key_hash: crate::auth::api_key_hash(&session_key.clone()),
            key_hint: String::new(),
            label: "direct-lab".into(),
            node_id: "gateway".into(),
            origin_node_id: "gateway".into(),
            active: true,
            created_at: "2026-09-07T00:00:00Z".into(),
        });
    }
    let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));
    http.set_auth_state(state.auth.clone());
    crate::infer_server::http_client().set_auth_state(state.auth.clone());

    let sent = crate::relay::send_frame_or_tunnel(
        &http,
        &steps[0],
        None,
        "/v1/prefill",
        &meta,
        None,
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        "head",
        |_| {},
    )
    .await
    .expect("the first hop must accept the frame");
    assert_eq!(sent.meta["accepted"], true);

    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if inbox.outstanding() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(observed.is_ok(), "the result did not reach the gateway");
    assert_eq!(head_missing.load(Ordering::Relaxed), 0);
    assert_eq!(tail_missing.load(Ordering::Relaxed), 0);
}

/// Network legs paid per token, read from the clock. With the computation at zero, the
/// time per token is the number of traversals times `LINK_MS`.
fn legs(o: &Outcome) -> f64 {
    o.ms_per_token / LINK_MS as f64
}

/// The minimum possible: the gateway has to reach the head and hear the tail, and
/// between the hops there is no shortcut. One leg fewer at each end when the
/// gateway lives there.
fn minimum_legs(n_hops: usize, entry: usize) -> f64 {
    let inner = (n_hops - 1) as f64;
    let to_head = if entry == 0 { 0.0 } else { 1.0 };
    let from_tail = if entry == n_hops - 1 { 0.0 } else { 1.0 };
    inner + to_head + from_tail
}

// ── The law ───────────────────────────────────────────────────────────────────

/// **The cost per token does not depend on where the entry point is.**
///
/// This is the whole fix, measured. For each chain size, with the
/// gateway at the head, in the middle and at the tail, the time per token has to be the
/// theoretical minimum - one traversal per link of the chain, plus the trip to the
/// head and the return from the tail when the gateway does not live at one of the ends.
///
/// The proof that this is a fix, and not the behaviour as it always was, is in
/// the last assertion: on the bubbled ring the same chain with the gateway at the
/// tail pays double the traversals, which was the drop from 25 to 14 tok/s
/// measured on the captain's cluster on 2026-08-14.
///
/// A one-leg tolerance (50 ms with `LINK_MS` 50) absorbs the overhead of
/// HTTP calls on loopback with tensors of 5120 positions; it remains
/// smaller than the whole leg that separates a right result from a wrong one.
#[tokio::test(flavor = "multi_thread")]
async fn the_cost_per_token_does_not_depend_on_where_the_entry_point_is() {
    const TOLERANCE: f64 = 1.0;

    for n_hops in 2..=4usize {
        let mut measured = Vec::new();
        for entry in 0..n_hops {
            let outcome = run_scenario(n_hops, entry, true, false).await;
            let expected = minimum_legs(n_hops, entry);
            let got = legs(&outcome);
            println!(
                "direct  N={n_hops} entry={entry}  {:6.1} ms/token  = {got:.2} legs (minimum {expected:.0})",
                outcome.ms_per_token
            );
            assert!(
                (got - expected).abs() < TOLERANCE,
                "N={n_hops} entry={entry}: {got:.2} legs per token, expected {expected:.0}"
            );
            assert_eq!(
                outcome.deliveries,
                DECODE_TOKENS as u64 - 1 + outcome.prefill_chunks as u64,
                "each frame must be delivered directly exactly once, in the decode and in the prefill"
            );
            measured.push(got);
        }

        // The two ends are the captain's case, and have to tie: with N=2 it is
        // exactly order A against order B.
        let head = measured[0];
        let tail = measured[n_hops - 1];
        assert!(
            (head - tail).abs() < TOLERANCE,
            "N={n_hops}: entering through the head cost {head:.2} legs and through the tail {tail:.2}"
        );
        // Entering through the middle costs one extra leg and never more than that - it is the
        // irreducible price of not being at either of the two ends.
        let worst = measured.iter().cloned().fold(f64::MIN, f64::max);
        assert!(
            worst - head < 1.0 + TOLERANCE,
            "N={n_hops}: the worst entry cost {worst:.2} legs against {head:.2} at the head"
        );
    }

    // The baseline: the bubbled ring, with the gateway at the tail, pays double.
    let ring_tail = run_scenario(2, 1, false, false).await;
    let direct_tail = run_scenario(2, 1, true, false).await;
    println!(
        "ring    N=2 entry=1  {:6.1} ms/token  = {:.2} legs",
        ring_tail.ms_per_token,
        legs(&ring_tail)
    );
    assert!(
        (legs(&ring_tail) - 4.0).abs() < TOLERANCE,
        "the ring with the remote head should pay 4 traversals, it paid {:.2}",
        legs(&ring_tail)
    );
    assert!(
        legs(&direct_tail) < legs(&ring_tail) - 1.0,
        "the direct reply must be cheaper than the ring in the case that hurt"
    );
    assert_eq!(
        ring_tail.deliveries, 0,
        "the ring does not use the direct delivery"
    );
}

/// The long prompt needs to carry the absolute position the head published
/// inside the direct ACK, both in a cold session and after reusing a
/// prefix. This scenario exercises the same production chain and makes the fake hops
/// refuse any continuation without `start_pos`.
#[tokio::test(flavor = "multi_thread")]
async fn direct_prefill_preserves_the_absolute_position_with_and_without_reuse() {
    for reuse_prefix in [false, true] {
        let outcome = run_scenario(2, 1, true, reuse_prefix).await;
        assert_eq!(
            outcome.deliveries,
            DECODE_TOKENS as u64 - 1 + outcome.prefill_chunks as u64
        );
        assert_eq!(
            outcome.prefill_deliveries, outcome.prefill_chunks as u64,
            "each prefill chunk must reach the gateway exactly once"
        );
        let mut starts = outcome.head_starts;
        starts.sort_by_key(|(chunk, _, _)| *chunk);
        if reuse_prefix {
            assert_eq!(starts, vec![(0, None, 1)]);
        } else {
            assert_eq!(starts[0], (0, None, 0));
            assert_eq!(starts[1], (1, Some(2), 2));
        }
    }
}
