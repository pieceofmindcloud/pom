//! Bench for prefix reuse across turns: **a cache hit has to become
//! measured savings.**
//!
//! The `prefix_cache_lab` next door proves the in-memory cache's accounting -
//! the *choice*. This one proves the *execution*: how many prompt tokens the
//! pipeline stopped reprocessing, turn by turn, through the same path the chat
//! screen and an OpenAI client use (`openai::render_prompt`,
//! `openai::conversation_cache_key`, `gateway::generate_stream_with_stops`,
//! `prefill::run_with_options`, `/v1/prefill`, `/v1/step`, `/v1/session/rewind`,
//! `/v1/session/release`). That was exactly where the system reported "session
//! reused" and executed something else: in 100% of the turns measured in
//! production, the `cached_tokens` put to use was zero.
//!
//! ## The three transports, not one
//!
//! Prefill has three paths - hub, ring and direct reply - and each one builds
//! the next hop's request in a different place. A test that exercises only one
//! of them passes while the other two are broken, which is the easiest way
//! for this bench to lie. Each scenario runs on all three, with two or more
//! hops, so that head and tail are distinct machines as in production and
//! the propagation is not tied to a three-node topology.
//!
//! ## The engine the fake hops imitate
//!
//! The fake hop is not a lenient engine. It is the **most restricted** one the
//! product has to serve: a recurrent/hybrid model (the `gated_delta_net` of
//! Qwen3-Next, the production Ornith), where `llama_memory_seq_rm` **refuses**
//! any partial removal larger than the snapshot window and only accepts erasing
//! the whole sequence - the cause of the refusal is in
//! `llama.cpp/src/llama-memory-recurrent.cpp::seq_rm`. Asking this hop to
//! discard positions from the middle of the KV is a test failure, not a refusal
//! by the model: that was the dependency the product decision ordered out of
//! the design, and the bench exists so that it does not come back by oversight.
//!
//! **Declared limit:** with no GGUF on this machine there is no real llama.cpp.
//! What the bench proves is the contract between gateway and peers - where the
//! four defects lived. What it does not prove is what llama.cpp does with the
//! batch; that is cluster measurement, recorded in the task's write-up.

#![cfg(test)]

use crate::openai::{ChatMessage, Content};
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const HIDDEN_DIM: usize = 8;
const FAKE_PREFILL_MS_PER_TOKEN: u64 = 2;

// ── The fake engine, with KV accounting ───────────────────────────────────────

/// The KV of a session on this hop, in the same shape as `infer_server::Session`:
/// the prompt's sequence and, separately, the generation's.
#[derive(Default, Clone)]
struct FakeSession {
    /// Prompt tokens present in the KV of the prompt sequence.
    prompt_kv: Vec<i32>,
    /// Boundary recorded when the prompt ended. The token after it and the
    /// generated tokens live in the generation sequence and are removed together.
    prompt_len: u32,
    /// How many positions exist in the generation sequence beyond the stable
    /// prefix. Includes the prompt tail that cannot be retained and the output.
    gen_len: u32,
    /// Boundary the head measured for this prompt.
    stable_len: u32,
    /// This turn's target prompt, on the head (which is the one that tokenizes).
    target: Vec<i32>,
}

impl FakeSession {
    fn n_past(&self) -> u32 {
        self.prompt_kv.len() as u32 + self.gen_len
    }
}

#[derive(Default)]
struct Engine {
    sessions: HashMap<String, FakeSession>,
    /// One item per ingested prompt chunk, with how many tokens it cost.
    prompt_chunks: Vec<u32>,
    /// Requests that would require removing positions from the middle of the KV.
    /// A recurrent engine would refuse; the bench records it so the test fails on it.
    partial_removals: Vec<String>,
    /// Sessions that vanished before the gateway managed to preserve them.
    rewind_on_unknown_session: u32,
    /// Each prefill frame actually ingested by the peer. The test cannot
    /// infer the savings from the gateway's request: it has to compare the
    /// declared position with the remote counter before and after the decode.
    frames: Vec<ObservedFrame>,
    /// Controlled failure of the bench: removes the session before a chunk and
    /// makes the peer answer like the real server when the caller has not yet
    /// asked for a reset. The counter allows testing both the single recovery
    /// and the one-attempt limit.
    unknown_session_at_chunk: Option<u32>,
    unknown_session_failures_remaining: usize,
    /// Controlled transient HTTP failure to simulate the connection being cut
    /// after the head accepts a chunk, but before the next hop accepts it.
    transport_failure_at_chunk: Option<u32>,
    transport_failures_remaining: usize,
}

#[derive(Clone, Copy)]
enum FaultKind {
    UnknownSession,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedFrame {
    session_id: String,
    chunk_index: u32,
    start_pos: u32,
    next_pos: u32,
    n_past_before: u32,
    n_past_after: u32,
    reuse_prefix: bool,
}

#[derive(Clone)]
struct Hop {
    is_head: bool,
    engine: Arc<Mutex<Engine>>,
    local_prefill_order: crate::infer_server::DirectForwardOrder,
    direct_forward_order: crate::infer_server::DirectForwardOrder,
}

/// The bench's "tokenizer": one word, one token.
///
/// Stable and additive, like the real one on a chat template: appending
/// text at the end of the prompt appends tokens at the end of the list, without
/// touching the earlier ones. It is the premise reuse requires, and the bench
/// makes it explicit instead of leaving it implicit in an absent GGUF.
fn tokenize(prompt: &str) -> Vec<i32> {
    prompt
        .split_whitespace()
        .map(|word| {
            let mut h: i32 = 7;
            for b in word.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as i32);
            }
            h
        })
        .collect()
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

fn error(msg: String) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}

async fn fake_health() -> impl IntoResponse {
    Json(json!({"status": "ok", "model_loaded": true, "hidden_dim": HIDDEN_DIM}))
}

async fn fake_rollback() -> impl IntoResponse {
    Json(json!({"rolled_back": true}))
}

struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    tensor: Option<(Vec<f32>, TensorShape)>,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
}

/// Read with the same rules as the real `infer_server`.
fn read(body: &Bytes) -> Incoming {
    let (meta, tensor) = wire::decode(body).expect("valid frame");
    let declared: Option<Vec<crate::pipeline::PipelineStep>> = meta
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
        tensor,
        meta,
    }
}

/// Mirrors `infer_server::relay_and_bubble`.
async fn bubble(
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
    .map_err(|e| error(e.to_string()))?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(0);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

/// `/v1/prefill`, with the same decision tree as the real `infer_server`:
/// `reuse_prefix` on chunk 0, `start_pos` saying where to ingest from, and
/// the reply declaring what **was executed** (`reuse_prefix`,
/// `cached_tokens`), which is what the following hops obey.
/// What ingesting a chunk left ready for the reply.
struct Ingested {
    n_tokens: usize,
    is_final_chunk: bool,
    local: Value,
}

/// All the KV accounting, **synchronous**: the fake engine's lock is born and
/// dies here, far from any `await`.
fn ingest(hop: &Hop, inc: &Incoming, sid: &str) -> Result<Ingested, String> {
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let reuse_requested = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);
    let start_pos = inc.meta["start_pos"].as_u64().map(|v| v as u32);
    let is_final_chunk_received = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);
    let stable_from_head = inc.meta["stable_len"].as_u64().map(|v| v as u32);

    let mut engine = hop.engine.lock().unwrap();

    if engine.unknown_session_at_chunk == Some(chunk_index)
        && engine.unknown_session_failures_remaining > 0
    {
        engine.unknown_session_failures_remaining -= 1;
        engine.sessions.remove(sid);
        return Err(format!("unknown session '{sid}' - send reset=true first"));
    }

    let mut reused = false;
    let mut cached_tokens = 0u32;
    if chunk_index == 0 {
        let wants_reuse = reuse_requested && engine.sessions.contains_key(sid);
        if !wants_reuse {
            // Reset: full cleanup, which every engine accepts.
            engine
                .sessions
                .insert(sid.to_string(), FakeSession::default());
        }
        if hop.is_head {
            let tokens = tokenize(inc.meta["prompt"].as_str().unwrap_or_default());
            if wants_reuse {
                // The production rule itself, not a copy: if it changes,
                // the bench changes with it.
                let session = engine.sessions.get(sid).cloned().unwrap_or_default();
                let decision = crate::infer_server::plan_prefix_reuse(
                    &session.prompt_kv,
                    session.n_past(),
                    &tokens,
                );
                reused = decision.reused;
                cached_tokens = decision.cached_tokens;
                if !reused {
                    engine
                        .sessions
                        .insert(sid.to_string(), FakeSession::default());
                }
            }
            let session = engine.sessions.entry(sid.to_string()).or_default();
            session.gen_len = 0;
            // The last token stands for the template's tail. This way the bench
            // exercises the same split the real server makes with
            // `stable_prefix_len`: the retained prompt never receives the generation.
            session.stable_len = tokens.len().saturating_sub(1) as u32;
            session.target = tokens;
        } else {
            let session = engine.sessions.entry(sid.to_string()).or_default();
            session.gen_len = 0;
            // The intermediate hop does not tokenize, so the authoritative decision
            // comes in the frame and has to be echoed to the next hop.
            reused = reuse_requested;
            if let Some(stable) = stable_from_head {
                session.stable_len = stable;
            }
        }
    }

    let n_past = engine.sessions.entry(sid.to_string()).or_default().n_past();
    let expected = start_pos.unwrap_or(if chunk_index == 0 {
        n_past
    } else {
        chunk_index * chunk_size
    });
    if expected < n_past {
        // A recurrent engine would answer `false` here. Recording and refusing is
        // what makes the bench fail instead of hiding the defect.
        engine.partial_removals.push(format!(
            "chunk {chunk_index} asked for position {expected} with the session at {n_past}"
        ));
        return Err(
            "this model does not accept discarding positions from the middle of the cache".into(),
        );
    }
    if expected > n_past {
        return Err(format!(
            "chunk out of order: pos {expected} with the session at {n_past}"
        ));
    }

    let (n_tokens, more, total) = if hop.is_head {
        let target = engine.sessions[sid].target.clone();
        let remaining = target.len().saturating_sub(expected as usize);
        let n = remaining.min(chunk_size as usize);
        if n == 0 {
            return Err(format!("chunk {chunk_index} has no tokens"));
        }
        let new_items = target[expected as usize..expected as usize + n].to_vec();
        let session = engine.sessions.get_mut(sid).unwrap();
        let stable = session.stable_len as usize;
        let retained = new_items
            .len()
            .min(stable.saturating_sub(expected as usize));
        session.prompt_kv.extend_from_slice(&new_items[..retained]);
        session.gen_len += new_items.len().saturating_sub(retained) as u32;
        (n, expected as usize + n < target.len(), target.len() as u32)
    } else {
        // Middle/tail hop: ingests what the head sent, without tokenizing.
        let n = inc.tensor.as_ref().map(|(_, s)| s[0]).unwrap_or(1);
        let session = engine.sessions.get_mut(sid).unwrap();
        let retained = n.min(session.stable_len.saturating_sub(expected) as usize);
        session
            .prompt_kv
            .extend(std::iter::repeat_n(0i32, retained));
        session.gen_len += (n - retained) as u32;
        (n, false, 0)
    };
    engine.prompt_chunks.push(n_tokens as u32);

    let is_final_chunk = if hop.is_head {
        !more
    } else {
        is_final_chunk_received
    };
    if is_final_chunk {
        let session = engine.sessions.get_mut(sid).unwrap();
        session.prompt_len = session.stable_len;
    }

    let n_past_after = engine.sessions[sid].n_past();
    engine.frames.push(ObservedFrame {
        session_id: sid.to_string(),
        chunk_index,
        start_pos: expected,
        next_pos: expected + n_tokens as u32,
        n_past_before: n_past,
        n_past_after,
        reuse_prefix: reused,
    });

    let mut local = json!({
        "processing_ms": 0,
        "node_timings": {},
        "start_pos": expected,
        "next_pos": expected + n_tokens as u32,
    });
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(total);
    }
    local["stable_len"] = json!(engine.sessions[sid].stable_len);
    if chunk_index == 0 {
        local["reuse_prefix"] = json!(reused);
        local["cached_tokens"] = json!(cached_tokens);
    }
    if inc.is_last {
        local["done"] = json!(false);
        local["more_chunks"] = json!(!is_final_chunk);
        if is_final_chunk {
            local["token_id"] = json!(1);
            local["text"] = json!("ok");
        }
    }
    Ok(Ingested {
        n_tokens,
        is_final_chunk,
        local,
    })
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let sid = inc.meta["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let reuse_requested = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);
    let prefill_sequence_id = inc
        .meta
        .get("prefill_sequence_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let local_turn = if let Some(sequence_id) = &prefill_sequence_id {
        if chunk_index == 0 {
            hop.local_prefill_order
                .ensure_sequence(sequence_id)
                .expect("initialize fake node prefill order");
        }
        Some(
            hop.local_prefill_order
                .wait_turn(sequence_id, chunk_index)
                .await
                .expect("fake node receives chunks in session order"),
        )
    } else {
        None
    };

    {
        let mut engine = hop.engine.lock().unwrap();
        if engine.transport_failure_at_chunk == Some(chunk_index)
            && engine.transport_failures_remaining > 0
        {
            engine.transport_failures_remaining -= 1;
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "prefill link cut during the chunk",
            )
                .into_response();
        }
    }

    let Ingested {
        n_tokens,
        is_final_chunk,
        mut local,
    } = match ingest(&hop, &inc, &sid) {
        Ok(ok) => {
            if let Some(turn) = local_turn {
                turn.finish(true, ok.is_final_chunk);
            }
            ok
        }
        Err(msg) if msg.contains("session") && msg.contains("unknown") => {
            return (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(msg) => return error(msg),
    };

    // Make the executed work visible in TTFT. The real engine's prefill cost
    // is proportional to the tokens it actually ingests; this deterministic
    // delay prevents scheduler jitter from turning the regression into a test
    // of the chosen cache flag instead of the work that followed it.
    tokio::time::sleep(std::time::Duration::from_millis(
        n_tokens as u64 * FAKE_PREFILL_MS_PER_TOKEN,
    ))
    .await;
    local["processing_ms"] = json!(n_tokens as u64 * FAKE_PREFILL_MS_PER_TOKEN);

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

    // ── Direct reply ────────────────────────────────────────────────────────
    if let Some(reply_to) = inc.reply_to {
        let hop_timings = crate::direct::accumulate(
            &inc.meta,
            crate::direct::hop_entry(
                &local["processing_ms"],
                &local["node_timings"],
                inc.inbound_bytes,
            ),
        );
        let progress = local
            .get("owns_tokens")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| crate::infer_server::prompt_progress_for_forward(&inc.meta, &local));
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
            if let Some(sequence_id) = &prefill_sequence_id {
                if chunk_index == 0 {
                    hop.direct_forward_order
                        .ensure_sequence(sequence_id)
                        .expect("initialize fake direct forwarding order");
                }
            }
            let mut forward = json!({
                "session_id": sid,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                // The production functions, so the forwarding is production's.
                "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                    &local, reuse_requested, chunk_index),
                "is_final_chunk": is_final_chunk,
                "start_pos": local["start_pos"].clone(),
                "next_pos": local["next_pos"].clone(),
                "stable_len": local["stable_len"].clone(),
                "prompt_progress":
                    crate::infer_server::prompt_progress_for_forward(&inc.meta, &local),
                "hop_timings": hop_timings,
            });
            if let Some(sequence_id) = &prefill_sequence_id {
                forward["prefill_sequence_id"] = json!(sequence_id);
            }
            tokio::spawn(crate::infer_server::forward_direct_ordered(
                inc.next_hops,
                "/v1/prefill",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
                prefill_sequence_id
                    .as_ref()
                    .map(|_| hop.direct_forward_order.clone()),
                prefill_sequence_id,
                Some(chunk_index),
            ));
        }
        return crate::infer_server::direct_ack_with_progress(progress);
    }

    // ── Ring ────────────────────────────────────────────────────────────────
    if !inc.next_hops.is_empty() {
        let base = json!({
            "session_id": sid,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                &local, reuse_requested, chunk_index),
            "is_final_chunk": is_final_chunk,
            "start_pos": local["start_pos"].clone(),
            "next_pos": local["next_pos"].clone(),
            "stable_len": local["stable_len"].clone(),
        });
        return match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape)).await {
            Ok(mut bubbled) => {
                crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
                frame(&bubbled, None)
            }
            Err(resp) => resp,
        };
    }

    // ── Hub ─────────────────────────────────────────────────────────────────
    if inc.is_last {
        frame(&local, None)
    } else {
        frame(&local, Some((&residual, shape)))
    }
}

/// `/v1/step`: one generated token. It does **not** enter the prompt sequence.
async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let sid = inc.meta["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let generated = {
        let mut engine = hop.engine.lock().unwrap();
        let Some(session) = engine.sessions.get_mut(&sid) else {
            return error("unknown session — run the prefill first".into());
        };
        session.gen_len += 1;
        session.gen_len
    };

    // Three tokens per turn, then EOS: what is measured here is the prefill.
    let done = generated >= 3;
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        local["done"] = json!(done);
        local["token_id"] = json!(2);
        local["text"] = json!(if done { "" } else { "x" });
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
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/step",
                json!({"session_id": sid, "hop_timings": hop_timings}),
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
            ));
        }
        return crate::infer_server::direct_ack();
    }

    if !inc.next_hops.is_empty() {
        return match bubble(
            &inc.next_hops,
            "/v1/step",
            json!({"session_id": sid}),
            (&residual, shape),
        )
        .await
        {
            Ok(bubbled) => frame(&bubbled, None),
            Err(resp) => resp,
        };
    }

    if inc.is_last {
        frame(&local, None)
    } else {
        frame(&local, Some((&residual, shape)))
    }
}

/// `/v1/session/rewind`: retention, on the peer's side. It erases the whole
/// generation sequence - full removal - and returns the session to the end of the prompt.
async fn fake_rewind(State(hop): State<Hop>, Json(req): Json<Value>) -> impl IntoResponse {
    let sid = req["session_id"].as_str().unwrap_or_default().to_string();
    let request = req["n_tokens"].as_u64().unwrap_or(0) as u32;
    let mut engine = hop.engine.lock().unwrap();
    let Some(session) = engine.sessions.get_mut(&sid) else {
        engine.rewind_on_unknown_session += 1;
        return Json(json!({"rewound": false, "reason": "unknown session"}));
    };
    // The tail does not tokenize, so its prompt is the number of positions the
    // head sent - the same `prompt_len` as `infer_server`.
    if request != session.prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!("prompt does not match: {request} != {}", session.prompt_len),
        }));
    }
    session.gen_len = 0;
    Json(json!({"rewound": true, "n_past": session.prompt_len}))
}

async fn fake_release(State(hop): State<Hop>, Json(req): Json<Value>) -> impl IntoResponse {
    let sid = req["session_id"].as_str().unwrap_or_default().to_string();
    hop.engine.lock().unwrap().sessions.remove(&sid);
    Json(json!({"released": true}))
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
        .route("/v1/session/rollback", post(fake_rollback))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── The conversation, through the end user's path ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Hub,
    Ring,
    Direct,
}

impl Transport {
    fn caps(self) -> Vec<String> {
        match self {
            Transport::Hub => Vec::new(),
            Transport::Ring => vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()],
            Transport::Direct => vec![
                crate::pipeline::CAP_RELAY_CHAIN.to_string(),
                crate::pipeline::CAP_RELAY_DIRECT.to_string(),
            ],
        }
    }

    fn all() -> [Transport; 3] {
        [Transport::Hub, Transport::Ring, Transport::Direct]
    }
}

fn msg(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: Content::Text(text.into()),
        tool_calls: None,
    }
}

#[derive(Debug)]
struct Turn {
    /// Prompt tokens the head reprocessed in this turn.
    reprocessed: u32,
    /// Chunks that crossed the pipeline in this turn, on the head.
    chunks: u32,
    /// What the gateway reported as used from the cache.
    cached_tokens: u32,
    prompt_tokens: u32,
    /// Prefill time declared by the inference path. It is the deterministic
    /// metric of the simulated work; `first_word_us` includes process
    /// scheduling and only serves to check that the telemetry exists.
    prefill_ms: u64,
    first_word_us: u128,
    /// Greatest prefill pipeline depth observed and the effective ceiling.
    pipeline_depth: u32,
    max_inflight: u32,
    overlap_enabled: bool,
    failed: Option<String>,
}

struct Measurement {
    turns: Vec<Turn>,
    head: Arc<Mutex<Engine>>,
    tail: Arc<Mutex<Engine>>,
    hops: Vec<Arc<Mutex<Engine>>>,
}

/// Runs `turns` turns of a conversation that only grows, through the same path
/// `/v1/chat/completions` uses - including `render_prompt` and
/// `conversation_cache_key`, which is where the rendering asymmetry lived.
///
/// No pause at all between the turns: it is the normal behavior of a chat, and
/// it was what made the next turn arrive before the previous one's release.
async fn conversation(transport: Transport, turns: usize) -> Measurement {
    conversation_with(transport, turns, true).await
}

/// `wait_for_end = false` sends the next turn as soon as the final event
/// reaches the client, without waiting for the gateway task to finish - which
/// is what a real chat does and what exposed the end-of-turn race: the `[DONE]`
/// went out before the rewind ran, and the next turn arrived mid-cleanup.
async fn conversation_with(transport: Transport, turns: usize, wait_for_end: bool) -> Measurement {
    conversation_with_hops(transport, turns, wait_for_end, 2).await
}

async fn conversation_with_hops(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
) -> Measurement {
    conversation_with_hops_fault(transport, turns, wait_for_end, hop_count, None).await
}

async fn conversation_with_hops_fault(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
) -> Measurement {
    conversation_with_hops_fault_kind(
        transport,
        turns,
        wait_for_end,
        hop_count,
        fault,
        FaultKind::UnknownSession,
    )
    .await
}

async fn conversation_with_hops_transport_fault(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
) -> Measurement {
    conversation_with_hops_fault_kind(
        transport,
        turns,
        wait_for_end,
        hop_count,
        fault,
        FaultKind::Transport,
    )
    .await
}

async fn conversation_with_hops_fault_kind(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
    fault_kind: FaultKind,
) -> Measurement {
    // The fake peers only serve HTTP; `PeerInfo::inference_candidates` reads the
    // global TLS policy, which other tests toggle in parallel.
    let _scheme_lock = crate::peer_client::GLOBAL_POLICY.lock();
    crate::peer_client::set_tls_enabled(false);

    assert!(hop_count >= 1);
    let engines: Vec<Arc<Mutex<Engine>>> = (0..hop_count)
        .map(|_| Arc::new(Mutex::new(Engine::default())))
        .collect();
    let mut urls = Vec::new();
    for (i, engine) in engines.iter().enumerate() {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                engine: engine.clone(),
                local_prefill_order: crate::infer_server::DirectForwardOrder::default(),
                direct_forward_order: crate::infer_server::DirectForwardOrder::default(),
            })
            .await,
        );
    }

    if let Some((hop_index, chunk_index, failures)) = fault {
        assert!(hop_index < hop_count);
        let mut engine = engines[hop_index].lock().unwrap();
        match fault_kind {
            FaultKind::UnknownSession => {
                engine.unknown_session_at_chunk = Some(chunk_index);
                engine.unknown_session_failures_remaining = failures;
            }
            FaultKind::Transport => {
                engine.transport_failure_at_chunk = Some(chunk_index);
                engine.transport_failures_remaining = failures;
            }
        }
    }

    let caps = transport.caps();
    let peers: Vec<crate::network::PeerInfo> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = crate::gateway::testkit::peer(
                &format!("hop{i}"),
                &format!("m{i}"),
                9000 + i as u16,
            );
            p.addr = "127.0.0.1:42777".into();
            p.inference_port = Some(port);
            p.model = "lab".into();
            p.start_layer = i as u32;
            p.end_layer = i as u32;
            p.total_layers = hop_count as u32;
            p.inference_frame_capabilities = caps.clone();
            if matches!(fault_kind, FaultKind::Transport) {
                // Keeps the old announced address in front and leaves the
                // current `addr` as fallback. The scenario proves both the
                // prefill recovery and the release through the live route.
                p.addr_candidates = vec!["127.0.0.1:1".into()];
            }
            p
        })
        .collect();

    let state = crate::gateway::testkit::state_with("eu", peers).await;
    let mut config = crate::gateway::testkit::spec_config();
    config.speculative_decoding_enabled = false;
    // Small chunks: that is how the savings show up as *fewer chunks*, and
    // not only as fewer tokens.
    config.prefill_chunk_tokens = 4;
    state.runtime.seed_config_for_test(config).await;
    let state = crate::gateway::testkit::with_self_reachable_gateway(state).await;

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut measured_turns = Vec::new();
    for turn in 0..turns {
        // The real discovery loop refreshes `last_seen`; this fixture has no
        // UDP traffic, so refresh the synthetic peers before every request.
        // Otherwise a busy full test process can let the 30-second liveness
        // window expire between turns and make the direct path fail before it
        // reaches the cache code.
        {
            let now = chrono::Utc::now();
            let mut peers = state.network.peers.write().await;
            for peer in peers.values_mut() {
                peer.last_seen = now;
            }
        }
        let question = if turn == 0 {
            "pergunta inicial com bastante contexto para aquecer o prompt e medir o custo frio"
                .to_string()
        } else {
            format!("short followup {turn}")
        };
        history.push(msg("user", &question));
        let prompt = crate::openai::render_prompt(&history, &[]);
        let cache_key = crate::openai::conversation_cache_key(&history, &[]);

        let before = engines[0].lock().unwrap().prompt_chunks.len();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let bg = tokio::spawn(crate::gateway::generate_stream_with_stops(
            crate::gateway::GenerationInput {
                probe: None,
                state: state.clone(),
                inference_token: "test-api-key".into(),
                model: "lab".into(),
                prompt,
                images: Vec::new(),
                chat_template: true,
                thinking_enabled: true,
                budget: crate::gateway::OutputBudget::single(8),
                stops: Vec::new(),
                cache_key: Some(cache_key),
                preferred_head: None,
                request_details: serde_json::json!({}),
                trace_id: None,
            },
            tx,
        ));

        let mut response = String::new();
        let mut measured = Turn {
            reprocessed: 0,
            chunks: 0,
            cached_tokens: 0,
            prompt_tokens: 0,
            prefill_ms: 0,
            first_word_us: 0,
            pipeline_depth: 0,
            max_inflight: 0,
            overlap_enabled: false,
            failed: None,
        };
        let started = std::time::Instant::now();
        let mut saw_first_word = false;
        while let Some(event) = rx.recv().await {
            let terminal = match event {
                crate::gateway::GenEvent::Token(t) => {
                    if !saw_first_word && !t.is_empty() {
                        measured.first_word_us = started.elapsed().as_micros();
                        saw_first_word = true;
                    }
                    response.push_str(&t);
                    false
                }
                crate::gateway::GenEvent::Reasoning(t) => {
                    if !saw_first_word && !t.is_empty() {
                        measured.first_word_us = started.elapsed().as_micros();
                        saw_first_word = true;
                    }
                    false
                }
                crate::gateway::GenEvent::Done(s) => {
                    if let Some(trace) = &s.trace {
                        measured.cached_tokens = trace.cached_tokens;
                        measured.prompt_tokens = trace.input_tokens;
                        measured.prefill_ms = s.timings.prefill_ms;
                        measured.pipeline_depth = trace.prefill_pipeline_depth_max;
                        measured.max_inflight = trace.prefill_max_inflight_chunks;
                        measured.overlap_enabled = trace.prefill_overlap_enabled;
                    }
                    true
                }
                crate::gateway::GenEvent::Failed(e) => {
                    measured.failed = Some(e);
                    true
                }
                crate::gateway::GenEvent::FailedWithTelemetry { error, .. } => {
                    measured.failed = Some(error);
                    true
                }
            };
            if terminal && !wait_for_end {
                break;
            }
        }
        if wait_for_end {
            let _ = bg.await;
        }
        {
            let e = engines[0].lock().unwrap();
            let this_turn = &e.prompt_chunks[before..];
            measured.reprocessed = this_turn.iter().sum();
            measured.chunks = this_turn.len() as u32;
        }
        history.push(msg(
            "assistant",
            if response.is_empty() { "ok" } else { &response },
        ));
        measured_turns.push(measured);
    }
    Measurement {
        turns: measured_turns,
        head: engines[0].clone(),
        tail: engines.last().unwrap().clone(),
        hops: engines,
    }
}

// ── What the bench asserts ────────────────────────────────────────────────────

/// The report's reproduction, turned upside down: what was 0 in 100% of the
/// turns has to be positive from the second one on, and the prefill cost
/// has to stop growing with the conversation.
#[tokio::test]
async fn from_the_second_turn_on_the_prompt_is_not_reprocessed() {
    for transport in Transport::all() {
        let measurement = conversation(transport, 4).await;
        let turns = &measurement.turns;
        let context = format!("{transport:?}");

        eprintln!(
            "[{context}] prefix-cache metrics: {}",
            turns
                .iter()
                .enumerate()
                .map(|(i, t)| format!(
                    "turn={i} cached={} prefill_ms={} ttft_us={} chunks={} prompt={} reprocessed={}",
                    t.cached_tokens, t.prefill_ms, t.first_word_us, t.chunks, t.prompt_tokens, t.reprocessed
                ))
                .collect::<Vec<_>>()
                .join("; ")
        );

        for (i, t) in turns.iter().enumerate() {
            assert!(
                t.failed.is_none(),
                "[{context}] turn {i} failed: {:?}",
                t.failed
            );
        }

        assert_eq!(
            turns[0].cached_tokens, 0,
            "[{context}] the first turn is cold by definition"
        );
        assert_eq!(
            turns[0].reprocessed, turns[0].prompt_tokens,
            "[{context}] the first turn ingests the whole prompt"
        );

        for (i, t) in turns.iter().enumerate().skip(1) {
            assert!(
                t.cached_tokens > 0,
                "[{context}] turn {i} reused nothing of the prefix: {turns:?}"
            );
            assert_eq!(
                t.cached_tokens + t.reprocessed,
                t.prompt_tokens,
                "[{context}] turn {i}: cache + decoded must add up to the prompt"
            );
            assert_eq!(
                t.cached_tokens,
                turns[i - 1].prompt_tokens.saturating_sub(1),
                "[{context}] turn {i} should reuse the stable prefix of the previous turn"
            );
            assert!(
                t.first_word_us > 0,
                "[{context}] turn {i} did not record the time to the first word"
            );
        }

        // The property the captain sees as time to the first word: the
        // work of a turn is the size of the **new** text, not of the whole
        // conversation. Without reuse this series grows every turn - it was the
        // flat 3.8s TTFT with the prompt growing, and the 171s to append
        // 1043 characters, from the report.
        let work: Vec<u32> = turns.iter().map(|t| t.reprocessed).collect();
        let prompts: Vec<u32> = turns.iter().map(|t| t.prompt_tokens).collect();
        let chunks: Vec<u32> = turns.iter().map(|t| t.chunks).collect();
        assert!(
            prompts.windows(2).all(|w| w[1] > w[0]),
            "[{context}] the conversation had to be growing: {prompts:?}"
        );
        assert!(
            work[1..].iter().all(|t| *t < prompts[1]),
            "[{context}] algum turno reprocessou o prompt quase inteiro: \
             trabalho={work:?} prompts={prompts:?}"
        );
        assert!(
            work[1..].windows(2).all(|w| w[1] <= w[0] + 2),
            "[{context}] the cost per turn grows with the conversation: {work:?}"
        );
        assert!(
            turns[1..]
                .iter()
                .all(|t| t.prefill_ms < turns[0].prefill_ms),
            "[{context}] the measured prefill did not drop after the first turn: {:?}",
            turns.iter().map(|t| t.prefill_ms).collect::<Vec<_>>()
        );
        // Fewer tokens to ingest are fewer chunks crossing the pipeline,
        // which is the number the production log shows.
        assert!(
            chunks[1..].iter().all(|p| *p <= chunks[0] + 1),
            "[{context}] the number of chunks grew with the conversation: {chunks:?}"
        );

        for (name, engine) in [("head", &measurement.head), ("cauda", &measurement.tail)] {
            let engine = engine.lock().unwrap();
            assert!(
                engine.partial_removals.is_empty(),
                "[{context}] the reuse depended on a partial KV removal at the {name}, which the design forbids: {:?}",
                engine.partial_removals
            );
            assert_eq!(
                engine.rewind_on_unknown_session, 0,
                "[{context}] the session of the {name} was destroyed before the gateway preserved the prefix"
            );
            for session in engine.sessions.values() {
                assert_eq!(
                    session.prompt_kv.len() as u32,
                    session.prompt_len,
                    "[{context}] {name} retained generation tokens in the prompt sequence"
                );
                assert_eq!(
                    session.gen_len, 0,
                    "[{context}] {name} left the generation sequence alive after the rewind"
                );
            }
        }
    }
}

/// The prefill of a multi-chunk prompt has to **fill the pipeline**: with
/// the chain slower than the head, the head's pump runs ahead and several
/// chunks are in flight at the same time. Ring mode stays serialized.
#[tokio::test]
async fn a_multi_chunk_prefill_fills_the_pipeline() {
    // 4 hops: the downstream (3 hops) is ~3x the head's cost, so the head's
    // pump fills the window. The cold prompt of turn 0 has several chunks of 4
    // tokens.
    for transport in [Transport::Hub, Transport::Direct] {
        let measurement = conversation_with_hops(transport, 1, true, 4).await;
        let t = &measurement.turns[0];
        assert!(
            t.failed.is_none(),
            "[{transport:?}] turn failed: {:?}",
            t.failed
        );
        assert!(
            t.chunks >= 2,
            "[{transport:?}] the cold prompt had to have more than one chunk: {}",
            t.chunks
        );
        assert!(
            t.overlap_enabled,
            "[{transport:?}] the overlap should be on in a plan with several hops"
        );
        assert!(
            t.pipeline_depth >= 2,
            "[{transport:?}] the pipeline did not fill: depth {} (cap {})",
            t.pipeline_depth,
            t.max_inflight
        );
        assert!(
            t.pipeline_depth <= t.max_inflight,
            "[{transport:?}] depth {} above the cap {}",
            t.pipeline_depth,
            t.max_inflight
        );
    }

    // On the ring the overlap belongs to the hops (`ring_overlap`): the gateway
    // stays at one chunk at a time, and each hop answers the intermediary after
    // its own compute and forwards in the background, in order.
    let ring = conversation_with_hops(Transport::Ring, 1, true, 4).await;
    let t = &ring.turns[0];
    assert!(t.failed.is_none(), "[Ring] turn failed: {:?}", t.failed);
    assert!(
        t.overlap_enabled,
        "[Ring] the hop overlap should be requested"
    );
    assert!(
        t.pipeline_depth <= 1,
        "[Ring] the gateway does not send more than one chunk at a time: depth {}",
        t.pipeline_depth
    );
}

/// A peer can lose its state between two chunks of a cold prefill. The
/// caller has the whole prompt and has to rebuild all the hops from
/// chunk 0; repeating only the current hidden state would leave the peer without context.
#[tokio::test]
async fn unknown_session_during_prefill_is_recovered() {
    for transport in Transport::all() {
        let measurement =
            conversation_with_hops_fault(transport, 1, true, 2, Some((1, 1, 1))).await;
        let turn = &measurement.turns[0];
        assert!(
            turn.failed.is_none(),
            "{transport:?} failed after losing the session: {:?}",
            turn.failed
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            assert_eq!(
                engine
                    .frames
                    .iter()
                    .filter(|frame| frame.chunk_index == 0)
                    .count(),
                2,
                "{transport:?} did not restart the prefill from chunk 0 at hop {i}: {:?}",
                engine.frames
            );
        }
        assert!(
            turn.reprocessed > turn.prompt_tokens,
            "{transport:?} did not record the cost of the cold rebuild: {turn:?}"
        );

        let hop = measurement.hops[1].lock().unwrap();
        assert_eq!(
            hop.rewind_on_unknown_session, 0,
            "{transport:?} ended without a preservable session on the recovered peer"
        );
    }
}

/// A persistent loss must not make the gateway repeat the prefill
/// forever. After one rebuild, the failure reaches the client without the
/// internal instruction only a peer could execute.
#[tokio::test]
async fn unknown_session_recovery_is_bounded() {
    for transport in Transport::all() {
        let measurement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation_with_hops_fault(transport, 1, true, 2, Some((1, 1, usize::MAX))),
        )
        .await
        .expect("the unknown session recovery cannot loop");
        let turn = &measurement.turns[0];
        let message = turn
            .failed
            .as_deref()
            .expect("the persistent refusal should reach the client");
        assert!(
            !message.contains("reset=true"),
            "internal message leaked: {message}"
        );
        assert!(
            message.contains("the request was not executed"),
            "the final message does not explain the impact to the user: {message}"
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            let cold_attempts = engine
                .frames
                .iter()
                .filter(|frame| frame.chunk_index == 0)
                .count();
            assert!(
                cold_attempts <= 2,
                "{transport:?} made a third cold attempt at hop {i}: {cold_attempts}",
            );
        }
    }
}

/// If the connection drops after the head accepts the chunk, a recurrent engine
/// cannot receive the partial repetition: it cannot remove the middle of the KV.
/// The recovery must fail without asking for that removal and release the
/// sessions left on the hops that were reached.
#[tokio::test]
async fn mid_prefill_transport_failure_does_not_corrupt_recurrent_session() {
    for transport in Transport::all() {
        let measurement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation_with_hops_transport_fault(transport, 1, true, 2, Some((1, 1, 2))),
        )
        .await
        .expect("the transport failure cannot leave the session hanging");
        let turn = &measurement.turns[0];
        assert!(
            turn.failed.is_some(),
            "{transport:?} should return the persistent failure to the client"
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            assert!(
                engine.partial_removals.is_empty(),
                "{transport:?} asked for a partial removal at hop {i}: {:?}",
                engine.partial_removals
            );
            assert!(
                engine.sessions.is_empty(),
                "{transport:?} left the session at hop {i}: {:?}",
                engine.sessions.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// The additional defect of §7 of the report: a chat sends the next turn as soon
/// as the stream closes, and the previous turn's cleanup had to be ready
/// before that. Here the client does not wait for the gateway task - it acts on
/// the final event, as the chat screen acts on `[DONE]`.
#[tokio::test]
async fn a_follow_up_sent_the_instant_the_stream_closes_still_hits() {
    for transport in Transport::all() {
        let measurement = conversation_with(transport, 4, false).await;
        let cached: Vec<u32> = measurement.turns.iter().map(|t| t.cached_tokens).collect();
        for (i, t) in measurement.turns.iter().enumerate() {
            assert!(
                t.failed.is_none(),
                "[{transport:?}] turn {i} failed: {:?}",
                t.failed
            );
        }
        assert!(
            cached[1..].iter().all(|c| *c > 0),
            "[{transport:?}] the immediate turn lost the cache: {cached:?}"
        );
    }
}

/// Defect D, isolated: the transition from turn 1 to turn 2 is the one that
/// **never** hit, because turn 1 rendered raw and turn 2 labeled. It is the
/// first one a real chat meets.
#[tokio::test]
async fn the_first_to_second_turn_transition_hits() {
    for transport in Transport::all() {
        let measurement = conversation(transport, 2).await;
        assert!(
            measurement.turns[1].cached_tokens > 0,
            "[{transport:?}] the 1→2 transition reused nothing: {:?}",
            measurement.turns
        );
    }
}

/// The real conversation uses three machines in the incident's pipeline. The
/// state that counts for the contract is not the chunk's logical index, but the
/// remote counter of the peer that has just executed the frame.
#[tokio::test]
async fn three_hop_follow_ups_keep_each_remote_position_in_sync() {
    for transport in [Transport::Ring, Transport::Direct] {
        for hop_count in [3, 5] {
            let measurement = conversation_with_hops(transport, 4, true, hop_count).await;

            for (hop_index, engine) in measurement.hops.iter().enumerate() {
                let engine = engine.lock().unwrap();
                assert!(
                    !engine.frames.is_empty(),
                    "{transport:?}/{hop_count} hops did not run the prefill at hop {hop_index}"
                );
                for frame in &engine.frames {
                    assert_eq!(
                        frame.start_pos, frame.n_past_before,
                        "{transport:?}/{hop_count} hops, session {}: chunk {} arrived at position {} but the peer was at {}",
                        frame.session_id,
                        frame.chunk_index,
                        frame.start_pos,
                        frame.n_past_before
                    );
                    assert_eq!(frame.next_pos, frame.n_past_after);
                }
                assert_eq!(
                    engine
                        .frames
                        .iter()
                        .map(|frame| frame.session_id.as_str())
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    1,
                    "{transport:?}/{hop_count} hops used more than one session in the same conversation at hop {hop_index}"
                );
            }

            assert!(
                measurement.turns[1..]
                    .iter()
                    .all(|turn| turn.cached_tokens > 0),
                "{transport:?}/{hop_count} hops lost the cache in the follow-ups: {:?}",
                measurement.turns
            );
        }
    }
}

/// A whole node also has to advance by absolute position. The case reproduces
/// the session retained at 42 tokens and a following prompt that requires a
/// second chunk: `chunk_index * 512` cannot replace the confirmed `next_pos`.
#[tokio::test]
async fn single_node_long_follow_up_does_not_send_chunk_by_index() {
    let measurement = conversation_with_hops(Transport::Direct, 2, true, 1).await;
    let engine = measurement.head.lock().unwrap();
    let first_session = engine
        .frames
        .first()
        .map(|frame| frame.session_id.clone())
        .expect("the node did not receive the first frame");
    let follow_up_frames: Vec<_> = engine
        .frames
        .iter()
        .filter(|frame| frame.session_id == first_session && frame.chunk_index > 0)
        .collect();
    assert!(
        follow_up_frames
            .iter()
            .all(|frame| frame.start_pos == frame.n_past_before),
        "a chunk was sent by the logical position, not by the remote position: {follow_up_frames:?}"
    );
}
