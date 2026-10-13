use super::*;

// ── Direct reply (`relay/v2-direct`) ──────────────────────────────────────

/// Where the result of this chain must go, when whoever dispatched it asked
/// for a direct reply. Absent from every frame of hub mode and of the bubbled
/// ring - this field, and only this field, turns the new path on.
pub(crate) fn direct_reply_to(meta: &Value) -> Option<Value> {
    meta.get("reply_to").filter(|v| !v.is_null()).cloned()
}

/// What this hop returns to its caller when the chain replies directly: an
/// acknowledgement, and nothing else. The real reply does not pass through
/// here - see [`crate::direct`].
pub(crate) fn direct_ack() -> Response {
    direct_ack_with_progress(None)
}

pub(crate) fn direct_ack_with_progress(progress: Option<Value>) -> Response {
    let mut meta = json!({"accepted": true});
    if let Some(progress) = progress {
        meta["prompt_progress"] = progress;
    }
    match wire::encode_batch_single(&meta, None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Forwards the residual to the next hop and disappears. Runs in its own task:
/// whoever called us has already been accepted and expects nothing more from here.
///
/// If the forward fails, we are the one who reports it - straight to the gateway,
/// naming the node that actually died. This is the path that replaces the
/// `RelayFailure` climbing up the nested stack of the bubbled ring.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_direct(
    hops: Vec<PipelineStep>,
    path: &'static str,
    base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
) {
    forward_direct_ordered(
        hops, path, base_meta, residual, shape, dtype, attempts, reply_to, None, None, None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_direct_ordered(
    hops: Vec<PipelineStep>,
    path: &'static str,
    mut base_meta: Value,
    residual: Vec<f32>,
    shape: TensorShape,
    dtype: WireDtype,
    attempts: u32,
    reply_to: Value,
    order: Option<DirectForwardOrder>,
    sequence_id: Option<String>,
    chunk_index: Option<u32>,
) {
    let next = hops[0].clone();
    let remaining = &hops[1..];
    let terminal = base_meta
        .get("is_final_chunk")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let turn = match (order, sequence_id, chunk_index) {
        (Some(order), Some(sequence_id), Some(chunk_index)) => {
            match order.wait_turn(sequence_id, chunk_index).await {
                Ok(turn) => Some(turn),
                Err(error) => {
                    let failure = crate::relay::RelayFailure {
                        failed_node_id: next.node_id.clone(),
                        kind: "protocol".into(),
                        message: error.to_string(),
                    };
                    crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
                    return;
                }
            }
        }
        _ => None,
    };
    let destination_hop = base_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["exec_start"] = json!(next.exec_start);
    base_meta["exec_end"] = json!(next.exec_end);
    base_meta["reply_to"] = reply_to.clone();

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http_client(),
        &next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some((&residual, shape)),
        dtype,
        dtype.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if let Some(turn) = turn {
                turn.finish(true, terminal);
            }
            crate::direct::deliver_transport(http_client(), &reply_to, destination_hop, &sent)
                .await;
        }
        Err(e) => {
            if let Some(turn) = turn {
                turn.finish(false, false);
            }
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            warn!(
                "direct forward to {} failed: {} - notifying the gateway",
                failure.failed_node_id, failure.message
            );
            crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
        }
    }
}

pub(super) fn issue_kind_label(kind: Option<crate::relay::SendIssueKind>) -> &'static str {
    match kind {
        Some(crate::relay::SendIssueKind::Timeout) => "timeout",
        Some(crate::relay::SendIssueKind::Transport) => "transport",
        Some(crate::relay::SendIssueKind::Http) => "http",
        Some(crate::relay::SendIssueKind::Protocol) => "protocol",
        None => "unknown",
    }
}

/// Search `history` for the most recent prior occurrence of its own trailing
/// `m` tokens, and propose whatever followed there as a draft continuation
/// of up to `k - 1` tokens. An empty result (no history, no match, or the
/// match runs off the end of `history` with nothing after it) means "draft
/// nothing" — the caller then decodes exactly `[confirmed_token]`, identical
/// to today's non-speculative behavior. This never looks at model weights or
/// probabilities — it's a plain array search — which is exactly why
/// verification (`longest_match`, below) can never trust it: the draft is
/// only ever a guess about what to *batch*, never a substitute for sampling.
/// `m` is the **longest** n-gram tried, not the only one: the search walks
/// `m, m-1, … 1` and stops at the first length that matches, so a window that
/// finds nothing degrades into a shorter, weaker guess instead of into no
/// guess at all. Longer wins because it is stronger evidence; how much of the
/// continuation is trusted scales with the matched length (`m` tokens of
/// context authorise at most `m + 1` guesses), because a one-token match
/// pointing anywhere in the history is nearly a coin flip and every extra
/// drafted token widens the batch on every hop of the ring.
/// Is it worth drafting in this round?
///
/// Speculating is a bet: a rejected draft must be undone in the cache before
/// the next round (`gateway::rollback_sessions`, via `/v1/session/rollback`
/// -> `kv_remove` -> `llama_memory_seq_rm`).
///
/// `kv_can_shift` does not decide whether the tail builds the next `next_draft`:
/// it is the K-shift capability used by llama.cpp's reference `--ctx-shift`,
/// not the ability to undo a draft. The real speculative rollback uses
/// `kv_remove` (`llama_memory_seq_rm`) after each batch and fails closed if the
/// engine refuses the removal. The remote path does this rollback locally on each
/// hop before bubbling the reply; the legacy path keeps the `handle_rollback`
/// endpoint and the gateway fan-out.
pub(crate) fn mtp_tail_speculation_reason(
    mtp_ready: bool,
    mtp_unavailable_reason: Option<&str>,
) -> Option<&'static str> {
    if !mtp_ready {
        return Some(match mtp_unavailable_reason {
            Some(reason) if reason == crate::llm::MTP_NO_NEXTN_REASON => {
                SPECULATION_REASON_TAIL_WITHOUT_NEXTN
            }
            Some(_) => SPECULATION_REASON_MTP_RUNTIME_DISABLED,
            None => SPECULATION_REASON_MODEL_WITHOUT_MTP,
        });
    }
    None
}

pub(crate) fn should_draft(speculate: Option<Speculate>, mtp_ready: bool) -> bool {
    match speculate {
        None => false,
        Some(s) if s.source == DraftSource::Mtp => {
            // The draft may be born here (local `nextn` block, single
            // machine) or arrive ready from the gateway (the tail of a
            // sliced pipeline produced it in the previous round).
            mtp_ready || s.draft.is_some()
        }
        Some(_) => true,
    }
}

/// The invitation to draft that the head receives in a round.
///
/// `source` is the field that separates the two mechanisms, and it exists because
/// they are **not** the same thing with different numbers: `Lookup` is a search
/// for repetition in the text already generated (`find_draft`, no weights
/// involved) and `Mtp` is the `nextn` block trained inside the GGUF
/// (`crate::mtp`). A `speculate` without `source` comes from an old gateway and
/// means `Lookup`, which is what it always meant.
#[derive(Debug, Clone)]
pub(crate) struct Speculate {
    /// Batch width: the confirmed token plus up to `k - 1` guesses.
    pub k: u32,
    /// History window of `find_draft`. Ignored by `Mtp`, which looks at no
    /// history at all.
    pub m: u32,
    pub source: DraftSource,
    /// Ready-made draft, produced by the tail in a sliced pipeline. The head uses
    /// this one instead of calling `mtp_draft`. `None` means the head must
    /// produce its own draft (whole model) or that there is no draft.
    pub draft: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSource {
    Lookup,
    Mtp,
}

impl DraftSource {
    /// The name on the wire, mirroring o `DraftSource::wire` de `prefill.rs`.
    pub(crate) fn wire(&self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

impl Speculate {
    /// Reads the invitation from the frame's `speculate` field.
    ///
    /// Public to the crate because the `spec_lab` bench speaks the real
    /// protocol: a second copy of this reading there would come to count as the
    /// contract without ever having been checked against this one.
    pub(crate) fn from_meta(meta: &Value) -> Option<Self> {
        let v = meta.get("speculate")?;
        let draft = v.get("draft").and_then(|d| {
            d.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64())
                    .map(|v| v as i32)
                    .collect()
            })
        });
        Some(Speculate {
            k: v.get("k")?.as_u64()? as u32,
            m: v.get("m")?.as_u64()? as u32,
            source: match v.get("source").and_then(|s| s.as_str()) {
                Some("mtp") => DraftSource::Mtp,
                _ => DraftSource::Lookup,
            },
            draft,
        })
    }

    /// Serializes the invitation into the frame's `speculate` field - the
    /// counterpart of `from_meta`. The legacy direct path uses the value filled in
    /// by the gateway; on the remote stream, the head fills in its own value
    /// before the next round.
    pub(crate) fn to_meta(&self) -> Value {
        let mut v = json!({"k": self.k, "m": self.m, "source": self.source.wire()});
        if let Some(draft) = &self.draft {
            v["draft"] = json!(draft);
        }
        v
    }
}

pub(crate) fn find_draft(history: &[i32], m: usize, k: u32) -> Vec<i32> {
    let extra = k.saturating_sub(1) as usize;
    if extra == 0 {
        return Vec::new();
    }
    for len in (1..=m.max(1)).rev() {
        if history.len() <= len {
            continue;
        }
        let needle = &history[history.len() - len..];
        let allowed = extra.min(len + 1);
        // Most recent first: `start` walks backward from the last position a
        // *prior* (non-overlapping-with-needle) occurrence could begin.
        for start in (0..history.len() - len).rev() {
            if &history[start..start + len] != needle {
                continue;
            }
            let continuation_start = start + len;
            if continuation_start >= history.len() {
                continue;
            }
            let continuation_end = (continuation_start + allowed).min(history.len());
            return history[continuation_start..continuation_end].to_vec();
        }
    }
    Vec::new()
}

/// How many leading positions `sampled` and `drafted` agree on. `sampled`
/// holds one real, freshly-sampled token per decoded position (always
/// computed, whether or not it ends up "accepted"); `drafted` holds what was
/// guessed for positions `1..`, one shorter than `sampled` since position 0
/// is the already-confirmed seed token with nothing in `drafted` to compare
/// it against. The accepted output is always `sampled[..=result]` — the
/// matched prefix plus one more real sample, which is the genuine next token
/// whether it means "the draft's next unverified guess" (full match) or
/// "the correction at the first place the draft was wrong" (partial match).
pub(crate) fn longest_match(sampled: &[i32], drafted: &[i32]) -> usize {
    sampled
        .iter()
        .zip(drafted)
        .take_while(|(a, b)| a == b)
        .count()
}

/// New `n_past` after discarding the speculative positions that weren't
/// confirmed. `batch_width` positions were decoded into the KV cache;
/// `accept_count` of them (the confirmed seed token plus every draft token
/// that matched — see `longest_match`) are valid and kept, the rest must be
/// rolled back with `LlmEngine::kv_remove`. Saturating throughout: a caller
/// passing a degenerate `accept_count > batch_width` gets "roll back
/// nothing" rather than a panic or a wrapped position.
pub(super) fn rollback_from(n_past_after_batch: u32, batch_width: u32, accept_count: u32) -> u32 {
    n_past_after_batch.saturating_sub(batch_width.saturating_sub(accept_count))
}

#[cfg(test)]
mod chat_template_tests {
    use super::use_chat_template;

    #[test]
    fn request_mode_overrides_the_legacy_server_default() {
        assert!(use_chat_template(Some(true), false));
        assert!(!use_chat_template(Some(false), true));
        assert!(use_chat_template(None, true));
    }
}

#[cfg(test)]
mod speculative_tests {
    use super::*;

    fn lookup(k: u32, m: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m,
            source: DraftSource::Lookup,
            draft: None,
        })
    }

    fn mtp(k: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m: 0,
            source: DraftSource::Mtp,
            draft: None,
        })
    }

    /// A lookup request always drafts - the decision to accept the undo is no
    /// longer static (`kv_can_shift`), it is the real `seq_rm` in
    /// `handle_rollback`, checked in real time. See the doc of `should_draft`.
    #[test]
    fn a_lookup_round_always_drafts_when_asked() {
        assert!(should_draft(lookup(4, 3), false));
        assert!(should_draft(lookup(4, 3), true));
    }

    /// Asking an engine for MTP when it has no `nextn` head and no ready draft is
    /// the normal case of a slice that is not the final one: refusing is the
    /// right answer, and the gateway reads the refusal (`speculated: false`) and
    /// stops asking. Drafting by lookup instead would swap the mechanism under
    /// whoever asked.
    #[test]
    fn mtp_asked_of_an_engine_without_the_nextn_head_is_refused() {
        assert!(!should_draft(mtp(4), false));
        assert!(should_draft(mtp(4), true));
    }

    #[test]
    fn tail_reasons_separate_nextn_model_and_readiness() {
        assert_eq!(
            mtp_tail_speculation_reason(false, Some(crate::llm::MTP_NO_NEXTN_REASON)),
            Some(SPECULATION_REASON_TAIL_WITHOUT_NEXTN)
        );
        assert_eq!(
            mtp_tail_speculation_reason(false, Some("allocation failure")),
            Some(SPECULATION_REASON_MTP_RUNTIME_DISABLED)
        );
        assert_eq!(mtp_tail_speculation_reason(true, None), None);
    }

    /// A ready-made MTP draft (the tail of a sliced pipeline produced it in the
    /// previous round) makes the head accept even without a local `nextn` head
    /// - it only needs to verify the batch, not generate it.
    #[test]
    fn a_premade_mtp_draft_is_accepted_without_the_local_nextn_head() {
        let mut spec = mtp(4).unwrap();
        spec.draft = Some(vec![1, 2, 3]);
        assert!(should_draft(Some(spec), false));
    }

    /// With no speculation requested, the rest does not matter.
    #[test]
    fn without_speculation_asked_there_is_no_draft() {
        assert!(!should_draft(None, true));
        assert!(!should_draft(None, false));
    }

    /// The `source` field is what separates the two mechanisms on the wire. A
    /// frame from an old gateway does not carry it, and "absent" must keep
    /// meaning lookup: it was the only speculation that existed when it was
    /// written.
    #[test]
    fn a_frame_without_a_draft_source_still_means_lookup() {
        let spec = Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 3}}))
            .expect("k and m are enough");
        assert_eq!(spec.source, DraftSource::Lookup);
        assert_eq!((spec.k, spec.m), (4, 3));
    }

    #[test]
    fn a_frame_asking_for_mtp_says_so() {
        let spec = Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 0, "source": "mtp"}}))
            .expect("complete frame");
        assert_eq!(spec.source, DraftSource::Mtp);
    }

    #[test]
    fn a_frame_without_speculate_asks_for_no_draft_at_all() {
        assert!(Speculate::from_meta(&json!({"session_id": "x"})).is_none());
    }

    #[test]
    fn find_draft_returns_nothing_without_a_match() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 3, 4), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_proposes_what_followed_the_match() {
        // Trailing 3 tokens [1,2,3] last occurred at index 1..4; [7,1]
        // followed it there.
        assert_eq!(find_draft(&[9, 1, 2, 3, 7, 1, 2, 3], 3, 3), vec![7, 1]);
    }

    #[test]
    fn find_draft_needs_more_history_than_just_the_needle() {
        assert_eq!(find_draft(&[1, 2, 3], 3, 5), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_k_of_one_never_drafts_anything() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 2, 1), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_picks_the_most_recent_occurrence() {
        // [1,2,3] occurs at 0..3 and 4..7; the earlier one is followed by
        // 99, the later (more recent) one by 88. The more recent one must win.
        let history = [1, 2, 3, 99, 1, 2, 3, 88, 1, 2, 3];
        assert_eq!(find_draft(&history, 3, 2), vec![88]);
    }

    /// The window is the **largest** n-gram tried, not the only one. Without the
    /// fallback, a text where only the last token repeats drafts nothing and
    /// speculation sits idle exactly where it is cheap (verifying K tokens on a
    /// MoE costs almost the same as verifying 1).
    #[test]
    fn find_draft_falls_back_to_a_shorter_ngram_when_the_window_does_not_match() {
        // Window 3 = [7,8,9], which never occurred before. The suffix [9] did
        // occur, followed by 42.
        let history = [1, 9, 42, 43, 7, 8, 9];
        assert_eq!(find_draft(&history, 3, 4), vec![42, 43]);
    }

    /// The longest n-gram still takes precedence: it is the strongest evidence,
    /// and a short match pointing elsewhere must not override it.
    #[test]
    fn find_draft_prefers_the_longest_ngram_available() {
        //           0  1  2  3   4  5  6  7
        let history = [5, 6, 7, 90, 9, 70, 5, 6, 7];
        // Window 3 ([5,6,7]) matches at 0..3, followed by 90. The suffix [7]
        // matches later (index 2) - but the 3-length one wins.
        assert_eq!(find_draft(&history, 3, 2), vec![90]);
    }

    /// A one-token match is only weak evidence; it drafts less than a long
    /// match. Rejection corrupts nothing (verification is always against the
    /// really sampled token), but every extra drafted token widens the batch on
    /// every hop of the ring.
    #[test]
    fn find_draft_trusts_a_short_match_for_fewer_tokens() {
        let history = [4, 1, 2, 3, 9, 9, 9, 4];
        // Suffix [4] matches at index 0; k=8 would ask for 7 tokens, but a
        // match of length 1 only authorises 2.
        assert_eq!(find_draft(&history, 1, 8), vec![1, 2]);
    }

    #[test]
    fn longest_match_stops_at_the_first_disagreement() {
        assert_eq!(longest_match(&[1, 2, 9, 4], &[1, 2, 3]), 2);
    }

    #[test]
    fn longest_match_of_a_full_match_is_the_drafts_whole_length() {
        assert_eq!(longest_match(&[1, 2, 3, 4], &[1, 2, 3]), 3);
    }

    #[test]
    fn longest_match_against_an_empty_draft_is_zero() {
        assert_eq!(longest_match(&[1, 2, 3], &[]), 0);
    }

    #[test]
    fn rollback_from_a_full_match_is_a_no_op() {
        assert_eq!(rollback_from(100, 4, 4), 100);
    }

    #[test]
    fn rollback_from_a_partial_match_removes_the_gap() {
        // n_past was 96 before the batch, 4 positions were decoded, only 2
        // (the seed + one draft token) were confirmed — 2 must come off.
        assert_eq!(rollback_from(100, 4, 2), 98);
    }

    #[test]
    fn rollback_from_saturates_instead_of_underflowing() {
        assert_eq!(rollback_from(100, 4, 0), 96);
        // Degenerate: accept_count larger than batch_width still can't
        // remove more than the whole batch.
        assert_eq!(rollback_from(100, 4, 9), 100);
    }
}
