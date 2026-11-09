//! GPU work metering for a plugin engine split in stages (Strata), node side
//! (`docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`).
//!
//! The engine runs in its own process; the node sees its work only where it
//! crosses between machines, on the stage link (`plugin_stage_link.rs`), and
//! in the generation stream of the machine that serves the request. That is
//! enough, with no change to the engine:
//!
//! - **An earlier stage** (the machine answering `link_frame`) sees each
//!   request of the later stage with the answer its engine gave. The request
//!   names the work - `'P'` a prompt piece of `n` tokens at `pos0`, `'W'`/`'V'`
//!   a verify window of `T` tokens (with `'C'`/`'V'` committing the previous
//!   window's first `n_keep`), `'R'` a continuation after `n` ids (the cached
//!   prefix), `'Z'` a new session - and the answer carries the rows. It counts
//!   the positions, hashes the rows as the later machine decodes them (its
//!   output link), proves the prompt rows with TOPLOC and, on the first stage,
//!   hashes the token ids (its input link) and keeps the computed sequence.
//! - **A later stage** (the machine relaying its engine's requests,
//!   `forward`) hashes the rows its earlier stage answered: its input link.
//! - **The serving stage** counts its own work from the `done` line of the
//!   generation (`prompt_tokens`, `cached_tokens`, `completion_tokens`).
//!
//! At the end of the request the gateway posts the bundle nonce to every
//! stage (`POST /api/internal/plugins/:code/engine/metering/:deploy`), which
//! signs the turn's receipt and starts a new turn. The stage server of a
//! split model holds one session at a time, so the link's frames between two
//! closes are one turn's.
//!
//! Re-executing a plugin engine's work needs the engine itself
//! (`engine.verify`, not yet in the engine): until then the verifier answers
//! these receipts' samples as void, and they are paid on receipts alone.

use crate::gateway::GatewayState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use metering_core::chain::LinkChain;
use metering_core::receipt::{Computed, Drafted, ModelRef, ReceiptClaims, WireFormats};
use serde_json::{json, Value};
use std::sync::Mutex;

const KIND_BYTES: u8 = 1;
const KIND_TENSOR: u8 = 2;

/// One request of the stage protocol, as far as metering reads it.
#[derive(Debug, PartialEq)]
pub(crate) enum Request {
    /// A new session from position 0.
    Zero,
    /// Continue after these ids (the prefix the stage already holds).
    Resume(Vec<i32>),
    /// A prompt piece.
    Prompt { pos0: u32, tokens: Vec<i32> },
    /// A verify window; `keep` is the previous window's commit folded in.
    Window { pos0: u32, tokens: Vec<i32>, keep: Option<u32> },
    /// The previous window's first `n` tokens kept.
    Commit(u32),
    /// The later stage's hello.
    Hello,
    Other,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn i64_at(b: &[u8], at: usize) -> Option<i64> {
    b.get(at..at + 8).map(|x| i64::from_le_bytes(x.try_into().unwrap()))
}

/// Reads a request frame (the link envelope's kind byte, then the payload).
pub(crate) fn parse_request(frame: &[u8]) -> Request {
    if frame.first() != Some(&KIND_BYTES) || frame.len() < 2 {
        return Request::Other;
    }
    let p = &frame[1..];
    let parse = || -> Option<Request> {
        Some(match p[0] {
            b'Z' => Request::Zero,
            b'R' => {
                let n = u32_at(p, 1)? as usize;
                Request::Resume((0..n).map(|i| u32_at(p, 5 + 4 * i).map(|v| v as i32)).collect::<Option<_>>()?)
            }
            b'P' => {
                let pos0 = i64_at(p, 1)? as u32;
                let n = u32_at(p, 9)? as usize;
                let tokens = (0..n).map(|i| i64_at(p, 13 + 8 * i).map(|v| v as i32)).collect::<Option<_>>()?;
                Request::Prompt { pos0, tokens }
            }
            b'W' | b'V' => {
                let pos0 = i64_at(p, 1)? as u32;
                let t = u32_at(p, 9)? as usize;
                let tokens = (0..t).map(|i| u32_at(p, 13 + 4 * i).map(|v| v as i32)).collect::<Option<_>>()?;
                let keep = if p[0] == b'V' { Some(u32_at(p, 13 + 4 * t)?) } else { None };
                Request::Window { pos0, tokens, keep }
            }
            b'C' => Request::Commit(u32_at(p, 1)?),
            b'H' => Request::Hello,
            _ => Request::Other,
        })
    };
    parse().unwrap_or(Request::Other)
}

/// The f32 rows of an engine-side answer frame (kind 2: meta length, meta,
/// values), or `None` for an answer without rows.
pub(crate) fn answer_rows(frame: &[u8]) -> Option<Vec<f32>> {
    if frame.first() != Some(&KIND_TENSOR) {
        return None;
    }
    let meta_len = u32_at(frame, 1)? as usize;
    let values = frame.get(5 + meta_len..)?;
    if values.len() % 4 != 0 || values.is_empty() {
        return None;
    }
    Some(values.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A machine's turn on a split plugin engine.
#[derive(Debug, Default)]
pub(crate) struct StageMeter {
    pub(crate) t0: u64,
    pub(crate) computed: Computed,
    pub(crate) cached: u32,
    pub(crate) in_chain: LinkChain,
    pub(crate) out_chain: LinkChain,
    pub(crate) wire: WireFormats,
    /// The first stage's computed sequence, by position.
    pub(crate) seq: Vec<i32>,
    proofs: crate::metering::ProofBuffer,
    /// The last window, until its commit says how many of its tokens were
    /// kept: only kept rows are proved (rejected drafts never reach the
    /// sequence a verifier re-executes) and only kept prompt tokens enter the
    /// input chain.
    held: Option<HeldWindow>,
}

#[derive(Debug)]
struct HeldWindow {
    pos0: u32,
    tokens: Vec<i32>,
    /// The prompt part of its rows.
    rows: Option<Vec<f32>>,
    /// The later stage said the window is part of the prompt.
    prompt: bool,
}

impl StageMeter {
    fn new() -> Self {
        Self { t0: crate::metering::now_ms(), ..Default::default() }
    }

    fn place(&mut self, at: u32, tokens: &[i32]) {
        self.seq.resize(at as usize, 0);
        self.seq.extend_from_slice(tokens);
    }

    /// The held window's commit: `keep` of its tokens were kept.
    fn commit(&mut self, keep: u32, first: bool) {
        let Some(w) = self.held.take() else { return };
        let n = w.tokens.len();
        let kept = (keep as usize).min(n);
        if w.prompt {
            self.computed.prefill += kept as u32;
            self.computed.verify_rejected += (n - kept) as u32;
            if first {
                self.in_chain.absorb_tokens(w.pos0, &w.tokens[..kept]);
            }
        } else if n == 1 && kept == 1 {
            self.computed.decode += 1;
        } else {
            self.computed.verify_accepted += kept as u32;
            self.computed.verify_rejected += (n - kept) as u32;
        }
        if let Some(rows) = w.rows.filter(|r| kept > 0 && r.len() % n == 0) {
            let width = rows.len() / n;
            self.proofs.push_rows(&rows[..kept * width], width, w.pos0);
        }
    }

    /// A request this machine's engine served, with its answer rows as the
    /// later machine decodes them. `first` says this machine holds layer 0.
    pub(crate) fn served(&mut self, request: &Request, rows: Option<&[f32]>, dtype: &str, first: bool) {
        self.served_in(request, rows, dtype, first, false)
    }

    /// `prompt_phase`: the later machine said the frame is part of the
    /// prompt. Some engines (Flash-Next, DeepSeek-V4.1) read a prompt as
    /// verify windows (`'W'`/`'V'`), committed like decode windows; the tag
    /// only decides whether the kept positions count as prompt (it lags at the
    /// turn from prompt to decode, which the commit makes harmless).
    pub(crate) fn served_in(&mut self, request: &Request, rows: Option<&[f32]>, dtype: &str, first: bool, prompt_phase: bool) {
        match request {
            Request::Zero => {
                self.held = None;
                self.seq.clear();
            }
            Request::Resume(ids) => {
                self.cached += ids.len() as u32;
                if first {
                    self.seq = ids.clone();
                }
            }
            Request::Prompt { pos0, tokens } => {
                self.computed.prefill += tokens.len() as u32;
                if first {
                    self.in_chain.absorb_tokens(*pos0, tokens);
                    self.place(*pos0, tokens);
                }
                if let Some(rows) = rows.filter(|r| !tokens.is_empty() && r.len() % tokens.len() == 0) {
                    self.proofs.push_rows(rows, rows.len() / tokens.len(), *pos0);
                }
            }
            Request::Window { pos0, tokens, keep } => {
                match keep {
                    Some(keep) => self.commit(*keep, first),
                    // A window with no commit before it: the last one's
                    // outcome is unknown, so it proves nothing.
                    None => {
                        if let Some(w) = self.held.take() {
                            self.computed.verify_rejected += w.tokens.len() as u32;
                        }
                    }
                }
                if first {
                    self.place(*pos0, tokens);
                }
                self.held = Some(HeldWindow {
                    pos0: *pos0,
                    tokens: tokens.clone(),
                    rows: rows.map(|r| prompt_part(r, tokens.len()).into_owned()),
                    prompt: prompt_phase,
                });
            }
            Request::Commit(keep) => self.commit(*keep, first),
            Request::Hello | Request::Other => {}
        }
        if let Some(rows) = rows {
            let n = match request {
                Request::Prompt { tokens, .. } | Request::Window { tokens, .. } => tokens.len() as u32,
                _ => 0,
            };
            self.out_chain.absorb(0..n, &f32_bytes(rows));
            self.wire.output = dtype.to_string();
        }
    }

    /// Rows this machine's earlier stage answered: its input.
    pub(crate) fn relayed(&mut self, request: &Request, rows: &[f32], dtype: &str) {
        let n = match request {
            Request::Prompt { tokens, .. } | Request::Window { tokens, .. } => tokens.len() as u32,
            _ => 0,
        };
        self.in_chain.absorb(0..n, &f32_bytes(rows));
        self.wire.input = dtype.to_string();
    }

    /// The serving stage's own counts, from the generation's `done` line.
    pub(crate) fn done(&mut self, done: &Value) {
        let n = |k: &str| done[k].as_u64().unwrap_or(0) as u32;
        let (prompt, cached) = (n("prompt_tokens"), n("cached_tokens"));
        self.cached += cached.min(prompt);
        self.computed.prefill += prompt.saturating_sub(cached);
        self.computed.decode += n("completion_tokens");
    }
}

static METERS: Mutex<Option<(String, StageMeter)>> = Mutex::new(None);

/// `(window_floats, prompt_floats)` of this machine's stage, from the hello
/// its engine answered. A window row is the prompt row (the residual
/// streams) followed by values that depend on how the sequence was cut into
/// windows (on Flash-Next 12804 = 10240 + 2560 + 4); only the prompt part is
/// proved, so a verifier reading the sequence in other windows, or as prompt
/// pieces, reproduces it.
static ROW_FLOATS: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// The hello's answer: kind 1, status 0, then the stage's fields ending
/// with u32 window_floats, prompt_floats, features. Read from the end: the
/// fields before them have changed across engine versions.
pub(crate) fn hello_floats(answer: &[u8]) -> Option<(usize, usize)> {
    if answer.first() != Some(&KIND_BYTES) || answer.get(1) != Some(&0) || answer.len() < 2 + 12 {
        return None;
    }
    let end = answer.len();
    Some((u32_at(answer, end - 12)? as usize, u32_at(answer, end - 8)? as usize))
}

/// The prompt part of window rows (`n` rows), when the stage's widths are known.
pub(crate) fn prompt_part(rows: &[f32], n: usize) -> std::borrow::Cow<'_, [f32]> {
    let widths = ROW_FLOATS.lock().ok().and_then(|w| *w);
    match widths {
        Some((wf, pf)) if n > 0 && wf > pf && rows.len() == n * wf => {
            std::borrow::Cow::Owned(rows.chunks_exact(wf).flat_map(|r| r[..pf].iter().copied()).collect())
        }
        _ => std::borrow::Cow::Borrowed(rows),
    }
}

/// Runs `f` on this machine's meter of `deploy`, opening it while metering is on.
fn with_meter(deploy: &str, f: impl FnOnce(&mut StageMeter)) {
    if crate::metering::context().is_none() {
        return;
    }
    let Ok(mut slot) = METERS.lock() else { return };
    if slot.as_ref().map(|(d, _)| d.as_str()) != Some(deploy) {
        *slot = Some((deploy.to_string(), StageMeter::new()));
    }
    if let Some((_, meter)) = slot.as_mut() {
        f(meter);
    }
}

/// The earlier stage's side: `frame` is the request its engine read, and
/// `network_answer` the answer as it leaves for the later machine.
pub(crate) fn on_served(cfg: &crate::runtime::NodeConfig, frame: &[u8], network_answer: &[u8], prompt_phase: bool) {
    if crate::metering::context().is_none() {
        return;
    }
    let request = parse_request(frame);
    if request == Request::Hello {
        if let Some(widths) = hello_floats(network_answer) {
            if let Ok(mut slot) = ROW_FLOATS.lock() {
                *slot = Some(widths);
            }
        }
        return;
    }
    // What the later machine decodes: the wire's tensor of the answer.
    let rows = network_answer
        .first()
        .filter(|k| **k == KIND_TENSOR)
        .and_then(|_| crate::wire::decode(&network_answer[1..]).ok())
        .and_then(|(_, tensor)| tensor.map(|(values, _)| values));
    let first = cfg.start_layer == 0;
    let dtype = cfg.hidden_state_dtype.as_str();
    // `<SLUG>_METERING_TRACE=1`: one line per served request, to compare a
    // failing chunk's positions with what the stage was asked.
    if crate::brand::env("METERING_TRACE").is_ok_and(|v| v == "1") {
        let shape = match &request {
            Request::Prompt { pos0, tokens } => format!("P pos {pos0} n {}", tokens.len()),
            Request::Window { pos0, tokens, keep } => format!("W pos {pos0} n {} keep {keep:?}", tokens.len()),
            Request::Commit(k) => format!("C {k}"),
            Request::Resume(ids) => format!("R {}", ids.len()),
            Request::Zero => "Z".into(),
            Request::Hello | Request::Other => "other".into(),
        };
        tracing::info!(target: "pom::metering", "served {shape} prompt_phase {prompt_phase} rows {}", rows.as_ref().map_or(0, |r| r.len()));
    }
    with_meter(&cfg.deploy_id, |m| m.served_in(&request, rows.as_deref(), dtype, first, prompt_phase));
}

/// The later stage's side: its engine's request and the earlier stage's
/// answer as the engine reads it.
pub(crate) fn on_relayed(cfg: &crate::runtime::NodeConfig, frame: &[u8], answer: &[u8]) {
    if crate::metering::context().is_none() {
        return;
    }
    let Some(rows) = answer_rows(answer) else { return };
    let request = parse_request(frame);
    let dtype = cfg.hidden_state_dtype.as_str();
    with_meter(&cfg.deploy_id, |m| m.relayed(&request, &rows, dtype));
}

/// The serving stage's `done` line.
pub(crate) fn on_done(deploy: &str, done: &Value) {
    with_meter(deploy, |m| m.done(done));
}

/// Signs the turn's receipt for the gateway's nonce and starts a new turn.
pub(crate) fn close(cfg: &crate::runtime::NodeConfig, code: &str, bnd: &str) -> Option<(String, Vec<i32>)> {
    let ctx = crate::metering::context()?;
    let mut meter = {
        let mut slot = METERS.lock().ok()?;
        match slot.take() {
            Some((deploy, meter)) if deploy == cfg.deploy_id => meter,
            _ => StageMeter::new(),
        }
    };
    // A window never committed kept nothing.
    meter.commit(0, false);
    let (summ, chunks) = meter.proofs.finish(&ctx);
    let claims = ReceiptClaims {
        rid: uuid::Uuid::new_v4().simple().to_string(),
        net: ctx.net.clone(),
        cyc: metering_core::cycle::cycle_of(meter.t0, ctx.settings.cycle_secs),
        bnd: bnd.to_string(),
        node: ctx.node_id.clone(),
        node_key: hex::encode(ctx.key.verifying_key().as_bytes()),
        mtok: None,
        model: ModelRef {
            id: cfg.model_label.clone(),
            digest: crate::metering::model_digest(&cfg.model_path),
        },
        range: [cfg.start_layer, cfg.end_layer + 1],
        engine: code.to_string(),
        wire: meter.wire.clone(),
        computed: meter.computed.clone(),
        drafted: Drafted::default(),
        cached: meter.cached,
        in_chain: meter.in_chain.digest(),
        out_chain: meter.out_chain.digest(),
        summ,
        chunks,
        shard: None,
        t0: meter.t0,
        t1: crate::metering::now_ms(),
        ms: 0,
    };
    let token = metering_core::receipt::sign_receipt(&claims, &ctx.key).ok()?;
    crate::metering::proofs_commit(&claims.rid, claims.cyc, meter.proofs.take_chunks());
    Some((token, meter.seq))
}

/// `POST /api/internal/plugins/:code/engine/metering/:deploy` `{metering_bundle}`:
/// this stage's receipt of the turn that just ended.
pub(crate) async fn metering_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(response) = crate::plugin_engine::check_peer_call(&auth, &state, &code).await {
        return response;
    }
    let Some(cfg) = state.runtime.config().await.filter(|c| c.deploy_id == deploy) else {
        return (StatusCode::CONFLICT, axum::Json(json!({"error": "this machine does not serve that deployment"})))
            .into_response();
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(bnd) = crate::metering::nonce_of(&body) else {
        return axum::Json(json!({})).into_response();
    };
    let mut answer = json!({});
    if let Some((receipt, tokens)) = close(&cfg, &code, &bnd) {
        answer["receipt"] = json!(receipt);
        if !tokens.is_empty() {
            use base64::Engine;
            let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
            answer["metering_tokens"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    axum::Json(answer).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_frame(payload: Vec<u8>) -> Vec<u8> {
        let mut f = vec![KIND_BYTES];
        f.extend(payload);
        f
    }

    fn prompt(pos0: i64, tokens: &[i64]) -> Vec<u8> {
        let mut p = vec![b'P'];
        p.extend(pos0.to_le_bytes());
        p.extend((tokens.len() as u32).to_le_bytes());
        for t in tokens {
            p.extend(t.to_le_bytes());
        }
        p.push(0);
        bytes_frame(p)
    }

    fn window(tag: u8, pos0: i64, tokens: &[i32], keep: Option<u32>) -> Vec<u8> {
        let mut p = vec![tag];
        p.extend(pos0.to_le_bytes());
        p.extend((tokens.len() as u32).to_le_bytes());
        for t in tokens {
            p.extend(t.to_le_bytes());
        }
        if let Some(k) = keep {
            p.extend(k.to_le_bytes());
        }
        bytes_frame(p)
    }

    #[test]
    fn every_request_of_the_stage_protocol_is_read() {
        assert_eq!(parse_request(&bytes_frame(vec![b'Z'])), Request::Zero);
        let mut r = vec![b'R'];
        r.extend(2u32.to_le_bytes());
        r.extend(5i32.to_le_bytes());
        r.extend(6i32.to_le_bytes());
        assert_eq!(parse_request(&bytes_frame(r)), Request::Resume(vec![5, 6]));
        assert_eq!(parse_request(&prompt(3, &[7, 8])), Request::Prompt { pos0: 3, tokens: vec![7, 8] });
        assert_eq!(
            parse_request(&window(b'V', 9, &[1, 2], Some(1))),
            Request::Window { pos0: 9, tokens: vec![1, 2], keep: Some(1) }
        );
        let mut c = vec![b'C'];
        c.extend(4u32.to_le_bytes());
        assert_eq!(parse_request(&bytes_frame(c)), Request::Commit(4));
        assert_eq!(parse_request(&[KIND_BYTES, b'P', 1]), Request::Other);
        assert_eq!(parse_request(&[KIND_TENSOR]), Request::Other);
    }

    #[test]
    fn answer_rows_read_the_engine_side_tensor() {
        let mut f = vec![KIND_TENSOR];
        f.extend(1u32.to_le_bytes());
        f.push(0);
        f.extend(1.5f32.to_le_bytes());
        f.extend((-2.0f32).to_le_bytes());
        assert_eq!(answer_rows(&f), Some(vec![1.5, -2.0]));
        assert_eq!(answer_rows(&[KIND_BYTES, 0]), None);
    }

    #[test]
    fn a_first_stage_counts_links_and_keeps_its_sequence() {
        let mut m = StageMeter::new();
        let rows = vec![0.5f32; 2 * 64];
        m.served(&parse_request(&prompt(0, &[7, 8])), Some(&rows), "f16", true);
        // A plain decode step, committed.
        m.served(&parse_request(&window(b'W', 2, &[9], None)), Some(&vec![0.1; 64]), "f16", true);
        let mut c = vec![b'C'];
        c.extend(1u32.to_le_bytes());
        m.served(&parse_request(&bytes_frame(c)), None, "f16", true);
        // A verify window keeping 1 of 3, then one keeping both of its 2.
        m.served(&parse_request(&window(b'W', 3, &[10, 11, 12], None)), Some(&vec![0.1; 192]), "f16", true);
        m.served(&parse_request(&window(b'V', 4, &[11, 13], Some(1))), Some(&vec![0.1; 128]), "f16", true);
        m.commit(2, true);
        assert_eq!(m.computed.prefill, 2);
        assert_eq!(m.computed.decode, 1);
        assert_eq!((m.computed.verify_accepted, m.computed.verify_rejected), (3, 2));
        assert_eq!(m.in_chain, LinkChain::of_tokens(0, &[7, 8]));
        assert_eq!(m.seq, vec![7, 8, 9, 10, 11, 13]);
        assert_eq!(m.wire.output, "f16");
    }

    #[test]
    fn a_prompt_read_as_windows_is_metered_and_proved_as_prompt() {
        let mut m = StageMeter::new();
        let rows = vec![0.25f32; 3 * 64];
        m.served_in(&parse_request(&window(b'W', 0, &[5, 6, 7], None)), Some(&rows), "f8", true, true);
        // Counted at its commit, folded into the next window.
        assert_eq!(m.computed.prefill, 0);
        m.served_in(&parse_request(&window(b'V', 3, &[8, 9], Some(3))), Some(&vec![0.25; 128]), "f8", true, true);
        assert_eq!(m.computed.prefill, 3);
        assert_eq!(m.in_chain, LinkChain::of_tokens(0, &[5, 6, 7]));
        // The tag lags into the decode: a window keeping 1 of 2 counts 1.
        m.commit(1, true);
        assert_eq!(m.computed.prefill, 4);
        assert_eq!(m.computed.verify_rejected, 1);
        assert_eq!(m.in_chain, LinkChain::of_tokens(0, &[5, 6, 7, 8]));
        assert_eq!(m.seq, vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn only_the_kept_rows_of_a_window_are_proved() {
        crate::metering::with_test_context(|| {
            let mut m = StageMeter::new();
            // 40 windows of 4 tokens, each keeping 2: 80 kept rows.
            for w in 0..40u32 {
                let rows = vec![(w as f32).sin(); 4 * 64];
                m.served(&parse_request(&window(b'W', (w * 2) as i64, &[1, 2, 3, 4], None)), Some(&rows), "f16", false);
                let mut c = vec![b'C'];
                c.extend(2u32.to_le_bytes());
                m.served(&parse_request(&bytes_frame(c)), None, "f16", false);
            }
            let ctx = crate::metering::context().unwrap();
            let (_, chunks) = m.proofs.finish(&ctx);
            // 80 rows of 64 values: two full chunks of 32 rows and a tail of 16.
            assert_eq!(chunks, Some(3));
            let leaves = m.proofs.take_chunks();
            let (positions, _) = metering_core::toploc::decode_leaf(&leaves[0]).unwrap();
            assert_eq!(&positions[..4], &[0, 1, 2, 3]);
        });
    }

    #[test]
    fn only_the_prompt_part_of_a_window_row_is_kept() {
        let mut hello = vec![KIND_BYTES, 0];
        for v in [7u32, 0, 24, 5, 3, 1] {
            hello.extend(v.to_le_bytes());
        }
        assert_eq!(hello_floats(&hello), Some((5, 3)));
        *ROW_FLOATS.lock().unwrap() = Some((5, 3));
        let rows: Vec<f32> = (0..10).map(|v| v as f32).collect();
        assert_eq!(&*prompt_part(&rows, 2), &[0.0, 1.0, 2.0, 5.0, 6.0, 7.0]);
        // Rows of another width are left as they are.
        assert_eq!(prompt_part(&rows[..6], 2).len(), 6);
        *ROW_FLOATS.lock().unwrap() = None;
    }

    #[test]
    fn a_resume_counts_the_cached_prefix() {
        let mut m = StageMeter::new();
        m.served(&Request::Resume(vec![1, 2, 3]), None, "f16", true);
        m.served(&parse_request(&prompt(3, &[4])), None, "f16", true);
        assert_eq!(m.cached, 3);
        assert_eq!(m.seq, vec![1, 2, 3, 4]);
        assert_eq!(m.in_chain, LinkChain::of_tokens(3, &[4]));
    }

    #[test]
    fn earlier_output_and_later_input_link_through_the_wire() {
        for dtype in [crate::wire::WireDtype::F16, crate::wire::WireDtype::F8] {
            let values: Vec<f32> = (0..3 * 64).map(|i| (i as f32 * 0.37).sin() * 4.0).collect();
            // The engine's answer, as the earlier machine sends it and the
            // later machine's engine reads it.
            let mut engine_answer = vec![KIND_TENSOR];
            engine_answer.extend(1u32.to_le_bytes());
            engine_answer.push(0);
            engine_answer.extend(f32_bytes(&values));
            let meta = json!({ "meta": "00" });
            let encoded = crate::wire::encode(&meta, Some((&values, vec![1, values.len()])), dtype).unwrap();
            let mut network = vec![KIND_TENSOR];
            network.extend(&encoded);
            let (_, Some((received, _))) = crate::wire::decode(&encoded).unwrap() else { panic!() };
            let mut later_side = vec![KIND_TENSOR];
            later_side.extend(1u32.to_le_bytes());
            later_side.push(0);
            later_side.extend(f32_bytes(&received));

            let request = parse_request(&prompt(0, &[1, 2, 3]));
            let mut earlier = StageMeter::new();
            let rows = crate::wire::decode(&network[1..]).unwrap().1.unwrap().0;
            earlier.served(&request, Some(&rows), dtype.as_str(), true);
            let mut later = StageMeter::new();
            later.relayed(&request, &answer_rows(&later_side).unwrap(), dtype.as_str());
            assert_eq!(earlier.out_chain, later.in_chain, "{dtype:?}");
        }
    }

    #[test]
    fn the_serving_stage_counts_from_its_done_line() {
        let mut m = StageMeter::new();
        m.done(&json!({"prompt_tokens": 40, "cached_tokens": 10, "completion_tokens": 7}));
        assert_eq!((m.computed.prefill, m.cached, m.computed.decode), (30, 10, 7));
    }
}
