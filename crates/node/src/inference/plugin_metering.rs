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
    /// Width of the last verify window, until its commit splits it.
    pending: u32,
    pub(crate) in_chain: LinkChain,
    pub(crate) out_chain: LinkChain,
    pub(crate) wire: WireFormats,
    /// The first stage's computed sequence, by position.
    pub(crate) seq: Vec<i32>,
    proofs: crate::metering::ProofBuffer,
}

impl StageMeter {
    fn new() -> Self {
        Self { t0: crate::metering::now_ms(), ..Default::default() }
    }

    fn place(&mut self, at: u32, tokens: &[i32]) {
        self.seq.resize(at as usize, 0);
        self.seq.extend_from_slice(tokens);
    }

    fn commit(&mut self, keep: u32) {
        let width = std::mem::take(&mut self.pending);
        let kept = keep.min(width);
        self.computed.verify_accepted += kept;
        self.computed.verify_rejected += width - kept;
    }

    /// A request this machine's engine served, with its answer rows as the
    /// later machine decodes them. `first` says this machine holds layer 0.
    pub(crate) fn served(&mut self, request: &Request, rows: Option<&[f32]>, dtype: &str, first: bool) {
        match request {
            Request::Zero => self.seq.clear(),
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
                if let Some(keep) = keep {
                    self.commit(*keep);
                }
                if tokens.len() == 1 && self.pending == 0 {
                    self.computed.decode += 1;
                } else {
                    self.pending += tokens.len() as u32;
                }
                if first {
                    self.place(*pos0, tokens);
                }
            }
            Request::Commit(keep) => self.commit(*keep),
            Request::Other => {}
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
pub(crate) fn on_served(cfg: &crate::runtime::NodeConfig, frame: &[u8], network_answer: &[u8]) {
    if crate::metering::context().is_none() {
        return;
    }
    let request = parse_request(frame);
    // What the later machine decodes: the wire's tensor of the answer.
    let rows = network_answer
        .first()
        .filter(|k| **k == KIND_TENSOR)
        .and_then(|_| crate::wire::decode(&network_answer[1..]).ok())
        .and_then(|(_, tensor)| tensor.map(|(values, _)| values));
    let first = cfg.start_layer == 0;
    let dtype = cfg.hidden_state_dtype.as_str();
    with_meter(&cfg.deploy_id, |m| m.served(&request, rows.as_deref(), dtype, first));
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
    // A window never committed kept nothing of its drafts.
    meter.commit(0);
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
        m.served(&parse_request(&window(b'W', 2, &[9], None)), Some(&vec![0.1; 64]), "f16", true);
        m.served(&parse_request(&window(b'W', 3, &[10, 11, 12], None)), Some(&vec![0.1; 192]), "f16", true);
        m.served(&parse_request(&window(b'V', 4, &[11, 13], Some(1))), Some(&vec![0.1; 128]), "f16", true);
        m.commit(2);
        assert_eq!(m.computed.prefill, 2);
        assert_eq!(m.computed.decode, 1);
        assert_eq!((m.computed.verify_accepted, m.computed.verify_rejected), (3, 2));
        assert_eq!(m.in_chain, LinkChain::of_tokens(0, &[7, 8]));
        assert_eq!(m.seq, vec![7, 8, 9, 10, 11, 13]);
        assert_eq!(m.wire.output, "f16");
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
