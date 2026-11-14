//! Per-session GPU work meter (spec 2, 3 of
//! `docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`).
//!
//! While this node has a metering context (`crate::metering::context`, i.e.
//! metering is on for the network) every session it computes is metered.
//! The gateway names the bundle only at the end, in the `release` it already
//! sends to every step (`metering_bundle`): the hop then signs the receipt
//! for that nonce and answers it. A release without a nonce (an older or
//! unmetered gateway) drops the meter. With metering off nothing is opened.
//! Carrying the nonce at release, not in the prefill meta, means none of the
//! forwarding paths (ring, direct, link, hub) has to propagate a field.
//!
//! The hooks sit in the compute functions (`prefill_blocking`,
//! `prefill_batch_blocking`, `step_blocking`, `step_batch_blocking`), which
//! every reply mode reaches: the input is the decoded tensor (or the head's
//! token ids) and the output is the encoded frame. The link digests hash f32
//! values as the next hop decodes them, so the output side hashes
//! `decode(encoded bytes)`: for f16 and f32 that equals the input of the next
//! hop exactly, and for f8/i4 it is what the codec delivers on the other end.

use super::*;
use metering_core::chain::LinkChain;
use metering_core::receipt::{Computed, Drafted, ModelRef, ReceiptClaims, WireFormats};

/// At most this many live meters per configured session slot; the oldest are
/// dropped past it (a session that never got its release).
const METERS_PER_SLOT: usize = 4;

#[derive(Clone, Debug)]
pub(crate) struct SessionMeter {
    pub(crate) t0: u64,
    pub(crate) computed: Computed,
    pub(crate) drafted: Drafted,
    pub(crate) cached: u32,
    /// Positions of a speculative batch not yet split by a rollback.
    pub(crate) pending_verify: u32,
    pub(crate) in_chain: LinkChain,
    pub(crate) out_chain: LinkChain,
    /// The forwarded frames' digest, set at close (`crate::metering::take_forwarded`).
    pub(crate) out_chain_digest: Option<String>,
    pub(crate) wire: WireFormats,
    pub(crate) compute_ms: u64,
    pub(crate) proofs: crate::metering::ProofBuffer,
    /// The head only: the token sequence it computed, by position (prompt,
    /// then the tokens fed back), kept as the KV holds it so a rollback
    /// trims it. Returned with the receipt for re-execution.
    pub(crate) seq: Vec<i32>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn dtype_label(dtype: WireDtype) -> String {
    dtype.as_str().to_string()
}

impl SessionMeter {
    pub(crate) fn new() -> Self {
        Self {
            t0: now_ms(),
            computed: Computed::default(),
            drafted: Drafted::default(),
            cached: 0,
            pending_verify: 0,
            in_chain: LinkChain::default(),
            out_chain: LinkChain::default(),
            out_chain_digest: None,
            wire: WireFormats::default(),
            compute_ms: 0,
            proofs: crate::metering::ProofBuffer::default(),
            seq: Vec::new(),
        }
    }
}

/// The bundle nonce of a release body, when the gateway sent one.
pub(crate) fn bundle_nonce(meta: &Value) -> Option<String> {
    crate::metering::nonce_of(meta)
}

/// What a receipt says about the slice that did the work.
pub(crate) struct SliceRef<'a> {
    pub(crate) model_label: &'a str,
    pub(crate) model_path: &'a str,
    pub(crate) start_layer: u32,
    /// Inclusive, as `InferState` keeps it.
    pub(crate) end_layer: u32,
    pub(crate) shard: Option<[u32; 2]>,
}

/// The live meters of an inference server, by session id. Kept apart from
/// `InferState` so it is testable without a loaded engine.
#[derive(Default)]
pub(crate) struct MeterTable {
    meters: HashMap<String, SessionMeter>,
}

impl MeterTable {
    /// The meter of `sid`, opened on first use while metering is on. At most
    /// `METERS_PER_SLOT` per session slot; the oldest is dropped past it.
    fn open(&mut self, sid: &str, max_sessions: u32) -> Option<&mut SessionMeter> {
        if !self.meters.contains_key(sid) {
            crate::metering::context()?;
            let cap = (max_sessions as usize).max(1) * METERS_PER_SLOT;
            if self.meters.len() >= cap {
                if let Some(oldest) = self.meters.iter().min_by_key(|(_, m)| m.t0).map(|(k, _)| k.clone()) {
                    self.meters.remove(&oldest);
                }
            }
            self.meters.insert(sid.to_string(), SessionMeter::new());
        }
        self.meters.get_mut(sid)
    }

    pub(crate) fn len(&self) -> usize {
        self.meters.len()
    }

    fn place(seq: &mut Vec<i32>, at: u32, tokens: &[i32]) {
        seq.resize(at as usize, 0);
        seq.extend_from_slice(tokens);
    }

    /// The head's input: the prompt token ids of a chunk, after `prefix`
    /// (the cached prompt before it), so the kept sequence starts at 0.
    pub(crate) fn input_tokens(&mut self, sid: &str, max_sessions: u32, first_pos: u32, tokens: &[i32], prefix: &[i32]) {
        if let Some(m) = self.open(sid, max_sessions) {
            m.in_chain.absorb_tokens(first_pos, tokens);
            Self::place(&mut m.seq, 0, &prefix[..prefix.len().min(first_pos as usize)]);
            Self::place(&mut m.seq, first_pos, tokens);
        }
    }

    /// The head's decode input, fed at `first_pos`. Not in the input chain:
    /// these are the model's own outputs, committed by the tail.
    pub(crate) fn fed_tokens(&mut self, sid: &str, max_sessions: u32, first_pos: u32, tokens: &[i32]) {
        if let Some(m) = self.open(sid, max_sessions) {
            Self::place(&mut m.seq, first_pos, tokens);
        }
    }

    /// The KV of `sid` now ends at `n_past`: drop what a rollback discarded.
    pub(crate) fn truncate(&mut self, sid: &str, n_past: u32) {
        if let Some(m) = self.meters.get_mut(sid) {
            m.seq.truncate(n_past as usize);
        }
    }

    /// A later hop's input: the decoded hidden state of `rows` positions.
    pub(crate) fn input_tensor(&mut self, sid: &str, max_sessions: u32, _first_pos: u32, rows: u32, values: &[f32], dtype: WireDtype) {
        if let Some(m) = self.open(sid, max_sessions) {
            let bytes = f32_bytes(values);
            // Positions stay out of hidden-state links: the two ends see the
            // same frames in the same order, but not always the same position
            // bookkeeping (a forward frame carries no start position).
            m.in_chain.absorb(0..rows, &bytes);
            m.wire.input = dtype_label(dtype);
        }
    }

    /// The output frame of `rows` positions, as encoded. `hidden` (the f32
    /// output before encoding and its row width) is given only for rows a
    /// verifier can reproduce: prefill and plain decode steps, not the
    /// speculative batches whose rejected rows never reach the final sequence.
    pub(crate) fn output(&mut self, sid: &str, first_pos: u32, rows: u32, encoded: &[u8], dtype: WireDtype, hidden: Option<(&[f32], usize)>) {
        let Some(m) = self.meters.get_mut(sid) else { return };
        let decoded = match wire::decode(encoded) {
            Ok((_, Some((values, _)))) => f32_bytes(&values),
            _ => encoded.to_vec(),
        };
        m.out_chain.absorb(0..rows, &decoded);
        m.wire.output = dtype_label(dtype);
        if let Some((values, width)) = hidden {
            m.proofs.push_rows(values, width, first_pos);
        }
    }

    /// The last stage's rows a verifier can reproduce: its final hidden state
    /// after the output norm (llama.cpp's embeddings output), for plain decode
    /// steps. The last stage sends tokens, not a hidden state, so these rows
    /// prove its work and enter no link chain.
    pub(crate) fn tail_rows(&mut self, sid: &str, first_pos: u32, values: &[f32], width: usize) {
        if let Some(m) = self.meters.get_mut(sid) {
            m.proofs.push_rows(values, width, first_pos);
        }
    }

    pub(crate) fn prefill(&mut self, sid: &str, max_sessions: u32, computed: u32, cached: u32, ms: u64) {
        if let Some(m) = self.open(sid, max_sessions) {
            m.computed.prefill += computed;
            m.cached += cached;
            m.compute_ms += ms;
        }
    }

    /// A decode step of `width` positions: one is a plain decode, more is a
    /// speculative batch split later by the rollback.
    pub(crate) fn step(&mut self, sid: &str, max_sessions: u32, width: u32, ms: u64) {
        if let Some(m) = self.open(sid, max_sessions) {
            if width <= 1 {
                m.computed.decode += 1;
            } else {
                m.pending_verify += width;
            }
            m.compute_ms += ms;
        }
    }

    pub(crate) fn drafted(&mut self, sid: &str, tokens: u32, layers: u32) {
        if let Some(m) = self.meters.get_mut(sid) {
            m.drafted.tokens += tokens;
            m.drafted.layers = m.drafted.layers.max(layers);
        }
    }

    pub(crate) fn rollback(&mut self, sid: &str, accept_count: u32, batch_width: u32) {
        if let Some(m) = self.meters.get_mut(sid) {
            let width = batch_width.min(m.pending_verify);
            let accepted = accept_count.min(width);
            m.computed.verify_accepted += accepted;
            m.computed.verify_rejected += width - accepted;
            m.pending_verify -= width;
        }
    }

    /// Closes the meter of `sid` and, when the gateway named a bundle, signs
    /// its receipt. Returns the receipt and the head's computed sequence.
    pub(crate) fn close(&mut self, sid: &str, bnd: Option<String>, slice: &SliceRef) -> Option<(String, Vec<i32>)> {
        let mut m = self.meters.remove(sid)?;
        let bnd = bnd?;
        let ctx = crate::metering::context()?;
        m.computed.verify_accepted += std::mem::take(&mut m.pending_verify);
        let (summ, chunks) = m.proofs.finish(&ctx);
        // The frames really forwarded to the next hop win over the replies
        // (hub mode has none: the gateway forwards the reply).
        if let Some((digest, dtype)) = crate::metering::take_forwarded(sid) {
            m.out_chain_digest = Some(digest);
            m.wire.output = dtype;
        }
        let claims = ReceiptClaims {
            rid: uuid::Uuid::new_v4().simple().to_string(),
            net: ctx.net.clone(),
            cyc: metering_core::cycle::cycle_of(m.t0, ctx.settings.cycle_secs),
            bnd,
            node: ctx.node_id.clone(),
            node_key: hex::encode(ctx.key.verifying_key().as_bytes()),
            mtok: None,
            model: ModelRef {
                id: slice.model_label.to_string(),
                digest: crate::metering::model_digest(slice.model_path),
            },
            range: [slice.start_layer, slice.end_layer + 1],
            engine: "llama.cpp".into(),
            wire: m.wire,
            computed: m.computed,
            drafted: m.drafted,
            cached: m.cached,
            in_chain: m.in_chain.digest(),
            out_chain: m.out_chain_digest.take().unwrap_or_else(|| m.out_chain.digest()),
            summ,
            chunks,
            shard: slice.shard,
            t0: m.t0,
            t1: now_ms(),
            ms: m.compute_ms,
        };
        let token = metering_core::receipt::sign_receipt(&claims, &ctx.key).ok()?;
        crate::metering::proofs_commit(&claims.rid, claims.cyc, m.proofs.take_chunks());
        Some((token, m.seq))
    }
}

/// Thin forwards: the compute functions call these with the state they hold.
impl InferState {
    pub(crate) fn meter_input_tokens(&mut self, sid: &str, first_pos: u32, tokens: &[i32]) {
        let prefix: Vec<i32> = self
            .sessions
            .get(sid)
            .map(|s| s.prompt_tokens.iter().take(first_pos as usize).copied().collect())
            .unwrap_or_default();
        self.meters.input_tokens(sid, self.max_sessions, first_pos, tokens, &prefix);
    }

    pub(crate) fn meter_fed_tokens(&mut self, sid: &str, first_pos: u32, tokens: &[i32]) {
        self.meters.fed_tokens(sid, self.max_sessions, first_pos, tokens);
    }

    pub(crate) fn meter_truncate(&mut self, sid: &str, n_past: u32) {
        self.meters.truncate(sid, n_past);
    }

    pub(crate) fn meter_input_tensor(&mut self, sid: &str, first_pos: u32, rows: u32, values: &[f32], dtype: WireDtype) {
        self.meters.input_tensor(sid, self.max_sessions, first_pos, rows, values, dtype);
    }

    pub(crate) fn meter_output(&mut self, sid: &str, first_pos: u32, rows: u32, encoded: &[u8], dtype: WireDtype, hidden: Option<(&[f32], usize)>) {
        self.meters.output(sid, first_pos, rows, encoded, dtype, hidden);
    }

    /// On the last stage, before a decode: llama.cpp's embeddings output on
    /// while metering is on (the rows `meter_tail_rows` proves), off
    /// otherwise. Changed only when it differs; a context that cannot give
    /// per-token rows stays off and its receipts carry no proofs, as before.
    pub(crate) fn meter_tail_ready(&mut self) {
        let want = crate::metering::context().is_some() && self.engine.output_embeddings_supported();
        if want != self.tail_embeddings {
            match self.engine.set_output_embeddings(want) {
                Ok(()) => self.tail_embeddings = want,
                Err(error) => {
                    tracing::warn!("metering: the last stage cannot prove its work: {error:#}");
                    self.tail_embeddings = false;
                }
            }
        }
    }

    /// On the last stage, after a plain decode step: the post-norm row of the
    /// output `index`, at position `pos`, into the session's proofs.
    pub(crate) fn meter_tail_rows(&mut self, sid: &str, pos: u32, index: i32) {
        if !self.tail_embeddings {
            return;
        }
        match self.engine.output_embeddings(&[index]) {
            Ok(rows) => {
                let width = self.engine.n_embd;
                self.meters.tail_rows(sid, pos, &rows, width);
            }
            Err(error) => tracing::debug!("metering: no output row for the last stage: {error:#}"),
        }
    }

    pub(crate) fn meter_prefill(&mut self, sid: &str, computed: u32, cached: u32, ms: u64) {
        self.meters.prefill(sid, self.max_sessions, computed, cached, ms);
    }

    pub(crate) fn meter_step(&mut self, sid: &str, width: u32, ms: u64) {
        self.meters.step(sid, self.max_sessions, width, ms);
    }

    pub(crate) fn meter_drafted(&mut self, sid: &str, tokens: u32, layers: u32) {
        self.meters.drafted(sid, tokens, layers);
    }

    pub(crate) fn meter_rollback(&mut self, sid: &str, accept_count: u32, batch_width: u32) {
        self.meters.rollback(sid, accept_count, batch_width);
    }

    pub(crate) fn meter_close(&mut self, sid: &str, bnd: Option<String>) -> Option<(String, Vec<i32>)> {
        let slice = SliceRef {
            model_label: &self.model_label,
            model_path: &self.model_path,
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            shard: self.tp.as_ref().filter(|tp| tp.size > 1).map(|tp| [tp.rank, tp.size]),
        };
        self.meters.close(sid, bnd, &slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metering_core::receipt::open_receipt;

    use crate::metering::with_test_context as with_context;

    fn slice() -> SliceRef<'static> {
        SliceRef { model_label: "m", model_path: "/nonexistent", start_layer: 0, end_layer: 9, shard: None }
    }

    fn frame(values: &[f32], rows: usize, dtype: WireDtype) -> Vec<u8> {
        let width = values.len() / rows;
        wire::encode(&json!({"done": false}), Some((values, vec![rows, width])), dtype).unwrap()
    }

    #[test]
    fn with_metering_off_nothing_is_opened() {
        let _guard = crate::metering::test_guard();
        crate::metering::set_context(None);
        let mut t = MeterTable::default();
        t.prefill("s", 4, 10, 0, 1);
        t.input_tokens("s", 4, 0, &[1, 2], &[]);
        assert_eq!(t.len(), 0);
        assert!(t.close("s", Some("b".into()), &slice()).is_none());
    }

    #[test]
    fn a_turn_counts_prefill_decode_cache_and_speculation() {
        with_context(|| {
            let mut t = MeterTable::default();
            t.input_tokens("s", 4, 2, &[7, 8, 9], &[5, 6]);
            t.prefill("s", 4, 3, 2, 5);
            t.fed_tokens("s", 4, 5, &[10]);
            t.step("s", 4, 1, 1);
            t.fed_tokens("s", 4, 6, &[11, 12, 13]);
            t.step("s", 4, 3, 1);
            t.rollback("s", 1, 3);
            t.truncate("s", 7);
            let (token, seq) = t.close("s", Some("bundle1".into()), &slice()).unwrap();
            let r = open_receipt(&token).unwrap();
            assert_eq!(r.bnd, "bundle1");
            assert_eq!(r.range, [0, 10]);
            assert_eq!((r.computed.prefill, r.computed.decode), (3, 1));
            assert_eq!((r.computed.verify_accepted, r.computed.verify_rejected), (1, 2));
            assert_eq!(r.cached, 2);
            assert_eq!(seq, vec![5, 6, 7, 8, 9, 10, 11]);
            // The head's input chain covers the computed prompt only.
            assert_eq!(r.in_chain, metering_core::chain::LinkChain::of_tokens(2, &[7, 8, 9]).digest());
        });
    }

    #[test]
    fn the_last_stage_proves_its_decode_rows_without_touching_its_links() {
        with_context(|| {
            let mut t = MeterTable::default();
            let width = 128;
            t.input_tensor("s", 4, 0, 1, &vec![0.5f32; width], WireDtype::F16);
            let before = t.meters["s"].out_chain.digest();
            // One post-norm row per plain decode step, at its own position.
            for pos in 10..10 + 40u32 {
                let row: Vec<f32> = (0..width).map(|i| ((i as u32 * 31 + pos) % 97) as f32 / 9.0 - 5.0).collect();
                t.tail_rows("s", pos, &row, width);
            }
            assert_eq!(t.meters["s"].out_chain.digest(), before, "the tail's rows are no output frame");
            let r = open_receipt(&t.close("s", Some("b".into()), &slice()).unwrap().0).unwrap();
            assert!(r.summ.is_some(), "the last stage's receipt is provable");
            assert!(r.chunks.unwrap_or(0) >= 2, "40 rows make a full chunk and a partial one");
        });
    }

    #[test]
    fn a_release_without_a_nonce_drops_the_meter() {
        with_context(|| {
            let mut t = MeterTable::default();
            t.prefill("s", 4, 3, 0, 1);
            assert!(t.close("s", None, &slice()).is_none());
            assert_eq!(t.len(), 0);
        });
    }

    #[test]
    fn sender_output_and_receiver_input_link_for_every_wire_format() {
        with_context(|| {
            for dtype in [WireDtype::F32, WireDtype::F16, WireDtype::F8, WireDtype::I4G64] {
                let mut a = MeterTable::default();
                let mut b = MeterTable::default();
                let rows = 4;
                let values: Vec<f32> = (0..rows * 128).map(|i| ((i * 37) % 101) as f32 / 7.0 - 7.0).collect();
                let bytes = frame(&values, rows, dtype);
                a.prefill("s", 4, rows as u32, 0, 0);
                a.output("s", 0, rows as u32, &bytes, dtype, Some((&values, 128)));
                let (_, Some((received, _))) = wire::decode(&bytes).unwrap() else { panic!() };
                b.input_tensor("s", 4, 0, rows as u32, &received, dtype);
                let ra = open_receipt(&a.close("s", Some("b".into()), &slice()).unwrap().0).unwrap();
                let rb = open_receipt(&b.close("s", Some("b".into()), &slice()).unwrap().0).unwrap();
                assert_eq!(ra.out_chain, rb.in_chain, "{dtype:?}");
                assert_eq!(ra.wire.output, dtype.as_str());
            }
        });
    }

    #[test]
    fn only_the_sessions_own_frames_enter_its_chain() {
        with_context(|| {
            let mut t = MeterTable::default();
            let v = vec![1.0f32; 128];
            t.input_tensor("a", 4, 0, 1, &v, WireDtype::F16);
            t.input_tensor("b", 4, 0, 1, &[2.0f32; 128], WireDtype::F16);
            let ra = open_receipt(&t.close("a", Some("x".into()), &slice()).unwrap().0).unwrap();
            let mut only = metering_core::chain::LinkChain::default();
            only.absorb(0..1, &f32_bytes(&v));
            assert_eq!(ra.in_chain, only.digest());
        });
    }

    #[test]
    fn the_table_is_bounded() {
        with_context(|| {
            let mut t = MeterTable::default();
            for i in 0..20 {
                t.prefill(&format!("s{i}"), 1, 1, 0, 0);
            }
            assert!(t.len() <= METERS_PER_SLOT);
        });
    }

    #[test]
    fn bundle_nonce_is_read_only_when_well_formed() {
        assert_eq!(bundle_nonce(&json!({"metering_bundle": "ab12"})).as_deref(), Some("ab12"));
        assert_eq!(bundle_nonce(&json!({})), None);
        assert_eq!(bundle_nonce(&json!({"metering_bundle": ""})), None);
        assert_eq!(bundle_nonce(&json!({"metering_bundle": "a b"})), None);
        assert_eq!(bundle_nonce(&json!({"metering_bundle": "x".repeat(65)})), None);
    }
}
