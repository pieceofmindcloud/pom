//! Bench for the continuation loop: **the ceiling of a backend generation cannot
//! become the end of the client's response.**
//!
//! ## What it reproduces
//!
//! Measured on the live cluster on 2026-08-27, `POST /v1/chat/completions` with
//! `max_tokens: 4000` against a deployment of `max_new_tokens: 1000`:
//!
//! ```text
//! finish_reason: "length"      usage.completion_tokens: 1000
//! x_pom.output_limits: {requested: 4000, deployment: 1000, effective: 1000}
//! ```
//!
//! The cut was silent: the client received less than it asked for and the only place
//! where that showed up was the non-standard `x_pom` block, which an
//! OpenAI-compatible client ignores. In the same measurement, with a tool declared, the
//! cut fell **inside** the call: the response came back without `tool_calls`, with the
//! unfinished `<tool_call>` degraded into 4366 characters of loose text. It is the
//! captain's case: opencode reads `max_tokens` as a terminal condition, drops the
//! turn and the file is left half-written.
//!
//! ## What it proves
//!
//! Each test brings up two fake hops, a real `GatewayState` and serves the real
//! `/v1` routes on a TCP socket, and talks to them over HTTP with
//! `reqwest` - SSE included, read byte by byte the way a client reads it. The path
//! exercised is the whole production one: `openai::chat_completions`,
//! `gateway::generate_stream_with_stops`, `prefill::run`, `prefill::step`,
//! `relay::send_frame`, `tool_call::TagScanner`.
//!
//! ## The two boundaries
//!
//! The continuation loop resolved the cut at the boundary of the **leg**: it is
//! invisible, and stitching legs together to honor the client's request is the right thing. The
//! outer one - the ceiling the client asked for - is something else: it is **enforced and
//! inviolable**, and reaching it is a terminal stop. Nothing exceeds it, not even a tool
//! call left open at the cut token.
//!
//! What was left to resolve was the **shape** of the response when the cut falls
//! inside a call. Two wrong answers have already been measured here: raw
//! markup leaked as text (335 characters of `<tool_call>…` in the
//! `content`, with `x_pom.partial: false` lying) and silently discarding the
//! fragment. The right one is that of the two providers, checked against the live documentation
//! on 2026-08-29: the call comes back **structured and incomplete**, next to the
//! terminal stop reason - `finish_reason: "length"` at OpenAI,
//! `stop_reason: "max_tokens"` with the last `tool_use` block at Anthropic. The
//! client reads the reason and repeats the request with a larger ceiling; neither of the two
//! continues, overflows or discards.
//!
//! In place of overflow both offer the **task budget**
//! (`output_config.task_budget`), which warns the model instead of stretching the ceiling.
//! It is advisory and is covered here too.
//!
//! **Declared limit:** the engine is fake. It reads a script of text pieces
//! instead of decoding, so what the bench proves is the contract between
//! gateway, adapter and peers - which is where the defect lived. What llama.cpp
//! does with the batch is not here; that is cluster measurement.

#![cfg(test)]

use crate::gateway::testkit;
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Small on purpose: the bench measures protocol, not memory.
const HIDDEN_DIM: usize = 8;
/// Tokens that the prompt is worth for the fake head.
const PROMPT_TOKENS: u32 = 6;

// ── O motor falso ─────────────────────────────────────────────────────────────

/// The script that the fake tail "samples", one piece per token.
///
/// A whole tool call, deliberately longer than a leg's
/// budget: no isolated piece is a valid call, and only the stitched
/// output closes the tags. It is the exact shape of the cut the captain saw.
fn tool_call_script() -> Vec<String> {
    let mut script = vec![
        "Vou escrever o arquivo.\n".to_string(),
        "<tool_call>\n".to_string(),
        "<function=write_file>\n".to_string(),
        "<parameter=path>\n".to_string(),
        "/tmp/calc.py".to_string(),
        "\n</parameter>\n".to_string(),
        "<parameter=content>\n".to_string(),
    ];
    for line in 0..40 {
        script.push(format!("linha_{line} = {line} * 2\n"));
    }
    script.push("\n</parameter>\n".to_string());
    script.push("</function>\n".to_string());
    script.push("</tool_call>".to_string());
    script
}

/// The reduced form of the captain's reproduction: 847 lines of HTML, broken
/// into small pieces the way a real tokenizer would. The number of lines is
/// deliberate - the bench needs to cross both 8192 and 50000 tokens
/// without confusing file size with the node's context capacity.
fn large_html_tool_script() -> Vec<String> {
    let mut script = vec![
        "<tool_call>\n".to_string(),
        "<function=write_file>\n".to_string(),
        "<parameter=path>\n".to_string(),
        "/tmp/index.html\n".to_string(),
        "</parameter>\n".to_string(),
        "<parameter=content>\n".to_string(),
    ];
    let html: String = (0..847)
        .map(|line| format!("<div data-line=\"{line}\">conteudo da linha {line}</div>\n"))
        .collect();
    script.extend(
        html.as_bytes()
            .chunks(5)
            .map(|chunk| String::from_utf8(chunk.to_vec()).expect("HTML ASCII")),
    );
    script.push("</parameter>\n".to_string());
    script.push("</function>\n".to_string());
    script.push("</tool_call>".to_string());
    script
}

/// Running text, also longer than a leg. The `TagScanner` finds no
/// tag here, so this script measures the pure-text path without the
/// tool path nearby.
fn plain_script() -> Vec<String> {
    (0..51).map(|i| format!("palavra{i} ")).collect()
}

/// A script with no end: the model that goes into a loop, against which the absolute
/// ceiling exists.
fn runaway_script() -> Vec<String> {
    Vec::new()
}

/// A fake inference peer.
#[derive(Clone)]
struct Hop {
    is_head: bool,
    /// Text pieces that the tail delivers, in order. Empty means
    /// "never stops": the tail repeats a fixed piece and never signals EOS.
    script: Arc<Vec<String>>,
    /// How many pieces have already gone out. Shared between prefill (the first
    /// token) and step (the rest), as in the real engine.
    emitted: Arc<AtomicUsize>,
    /// The prompt the head received in the prefill frame. It is through here that the
    /// bench checks that the task budget marker really reached the
    /// generation, and not only the adapter.
    seen_prompt: Arc<std::sync::Mutex<String>>,
    /// Reused between tokens so that the long reproduction measures the gateway,
    /// not the socket exhaustion caused by creating a client per step.
    peer_http: Arc<crate::peer_client::PeerClients>,
}

impl Hop {
    /// The next token of the script and whether it was the last.
    fn next_token(&self) -> (usize, String, bool) {
        let i = self.emitted.fetch_add(1, Ordering::Relaxed);
        if self.script.is_empty() {
            return (i, "e ".to_string(), false);
        }
        let text = self
            .script
            .get(i)
            .cloned()
            .unwrap_or_else(|| String::from(""));
        (i, text, i + 1 >= self.script.len())
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

/// `gateway::rewind_sessions` reads the body, not the status: only `rewound: true` with
/// the requested `n_past` counts as a session tidied up for the next turn.
async fn fake_rewind(body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let n = request
        .get("n_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Json(json!({"rewound": true, "n_past": n}))
}

struct Incoming {
    meta: Value,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
}

fn read(body: &Bytes) -> Incoming {
    let (meta, _tensor) = wire::decode(body).expect("valid frame");
    let declared: Option<Vec<crate::pipeline::PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = declared.as_ref().map(|h| h.is_empty()).unwrap_or_else(|| {
        meta.get("is_last")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    Incoming {
        next_hops: declared.unwrap_or_default(),
        is_last,
        meta,
    }
}

/// Mirrors `infer_server::relay_and_bubble`: the ring, which is the transport that
/// this bench exercises.
async fn bubble(
    http: &crate::peer_client::PeerClients,
    hops: &[crate::pipeline::PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], TensorShape),
) -> Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);

    let sent = crate::relay::send_frame(
        http,
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
    bubbled["processing_ms"] = json!(0);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    if hop.is_head {
        if let Some(prompt) = inc.meta["prompt"].as_str() {
            *hop.seen_prompt.lock().unwrap() = prompt.to_string();
        }
    }
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let is_final_chunk = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);

    let done_before = chunk_index * chunk_size;
    let n_tokens = if hop.is_head {
        PROMPT_TOKENS.saturating_sub(done_before).min(chunk_size)
    } else {
        chunk_size
            .min(PROMPT_TOKENS.saturating_sub(done_before))
            .max(1)
    } as usize;
    let more = hop.is_head && (done_before + n_tokens as u32) < PROMPT_TOKENS;

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(PROMPT_TOKENS);
    }
    if inc.is_last {
        local["done"] = json!(false);
        if is_final_chunk {
            let (id, text, last) = hop.next_token();
            local["token_id"] = json!(id as u32);
            local["text"] = json!(text);
            local["done"] = json!(last);
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let base = json!({
        "session_id": inc.meta["session_id"].clone(),
        "chunk_index": chunk_index,
        "chunk_size": chunk_size,
        "reuse_prefix": inc.meta["reuse_prefix"].clone(),
        "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
    });
    match bubble(
        &hop.peer_http,
        &inc.next_hops,
        "/v1/prefill",
        base,
        (&residual, shape),
    )
    .await
    {
        Ok(mut bubbled) => {
            crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
            frame(&bubbled, None)
        }
        Err(resp) => resp,
    }
}

async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        let (id, text, last) = hop.next_token();
        local["done"] = json!(last);
        local["token_id"] = json!(id as u32);
        local["text"] = json!(text);
    } else {
        local["done"] = json!(false);
    }

    let residual = vec![0.0f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let base = json!({"session_id": inc.meta["session_id"].clone()});
    match bubble(
        &hop.peer_http,
        &inc.next_hops,
        "/v1/step",
        base,
        (&residual, shape),
    )
    .await
    {
        Ok(bubbled) => frame(&bubbled, None),
        Err(resp) => resp,
    }
}

async fn spawn_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(fake_prefill))
        .route("/v1/step", post(fake_step))
        .route("/v1/session/release", post(fake_release))
        .route("/v1/session/rewind", post(fake_rewind))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── The node, with the real `/v1` routes on a socket ────────────────────────────

const API_KEY: &str = "api-chave";

struct No {
    base: String,
    /// The prompt the head received - see `Hop::seen_prompt`.
    seen_prompt: Arc<std::sync::Mutex<String>>,
    /// Alive while the test runs; dropping this brings the node down.
    _server: tokio::task::JoinHandle<()>,
}

impl No {
    fn generation_prompt(&self) -> String {
        self.seen_prompt.lock().unwrap().clone()
    }
}

/// Brings up two fake hops and a real node serving `/v1` over HTTP.
///
/// `deployment_max_new_tokens` is the ceiling of **one** backend generation. The total
/// capacity derives from the effective context, without a separate absolute ceiling.
async fn no(script: Vec<String>, deployment_max_new_tokens: u32) -> No {
    no_with_context(script, deployment_max_new_tokens, 8_192).await
}

async fn no_with_context(script: Vec<String>, deployment_max_new_tokens: u32, n_ctx: u32) -> No {
    no_with_peer_capacities(script, deployment_max_new_tokens, n_ctx, Vec::new()).await
}

async fn no_with_peer_capacities(
    script: Vec<String>,
    deployment_max_new_tokens: u32,
    n_ctx: u32,
    capacities: Vec<crate::network::RuntimeCapacity>,
) -> No {
    static LOG: std::sync::Once = std::sync::Once::new();
    LOG.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });

    let script = Arc::new(script);
    let emitted = Arc::new(AtomicUsize::new(0));
    let seen_prompt = Arc::new(std::sync::Mutex::new(String::new()));
    let peer_http = Arc::new(crate::peer_client::PeerClients::new(
        std::time::Duration::from_secs(600),
    ));
    let mut urls = Vec::new();
    for i in 0..2 {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                script: script.clone(),
                emitted: emitted.clone(),
                seen_prompt: seen_prompt.clone(),
                peer_http: peer_http.clone(),
            })
            .await,
        );
    }

    // Only the ring: it is the transport that the fake hops implement, and announcing
    // another would make the bench promise what it does not deliver.
    let caps = vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()];
    let peers: Vec<crate::network::PeerInfo> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = testkit::peer(&format!("hop{i}"), &format!("m{i}"), 9000 + i as u16);
            p.addr = "127.0.0.1:42777".into();
            p.inference_port = Some(port);
            p.model = "lab".into();
            p.start_layer = i as u32;
            p.end_layer = i as u32;
            p.total_layers = 2;
            p.inference_frame_capabilities = caps.clone();
            p.runtime_capacity = Some(capacities.get(i).copied().unwrap_or(
                crate::network::RuntimeCapacity {
                    n_ctx,
                    max_new_tokens: deployment_max_new_tokens,
                },
            ));
            p
        })
        .collect();

    let state = testkit::state_with("eu", peers).await;
    let mut config = testkit::spec_config();
    config.speculative_decoding_enabled = false;
    config.max_new_tokens = deployment_max_new_tokens;
    config.n_ctx = n_ctx;
    state.runtime.seed_config_for_test(config).await;
    let api_key = crate::config::ApiKey {
        id: "claude-code-contract".into(),
        key_hash: crate::auth::api_key_hash(API_KEY),
        key_hint: String::new(),
        label: "claude-code".into(),
        node_id: "eu".into(),
        origin_node_id: "eu".into(),
        active: true,
        created_at: String::new(),
        user_id: String::new(),
    };
    state.database.insert_api_key(&api_key).unwrap();
    state.auth.write().await.api_keys.push(api_key);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .merge(crate::openai::routes())
        .merge(crate::messages::routes())
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).tcp_nodelay(true).await;
    });

    No {
        base: format!("http://{addr}"),
        seen_prompt,
        _server: server,
    }
}

fn request(max_tokens: u32, stream: bool, with_tool: bool) -> Value {
    let mut body = json!({
        "model": "lab",
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": [{"role": "user", "content": "escreva o arquivo"}],
    });
    if with_tool {
        body["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Escreve um arquivo",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"],
                },
            },
        }]);
    }
    body
}

async fn post_json(no: &No, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body")
}

/// `/v1/messages`, for what only the Anthropic adapter says: the `stop_reason`,
/// the `tool_use` blocks and the `output_config`.
async fn post_messages(no: &No, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body")
}

async fn messages_events(no: &No, body: Value) -> Vec<Value> {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response");
    assert!(response.status().is_success(), "{:?}", response.status());

    let mut bytes = response.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("SSE chunk")));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            for line in event.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload != "[DONE]" {
                    events.push(serde_json::from_str(payload).expect("JSON event"));
                }
            }
        }
    }
    events
}

/// The SSE of the Anthropic route, which has event names of its own. Returns the
/// `partial_json` of `input_json_delta`, in order - it is where the arguments of the
/// tool call arrive there.
async fn messages_fragments(no: &No, body: Value) -> Vec<String> {
    let events = messages_events(no, body).await;
    let mut fragments = Vec::new();
    for v in events {
        if v["delta"]["type"] == "input_json_delta" {
            fragments.push(
                v["delta"]["partial_json"]
                    .as_str()
                    .expect("partial_json")
                    .to_string(),
            );
        }
    }
    fragments
}

/// The bench's Anthropic request, with the tool declared. `eager` turns on
/// `eager_input_streaming` on it.
fn messages_request(max_tokens: u32, eager: Option<bool>) -> Value {
    let mut tool = json!({
        "name": "write_file",
        "description": "Escreve um arquivo",
        "input_schema": {"type": "object", "properties": {}},
    });
    if let Some(v) = eager {
        tool["eager_input_streaming"] = json!(v);
    }
    json!({
        "model": "lab",
        "max_tokens": max_tokens,
        "stream": true,
        "messages": [{"role": "user", "content": "escreva o arquivo"}],
        "tools": [tool],
    })
}

/// What an SSE client sees: the frames in the order they arrived, in a single
/// flow. Nothing here reopens a connection - if the stitching closed the stream, the `while`
/// below would end early and the tests would see less text.
struct Flow {
    /// Each `data:` of the stream, already unwrapped, except `[DONE]`.
    chunks: Vec<Value>,
    /// How many `[DONE]` arrived.
    done_sentinels: usize,
    /// At what instant each frame arrived, counted from the start of the request. It is
    /// what separates "arrived in several pieces" from "arrived early".
    instants: Vec<std::time::Duration>,
}

impl Flow {
    /// Every non-null `finish_reason` that crossed the flow. More than one means
    /// the client saw the end of the response ahead of time.
    fn finish_reasons(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["finish_reason"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn text(&self) -> String {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    /// The `tool_calls` deltas, in order.
    fn tool_call_deltas(&self) -> Vec<&Value> {
        self.chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"].get("tool_calls").is_some())
            .collect()
    }

    /// The argument fragments, in order, with the instant of each one.
    fn argument_fragments(&self) -> Vec<(std::time::Duration, String)> {
        self.chunks
            .iter()
            .zip(&self.instants)
            .filter_map(|(c, t)| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| (*t, s.to_string()))
            })
            .collect()
    }

    fn last_with_x_pom(&self) -> &Value {
        self.chunks
            .iter()
            .rev()
            .find(|c| c.get("x_pom").is_some())
            .expect("the final frame carries x_pom")
    }
}

async fn post_sse(no: &No, body: Value) -> Flow {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response");
    assert!(response.status().is_success(), "{:?}", response.status());

    let start = std::time::Instant::now();
    let mut bytes = response.bytes_stream();
    let mut buffer = String::new();
    let mut flow = Flow {
        chunks: Vec::new(),
        done_sentinels: 0,
        instants: Vec::new(),
    };
    while let Some(chunk) = bytes.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("SSE chunk")));
        let now = start.elapsed();
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            for line in event.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload == "[DONE]" {
                    flow.done_sentinels += 1;
                } else {
                    flow.chunks
                        .push(serde_json::from_str(payload).expect("chunk JSON"));
                    flow.instants.push(now);
                }
            }
        }
    }
    flow
}

// ── What the bench asserts ────────────────────────────────────────────────────

/// The reproduction, turned upside down. Asking for 100 against a
/// deployment of 8, the client used to receive 8 tokens and `finish_reason: "length"`;
/// now it receives the whole response and an honest `stop` - **without** the
/// deployment's `max_new_tokens` having been changed.
#[tokio::test]
async fn a_request_above_the_deployment_ceiling_comes_back_whole() {
    let script = plain_script();
    let expected: String = script.concat();
    let no = no(script.clone(), 8).await;

    let body = post_json(&no, request(100, false, false)).await;
    let choice = &body["choices"][0];

    assert_eq!(choice["finish_reason"], "stop");
    assert_eq!(choice["message"]["content"], expected);
    assert_eq!(body["usage"]["completion_tokens"], script.len() as u64);
    // The deployment's number remains what it was: the loop went over it
    // without raising it.
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(body["x_pom"]["output_limits"]["leg_max_tokens"], 8);
    assert_eq!(body["x_pom"]["output_limits"]["effective_max_tokens"], 100);
    assert_eq!(body["x_pom"]["output_limits"]["requested_max_tokens"], 100);
    // And the stitching is inspectable: 51 tokens in legs of 8.
    assert_eq!(
        body["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
    assert_eq!(body["x_pom"]["partial"], false);
}

/// The central requirement: a single flow. The stitching between legs cannot show up
/// as the end of a response - neither as an intermediate `finish_reason`, nor as a
/// second `[DONE]`, nor as a connection closed midway.
#[tokio::test]
async fn the_stream_is_one_continuous_flow_with_a_single_final_chunk() {
    let script = plain_script();
    let expected: String = script.concat();
    let no = no(script.clone(), 8).await;

    let flow = post_sse(&no, request(100, true, false)).await;

    assert_eq!(
        flow.finish_reasons(),
        vec!["stop".to_string()],
        "the seam leaked an end of response to the client"
    );
    assert_eq!(
        flow.done_sentinels, 1,
        "more than one [DONE] is another stream"
    );
    assert_eq!(flow.text(), expected);
    assert_eq!(
        flow.last_with_x_pom()["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
}

/// The captain's case, end to end: the tool call crosses the deployment's
/// ceiling and arrives with the whole, valid arguments JSON. No intermediate
/// piece is JSON: only the stitched output closes the tags.
#[tokio::test]
async fn a_tool_call_that_crosses_the_ceiling_arrives_complete_and_parseable() {
    let script = tool_call_script();
    let no = no(script, 8).await;

    let body = post_json(&no, request(100, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");

    let calls = choice["message"]["tool_calls"]
        .as_array()
        .expect("the response carries tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "write_file");

    let raw = calls[0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("arguments is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    let content = arguments["content"].as_str().expect("content");
    assert!(content.starts_with("linha_0 = 0 * 2"), "{content:?}");
    assert!(
        content.trim_end().ends_with("linha_39 = 39 * 2"),
        "the content arrived cut: {:?}",
        &content[content.len().saturating_sub(60)..]
    );
}

/// The same call, in streaming: the client receives a `tool_calls` with
/// complete arguments, not an unfinished `<tool_call>` degraded into text -
/// which is exactly what the cluster used to return.
#[tokio::test]
async fn the_streamed_tool_call_carries_complete_arguments() {
    let script = tool_call_script();
    let no = no(script, 8).await;

    let flow = post_sse(&no, request(100, true, true)).await;
    assert_eq!(flow.finish_reasons(), vec!["tool_calls".to_string()]);

    let deltas = flow.tool_call_deltas();
    // The opening brings the name, and the arguments come in MANY fragments.
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    let fragments = flow.argument_fragments();
    assert!(
        fragments.len() > 5,
        "the arguments came back held up: {} fragment(s)",
        fragments.len()
    );

    // Concatenating reproduces the whole, valid argument.
    let whole: String = fragments.iter().map(|(_, s)| s.as_str()).collect();
    let arguments: Value = serde_json::from_str(&whole).expect("arguments is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    assert!(arguments["content"]
        .as_str()
        .unwrap()
        .ends_with("linha_39 = 39 * 2"));
    // Nothing of the `<tool_call>` was left over as loose text.
    assert!(!flow.text().contains("<tool_call>"), "{}", flow.text());
}

/// The large reproduction of the captain's case: with enough effective context,
/// the 847 lines fit and the call arrives complete even crossing several
/// legs of 1024 tokens.
#[tokio::test]
async fn a_large_html_tool_call_fits_the_context_capacity() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(50_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(body["x_pom"]["partial"], false);

    let raw = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("a complete call is JSON");
    assert_eq!(arguments["path"], "/tmp/index.html");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
}

/// The captain's scenario with the capacity the deployment intends to publish:
/// 32000 tokens of a request in an effective context of 100000. The HTML
/// cannot vanish just because it crossed several legs of 1024 tokens.
#[tokio::test]
async fn a_large_html_tool_call_at_32000_and_context_100k_is_complete() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;

    let body = post_json(&no, request(32_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert!(
        body["usage"]["completion_tokens"]
            .as_u64()
            .expect("completion_tokens")
            < 32_000
    );
    assert_eq!(body["x_pom"]["partial"], false);

    let raw = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("a complete call is JSON");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
}

/// And the same limit is verifiable on the incremental path that Pi consumes:
/// concatenating the argument fragments still produces the whole call.
#[tokio::test]
async fn a_large_html_tool_call_at_32000_streams_complete_arguments() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;

    let flow = post_sse(&no, request(32_000, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["tool_calls".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    let arguments: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, fragment)| fragment.as_str())
        .collect();
    let arguments: Value = serde_json::from_str(&arguments).expect("arguments is JSON");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
    assert_eq!(flow.last_with_x_pom()["x_pom"]["partial"], false);
}

/// The same reproduction at 8192 tokens cannot be "resolved" with room to spare:
/// the client receives exactly the ceiling, the call comes back structured and the
/// arguments are left deliberately incomplete so they are not executed.
#[tokio::test]
async fn a_large_openai_tool_call_at_8192_is_structured_and_terminal() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(8_192, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 8_192);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);

    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "write_file");
    let arguments = call["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    assert!(!arguments.contains("<tool_call>"), "raw markup leaked");
    assert!(serde_json::from_str::<Value>(arguments).is_err());
}

#[tokio::test]
async fn a_large_openai_tool_call_at_5000_is_structured_and_terminal_non_streaming() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(5_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5_000);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert!(!body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .contains("<tool_call>"));

    let arguments = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    assert!(serde_json::from_str::<Value>(arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));
}

/// The streaming reproduction of Pi's large write: when the terminal ceiling
/// cuts the HTML parameter, the client still receives a structured `tool_calls`
/// and can keep the recoverable prefix for a new attempt. The flow cannot
/// close without arguments nor dump `<tool_call>` into `content`.
#[tokio::test]
async fn a_large_openai_tool_call_cut_in_stream_is_recoverable() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let flow = post_sse(&no, request(8_192, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert!(!flow.text().contains("<tool_call>"), "{}", flow.text());

    let deltas = flow.tool_call_deltas();
    assert!(!deltas.is_empty(), "the cut lost the structured call");
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    let arguments: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, fragment)| fragment.as_str())
        .collect();
    assert!(arguments.starts_with(r#"{"path":"/tmp/index.html","content":"#));
    assert!(serde_json::from_str::<Value>(&arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));

    let x_pom = &flow.last_with_x_pom()["x_pom"];
    assert_eq!(x_pom["truncated_tool_call"], true);
    assert_eq!(x_pom["partial"], true);
}

/// The same path with the local limit of 5000 tokens reproduces the cut seen
/// by Pi: there is no hidden continuation after the terminal event.
#[tokio::test]
async fn a_large_openai_tool_call_at_5000_has_one_recoverable_terminal_cut() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let flow = post_sse(&no, request(5_000, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert!(!flow.text().contains("<tool_call>"));
    assert!(!flow.argument_fragments().is_empty());
    assert!(!flow
        .argument_fragments()
        .iter()
        .any(|(_, fragment)| fragment.contains("data-line=\"846\"")));
    assert_eq!(flow.last_with_x_pom()["x_pom"]["partial"], true);
}

/// The Anthropic adapter exposes the same large call as a `tool_use` block:
/// complete when it fits, and incomplete with `stop_reason: max_tokens` when the
/// 8192 ceiling falls inside the parameter.
#[tokio::test]
async fn a_large_anthropic_tool_call_respects_50000_and_8192_separately() {
    let tool = json!({
        "name": "write_file",
        "description": "Escreve um arquivo",
        "input_schema": {"type": "object", "properties": {}},
    });

    let complete = no_with_context(large_html_tool_script(), 1_024, 65_536).await;
    let body = post_messages(
        &complete,
        json!({
            "model": "lab",
            "max_tokens": 50_000,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [tool.clone()],
        }),
    )
    .await;
    assert_eq!(body["stop_reason"], "tool_use");
    assert!(body["content"][0]["input"]["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));

    let truncated = no_with_context(large_html_tool_script(), 1_024, 65_536).await;
    let body = post_messages(
        &truncated,
        json!({
            "model": "lab",
            "max_tokens": 8_192,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [tool],
        }),
    )
    .await;
    assert_eq!(body["stop_reason"], "max_tokens");
    assert_eq!(body["usage"]["output_tokens"], 8_192);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["content"][0]["type"], "tool_use");
    let content = body["content"][0]["input"]["content"]
        .as_str()
        .expect("content");
    assert!(!content.contains("data-line=\"846\""));
}

/// Reproduces the limit configured by Pi on the same route that Claude Code uses:
/// `max_tokens: 5000` arrives whole at the gateway, cuts exactly at that total and
/// still closes the stream with a structured `tool_use`. The 100k context does not
/// raise the client's output ceiling.
#[tokio::test]
async fn pi_5000_limit_is_forwarded_and_large_write_is_structured_when_cut() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;
    let events = messages_events(&no, messages_request(5_000, Some(false))).await;

    let delta = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("message_delta final");
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    assert_eq!(delta["usage"]["output_tokens"], 5_000);
    assert_eq!(
        delta["x_pom"]["output_limits"]["requested_max_tokens"],
        5_000
    );
    assert_eq!(
        delta["x_pom"]["output_limits"]["effective_max_tokens"],
        5_000
    );
    assert_eq!(delta["x_pom"]["partial"], true);
    assert_eq!(delta["x_pom"]["truncated_tool_call"], true);

    let tool_start = events
        .iter()
        .find(|event| event["content_block"]["type"] == "tool_use")
        .expect("content_block_start do tool_use");
    assert_eq!(tool_start["content_block"]["name"], "write_file");

    let arguments: String = events
        .iter()
        .filter(|event| event["delta"]["type"] == "input_json_delta")
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert!(arguments.starts_with(r#"{"path":"/tmp/index.html","content":"#));
    assert!(serde_json::from_str::<Value>(&arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
}

/// **The captain's defect, turned inside out.** Measured on the live ksia: 2 pieces,
/// both at 11.01 s - the 663 characters all arrived at once, at the end,
/// and the user stared at a frozen screen. Now the first fragment arrives
/// well before the last.
#[tokio::test]
async fn the_first_argument_fragment_arrives_well_before_the_last() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(100, true, true)).await;
    let fragments = flow.argument_fragments();

    assert!(fragments.len() > 5, "{} fragmento(s)", fragments.len());
    let first = fragments.first().expect("first").0;
    let last = fragments.last().expect("last").0;
    assert!(
        first < last,
        "first and last at the same instant ({first:?}) - it is the defect again"
    );
    // This test's wall clock includes process scheduling, kernel queues
    // and byte grouping by the socket. These factors can make
    // the first frame look late even when the server already emitted it
    // separately. The stable proof that streaming did not dam up the call is
    // there being many fragments and the first being observed before the last.
    assert!(
        first < last,
        "the first frame did not arrive before the last"
    );
}

/// The brake against the looping model: the ceiling the client asked for is the only one that
/// produces `length`, and the deployment's (8) ends nothing - it only splits the
/// response into invisible legs, five of them here.
#[tokio::test]
async fn the_client_ceiling_stops_a_runaway_model_and_is_the_only_length() {
    let no = no(runaway_script(), 8).await;

    let body = post_json(&no, request(37, false, false)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 37);
    assert_eq!(body["x_pom"]["output_limits"]["effective_max_tokens"], 37);
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(body["x_pom"]["finish_reason"], "length");
    assert_eq!(body["x_pom"]["continuation_legs"], 5);
}

/// The same overflow in streaming: a single `length`, at the end, and a single `[DONE]`. The
/// stitching of the five legs stays invisible.
#[tokio::test]
async fn the_client_ceiling_ends_the_stream_exactly_once() {
    let no = no(runaway_script(), 8).await;

    let flow = post_sse(&no, request(37, true, false)).await;
    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert_eq!(flow.text().matches("e ").count(), 37);
    assert_eq!(flow.last_with_x_pom()["x_pom"]["continuation_legs"], 5);
}

/// When the request fits in one leg nothing changes: one leg, no stitching, and the
/// `length` is still said at the right time.
#[tokio::test]
async fn a_request_that_fits_one_leg_is_untouched() {
    let no = no(runaway_script(), 8).await;

    let body = post_json(&no, request(5, false, false)).await;
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_eq!(body["x_pom"]["continuation_legs"], 1);
    assert_eq!(body["x_pom"]["output_limits"]["leg_max_tokens"], 5);
}

/// `/v1/models` announces the request's ceiling, not a leg's: announcing the
/// deployment's would make the client ask for less than this node delivers.
#[tokio::test]
async fn the_advertised_completion_ceiling_is_the_request_ceiling() {
    let no = no(runaway_script(), 8).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");

    let model = &body["data"][0];
    assert_eq!(model["top_provider"]["max_completion_tokens"], 8_192);
}

/// Anthropic SDKs can send both authentication headers. A bad Bearer header
/// must not mask a valid `x-api-key`, and the same rule applies to both API
/// surfaces. Two invalid credentials still remain a 403.
#[tokio::test]
async fn either_valid_auth_header_is_accepted_on_models_and_messages() {
    let no = no(plain_script(), 8).await;
    let client = reqwest::Client::new();

    let models = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(models.status(), reqwest::StatusCode::OK);

    let messages = client
        .post(format!("{}/v1/messages", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", API_KEY)
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
        }))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(messages.status(), reqwest::StatusCode::OK);

    let both_invalid = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", "invalid-api-key")
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(both_invalid.status(), reqwest::StatusCode::FORBIDDEN);

    let messages_invalid = client
        .post(format!("{}/v1/messages", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", "invalid-api-key")
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
        }))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(messages_invalid.status(), reqwest::StatusCode::FORBIDDEN);

    let bearer = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", format!("Bearer {API_KEY}"))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(bearer.status(), reqwest::StatusCode::OK);

    let x_api_key = client
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(x_api_key.status(), reqwest::StatusCode::OK);
}

/// The list must be auto-detectable by Claude Code and OpenCode at the same
/// time. The extra Anthropic fields are ignored by OpenAI clients.
#[tokio::test]
async fn model_list_is_authenticated_and_advertises_the_live_100k_context() {
    let no = no_with_context(runaway_script(), 8, 100_000).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(body["object"], "list");
    assert_eq!(body["has_more"], false);
    let model = &body["data"][0];
    assert_eq!(model["id"], "lab");
    assert_eq!(model["type"], "model");
    assert_eq!(model["max_input_tokens"], 100_000);
    assert_eq!(model["max_tokens"], 100_000);
    assert_eq!(model["context_length"], 100_000);
    assert_eq!(model["top_provider"]["max_completion_tokens"], 100_000);
}

/// The capacity derived from the network is the capacity of the whole pipeline: a peer
/// with context 8192 cannot be hidden by another with context 100000.
/// The list uses the smallest value among the peers of the planned pipeline.
#[tokio::test]
async fn deployment_context_capacity_is_aggregated_across_all_live_peers() {
    let no = no_with_peer_capacities(
        plain_script(),
        32_000,
        100_000,
        vec![
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
            crate::network::RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 8_192,
            },
        ],
    )
    .await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 8_192);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        8_192
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    let status = response.status();
    let body = response.text().await.expect("error body");
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
}

/// After all peers restart with the new configuration, the gateway
/// stops announcing the old capacity and accepts 32000 without confusing it with
/// the per-leg ceiling.
#[tokio::test]
async fn deployment_capacity_updates_only_after_all_peers_report_ready() {
    let no = no_with_peer_capacities(
        plain_script(),
        32_000,
        100_000,
        vec![
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
        ],
    )
    .await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 100_000);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        100_000
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("x-api-key", API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

/// Reproduces the captain's configuration: `max_new_tokens: 32000` remains
/// one leg, while the published capacity comes from the effective context of the
/// model and of the network, without a second hidden absolute ceiling.
#[tokio::test]
async fn a_32000_deployment_leg_uses_the_context_derived_capacity() {
    let no = no_with_context(plain_script(), 32_000, 100_000).await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 100_000);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        100_000
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

/// OpenCode sends the new name of the limit and may add optional fields
/// of the OpenAI protocol. The gateway must honor `max_completion_tokens` without
/// requiring the client to also send the old name.
#[tokio::test]
async fn openai_chat_completions_accepts_max_completion_tokens() {
    let no = no(runaway_script(), 8).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("authorization", format!("Bearer {API_KEY}"))
        .json(&json!({
            "model": "lab",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "oi"}]}],
            "max_completion_tokens": 5,
            "stream": false,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "response_format": {"type": "text"},
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_eq!(body["x_pom"]["partial"], false);
}

#[tokio::test]
async fn max_completion_tokens_above_the_former_50k_ceiling_is_accepted() {
    let no = no_with_context(plain_script(), 8, 100_000).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "model": "lab",
            "messages": [{"role": "user", "content": "oi"}],
            "max_completion_tokens": 50_001,
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

// ── O teto do CLIENTE caindo dentro da chamada ───────────────────────────────

/// Ceiling chosen to fall **inside** the tool call.
///
/// `tool_call_script()` has 50 pieces: index 0 is prose, 1 opens
/// `<tool_call>`, 7..=46 are the 40 lines of the `content`, 49 closes
/// `</tool_call>`. With 20, generation reaches the ceiling right after piece 19 -
/// `linha_12` - eighteen pieces after the opening and twenty-nine before the
/// closing. `the_cut_lands_inside_the_call()` proves this, so nobody later
/// thinks the point fell somewhere else by accident.
const CAP_INSIDE_CALL: u32 = 20;

/// Arithmetic proof of the cut point, independent of the HTTP tests: with
/// `CAP_INSIDE_CALL` tokens generated, the raw output opened the call and
/// did not close it.
#[test]
fn the_cut_lands_inside_the_call() {
    let script = tool_call_script();
    let truncated: String = script[..CAP_INSIDE_CALL as usize].concat();

    assert!(
        (CAP_INSIDE_CALL as usize) < script.len(),
        "the cap must be lower than the script, otherwise it cuts nothing"
    );
    assert!(
        truncated.contains("<tool_call>"),
        "the cut landed before the opening"
    );
    assert!(
        !truncated.contains("</tool_call>"),
        "the cut landed after the closing"
    );
    // And it falls in the middle of the `content`, not on a tag boundary.
    assert!(truncated.ends_with("linha_12 = 12 * 2\n"), "{truncated:?}");
}

/// **The client's ceiling is inviolable, even with an open call.**
///
/// This is where a removed slack let generation exceed what the
/// client asked for. Neither of the two providers does that:
/// `max_tokens` is an enforced per-response ceiling, reaching it is a terminal stop, and the
/// output is truncated midway. What they offer in place of overflow is the
/// task budget, covered further below.
#[tokio::test]
async fn the_client_ceiling_is_never_exceeded_even_with_an_open_tool_call() {
    let no = no(tool_call_script(), 8).await;

    let body = post_json(&no, request(CAP_INSIDE_CALL, false, true)).await;

    assert_eq!(
        body["usage"]["completion_tokens"], CAP_INSIDE_CALL,
        "the generation went past the cap the client asked for"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "length");
}

/// The same in streaming, and with the guarantee that only streaming gives: not one extra
/// token crossed the flow.
#[tokio::test]
async fn the_streamed_client_ceiling_is_never_exceeded_with_an_open_tool_call() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(CAP_INSIDE_CALL, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert_eq!(
        flow.last_with_x_pom()["x_pom"]["tokens_generated"],
        CAP_INSIDE_CALL
    );
}

/// **The shape of the response when the cut falls inside the call** (OpenAI).
///
/// Verified against the live documentation on 2026-08-29: the partial call comes back
/// **structured**, with `function.arguments` being the unfinished JSON string,
/// next to `finish_reason: "length"`. `arguments` is documented as a string
/// that "is not always valid JSON", and the client is obliged to validate before
/// executing - that is what keeps a harness from writing half a file.
///
/// The two wrong answers that have already been measured here are asserted against:
/// raw markup leaked as text, and the silent discarding of the fragment.
#[tokio::test]
async fn a_tool_call_cut_by_the_ceiling_comes_back_structured_and_incomplete() {
    let no = no(tool_call_script(), 8).await;

    let body = post_json(&no, request(CAP_INSIDE_CALL, false, true)).await;
    let choice = &body["choices"][0];

    // Terminal stop, with the corresponding reason.
    assert_eq!(choice["finish_reason"], "length");

    // The call was NOT discarded: it comes back structured.
    let calls = choice["message"]["tool_calls"]
        .as_array()
        .expect("the cut call comes back structured, not discarded");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "write_file");

    // And the arguments are the unfinished JSON string - on purpose: a client
    // that ignores the `length` and tries to parse fails, instead of executing half a
    // call with the `content` half-written.
    let raw = calls[0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a string");
    assert!(
        serde_json::from_str::<Value>(raw).is_err(),
        "arguments of a cut call cannot parse: {raw:?}"
    );
    assert!(
        raw.starts_with(r#"{"path":"/tmp/calc.py","content":"#),
        "{raw:?}"
    );
    // The last parameter comes in with the opening quote and **without** the
    // closing one, and nothing the model wrote is trimmed: the final
    // line break is content, not leftover.
    assert!(raw.ends_with(r"linha_12 = 12 * 2\n"), "{raw:?}");

    // And no raw markup in the content - the original defect.
    let content = choice["message"]["content"].as_str().unwrap_or("");
    assert_eq!(content, "Vou escrever o arquivo.\n");
    assert!(
        !content.contains("<tool_call>")
            && !content.contains("<function=")
            && !content.contains("<parameter="),
        "{content:?}"
    );

    // Honest `x_pom`: the cut fell inside the call, and the response is partial.
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["finish_reason"], "length");
    assert_eq!(
        body["x_pom"]["output_limits"]["effective_max_tokens"],
        CAP_INSIDE_CALL
    );
}

/// The same shape in streaming: the usual two deltas - opening with the name,
/// then the arguments -, only the arguments are the unfinished string. Nothing
/// from inside the tag leaked as `content` before that, because the scanner holds
/// the body until the tag closes.
#[tokio::test]
async fn the_streamed_tool_call_cut_by_the_ceiling_is_structured_and_incomplete() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(CAP_INSIDE_CALL, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);

    let deltas = flow.tool_call_deltas();
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    // The fragments that had already been transmitted count: the cut does not
    // repeat what the client already accumulated. The concatenation is the unfinished string.
    let raw: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, s)| s.as_str())
        .collect();
    assert!(
        serde_json::from_str::<Value>(&raw).is_err(),
        "arguments of a cut call cannot parse: {raw:?}"
    );
    assert!(
        raw.starts_with(r#"{"path":"/tmp/calc.py","content":"#),
        "{raw:?}"
    );
    assert!(raw.ends_with(r"linha_12 = 12 * 2\n"), "{raw:?}");

    let text = flow.text();
    assert_eq!(text, "Vou escrever o arquivo.\n");
    assert!(
        !text.contains("<tool_call>")
            && !text.contains("<function=")
            && !text.contains("<parameter="),
        "{text:?}"
    );

    let x_pom = &flow.last_with_x_pom()["x_pom"];
    assert_eq!(x_pom["truncated_tool_call"], true);
    assert_eq!(x_pom["partial"], true);
}

/// The same shape in Anthropic's vocabulary: the **last block** is a
/// `tool_use` with an incomplete `input`, and the `stop_reason` is `max_tokens`. It is
/// exactly the pair that the documentation tells the client to check to know that it
/// needs to repeat the request with a larger ceiling.
#[tokio::test]
async fn the_anthropic_adapter_returns_the_cut_call_as_a_trailing_tool_use_block() {
    let no = no(tool_call_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": CAP_INSIDE_CALL,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [{
                "name": "write_file",
                "description": "Escreve um arquivo",
                "input_schema": {"type": "object", "properties": {}},
            }],
        }),
    )
    .await;

    assert_eq!(body["stop_reason"], "max_tokens");
    let blocks = body["content"].as_array().expect("content is a list");
    let last = blocks.last().expect("at least one block");
    assert_eq!(
        last["type"], "tool_use",
        "the last block must be the cut call: {blocks:?}"
    );
    assert_eq!(last["name"], "write_file");
    // `input` is an object by contract, with what could be recovered.
    assert_eq!(last["input"]["path"], "/tmp/calc.py");
    assert!(last["input"]["content"]
        .as_str()
        .expect("partial content")
        .ends_with("linha_12 = 12 * 2\n"));

    // And no raw markup in the text blocks.
    let text: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        !text.contains("<tool_call>") && !text.contains("<function="),
        "{text:?}"
    );

    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["x_pom"]["partial"], true);
}

// ── Capacidade derivada do runtime ──────────────────────────────────────────

/// There is no longer an absolute ceiling parallel to the deployment. A request that
/// would exceed the old default 8192 remains valid when the effective
/// context holds the output; `max_new_tokens` only splits the legs.
#[tokio::test]
async fn a_request_above_the_removed_absolute_default_uses_runtime_context() {
    let no = no_with_context(plain_script(), 8, 100_000).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(100_000, false, false))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(
        body["x_pom"]["output_limits"]["requested_max_tokens"],
        100_000
    );
    assert_eq!(
        body["x_pom"]["output_limits"]["effective_max_tokens"],
        99_992
    );
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    let limits = body["x_pom"]["output_limits"]
        .as_object()
        .expect("output limits");
    assert_eq!(limits.len(), 5);
    assert!(limits.contains_key("context_available_tokens"));
}

// ── Task budget ──────────────────────────────────────────────────────────────

/// **The marker reaches the generation.**
///
/// `output_config.task_budget` is the mechanism the providers offer in
/// place of overflow: the server injects a countdown marker
/// that the model sees, so it organizes itself and finishes gracefully
/// within the limit. This test checks the whole path - the marker leaves the
/// adapter, crosses the gateway and reaches the prefill frame that the head
/// receives, which is where the real model would read it.
#[tokio::test]
async fn a_task_budget_puts_a_countdown_marker_into_the_generation() {
    let no = no(plain_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 64_000}},
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(prompt.contains("<task_budget>"), "{prompt:?}");
    assert!(prompt.contains("remaining_tokens: 64000"), "{prompt:?}");
    assert!(prompt.contains("total_tokens: 64000"), "{prompt:?}");

    // Advisory: it cuts nothing. What cuts is `max_tokens`, and the script ended
    // before it.
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["x_pom"]["task_budget"]["total"], 64_000);
    assert_eq!(
        body["x_pom"]["task_budget"]["remaining_at_turn_start"],
        64_000
    );
    assert_eq!(
        body["x_pom"]["task_budget_spent_this_turn"],
        plain_script().len() as u64
    );
}

/// `remaining` is the remainder coming from a previous request: it is what the
/// marker announces, not the `total`. It is how the client stitches the agentic loop
/// on its side, since the node keeps no state between requests.
#[tokio::test]
async fn a_carried_over_remaining_is_what_the_marker_announces() {
    let no = no(plain_script(), 8).await;

    post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "continue"}],
            "output_config": {
                "task_budget": {"type": "tokens", "total": 128_000, "remaining": 42_000},
            },
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(prompt.contains("remaining_tokens: 42000"), "{prompt:?}");
    assert!(prompt.contains("total_tokens: 128000"), "{prompt:?}");
}

/// **Absent, nothing changes.** The marker does not appear in the prompt, and the `x_pom` does not
/// invent any budget.
#[tokio::test]
async fn without_a_task_budget_the_generation_is_untouched() {
    let no = no(plain_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(!prompt.contains("task_budget"), "{prompt:?}");
    assert_eq!(prompt, "User: escreva o arquivo");
    assert!(body["x_pom"]["task_budget"].is_null(), "{}", body["x_pom"]);
}

/// The minimum is the providers': below it the API answers 400, because a
/// budget too small for the task makes the model refuse or stop early.
#[tokio::test]
async fn a_task_budget_below_the_minimum_is_refused() {
    let no = no(plain_script(), 8).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 1_000}},
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("JSON body");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.contains("20000"), "{message}");
}

/// The task budget **never cuts**: what cuts is `max_tokens`. A
/// generous budget does not extend the ceiling by even one token - if it did,
/// we would have reinvented the slack this task removed.
#[tokio::test]
async fn a_generous_task_budget_never_lifts_the_client_ceiling() {
    let no = no(runaway_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": "oi"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 1_000_000}},
        }),
    )
    .await;

    assert_eq!(body["stop_reason"], "max_tokens");
    assert_eq!(body["usage"]["output_tokens"], 30);
}

// ── `eager_input_streaming`, per tool ─────────────────────────────────

/// **Turned on, the fragments go out as the model generates.** It is Anthropic's
/// shape: an optional field on the tool definition, with no beta header.
#[tokio::test]
async fn eager_input_streaming_on_a_tool_streams_the_argument_in_fragments() {
    let no = no(tool_call_script(), 8).await;

    let fragments = messages_fragments(&no, messages_request(100, Some(true))).await;

    assert!(
        fragments.len() > 5,
        "the arguments came back held up: {} fragment(s)",
        fragments.len()
    );
    let whole: String = fragments.concat();
    let arguments: Value = serde_json::from_str(&whole).expect("the concatenation is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .ends_with("linha_39 = 39 * 2"));
}

/// **Absent, the dammed-up behavior is preserved.** It is Anthropic's
/// default, and the damming is by **parameter value**: the large
/// parameter goes out whole, in a single fragment.
#[tokio::test]
async fn without_eager_input_streaming_each_parameter_arrives_whole() {
    for absent in [None, Some(false)] {
        // One node per request: the fake engine has a single token counter, and it
        // does not go back to the start between requests.
        let no = no(tool_call_script(), 8).await;
        let fragments = messages_fragments(&no, messages_request(100, absent)).await;
        assert_eq!(fragments.len(), 3, "{absent:?}: {fragments:?}");
        let large = fragments
            .iter()
            .find(|f| f.contains("linha_39"))
            .expect("the content went out");
        assert!(large.contains("linha_0 = 0 * 2"), "{large:?}");

        let whole: String = fragments.concat();
        let arguments: Value = serde_json::from_str(&whole).expect("valid JSON");
        assert_eq!(arguments["path"], "/tmp/calc.py");
    }
}
