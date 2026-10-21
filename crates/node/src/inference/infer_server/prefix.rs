use super::*;

// ── Session ───────────────────────────────────────────────────────────────────

/// What the head **did** with the retained prefix on chunk 0.
///
/// It travels back to the gateway and forward to the next hops. The distinction
/// matters: the gateway chooses to *try* reusing, the head is who decides
/// whether it worked, and it was exactly this difference that the old telemetry
/// did not show.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixDecision {
    pub(crate) reused: bool,
    pub(crate) cached_tokens: u32,
    /// How many tokens opened the retained prefix, even when the divergence
    /// forced the prefill to restart cold.
    pub(crate) matched_tokens: u32,
}

/// The reuse rule, with no session or engine around.
///
/// `retained` are exactly the prompt tokens of the previous turn that are in
/// the KV, and therefore its size has to be `n_past`. The prefix fits when
/// those tokens still open the new prompt and at least one token is left to
/// decode:
/// then the prefill continues from position `n_past` and **no position leaves
/// the KV**, which is the difference between working on every model and
/// depending on the engine accepting a partial `llama_memory_seq_rm`.
///
/// Note that the comparison is against `n_past`, not against the whole retained
/// prompt: what the previous turn kept in the KV ends before the volatile tail
/// of the chat template (see `stable_prefix_len`). Diverging after `n_past`
/// is the **normal** case - it is where the conversation grew.
///
/// It refuses when the `n_past` tokens no longer open the new prompt (the
/// conversation was edited, switched template, changed the tool list), when no
/// token would be left to decode, and when the session has no prompt in the KV.
/// Refusing costs the reuse of that turn and never the request.
pub(crate) fn plan_prefix_reuse(
    retained: &[i32],
    n_past: u32,
    new_items: &[i32],
) -> PrefixDecision {
    let common = retained
        .iter()
        .zip(new_items)
        .take_while(|(old, new)| old == new)
        .count() as u32;
    if retained.len() as u32 != n_past {
        return PrefixDecision {
            matched_tokens: common.min(n_past),
            ..PrefixDecision::default()
        };
    }
    // The last token of the new prompt has to be decoded: the logits of the
    // first generated token come from it.
    let can_decode = (n_past as usize) < new_items.len();
    if n_past > 0 && common >= n_past && can_decode {
        PrefixDecision {
            reused: true,
            cached_tokens: n_past,
            matched_tokens: n_past,
        }
    } else {
        PrefixDecision {
            matched_tokens: common.min(n_past),
            ..PrefixDecision::default()
        }
    }
}

/// How many tokens from the start of this prompt remain valid when the
/// conversation grows - the boundary between what is worth retaining and what
/// belongs to this turn.
///
/// **Measured, not assumed.** The chat template closes the prompt with the
/// tail that opens the assistant's turn (`<|im_end|>\n<|im_start|>assistant\n`
/// in chatml, something else in another model). That tail sits at the *end* of
/// the prompt, so the prompt of one turn is never a prefix of the prompt of the
/// next turn: the new text goes in before it. Measured on Ornith-1.0-35B: a
/// prompt of 50 tokens, 45 stable, 5 volatile - and it was exactly because of
/// 5 tokens that every reuse was refused, with retention and the cache working
/// perfectly.
///
/// The probe renders the same prompt with the continuation `render_prompt`
/// would append and compares the tokens: the common prefix is, by construction,
/// the part that survives. It holds for any template and any tokenizer,
/// because it is a measurement and not an assumption about the format. A
/// conservative probe costs a few tokens of reuse; it never corrupts, because
/// `plan_prefix_reuse` only reuses what is actually in the KV.
pub(super) fn stable_prefix_len(
    engine: &crate::llm::LlmEngine,
    text: &str,
    use_tmpl: bool,
    no_think: bool,
    tokens: &[i32],
) -> usize {
    // The exact shape of what the next turn appends - see `render_prompt`.
    const CONTINUATION: &str = "\n\nAssistant: x";
    let probed_text = if use_tmpl {
        match engine.apply_chat_template(
            &[("user", &format!("{text}{CONTINUATION}"))],
            true,
            no_think,
        ) {
            Ok(rendered) => rendered,
            // Without a probe there is no measured boundary: retain nothing that
            // may depend on a template we could not compare.
            Err(_) => return 0,
        }
    } else {
        format!("{text}{CONTINUATION}")
    };
    let probed_tokens = engine.tokenize(&probed_text, !use_tmpl, true);
    let common = tokens
        .iter()
        .zip(&probed_tokens)
        .take_while(|(a, b)| a == b)
        .count();
    // Reserve the final prompt token for `gen_seq`. Without this cap, a raw
    // prompt whose continuation tokenizes independently could leave the whole
    // prompt in `prompt_seq`, putting the first generated token back into the
    // retained sequence.
    retainable_prefix_len(common, tokens.len())
}

/// The stable boundary of a typed decision: only the text before `bytes`
/// (instructions and answer format, the same for every request with this
/// question) is worth retaining; the state and options after it change on
/// every request. Renders the prompt cut at `bytes` followed by a byte the
/// real prompt never has there, and keeps the tokens both share - the same
/// measurement `stable_prefix_len` does, at an earlier cut. An invalid offset
/// retains nothing extra (returns the full length, so the caller's `min`
/// keeps the ordinary boundary).
pub(super) fn bounded_stable_len(
    engine: &crate::llm::LlmEngine,
    text: &str,
    bytes: usize,
    use_tmpl: bool,
    no_think: bool,
    tokens: &[i32],
) -> usize {
    if bytes == 0 || bytes >= text.len() || !text.is_char_boundary(bytes) {
        return tokens.len();
    }
    let truncated = format!("{}\u{7}", &text[..bytes]);
    let probed_text = if use_tmpl {
        match engine.apply_chat_template(&[("user", &truncated)], true, no_think) {
            Ok(rendered) => rendered,
            Err(_) => return 0,
        }
    } else {
        truncated
    };
    let probed_tokens = engine.tokenize(&probed_text, !use_tmpl, true);
    let common = common_prefix_len(tokens, &probed_tokens);
    retainable_prefix_len(common, tokens.len())
}

pub(super) fn common_prefix_len(a: &[i32], b: &[i32]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

pub(super) fn retainable_prefix_len(common: usize, prompt_len: usize) -> usize {
    common.min(prompt_len.saturating_sub(1))
}

/// The absolute KV position at which this prompt chunk must start.
///
/// `start_pos` comes from the peer that tokenized. It may only be missing on
/// chunk 0, where the session's own position is the reference: zero in a
/// restarted session, or the end of the retained prefix when there was reuse.
/// For any later chunk, inferring the position with `chunk_index * chunk_size`
/// is incorrect: an earlier chunk may have ended at token 42, not at the
/// multiple 512. Refusing the frame without a position names the divergence and
/// lets the gateway do the cold fallback, instead of mixing KV from different
/// sessions.
pub(super) fn expected_prefill_pos(
    start_pos: Option<u32>,
    chunk_index: u32,
    session_n_past: u32,
) -> Result<u32> {
    match start_pos {
        Some(position) => Ok(position),
        None if chunk_index == 0 => Ok(session_n_past),
        None => bail!(
            "chunk {chunk_index} without an absolute start_pos; it is not safe to derive the position from the index"
        ),
    }
}

#[cfg(test)]
mod decision_prefix_tests {
    use super::*;

    /// Two decisions with the same question and different states: the head's
    /// bounded boundary must leave an append-only prefix the second request
    /// reuses. Needs a small chat GGUF:
    ///
    /// ```text
    /// POM_JEV_TEST_MODEL=<model.gguf> cargo test -p node --bin pom \
    ///   decision_prefix_tests -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires POM_JEV_TEST_MODEL (any small chat GGUF)"]
    fn a_new_state_reuses_the_retained_question_prefix() {
        let path = std::env::var("POM_JEV_TEST_MODEL").expect("POM_JEV_TEST_MODEL");
        let cfg = crate::llm::EngineConfig {
            n_ctx: 2048,
            offload_kqv: false,
            cache_type_k: llama_engine::KvCacheType::F16,
            cache_type_v: llama_engine::KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: llama_engine::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_ubatch: llama_engine::DEFAULT_N_UBATCH,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            mtp_placement: Default::default(),
            moe_experts_in_ram: false,
            moe_cache_mib: 0,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
            mmproj_path: None,
            mtp_draft_path: None,
        };
        let engine = crate::llm::LlmEngine::load_with_cache(
            &path,
            &cfg,
            &crate::llm::ModelCache::new(),
            false,
        )
        .expect("model loads");
        let question = crate::decisions::parse_questions(&serde_json::json!({
            "move": {"type": "choice", "instructions": "Pick the next move.",
                     "criteria": {"up": "goes up", "down": "goes down", "left": null}}
        }))
        .unwrap()
        .remove(0);
        let render = |state: serde_json::Value| {
            let messages: Vec<crate::openai::ChatMessage> =
                serde_json::from_value(serde_json::json!([
                    {"role": "system", "content": crate::decisions::SYSTEM_MESSAGE},
                    {"role": "user", "content": question.user_message(&state)},
                ]))
                .unwrap();
            crate::openai::render_prompt_with_images(&messages, &[])
                .unwrap()
                .0
        };
        let tokens_of = |text: &str| {
            let formatted = engine
                .apply_chat_template(&[("user", text)], true, true)
                .unwrap();
            engine.tokenize(&formatted, false, true)
        };
        let first = render(serde_json::json!({"snake": [[5, 10], [4, 10]], "food": [9, 3]}));
        let second = render(serde_json::json!({"snake": [[6, 10], [5, 10]], "food": [9, 3]}));
        let first_tokens = tokens_of(&first);
        let second_tokens = tokens_of(&second);
        let bytes = crate::decisions::reusable_prefix_end(&first, &question).unwrap();
        let stable = stable_prefix_len(&engine, &first, true, true, &first_tokens).min(
            bounded_stable_len(&engine, &first, bytes, true, true, &first_tokens),
        );
        assert!(stable > 20, "retained only {stable} tokens");
        assert!(stable < first_tokens.len());
        let decision = plan_prefix_reuse(&first_tokens[..stable], stable as u32, &second_tokens);
        assert!(decision.reused, "{decision:?}");
        assert_eq!(decision.cached_tokens, stable as u32);
        // Without the bound the whole first prompt is retained and the new
        // state diverges inside it: no reuse at all.
        let unbounded = stable_prefix_len(&engine, &first, true, true, &first_tokens);
        let cold = plan_prefix_reuse(&first_tokens[..unbounded], unbounded as u32, &second_tokens);
        assert!(!cold.reused);
        eprintln!(
            "retained {stable} of {} prompt tokens; second prompt has {}",
            first_tokens.len(),
            second_tokens.len()
        );
    }
}

#[cfg(test)]
mod prefix_rule_tests {
    use super::*;

    /// The case from the report: the cache hits, chunk 0 is judged against zero,
    /// and the reuse is undone before decoding.
    #[test]
    fn chunk_zero_of_a_hit_starts_where_the_retained_prefix_ends() {
        assert_eq!(expected_prefill_pos(None, 0, 1523).unwrap(), 1523);
    }

    #[test]
    fn chunk_zero_of_a_cold_session_starts_at_zero() {
        assert_eq!(expected_prefill_pos(None, 0, 0).unwrap(), 0);
    }

    /// From chunk 1 on, what rules is the position the peer returned.
    #[test]
    fn later_chunks_follow_the_position_the_head_reported() {
        assert_eq!(expected_prefill_pos(Some(1600), 1, 1600).unwrap(), 1600);
        assert!(expected_prefill_pos(None, 2, 1024).is_err());
    }

    #[test]
    fn partial_head_prefill_publishes_absolute_progress() {
        let meta = partial_prefill_meta(true, 600, 42, 512);

        assert_eq!(meta["done"], false);
        assert_eq!(meta["more_chunks"], true);
        assert_eq!(meta["n_prompt_tokens"], 600);
        assert_eq!(meta["start_pos"], 42);
        assert_eq!(meta["next_pos"], 512);
    }

    #[test]
    fn a_conversation_that_only_grows_reuses_the_whole_previous_prompt() {
        let turn1 = vec![10, 11, 12, 13];
        let turn2 = vec![10, 11, 12, 13, 20, 21];
        let d = plan_prefix_reuse(&turn1, 4, &turn2);
        assert!(d.reused);
        assert_eq!(d.cached_tokens, 4);
    }

    /// A divergence in the middle would require shrinking the KV. Restarting is
    /// the answer, and it costs nothing beyond the reuse of that turn.
    #[test]
    fn a_prompt_that_diverges_restarts_instead_of_shrinking_the_cache() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 4, &[10, 11, 99, 13, 20]);
        assert!(!d.reused);
        assert_eq!(d.cached_tokens, 0);
        assert_eq!(d.matched_tokens, 2);
    }

    /// Prompt identical to the retained one: no token would be left to decode,
    /// and stepping back just one would still be a partial removal. It restarts.
    #[test]
    fn an_identical_prompt_restarts_because_nothing_would_be_left_to_decode() {
        let tokens = vec![10, 11, 12, 13];
        let d = plan_prefix_reuse(&tokens, 4, &tokens);
        assert!(!d.reused);
    }

    #[test]
    fn stable_frontier_always_leaves_a_prompt_token_for_generation() {
        assert_eq!(retainable_prefix_len(9, 10), 9);
        assert_eq!(retainable_prefix_len(10, 10), 9);
        assert_eq!(retainable_prefix_len(1, 1), 0);
        assert_eq!(retainable_prefix_len(0, 0), 0);
    }

    /// The KV may hold fewer positions than the stored tokens (previous turn
    /// interrupted in the middle of the prefill). Reusing up to `n_past` would
    /// be a lie about what is in the cache.
    #[test]
    fn a_partially_ingested_prompt_is_not_offered_as_a_prefix() {
        let d = plan_prefix_reuse(&[10, 11, 12, 13], 2, &[10, 11, 12, 13, 20]);
        assert!(
            !d.reused,
            "the retained has 4 tokens but only 2 are in the KV"
        );
    }

    #[test]
    fn a_session_with_no_prompt_in_the_cache_is_not_a_hit() {
        assert!(!plan_prefix_reuse(&[], 0, &[10, 11]).reused);
    }

    /// Each session has a prompt sequence and the generation one paired with
    /// it - see `llm::kv_seq_budget`. The two sets cannot overlap.
    #[test]
    fn prompt_and_generation_sequences_never_collide() {
        let max_sessions = 4u32;
        let budget = crate::llm::kv_seq_budget(max_sessions);
        assert_eq!(budget, 8);
        let prompts: Vec<u32> = (0..max_sessions).collect();
        let generations: Vec<u32> = prompts.iter().map(|s| s + max_sessions).collect();
        for g in &generations {
            assert!(!prompts.contains(g), "sequence {g} serving both roles");
            assert!(*g < budget, "sequence {g} outside the context budget");
        }
    }
}

/// A live conversation on this peer, with the prompt KV separate from the
/// generation KV.
///
/// **The prompt is never extended.** `prompt_seq` receives the prompt chunks
/// and stops there; as soon as the turn is about to generate, `open_generation`
/// copies that KV to `gen_seq` and every generated token lands in the copy. At
/// the end of the turn the copy is erased whole - `kv_remove(gen, 0, -1)`, a
/// total removal, which every engine accepts - and `prompt_seq` still holds
/// exactly the prompt, ready for the next turn. This is why prefix reuse does
/// not depend on the model accepting partial KV removal: nothing needs to be
/// removed from the middle.
pub(super) struct Session {
    /// Prompt sequence. Only the prefill writes to it.
    pub(super) prompt_seq: u32,
    /// Generation sequence of this turn, opened on the first decode of a
    /// generated token. `None` between turns.
    pub(super) gen_seq: Option<u32>,
    /// Next logical position of the prompt in `prompt_seq`. In M-RoPE, it may
    /// be smaller than `prompt_cells`, because several physical cells share
    /// one temporal position.
    pub(super) prompt_len: u32,
    /// Where the part of the prompt that survives into the next turn ends, and
    /// therefore where the generation sequence starts - see `stable_prefix_len`.
    /// Everything from here on (the template tail and the generated tokens)
    /// lives in the copy and goes away at the end of the turn. `u32::MAX` is
    /// "I do not know yet", which keeps everything in the prompt sequence: it is
    /// the state of the hops before the head reports.
    pub(super) stable_len: u32,
    /// How many physical cells the prefill wrote to `prompt_seq`. It is separate
    /// from `n_past`: KV slots are cells, while the M-RoPE position is
    /// logical.
    pub(super) prompt_cells: u32,
    /// Next logical position used by the engine in this prompt.
    pub(super) n_past: u32,
    /// The prompt of this session went through `mtmd`. `prompt_tokens` then are
    /// not the real tokens of the media cells, and nothing that depends on that
    /// mapping may run - see `positions_match_cells`.
    pub(super) contains_media: bool,
    pub(super) recent_tokens: Vec<i32>,
    /// Stored by the first peer so subsequent chunks don't re-tokenize.
    pub(super) prompt_tokens: Vec<i32>,
    /// UTF-8 tail for this request's token stream. It must never be shared
    /// between sessions because a byte prefix from one generation can only be
    /// completed by a later token in that same generation.
    pub(super) utf8: crate::llm::Utf8Assembler,
    /// `true` between turns: the KV holds only the prompt and nobody is
    /// generating. A retained session is reclaimable cache - `alloc_seq` may
    /// evict it when a slot is missing, and the next turn merely loses the reuse.
    pub(super) retained: bool,
    pub(super) last_used: Instant,
}

impl Session {
    /// True when each physical KV cell of the prompt corresponds to one
    /// logical position - the pure text case. An M-RoPE prompt with media
    /// writes several cells per position, and then `prompt_tokens` stops
    /// indexing the KV cell by cell: nothing that depends on that mapping
    /// (prefix reuse, snapshot) may run.
    pub(super) fn positions_match_cells(&self) -> bool {
        self.prompt_cells == self.n_past && !self.contains_media
    }

    pub(super) fn new(seq_id: u32) -> Self {
        Self {
            prompt_seq: seq_id,
            gen_seq: None,
            prompt_len: 0,
            stable_len: u32::MAX,
            prompt_cells: 0,
            n_past: 0,
            contains_media: false,
            recent_tokens: Vec::new(),
            prompt_tokens: Vec::new(),
            utf8: crate::llm::Utf8Assembler::new(),
            retained: false,
            last_used: Instant::now(),
        }
    }

    /// Where the next decode writes: the generation copy when the turn is
    /// already generating, the prompt sequence while it is still coming in.
    pub(super) fn seq_id(&self) -> u32 {
        self.gen_seq.unwrap_or(self.prompt_seq)
    }

    /// Counts only cells written to the sequence that remains between turns.
    /// After `open_generation`, remaining prefill chunks go to the generation
    /// copy and do not change the physical size of the retained prefix.
    pub(super) fn record_prompt_sequence_cells(&mut self, written_seq: u32, cells: u32) {
        if written_seq == self.prompt_seq {
            self.prompt_cells = self.prompt_cells.saturating_add(cells);
        }
    }

    /// Applies in memory what `retain_prompt` decided about the KV.
    ///
    /// `trimmed_to_stable` says whether `prompt_seq` physically ended up with
    /// exactly `prompt_len` cells - because it already was so (nothing beyond
    /// the stable boundary was decoded into it this turn) or because the engine
    /// accepted discarding the excess. Only then do `prompt_cells` and
    /// `prompt_tokens` shrink together with `n_past`: without this,
    /// `prompt_cells` stays larger than `n_past` forever and
    /// `positions_match_cells` refuses every following reuse even with the
    /// prefix physically intact - that was exactly the
    /// `prefix_match_tokens == 0` of the report. When the engine refuses the
    /// trim, the session stays retained with the whole prompt (no reuse this
    /// turn), never with an inconsistent state.
    pub(super) fn apply_retention(&mut self, prompt_len: u32, trimmed_to_stable: bool) {
        self.gen_seq = None;
        self.n_past = prompt_len;
        if trimmed_to_stable {
            self.prompt_cells = prompt_len;
            self.prompt_tokens.truncate(prompt_len as usize);
        }
        self.recent_tokens.clear();
        let _ = self.finish_token_stream();
        self.retained = true;
        self.last_used = Instant::now();
    }

    pub(super) fn feed_token_piece(&mut self, piece: &[u8]) -> String {
        self.utf8.feed(piece)
    }

    pub(super) fn finish_token_stream(&mut self) -> String {
        self.utf8.flush()
    }
}

#[cfg(test)]
mod session_text_tests {
    use super::*;

    /// Reproduces the emission path that feeds the chat delta: two token pieces
    /// arrive in separate calls, but the consumer needs to see the whole
    /// character, not two U+FFFD created by independent conversions.
    #[test]
    fn streaming_token_pieces_reach_the_consumer_as_one_utf8_character() {
        let mut session = Session::new(0);

        let mut emitted = String::new();
        emitted.push_str(&session.feed_token_piece(b"caf\xc3"));
        emitted.push_str(&session.feed_token_piece(b"\xa9"));

        assert_eq!(emitted, "café");
    }

    /// The end of the session needs to drain a truncated prefix exactly once.
    #[test]
    fn ending_stream_flushes_pending_bytes_without_leaking_them() {
        let mut session = Session::new(0);

        assert_eq!(session.feed_token_piece(b"ok\xc3"), "ok");
        assert_eq!(session.finish_token_stream(), "\u{fffd}");
        assert_eq!(session.finish_token_stream(), "");
    }

    /// Two interleaved requests cannot share the UTF-8 tail.
    #[test]
    fn concurrent_sessions_keep_pending_utf8_isolated() {
        let mut first = Session::new(0);
        let mut second = Session::new(1);

        assert_eq!(first.feed_token_piece(b"Jo\xc3"), "Jo");
        assert_eq!(second.feed_token_piece(b"\xf0\x9f"), "");
        assert_eq!(first.feed_token_piece(b"\xa3o"), "ão");
        assert_eq!(second.feed_token_piece(b"\x98\x80"), "😀");
    }
}

#[cfg(test)]
mod retention_accounting_tests {
    use super::*;

    #[test]
    fn prefill_cell_count_tracks_only_the_sequence_that_received_the_cells() {
        let mut session = Session::new(2);
        let prompt_seq = session.prompt_seq;
        session.record_prompt_sequence_cells(prompt_seq, 4);
        session.n_past = 4;

        session.gen_seq = Some(6);
        let generation_seq = session.seq_id();
        session.record_prompt_sequence_cells(generation_seq, 3);
        session.n_past = 7;

        assert_eq!(session.prompt_cells, 4);
        assert!(!session.positions_match_cells());

        session.apply_retention(4, true);
        assert_eq!(session.prompt_cells, 4);
        assert!(session.positions_match_cells());
    }

    /// Builds a case in which extra cells really stayed in `prompt_seq`, to
    /// exercise the refusal of suffix removal. The normal prefill path writes
    /// the chunks after the stable boundary to `gen_seq`.
    fn end_of_turn_session(retained: &[i32], trailing: u32) -> Session {
        let mut s = Session::new(0);
        let total = retained.len() as u32 + trailing;
        s.prompt_tokens = retained.to_vec();
        s.prompt_tokens
            .extend(std::iter::repeat_n(999, trailing as usize));
        s.n_past = total;
        s.prompt_cells = total;
        s.stable_len = retained.len() as u32;
        s
    }

    /// Reproduces a refused suffix removal: `prompt_cells` (physical cell)
    /// stays larger than `n_past` (retained logical position) when the needed
    /// trim does not happen, and so `positions_match_cells` - the gate that
    /// `adopt_prompt_prefix` consults before even calling `plan_prefix_reuse`
    /// - never lets it through again. `prefix_match_tokens` comes out 0 even
    /// with the prefix physically intact.
    #[test]
    fn without_trimming_the_stable_boundary_positions_never_match_again() {
        let mut session = end_of_turn_session(&[1, 2, 3, 4, 5], 5);
        let prompt_len = session.stable_len; // 5, the boundary measured in the turn.

        session.apply_retention(prompt_len, false);

        assert_eq!(session.n_past, 5);
        assert_eq!(
            session.prompt_cells, 10,
            "without pruning, the physical count stays stuck on the trailing bytes"
        );
        assert!(
            !session.positions_match_cells(),
            "the reuse gate must refuse a physically inconsistent state"
        );
    }

    /// The fix: when the engine accepts (or there was nothing to cut),
    /// `prompt_cells` and `prompt_tokens` shrink together with `n_past`, and
    /// the gate opens again for the next turn.
    #[test]
    fn trimming_the_stable_boundary_makes_positions_match_again() {
        let mut session = end_of_turn_session(&[1, 2, 3, 4, 5], 5);
        let prompt_len = session.stable_len;

        session.apply_retention(prompt_len, true);

        assert_eq!(session.n_past, 5);
        assert_eq!(session.prompt_cells, 5);
        assert_eq!(session.prompt_tokens, vec![1, 2, 3, 4, 5]);
        assert!(session.positions_match_cells());
        assert_eq!(session.prompt_tokens.len() as u32, session.n_past);
    }

    /// End to end, with no engine at all: the same sequence of events as the
    /// report (second request with the same prefix) proves
    /// `prefix_match_tokens > 0` and `reused == true` using the production
    /// rule (`plan_prefix_reuse`), through the same gate that
    /// `adopt_prompt_prefix` uses (`positions_match_cells` +
    /// `prompt_tokens.len() == n_past`). This is the targeted test the task
    /// asks for: it proves by code that the fix removes the condition that
    /// today prevents the match, without needing a GGUF or a cluster.
    #[test]
    fn a_second_turn_with_the_same_prefix_hits_after_the_fix() {
        // Turn 1: prompt "system user1" (2 retained tokens) + chat template
        // tail (3 tokens: end of turn + assistant opening).
        let prompt_len = 2u32;

        // Without the fix (trimmed_to_stable=false), the gate stays closed
        // forever - the reported regression.
        let mut unrepaired = end_of_turn_session(&[10, 11], 3);
        unrepaired.apply_retention(prompt_len, false);
        assert!(!unrepaired.positions_match_cells());

        // With the fix, the gate opens and the production rule decides.
        let mut session = end_of_turn_session(&[10, 11], 3);
        session.apply_retention(prompt_len, true);
        assert!(session.positions_match_cells());
        assert_eq!(session.prompt_tokens.len() as u32, session.n_past);

        // Turn 2: the conversation grew (render_prompt guarantees that the raw
        // text of turn 1 is an exact prefix of turn 2 - see `openai.rs`), so
        // the new tokens start with exactly what was retained.
        let turn2 = vec![10, 11, 20, 21, 22];
        let decision = plan_prefix_reuse(&session.prompt_tokens, session.n_past, &turn2);

        assert!(
            decision.reused,
            "the second request with the same prefix must hit"
        );
        assert_eq!(decision.cached_tokens, 2);
        assert!(
            decision.matched_tokens > 0,
            "prefix_match_tokens must be > 0 on the second request, not 0 as in the report"
        );
        assert_eq!(decision.matched_tokens, 2);
    }
}

// ── KV cache snapshots (`--slot-save-path`) ──────────────────────────────────

/// Version of the sidecar file. It goes up when the `.json` format changes
/// incompatibly; the `.bin` is llama.cpp's own format (`LLAMA_STATE_SEQ_*`).
pub(super) const SLOT_SIDECAR_VERSION: u32 = 1;

/// Metadata that goes with each `<name>.bin`, written beside it as
/// `<name>.json`. The llama.cpp `.bin` validates the KV geometry (layer count,
/// row type and size) on restore, but does **not** validate the model or the
/// layer range; that is what this sidecar covers before touching the engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct SlotSidecar {
    pub(super) format_version: u32,
    /// Technical label of the model. The same on every peer of the deployment.
    pub(super) model_label: String,
    /// Path of the GGUF on this peer (a slice, or the whole model).
    pub(super) model_path: String,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) total_layers: u32,
    /// Hidden state width of the loaded engine.
    pub(super) n_embd: u32,
    /// How many prompt tokens the snapshot covers (the retained stable boundary,
    /// `Session::prompt_len`). It is the position the restored session sits at.
    pub(super) retained_tokens: u32,
    /// Real prompt tokens when this peer knows them (the head); empty on the
    /// hops that only saw hidden state. The `.bin` carries a token header of its
    /// own (advisory), so this field only serves prefix reuse.
    #[serde(default)]
    pub(super) prompt_tokens: Vec<i32>,
    /// RFC3339 of when the snapshot was written.
    pub(super) saved_at: String,
    /// Size of the `.bin` in bytes, as llama.cpp reported it.
    pub(super) bin_bytes: u64,
}

/// Refuses a snapshot name that is not a single safe path component. Same
/// discipline as `transfer::safe_model_path`: no separator, no `..`, no empty
/// or hidden name. The gateway composes `"<nome>__<node_id>"`, and the
/// `node_id` is the hex of a hash, so the set `[A-Za-z0-9._-]` is enough.
pub(super) fn sanitize_slot_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("empty snapshot name");
    }
    if name.len() > 200 {
        bail!("snapshot name too long (max 200)");
    }
    if name.starts_with('.') {
        bail!("snapshot name cannot start with '.'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("snapshot name only accepts letters, digits, '.', '_' and '-'");
    }
    Ok(name)
}

pub(super) fn slot_bin_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.bin"))
}

pub(super) fn slot_json_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.json"))
}

/// Deletes the `.bin` and the `.json` of a snapshot. Returns `true` when
/// something was removed; a missing snapshot is not an error (same ethic as
/// `/v1/session/release`).
pub(super) fn erase_slot(dir: &std::path::Path, name: &str) -> Result<bool> {
    let name = sanitize_slot_name(name)?;
    let mut removed = false;
    for path in [slot_bin_path(dir, name), slot_json_path(dir, name)] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(anyhow::anyhow!("delete {}: {error}", path.display())),
        }
    }
    Ok(removed)
}

/// Counts sidecars without opening the content. /health is queried on every
/// generation request and cannot hold the engine mutex while it reads and
/// parses each JSON.
pub(super) fn count_slot_entries(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("json")
                && path.file_stem().and_then(|s| s.to_str()) != Some("prefix-auto-index")
        })
        .count()
}

/// Result of `InferState::restore_slot`, echoed in the route's response so the
/// gateway can check that every hop restored the same boundary.
pub(super) struct SlotRestoreOutcome {
    pub(super) restored_tokens: u32,
}

#[cfg(test)]
mod slot_name_tests {
    use super::sanitize_slot_name;

    #[test]
    fn accepts_a_plain_component_and_the_gateway_node_namespacing() {
        assert_eq!(sanitize_slot_name("demo").unwrap(), "demo");
        assert_eq!(
            sanitize_slot_name("demo__a1b2c3d4e5f6").unwrap(),
            "demo__a1b2c3d4e5f6"
        );
        assert_eq!(sanitize_slot_name("  trimmed  ").unwrap(), "trimmed");
    }

    #[test]
    fn refuses_path_traversal_and_separators() {
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../escape",
            ".hidden",
            "a b",
            "sÿm",
        ] {
            assert!(
                sanitize_slot_name(bad).is_err(),
                "expected to refuse {bad:?}"
            );
        }
    }
}

#[cfg(test)]
mod slot_count_tests {
    use super::count_slot_entries;

    #[test]
    fn slot_count_uses_only_directory_entries() {
        let dir = std::env::temp_dir().join(format!("pom-slot-count-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).expect("temporary directory");
        std::fs::write(dir.join("quebrado.json"), b"conteudo que nao e json")
            .expect("corrupted sidecar");
        std::fs::write(dir.join("prefix-auto-index.json"), b"indice").expect("automatic index");
        std::fs::write(dir.join("nao-e-slot.txt"), b"outro arquivo").expect("extra file");

        let count = count_slot_entries(&dir);
        std::fs::remove_dir_all(&dir).expect("cleanup of the temporary directory");

        assert_eq!(count, 1);
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

pub(crate) struct InferState {
    pub(super) engine: LlmEngine,
    pub(super) moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
    pub(super) moe_fallback_reason: Option<String>,
    pub(super) sessions: HashMap<String, Session>,
    pub(super) seq_pool: Vec<u32>,
    pub(super) max_sessions: u32,
    pub(super) sampling: SamplingConfig,
    /// The model's template opens the thinking block in the prompt.
    pub(super) thinking_forced_open: bool,
    /// Per session, whether its current request started inside a thinking
    /// block: what the reasoning budget needs and the generated tokens alone
    /// do not say.
    pub(super) thinking_open: HashMap<String, bool>,
    pub(super) no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    pub(super) chat_template: bool,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    /// Total layers of the deployment (`NodeConfig::total_layers`). Kept to
    /// validate a KV snapshot before restoring it.
    pub(super) total_layers: u32,
    /// Technical label of the served model, shared by every peer of the
    /// same deployment. Identity of the KV snapshot.
    pub(super) model_label: String,
    /// Identity of the deployment announced together with the model, when it
    /// exists. The gateway uses the `/health` field to correct a stale P2P
    /// snapshot before planning a request.
    pub(super) deploy_id: String,
    /// Path of the GGUF loaded on this peer. Identity of the KV snapshot.
    pub(super) model_path: String,
    /// Directory of KV cache snapshots (`Config::slot_save_path`), or `None`
    /// when the feature is off on this machine. See `save_slot`.
    pub(super) slot_dir: Option<std::path::PathBuf>,
    /// Retained sessions whose KV left the context and is parked in host RAM
    /// (`NodeConfig::cache_ram_mib`). See `ram_cache.rs`.
    pub(super) ram_cache: KvRamCache,
    /// Woken whenever a sequence slot may have become free (end of session,
    /// retention, idle collection, eviction). `run_scheduler` waits on it when
    /// the front item is a prefill of a new session and there is no slot: the
    /// request stays in the FIFO queue instead of being refused with
    /// "seq pool exhausted".
    pub(super) slot_freed: Arc<tokio::sync::Notify>,
    pub(super) original_block_count: Option<u32>,
    pub(super) inference_path: crate::executor::InferencePath,
    /// Confidence cutoff of the MTP draft - see `NodeConfig::mtp_min_probability`.
    pub(super) mtp_min_probability: f32,
    /// Duration of the last TLS handshake accepted by this server, in ms.
    ///
    /// Measured in the accept loop (`serve.rs`), which is where the `TlsAcceptor`
    /// runs and the time is exact. It goes from here to the reply's `meta`, and
    /// from there to the gateway's per-hop telemetry. `None` when the node has
    /// no TLS.
    pub(super) tls_handshake_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// TP identity of this node. Rank 0 needs the member list to put the
    /// workers on the same decode; without it the barrier never closes.
    pub(super) tp: Option<crate::network::TpIdentity>,
    /// Measured speed of this slice, published in `/health`. See `perf.rs`.
    pub(super) perf: perf::SlicePerf,
}

pub(crate) type SharedState = Arc<Mutex<InferState>>;

pub(super) fn use_chat_template(requested: Option<bool>, configured_default: bool) -> bool {
    requested.unwrap_or(configured_default)
}

pub(super) fn record_terminal_engine(
    timings: &mut NodeTimings,
    path: crate::executor::InferencePath,
    moe_forwarded: bool,
) {
    if path.is_moe() && moe_forwarded {
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
}

/// Records in the meta what this peer **did** with the retained prefix.
///
/// Only chunk 0 carries the decision, because only it can reuse. The next
/// hops obey this field instead of the gateway's original request: whoever
/// tokenizes is who knows whether the prefix fit, and a hop reusing while the
/// head restarted (or the opposite) misaligns the positions of the whole
/// pipeline.
pub(super) fn apply_prefix_decision(
    meta: &mut Value,
    chunk_index: u32,
    prefix: PrefixDecision,
    requested_reuse: bool,
    requested_cached_tokens: u32,
    requested_prefix_match_tokens: u32,
    owns_tokens: bool,
) {
    if chunk_index != 0 {
        return;
    }
    if owns_tokens {
        meta["reuse_prefix"] = json!(prefix.reused);
        meta["cached_tokens"] = json!(prefix.cached_tokens);
        meta["prefix_match_tokens"] = json!(prefix.matched_tokens);
    } else {
        // Only the head can decide whether the tokens match. Every
        // intermediate hop must carry the authoritative decision it received;
        // recomputing it as `false` would make the next hop reset the session
        // and then receive a hidden state with a positive position.
        meta["reuse_prefix"] = json!(requested_reuse);
        meta["cached_tokens"] = json!(requested_cached_tokens);
        meta["prefix_match_tokens"] = json!(requested_prefix_match_tokens);
    }
}

#[cfg(test)]
mod prefix_decision_tests {
    use super::*;

    #[tokio::test]
    async fn direct_forward_waits_for_the_previous_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-1", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move {
            waiter_order.wait_turn("prefill-1", 1).await.map(|turn| {
                turn.finish(true, true);
                1
            })
        });

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!second.is_finished());
        first.finish(true, false);
        assert_eq!(second.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn direct_forward_failure_reaches_the_next_chunk() {
        let order = DirectForwardOrder::default();
        let first = order.wait_turn("prefill-2", 0).await.unwrap();
        let waiter_order = order.clone();
        let second = tokio::spawn(async move { waiter_order.wait_turn("prefill-2", 1).await });

        tokio::task::yield_now().await;
        first.finish(false, false);
        let error = match second.await.unwrap() {
            Ok(_) => panic!("the next chunk cannot overtake a failure"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("direct forward failed"));
    }

    #[test]
    fn an_intermediate_hop_echoes_the_head_decision() {
        let mut meta = json!({
            "reuse_prefix": false,
            "cached_tokens": 0,
        });

        apply_prefix_decision(&mut meta, 0, PrefixDecision::default(), true, 42, 17, false);

        assert_eq!(meta["reuse_prefix"], true);
        assert_eq!(meta["cached_tokens"], 42);
        assert_eq!(meta["prefix_match_tokens"], 17);
    }

    #[test]
    fn the_token_owner_reports_what_it_executed() {
        let mut meta = json!({
            "reuse_prefix": true,
            "cached_tokens": 99,
        });

        apply_prefix_decision(
            &mut meta,
            0,
            PrefixDecision {
                reused: false,
                cached_tokens: 0,
                matched_tokens: 0,
            },
            true,
            42,
            0,
            true,
        );

        assert_eq!(meta["reuse_prefix"], false);
        assert_eq!(meta["cached_tokens"], 0);
    }
}

/// Records in the meta the boundary measured by whoever tokenized, so the next
/// hops split the prompt at the same position. See `Session::stable_len`.
pub(super) fn apply_stable_len(meta: &mut Value, stable_len: u32) {
    if stable_len != u32::MAX {
        meta["stable_len"] = json!(stable_len);
    }
}

pub(super) fn partial_prefill_meta(
    more_chunks: bool,
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
) -> Value {
    json!({
        "done": false,
        "more_chunks": more_chunks,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

pub(super) fn terminal_prefill_meta(
    n_prompt_tokens: usize,
    start_pos: u32,
    next_pos: u32,
    done: bool,
) -> Value {
    json!({
        "done": done,
        "more_chunks": false,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": start_pos,
        "next_pos": next_pos,
    })
}

/// The inference port is a channel between peers. It accepts only a member of
/// the network, as the transport proves it (an admin of the chain or a node
/// holding a valid membership token), with the internal request marker; on a
/// private network the caller also presents the shared `connection_key`. API
/// keys belong to the client's routes on the gateway and never reach a peer.
pub(super) async fn require_inference_auth(
    State(config): State<crate::auth::AuthState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let public_transport = {
        let cfg = config.read().await;
        cfg.network.visibility == crate::config::NetworkVisibility::Public
            && parts
                .extensions
                .get::<crate::serve::TransportSecurity>()
                .is_some_and(|transport| transport.0)
            && parts
                .headers
                .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
                .and_then(|value| value.to_str().ok())
                == Some("1")
    };
    if public_transport {
        // The secure transport names the caller; membership is what admits
        // it. Any machine can open a TLS connection to a public node.
        if !crate::auth::caller_is_member(&parts.extensions).await {
            tracing::warn!(
                caller = parts
                    .extensions
                    .get::<crate::serve::TransportPeer>()
                    .map(|peer| peer.node_id.as_str())
                    .unwrap_or("none"),
                "inference call refused: the caller is not a member"
            );
            return crate::auth::AuthRejection::InvalidKey.into_response();
        }
        return next.run(Request::from_parts(parts, body)).await;
    }
    match crate::auth::validate_internal_node_headers(
        &config,
        &parts.headers,
        &parts.extensions,
        parts.uri.path(),
    )
        .await
    {
        Ok(()) => next.run(Request::from_parts(parts, body)).await,
        Err(rejection) => rejection.into_response(),
    }
}

#[cfg(test)]
mod internal_auth_tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn inference_listener_accepts_only_the_internal_connection_key() {
        let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
        state.auth.write().await.connection_key = "peer-secret".into();
        let app = Router::new()
            .route("/v1/prefill", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                state.auth.clone(),
                require_inference_auth,
            ))
            .with_state(state.auth.clone());

        let request = |authorization: Option<&str>, marker: Option<&str>| {
            let mut builder = Request::post("/v1/prefill");
            if let Some(authorization) = authorization {
                builder = builder.header("authorization", authorization);
            }
            if let Some(marker) = marker {
                builder = builder.header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, marker);
            }
            builder.body(axum::body::Body::empty()).unwrap()
        };

        assert_eq!(
            app.clone()
                .oneshot(request(None, None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some("Bearer client-api-key"), Some("1")))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some("Bearer peer-secret"), None))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.oneshot(request(Some("Bearer peer-secret"), Some("1")))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn public_inference_uses_secure_transport_without_the_local_admin_key() {
        let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.connection_key = "local-admin-credential".into();
            config.network.visibility = crate::config::NetworkVisibility::Public;
        }
        let app = Router::new()
            .route("/v1/prefill", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                state.auth.clone(),
                require_inference_auth,
            ))
            .with_state(state.auth.clone());

        let mut secure_request = Request::post("/v1/prefill")
            .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
            .body(axum::body::Body::empty())
            .unwrap();
        secure_request
            .extensions_mut()
            .insert(crate::serve::TransportSecurity(true));
        assert_eq!(
            app.clone().oneshot(secure_request).await.unwrap().status(),
            StatusCode::OK
        );

        let insecure_request = Request::post("/v1/prefill")
            .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(insecure_request)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let authorized_insecure_request = Request::post("/v1/prefill")
            .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
            .header("authorization", "Bearer local-admin-credential")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(authorized_insecure_request)
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
}
