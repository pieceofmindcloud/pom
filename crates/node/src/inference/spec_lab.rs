//! Speculation bench: **a speculative round that does not draft still has
//! to generate text**.
//!
//! The wire contract already says the head may refuse the draft - it is written
//! in the doc of `step_blocking::incoming_draft`: an absent `draft` means "this
//! round is not speculative, the old one-token format applies".
//!
//! **Correction of 2026-08-16, after this bench was written**: the head
//! thought it refused "whenever the model's KV cache does not shift"
//! (`kv_can_shift`) and called that a property of every hybrid or recurrent
//! model. It was a mistake on both counts - `kv_can_shift`
//! (`llama_memory_can_shift`) is about the K-shift of llama.cpp's reference
//! `--ctx-shift`, nothing to do with undoing a draft, and nothing in this project
//! turns `--ctx-shift` on; and Qwen3.5/3.8 (`LLM_ARCH_QWEN35`) has dedicated
//! support for partial recurrent rollback in llama.cpp
//! (`llm_arch_supports_rs_rollback`/`cparams.n_rs_seq`), which only needed to be
//! switched on (`EngineConfig::rs_seq_window`). `should_draft` no longer takes
//! `kv_can_shift` - today every lookup invocation drafts, and the only refusal
//! left is MTP with no local `nextn` head and no draft ready. What this
//! bench measured (refusal before drafting, followed by a direct reply) can
//! no longer happen on the lookup path; the real defect that can happen
//! now is *after* drafting - `gateway::rollback_sessions`
//! refused -, which is a different bench.
//!
//! What was missing was the other half of the contract: whoever asked for the
//! speculation could not read the old reply, and treated a legitimate reply as a
//! fatal error. The visible effect is the request dying with **one** token and
//! `partial: true`, which is what the captain saw on 2026-08-15 on Qwen3.8-27B.
//! That half still holds and is what the tests below still prove.
//!
//! Here the orchestration path is the whole production one -
//! `generate_stream_with_stops`,
//! `prefill::run`, `prefill::step_speculative`, `relay::send_frame`,
//! `infer_server::forward_direct`, `direct::deliver_result` - and the fake hops
//! decide with the production functions themselves (`should_draft`, `find_draft`,
//! `longest_match`). The only pretence is the engine: instead of decoding, the hop
//! returns zeroed tensors and tokens from a fixed alphabet.
//!
//! **Limit of this bench:** with no GGUF and no llama.cpp on this machine, the
//! real engine does not come in. What it proves is the contract between the
//! processes, which is where the defect lives; what it does not prove is what
//! llama.cpp does with the batch.

#![cfg(test)]

use crate::network::PeerInfo;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Small on purpose: the bench measures protocol, not memory.
const HIDDEN_DIM: usize = 8;
/// Tokens the test prompt is worth to the fake head.
const PROMPT_TOKENS: u32 = 6;

// ── The fake hop ──────────────────────────────────────────────────────────────

/// A fake inference peer.
#[derive(Clone)]
struct Hop {
    is_head: bool,
    /// Per-session state the bench needs: how many tokens have already come
    /// out, so the "engine" returns different text each round and stops at the end.
    emitted: Arc<AtomicU64>,
    /// How many times `/v1/step` was served on this hop - it is the hops'
    /// `decode.calls`, seen from the inside.
    step_calls: Arc<AtomicU64>,
    /// How many of those steps came with `reply_to`, that is, through the direct
    /// reply path. `step_speculative` never sends that field, so this
    /// counter is the proof that the chain left the ring.
    direct_steps: Arc<AtomicU64>,
}

/// Tokens the fake engine "samples", in order. It ends before running out so
/// that a test with a high `max_tokens` still closes by EOS.
const ALPHABET: [&str; 8] = ["A", "B", "C", "D", "E", "F", "G", "H"];

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

async fn fake_ok() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

/// The bench has no real cache for a rollback to fail against -
/// only the real llama.cpp decides that (`n_rs_seq`/`seq_rm`, see the module doc).
/// The shape of the reply matters: `gateway::rollback_sessions` reads the body,
/// not only the HTTP status, so `fake_ok` (`{"ok": true}`) would read as refused.
async fn fake_rollback() -> impl IntoResponse {
    Json(json!({"rolled_back": true}))
}

/// Read with the same rules as the real `infer_server`.
struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
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
    let reply_to = crate::infer_server::direct_reply_to(&meta);
    Incoming {
        inbound_bytes: body.len(),
        next_hops: declared.unwrap_or_default(),
        is_last,
        reply_to,
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
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(0);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
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
            // The first token of the generation, sampled by the tail.
            let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
            local["token_id"] = json!(i as u32);
            local["text"] = json!(ALPHABET[i % ALPHABET.len()]);
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

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
        return crate::infer_server::direct_ack();
    }

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
    match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape)).await {
        Ok(mut bubbled) => {
            crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
            frame(&bubbled, None)
        }
        Err(resp) => resp,
    }
}

/// Same decision tree as `step_blocking`, with the production functions at the
/// points that matter: `should_draft` decides whether the head drafts,
/// `find_draft` builds the draft and `longest_match` verifies at the tail.
async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.step_calls.fetch_add(1, Ordering::Relaxed);
    let inc = read(&body);
    if inc.reply_to.is_some() {
        hop.direct_steps.fetch_add(1, Ordering::Relaxed);
    }

    let speculate = crate::infer_server::Speculate::from_meta(&inc.meta);
    let incoming_draft: Option<Vec<i32>> = inc
        .meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // ── Head: decides the draft, exactly like `step_blocking` ────────────────
    let mut head_draft: Option<Vec<i32>> = None;
    let mut n_positions = 1usize;
    if hop.is_head {
        // `mtp_ready` is false on this bench: it moves frames between fake
        // processes, with no engine at all, so the only draft source it
        // can really exercise is the lookup.
        if crate::infer_server::should_draft(speculate.clone(), false) {
            let spec = speculate.expect("should_draft implies speculate");
            // History fabricated with a repeating pattern, so that
            // `find_draft` has something to find.
            let history: Vec<i32> = (0..24).map(|i| i % 4).collect();
            let draft = crate::infer_server::find_draft(&history, spec.m as usize, spec.k);
            n_positions = 1 + draft.len();
            head_draft = Some(draft);
        }
    } else {
        n_positions = incoming_draft.as_ref().map(|d| 1 + d.len()).unwrap_or(1);
    }
    let draft = head_draft.or(incoming_draft);

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        if let Some(draft) = draft {
            // Verification: the fake engine "samples" the alphabet in order, so
            // no draft matches - `longest_match` returns 0 and the round
            // confirms one token, which is the legitimate worst case.
            let mut sampled: Vec<i32> = Vec::new();
            let mut texts: Vec<String> = Vec::new();
            for _ in 0..n_positions {
                let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
                sampled.push(i as i32);
                texts.push(ALPHABET[i % ALPHABET.len()].to_string());
            }
            let keep =
                (crate::infer_server::longest_match(&sampled, &draft) + 1).min(sampled.len());
            // What went past the accepted count did not go out: give it back
            // to the counter.
            hop.emitted
                .fetch_sub((n_positions - keep) as u64, Ordering::Relaxed);
            let accepted: Vec<Value> = sampled[..keep]
                .iter()
                .zip(&texts[..keep])
                .map(|(id, text)| json!({"token_id": id, "text": text}))
                .collect();
            local["done"] = json!(false);
            local["accepted"] = json!(accepted);
            local["accept_count"] = json!(keep as u32);
            local["batch_width"] = json!(n_positions as u32);
        } else {
            // Old format, one token - it is what the contract requires when
            // `draft` is absent.
            let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
            local["done"] = json!(false);
            local["token_id"] = json!(i as u32);
            local["text"] = json!(ALPHABET[i % ALPHABET.len()]);
        }
    } else {
        local["done"] = json!(false);
        if let Some(draft) = &draft {
            local["draft"] = json!(draft);
        }
    }

    let residual = vec![0.0f32; n_positions * HIDDEN_DIM];
    let shape = vec![n_positions, HIDDEN_DIM];

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
            let mut forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "hop_timings": hop_timings,
            });
            if let Some(d) = local.get("draft") {
                forward["draft"] = d.clone();
            }
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
        return crate::infer_server::direct_ack();
    }

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let mut base = json!({"session_id": inc.meta["session_id"].clone()});
    if let Some(d) = local.get("draft") {
        base["draft"] = d.clone();
    }
    match bubble(&inc.next_hops, "/v1/step", base, (&residual, shape)).await {
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
        .route("/v1/session/release", post(fake_ok))
        .route("/v1/session/rollback", post(fake_rollback))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── The scenario, built on the end user's path ────────────────────────────────

/// What the client sees at the end of a `generate_stream_with_stops`.
struct Seen {
    text: String,
    tokens: u32,
    partial: bool,
    timings: Option<crate::gateway::GenerationTimings>,
    /// `decode.calls` of each hop in the trace - the number the captain read
    /// in `x_pom` and saw zeroed on both.
    decode_calls: Vec<u32>,
    /// How many times each hop served `/v1/step`, seen from inside the hops.
    step_calls: Vec<u64>,
    /// Of those, how many arrived through the direct reply path.
    direct_steps: Vec<u64>,
    failed: Option<String>,
}

/// Brings up two fake hops, a real gateway with the captain's config and runs
/// a generation through the same path `/v1/chat/completions` uses.
async fn generate(direct: bool, max_tokens: u32, n_batch: u32) -> Seen {
    // With `RUST_LOG=info` the bench prints the same lines the captain read
    // on the server - that is how one checks it reproduces his case, and not
    // a cousin of it.
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

    let emitted = Arc::new(AtomicU64::new(0));
    let step_calls: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let direct_steps: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();

    let mut urls = Vec::new();
    for i in 0..2 {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                emitted: emitted.clone(),
                step_calls: step_calls[i].clone(),
                direct_steps: direct_steps[i].clone(),
            })
            .await,
        );
    }

    // This fixture exposes only the transports it actually implements. The
    // fake hop serves one wire frame at a time, so advertising frame batching
    // would route the request through BatchCoordinator without a batch-aware
    // fake endpoint and make the fixture claim a capability it cannot honor.
    let mut caps = vec![
        crate::pipeline::CAP_RELAY_CHAIN.to_string(),
        crate::pipeline::CAP_SPECULATE_LOOKUP.to_string(),
    ];
    if direct {
        caps.push(crate::pipeline::CAP_RELAY_DIRECT.to_string());
    }

    let peers: Vec<PeerInfo> = urls
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
            p.total_layers = 2;
            p.inference_frame_capabilities = caps.clone();
            p
        })
        .collect();

    let state = crate::gateway::testkit::state_with("eu", peers).await;
    let mut config = crate::gateway::testkit::spec_config();
    config.n_batch = n_batch;
    state.runtime.seed_config_for_test(config).await;

    // The real gateway, on loopback, so the tail has somewhere to deliver the
    // direct reply. Without it direct mode would wait out the whole deadline.
    let state = crate::gateway::testkit::with_self_reachable_gateway(state).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let bg = tokio::spawn(crate::gateway::generate_stream_with_stops(
        crate::gateway::GenerationInput {
            probe: None,
            state,
            inference_token: "test-api-key".into(),
            model: "lab".into(),
            prompt: "some prompt for the bench".into(),
            images: Vec::new(),
            chat_template: true,
            thinking_enabled: true,
            budget: crate::gateway::OutputBudget::single(max_tokens),
            stops: Vec::new(),
            cache_key: None,
            preferred_head: None,
            request_details: serde_json::json!({}),
            trace_id: None,
        },
        tx,
    ));

    let mut seen = Seen {
        text: String::new(),
        tokens: 0,
        partial: false,
        timings: None,
        decode_calls: Vec::new(),
        step_calls: Vec::new(),
        direct_steps: Vec::new(),
        failed: None,
    };
    while let Some(ev) = rx.recv().await {
        match ev {
            crate::gateway::GenEvent::Token(t) => seen.text.push_str(&t),
            crate::gateway::GenEvent::Done(sum) => {
                seen.tokens = sum.tokens_generated;
                seen.partial = sum.partial;
                seen.timings = Some(sum.timings);
                seen.decode_calls = sum
                    .trace
                    .as_ref()
                    .map(|t| t.hops.iter().map(|h| h.decode.calls).collect())
                    .unwrap_or_default();
            }
            crate::gateway::GenEvent::Failed(e) => seen.failed = Some(e),
            crate::gateway::GenEvent::FailedWithTelemetry { error, .. } => {
                seen.failed = Some(error)
            }
        }
    }
    let _ = bg.await;
    seen.step_calls = step_calls
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    seen.direct_steps = direct_steps
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    seen
}

// ── The law ───────────────────────────────────────────────────────────────────
//
// The three laws that proved refusal-before-drafting via `kv_can_shift`
// left here on 2026-08-16 (see the module doc): the only refusal
// `should_draft` still makes is MTP with no local `nextn` head and no draft
// ready, which this bench (lookup only, no MTP) does not build. What
// still holds - every lookup invocation drafts, direct or through the
// ring - is proved below.

/// Whoever drafts stays on the ring, which is the only path
/// `step_speculative` knows - with no direct reply negotiated.
#[tokio::test]
async fn the_drafter_on_the_ring_generates_the_whole_text() {
    let seen = generate(false, 5, 512).await;

    assert_eq!(seen.failed, None, "the generation cannot fail");
    assert_eq!(seen.tokens, 5);
    assert!(!seen.partial);
    assert_eq!(seen.text, "ABCDE");
    let timings = seen.timings.expect("the response must carry the times");
    assert_eq!(timings.generation_tokens, seen.tokens);
    assert!(timings.ttft_ms >= timings.prefill_ms);
    // A sub-millisecond fake prefill is intentionally unmeasurable at the
    // public millisecond resolution; the contract here is that the prompt
    // was processed, not that a rate is fabricated from a zero duration.
    assert!(timings.prefill_tokens > 0);
    assert!(
        seen.direct_steps.iter().all(|c| *c == 0),
        "without a negotiated direct reply, no step can go out that way: {:?}",
        seen.direct_steps
    );
    assert!(seen.decode_calls.iter().all(|c| *c > 0));
}

/// And with a direct reply negotiated: every round speculates, so none
/// can be diverted - without this the previous test would pass by accident.
#[tokio::test]
async fn the_drafter_with_direct_reply_is_not_diverted_to_it() {
    let seen = generate(true, 5, 512).await;
    assert_eq!(seen.failed, None);
    assert_eq!(seen.tokens, 5);
    assert!(!seen.partial);
    assert_eq!(seen.text, "ABCDE");
    let timings = seen.timings.expect("the response must carry the times");
    assert_eq!(timings.generation_tokens, seen.tokens);
    assert!(timings.ttft_ms >= timings.prefill_ms);
    // See the no-ring case above: this fake can complete below one millisecond.
    assert!(timings.prefill_tokens > 0);
    assert_eq!(
        seen.direct_steps[0], 0,
        "every round speculated, so none could go out through the direct path"
    );
    assert!(seen.decode_calls.iter().all(|c| *c > 0));
}

#[tokio::test]
async fn a_one_token_response_keeps_the_rates_finite() {
    let seen = generate(false, 1, 512).await;

    assert_eq!(seen.failed, None);
    assert_eq!(seen.tokens, 1);
    let timings = seen.timings.expect("the response must carry the times");
    assert_eq!(timings.generation_tokens, 1);
    assert!(timings.total_tokens_per_sec.is_finite());
    assert!(timings.generation_tokens_per_sec.is_none_or(f64::is_finite));
}

/// `n_batch=512` only limits the width of one round in the engine, not the
/// output budget requested from the gateway.
#[tokio::test]
async fn a_budget_above_n_batch_is_not_truncated() {
    for n_batch in [512, 8] {
        let seen = generate(false, 2048, n_batch).await;

        assert_eq!(seen.failed, None, "the generation cannot fail");
        assert_eq!(seen.tokens, 2048, "n_batch={n_batch} cannot be the cap");
        assert!(!seen.partial);
        assert_eq!(seen.step_calls, vec![2047, 2047]);
    }
}
