//! HTTP inference server — the Rust replacement for `inference_server.py`.
//!
//! Exposes the same four endpoints the Python server did:
//!
//!   POST /v1/prefill          — one chunk of the prompt
//!   POST /v1/step             — one autoregressive token step
//!   POST /v1/session/release  — free a session's KV slot
//!   GET  /health              — readiness probe
//!
//! Wire format is defined in [`crate::wire`] (binary framing, f16 on the wire).
//! Decode calls run in `spawn_blocking` so they don't starve the tokio runtime.

use crate::llm::{sample_token, EngineConfig, LlmEngine, SamplingConfig};
use crate::runtime::NodeConfig;
use crate::wire::{self, WireDtype};
use anyhow::{bail, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

const SESSION_IDLE_TIMEOUT_SECS: u64 = 900;
/// Upper bound for one binary f16 hidden-state frame received from a peer.
///
/// A normal 512-token chunk with a 4096-wide hidden state is 4 MiB before its
/// small wire header. Keeping this limit explicit at 16 MiB allows the
/// documented 8 MiB frame while still rejecting unbounded peer input.
const INFERENCE_FRAME_BODY_LIMIT: usize = 16 * 1024 * 1024;

fn inference_frame_body_limit() -> usize {
    INFERENCE_FRAME_BODY_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::DefaultBodyLimit,
        http::Request,
    };
    use tower::ServiceExt;

    async fn accept_frame(body: Bytes) -> StatusCode {
        assert_eq!(body.len(), 4 * 1024 * 1024);
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn prefill_route_accepts_a_default_f16_frame_larger_than_axums_two_mib_default() {
        let app = Router::new()
            .route("/v1/prefill", post(accept_frame))
            .layer(DefaultBodyLimit::max(inference_frame_body_limit()));
        let request = Request::post("/v1/prefill")
            .body(Body::from(vec![0_u8; 4 * 1024 * 1024]))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024).await.unwrap().is_empty());
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

struct Session {
    seq_id: u32,
    n_past: u32,
    recent_tokens: Vec<i32>,
    /// Stored by the first peer so subsequent chunks don't re-tokenize.
    prompt_tokens: Vec<i32>,
    last_used: Instant,
}

impl Session {
    fn new(seq_id: u32) -> Self {
        Self {
            seq_id,
            n_past: 0,
            recent_tokens: Vec::new(),
            prompt_tokens: Vec::new(),
            last_used: Instant::now(),
        }
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

struct InferState {
    engine: LlmEngine,
    sessions: HashMap<String, Session>,
    seq_pool: Vec<u32>,
    max_sessions: u32,
    max_new_tokens: u32,
    n_batch: u32,
    sampling: SamplingConfig,
    no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    chat_template: bool,
    start_layer: u32,
    end_layer: u32,
    original_block_count: Option<u32>,
}

type SharedState = Arc<Mutex<InferState>>;

impl InferState {
    fn new(engine: LlmEngine, cfg: &NodeConfig) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        Self {
            engine,
            sessions: HashMap::new(),
            seq_pool,
            max_sessions,
            max_new_tokens: cfg.max_new_tokens,
            n_batch: cfg.n_batch,
            sampling: SamplingConfig {
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                top_k: cfg.top_k as i32,
                repeat_penalty: cfg.repeat_penalty,
                repeat_last_n: cfg.repeat_last_n as usize,
            },
            no_thinking: cfg.no_thinking,
            chat_template: cfg.no_thinking, // --no-thinking implies --chat-template
            start_layer: cfg.start_layer,
            end_layer: cfg.end_layer,
            original_block_count: None, // filled in after engine load
        }
    }

    fn alloc_seq(&mut self) -> Option<u32> {
        if self.seq_pool.is_empty() {
            let reaped = self.reap_idle();
            if reaped == 0 {
                return None;
            }
        }
        Some(self.seq_pool.remove(0))
    }

    fn free_seq(&mut self, seq_id: u32) {
        if !self.seq_pool.contains(&seq_id) {
            self.seq_pool.push(seq_id);
        }
    }

    fn reap_idle(&mut self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(SESSION_IDLE_TIMEOUT_SECS))
            .unwrap_or_else(Instant::now);
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.last_used < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for sid in stale {
            if let Some(s) = self.sessions.remove(&sid) {
                info!("reclaiming idle session {}… (seq={})", &sid[..8.min(sid.len())], s.seq_id);
                self.engine.kv_remove(s.seq_id, 0, -1);
                self.free_seq(s.seq_id);
            }
        }
        count
    }

    fn get_or_create(&mut self, sid: &str, reset: bool) -> Result<&mut Session> {
        if reset {
            if let Some(old) = self.sessions.remove(sid) {
                self.engine.kv_remove(old.seq_id, 0, -1);
                self.free_seq(old.seq_id);
            }
            let seq = self.alloc_seq().ok_or_else(|| {
                anyhow::anyhow!("seq pool esgotado (max {} sessões simultâneas)", self.max_sessions)
            })?;
            self.sessions.insert(sid.to_string(), Session::new(seq));
        }
        self.sessions
            .get_mut(sid)
            .ok_or_else(|| anyhow::anyhow!("sessão '{}' desconhecida — envie reset=true antes", sid))
    }

    fn finish(&mut self, sid: &str) {
        if let Some(s) = self.sessions.remove(sid) {
            self.engine.kv_remove(s.seq_id, 0, -1);
            self.free_seq(s.seq_id);
        }
    }
}

// ── Start ─────────────────────────────────────────────────────────────────────

/// Load the model and serve the inference HTTP server.
///
/// Blocks until the server exits or `stop_rx` fires.
pub async fn run(
    cfg: NodeConfig,
    mut stop_rx: tokio::sync::watch::Receiver<u64>,
    generation: u64,
) -> Result<()> {
    let need_embeddings = !cfg.single_peer && !cfg.no_embeddings;
    let engine_cfg = EngineConfig {
        n_ctx: cfg.n_ctx,
        n_batch: cfg.n_batch,
        n_gpu_layers: cfg.n_gpu_layers as i32,
        n_threads: 0,
        max_sessions: cfg.max_sessions.max(1),
        need_embeddings,
    };

    let model_path = cfg.model_path.clone();
    let engine = tokio::task::spawn_blocking(move || LlmEngine::load(&model_path, &engine_cfg))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    let mut state_inner = InferState::new(engine, &cfg);
    state_inner.original_block_count = crate::gguf::read_info(std::path::Path::new(&cfg.model_path))
        .ok()
        .and_then(|info| info.original_block_count);

    let state: SharedState = Arc::new(Mutex::new(state_inner));

    let router = Router::new()
        .route(
            "/v1/prefill",
            post(handle_prefill).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step",
            post(handle_step).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route("/v1/session/release", post(handle_release))
        .route("/health", get(handle_health))
        .with_state(state);

    let port = cfg.inference_port;
    let listener = bind_dual_stack(port).await?;
    info!("inference server pronto em [::]:{port}");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            loop {
                if stop_rx.changed().await.is_err() {
                    break;
                }
                if *stop_rx.borrow() != generation {
                    break;
                }
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("axum: {e}"))
}

// ── Socket ────────────────────────────────────────────────────────────────────

async fn bind_dual_stack(port: u16) -> Result<TcpListener> {
    use std::net::TcpListener as StdTcp;

    // Try dual-stack (IPv6 with IPV6_V6ONLY=0 so IPv4 works too).
    if let Ok(std_sock) = StdTcp::bind(format!("[::]:{port}")) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                let fd = std_sock.as_raw_fd();
                let val: libc::c_int = 0;
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    &val as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&val) as libc::socklen_t,
                );
            }
        }
        std_sock.set_nonblocking(true)?;
        return Ok(TcpListener::from_std(std_sock)?);
    }
    // IPv6 unavailable; fall back to IPv4.
    let std_sock = StdTcp::bind(format!("0.0.0.0:{port}"))?;
    std_sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_sock)?)
}

// ── Error helpers ─────────────────────────────────────────────────────────────

fn err400(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn err500(msg: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

// ── /health ───────────────────────────────────────────────────────────────────

async fn handle_health(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.lock().unwrap();
    Json(json!({
        "status": "ok",
        "model_loaded": true,
        "start_layer": s.start_layer,
        "end_layer": s.end_layer,
        "original_block_count": s.original_block_count,
        "hidden_dim": s.engine.n_embd,
        "vocab_size": s.engine.n_vocab,
        "max_sessions": s.max_sessions,
        "sessions_in_use": s.sessions.len(),
        "free_slots": s.seq_pool.len(),
        "n_ctx_total": s.engine.n_ctx(),
        "n_ctx_per_session": s.engine.n_ctx() / s.max_sessions.max(1),
    }))
}

// ── /v1/session/release ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReleaseRequest {
    session_id: String,
}

async fn handle_release(
    State(state): State<SharedState>,
    Json(req): Json<ReleaseRequest>,
) -> impl IntoResponse {
    let mut s = state.lock().unwrap();
    let existed = s.sessions.contains_key(&req.session_id);
    s.finish(&req.session_id);
    let free = s.seq_pool.len();
    let max = s.max_sessions;
    drop(s);
    Json(json!({
        "released": existed,
        "free_slots": free,
        "max_sessions": max,
    }))
}

// ── /v1/prefill ───────────────────────────────────────────────────────────────

async fn handle_prefill(
    State(state): State<SharedState>,
    body: Bytes,
) -> Response {
    let (meta, tensor) = match wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__legacy__")
        .to_string();
    let chunk_index = meta.get("chunk_index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let chunk_size = meta.get("chunk_size").and_then(|v| v.as_u64()).unwrap_or(512) as u32;
    let is_last = meta.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut is_final_chunk = meta
        .get("is_final_chunk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wire_dtype = WireDtype::F16;
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let prompt = meta
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Scale the hidden state once before moving into spawn_blocking.
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };

    let result = tokio::task::spawn_blocking(move || {
        prefill_blocking(
            state,
            sid_key,
            chunk_index,
            chunk_size,
            is_last,
            is_final_chunk,
            wire_dtype,
            prompt,
            tensor,
        )
    })
    .await;

    match result {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida")
                || e.to_string().contains("fora de ordem")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, e.to_string()).into_response()
        }
        Err(e) => err500(e),
    }
}

#[allow(clippy::too_many_arguments)]
fn prefill_blocking(
    state: SharedState,
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    is_last: bool,
    mut is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
) -> Result<Vec<u8>> {
    let mut s = state.lock().unwrap();

    // ── Session setup ────────────────────────────────────────────────────────

    s.get_or_create(&sid_key, chunk_index == 0)?;

    // First peer: tokenize the prompt on chunk 0.
    // We must not hold a &mut Session borrow while calling engine methods, so
    // compute everything we need, store results, then re-borrow.
    if let Some(ref text) = prompt {
        if chunk_index == 0 {
            let use_tmpl = s.chat_template;
            let no_think = s.no_thinking;
            let formatted = if use_tmpl {
                s.engine.apply_chat_template(&[("user", text)], true, no_think)
            } else {
                text.clone()
            };
            let add_bos = !use_tmpl;
            let tokens = s.engine.tokenize(&formatted, add_bos, true);
            s.sessions.get_mut(&sid_key).unwrap().prompt_tokens = tokens;
        }
    }

    // ── Rollback / order check ───────────────────────────────────────────────
    let expected_pos = chunk_index * chunk_size;
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if session.n_past > expected_pos {
            warn!(
                "refazendo chunk {chunk_index} da sessão {}… (pos {expected_pos})",
                &sid_key[..8.min(sid_key.len())]
            );
            let seq_id = session.seq_id;
            session.n_past = expected_pos;
            s.engine.kv_remove(seq_id, expected_pos as i32, -1);
        } else if session.n_past < expected_pos {
            bail!(
                "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                session.n_past
            );
        }
    }

    // ── Decode ───────────────────────────────────────────────────────────────
    // Extract the session values we need as plain integers, drop the borrow,
    // then call the engine (which needs &mut s.engine).
    let (n_past, seq_id, tokens_for_chunk) = {
        let session = &s.sessions[&sid_key];
        let toks = if tensor.is_none() {
            let start = expected_pos as usize;
            let end = (expected_pos + chunk_size) as usize;
            session
                .prompt_tokens
                .get(start..end.min(session.prompt_tokens.len()))
                .unwrap_or(&[])
                .to_vec()
        } else {
            vec![]
        };
        (session.n_past, session.seq_id, toks)
    };

    let n_tokens;
    if let Some((hidden, shape)) = tensor {
        n_tokens = shape[0];
        s.engine.decode_hidden(&hidden, n_tokens, n_past, seq_id, !is_last)?;
    } else {
        if tokens_for_chunk.is_empty() {
            bail!("chunk {chunk_index} não tem tokens");
        }
        n_tokens = tokens_for_chunk.len();
        s.engine.decode_tokens(&tokens_for_chunk, n_past, seq_id, !is_last)?;
    }

    // ── Session update ───────────────────────────────────────────────────────
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        session.n_past += n_tokens as u32;
        session.last_used = Instant::now();
    }

    let (total_tokens, n_past_now) = {
        let session = &s.sessions[&sid_key];
        (session.prompt_tokens.len() as u32, session.n_past)
    };
    let owns_tokens = total_tokens > 0; // this peer has prompt_tokens → token path
    let more = owns_tokens && (n_past_now < total_tokens);
    if owns_tokens && !more {
        is_final_chunk = true;
    }

    // ── Sampling / residual ──────────────────────────────────────────────────
    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        if !is_final_chunk {
            let out = wire::encode(&json!({"done": false, "more_chunks": more}), None, wire_dtype)?;
            return Ok(out);
        }
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1).to_vec();
        let next_id = sample_token(&logits, &recent, &sampling);
        let text = s.engine.token_to_piece(next_id, false);
        let done = next_id == eos;
        s.sessions.get_mut(&sid_key).unwrap().recent_tokens.push(next_id);
        if done {
            s.finish(&sid_key);
        }
        return wire::encode(
            &json!({"done": done, "token_id": next_id, "text": text, "more_chunks": false}),
            None,
            wire_dtype,
        );
    }

    let residual = s.engine.take_residual(n_tokens)?;
    let n_prompt_tokens = s.sessions[&sid_key].prompt_tokens.len();
    let n_embd = s.engine.n_embd;
    wire::encode(
        &json!({"done": false, "more_chunks": more, "n_prompt_tokens": n_prompt_tokens}),
        Some((&residual, [n_tokens, n_embd])),
        wire_dtype,
    )
}

// ── /v1/step ─────────────────────────────────────────────────────────────────

async fn handle_step(
    State(state): State<SharedState>,
    body: Bytes,
) -> Response {
    let (meta, tensor) = match wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__legacy__")
        .to_string();
    let is_last = meta.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false);
    let wire_dtype = WireDtype::F16;
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let token_id = meta.get("token_id").and_then(|v| v.as_i64()).map(|v| v as i32);

    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };

    let result = tokio::task::spawn_blocking(move || {
        step_blocking(state, sid_key, is_last, wire_dtype, token_id, tensor)
    })
    .await;

    match result {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, e.to_string()).into_response()
        }
        Err(e) => err500(e),
    }
}

fn step_blocking(
    state: SharedState,
    sid_key: String,
    is_last: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
) -> Result<Vec<u8>> {
    let mut s = state.lock().unwrap();

    let session = s
        .sessions
        .get_mut(&sid_key)
        .ok_or_else(|| anyhow::anyhow!("sessão '{}' desconhecida — faça o prefill antes", sid_key))?;
    session.last_used = Instant::now();
    let n_past = session.n_past;
    let seq_id = session.seq_id;

    let n_tokens;
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state.
        n_tokens = shape[0];
        let logits_all = !is_last;
        s.engine
            .decode_hidden(&hidden, n_tokens, n_past, seq_id, logits_all)?;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
        n_tokens = 1;
        let logits_all = !is_last;
        s.engine
            .decode_tokens(&[tid], n_past, seq_id, logits_all)?;
    }

    s.sessions.get_mut(&sid_key).unwrap().n_past += n_tokens as u32;

    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1).to_vec();
        let next_id = sample_token(&logits, &recent, &sampling);
        let text = s.engine.token_to_piece(next_id, false);
        let done = next_id == eos;
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        if done {
            s.finish(&sid_key);
        }
        return wire::encode(
            &json!({"done": done, "token_id": next_id, "text": text}),
            None,
            wire_dtype,
        );
    }

    let residual = s.engine.take_residual(n_tokens)?;
    wire::encode(
        &json!({"done": false}),
        Some((&residual, [n_tokens, s.engine.n_embd])),
        wire_dtype,
    )
}
