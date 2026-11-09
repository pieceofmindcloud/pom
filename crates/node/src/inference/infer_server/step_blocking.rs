use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn step_blocking(
    state: SharedState,
    sid_key: String,
    is_last: bool,
    remote_stream: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    capture_block: Option<u32>,
    // Present only for the head — everyone else either forwards
    // `incoming_draft` untouched (not the tail) or verifies against it (the
    // tail). See `Speculate` for why the draft's source travels along.
    speculate: Option<Speculate>,
    // What a previous hop already decided, forwarded via the wire `draft`
    // field. `None` means this round isn't speculative at all (the
    // pre-existing, single-token wire format applies); `Some(vec![])` means
    // it is speculative but nothing matched — still a batch of one, but
    // through the *new* wire format, since the caller committed to it by
    // asking to speculate in the first place.
    incoming_draft: Option<Vec<i32>>,
    // The composition of the batch decoded at the head, traveling on the wire
    // (`tokens`) to the tail. It is what feeds the `nextn` head's `observe` inside
    // `decode_hidden`: the tail knows *which* tokens the trunk decoded, and without
    // them the draft head does not stay up to date. `None` is an old peer that
    // does not forward the field - `decode_hidden` then turns MTP off with a warning.
    batch_tokens: Option<Vec<i32>>,
    // Batch width the gateway wants in the next round, coming from the head
    // via `mtp_k` on the wire. Only the tail uses it (to draft the right size); the
    // head echoes it from its own `speculate` (`mtp_k_hint`).
    mtp_k: Option<u32>,
    queued_at: Instant,
    scheduler_wait_us: u64,
    scheduler_wait_reason: Option<&'static str>,
    scheduler_session_existing: bool,
    pipeline_session: bool,
    profiling: Option<crate::prefill::TelemetryContext>,
) -> Result<TimedReply> {
    let mut timings = NodeTimings {
        session_id: Some(sid_key.clone()),
        queue_us: queued_at.elapsed().as_micros() as u64,
        scheduler_wait_us,
        scheduler_wait_reason,
        scheduler_session_existing,
        pipeline_session,
        profiling_spans: profiling.as_ref().map(|_| Vec::new()),
        ..Default::default()
    };
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    if let Some(context) = profiling.as_ref() {
        timings.record_profile_span(
            context,
            "scheduler-queue",
            queued_at,
            lock_started,
            None,
            None,
        );
        timings.record_profile_span(
            context,
            "lock-wait",
            lock_started,
            Instant::now(),
            None,
            None,
        );
    }

    let setup_started = Instant::now();
    // This turn's generation runs on a copy of the prompt's KV: from here on
    // nothing else writes to the prompt sequence, and that is what leaves it
    // reusable on the next turn without removing anything from the cache.
    s.open_generation(&sid_key)?;
    let session = s
        .sessions
        .get_mut(&sid_key)
        .ok_or_else(|| anyhow::anyhow!("unknown session '{}' — run the prefill first", sid_key))?;
    session.last_used = Instant::now();
    let n_past = session.n_past;
    let seq_id = session.seq_id();
    super::server::check_decode_fits_session(&s, n_past)?;
    timings.setup_us += setup_started.elapsed().as_micros() as u64;
    if let Some(context) = profiling.as_ref() {
        timings.record_profile_span(
            context,
            "session-setup",
            setup_started,
            Instant::now(),
            None,
            None,
        );
    }

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    let mut head_draft: Option<Vec<i32>> = None;
    // The composition of the batch this hop decoded, for the non-tail reply.
    // Initialized in both branches (tensor and head) before any
    // read, hence no initial value - see the use in reply_meta.
    let reply_tokens;
    // Echoed in the non-tail reply: at the head it is born from `speculate`
    // (mtp_k_hint), at middle hops it forwards what arrived from the wire. It is
    // how the tail finds out the batch width the gateway wants in the next round.
    let mut mtp_k_echo: Option<u32> = None;
    // The limit is per decode, not per session. The same lock serializes
    // this swap and the decode that consumes it.
    if let Err(error) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            bail!("{error}");
        }
    }
    let mtp_verification = speculate
        .as_ref()
        .is_some_and(|spec| spec.source == DraftSource::Mtp)
        || mtp_k.is_some();
    // GPU work meter (`meter.rs`): a later hop's input.
    if let Some((hidden, shape)) = tensor.as_ref() {
        s.meter_input_tensor(&sid_key, n_past, shape[0] as u32, hidden, wire_dtype);
    }
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state. `decode_hidden` already
        // handles any `n_tokens` — a speculative batch needs no special
        // handling here beyond what chunked prefill already exercises. The tail
        // of a sliced pipeline has the `nextn` block: `decode_hidden` feeds
        // `observe` with the batch composition (`batch_tokens`, coming from the
        // head via `tokens` on the wire); without it, it turns MTP off with a warning.
        n_tokens = shape[0];
        let logits_all = incoming_draft.is_some() || !is_last;
        // A round with neither `mtp_k` nor an MTP draft will not draft: the tail
        // neither accepts nor produces `next_draft` in it (see the
        // `mtp_tail_reason` block below). Running the `nextn` head's `observe`
        // anyway was a second decode per token on the critical path - on the
        // remote stream with two or more hops, every token.
        engine_timing = if mtp_verification {
            s.engine.decode_hidden(
                &hidden,
                &shape,
                n_tokens,
                n_past,
                seq_id,
                logits_all,
                batch_tokens.as_deref(),
            )?
        } else {
            s.engine.decode_hidden_without_mtp(
                &hidden,
                &shape,
                n_tokens,
                n_past,
                seq_id,
                logits_all,
                batch_tokens.as_deref(),
            )?
        };
        // `batch_tokens` stays alive: the tail uses it in the `next_draft` gate
        // (verification) and in the plain path.
        reply_tokens = batch_tokens.clone();
        // `Option<u32>` is Copy - the parameter's `mtp_k` stays alive for the
        // tail's plain path.
        mtp_k_echo = mtp_k;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("the first peer needs a token_id"))?;
        // Only the tail pushed to `recent_tokens` before this fix, so on a
        // real (head != tail) multi-hop pipeline the head's own history was
        // permanently empty — harmless until something needed it (repetition
        // penalty already reads only the tail's copy, correctly), but
        // prompt-lookup drafting below needs the head's history to search.
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(tid);

        // The batch width the gateway wants in the next round, extracted from the
        // `speculate` the head received and echoed in the reply - it is how the
        // tail finds out the draft size (see `mtp_k` in the parameter's doc).
        let mtp_k_hint = speculate
            .as_ref()
            .filter(|s| s.source == DraftSource::Mtp)
            .map(|s| s.k);
        let can_draft = should_draft(speculate.clone(), s.engine.mtp_ready());
        let batch: Vec<i32> = if let Some(spec) = speculate.filter(|_| can_draft) {
            let draft: Vec<i32> = if let Some(premade) = spec.draft {
                premade
            } else {
                match spec.source {
                    DraftSource::Mtp => {
                        let p_min = s.mtp_min_probability;
                        let mtp_started = Instant::now();
                        let draft = s.engine.mtp_draft(
                            tid,
                            n_past,
                            seq_id,
                            spec.k.saturating_sub(1),
                            p_min,
                        );
                        timings.mtp_draft_us += mtp_started.elapsed().as_micros() as u64;
                        if let Some(context) = profiling.as_ref() {
                            timings.record_profile_span(
                                context,
                                "mtp-draft",
                                mtp_started,
                                Instant::now(),
                                None,
                                None,
                            );
                        }
                        draft
                    }
                    DraftSource::Lookup => {
                        let session = &s.sessions[&sid_key];
                        let history: Vec<i32> = session
                            .prompt_tokens
                            .iter()
                            .chain(session.recent_tokens.iter())
                            .copied()
                            .collect();
                        find_draft(&history, spec.m as usize, spec.k)
                    }
                }
            };
            let mut batch = Vec::with_capacity(1 + draft.len());
            batch.push(tid);
            batch.extend_from_slice(&draft);
            head_draft = Some(draft);
            batch
        } else {
            vec![tid]
        };
        n_tokens = batch.len();
        s.meter_fed_tokens(&sid_key, n_past, &batch);
        let logits_all = head_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_tokens(&batch, n_past, seq_id, logits_all)?;
        // The composition of the batch decoded at the head - what the tail needs
        // for `observe`. It travels in the non-tail reply and is echoed up to it.
        reply_tokens = Some(batch);
        // Echoes the requested batch width for the tail to draft. The parameter's
        // `mtp_k` stays alive for the tail's plain path (the `or` uses a
        // copy).
        if let Some(k) = mtp_k_hint.or(mtp_k) {
            mtp_k_echo = Some(k);
        }
    }
    timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    s.perf.observe_decode(decode_started.elapsed(), n_tokens);
    if let Some(context) = profiling.as_ref() {
        timings.record_profile_span(
            context,
            "decode",
            decode_started,
            Instant::now(),
            None,
            None,
        );
    }
    timings.backend_call_us += engine_timing.backend_call_us;
    timings.backend_sync_us += engine_timing.backend_sync_us;
    timings.mtp_observe_us += engine_timing.mtp_observe_us;
    timings.merge_engine_capture(&engine_timing);

    s.sessions.get_mut(&sid_key).unwrap().n_past += n_tokens as u32;
    s.meter_step(&sid_key, n_tokens as u32, decode_started.elapsed().as_millis() as u64);

    // Whichever hop decided it — computed just now (the head) or forwarded
    // from one that did (everyone else) — this is what the tail verifies
    // against, and what a non-tail relay branch echoes onward.
    let draft = head_draft.or(incoming_draft);

    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        if let Some(draft) = draft {
            // ── Speculative verification ─────────────────────────────────
            //
            // Never trusts the draft: every position is sampled for real,
            // in order, through the exact same `sample_token_profiled` a
            // plain step uses — the draft only ever decided what got
            // batched, never what got output. Accepting positions this way
            // makes the result identical in distribution to running the
            // single-token path `keep` times in a row (see the "prompt
            // lookup decoding" writeup this shipped with for the full
            // argument); it is not an approximation.
            let verification_started = Instant::now();
            let mut recent = s.sessions[&sid_key].recent_tokens.clone();
            let mut sampled: Vec<i32> = Vec::with_capacity(n_tokens);
            let mut pieces: Vec<Vec<u8>> = Vec::with_capacity(n_tokens);
            for i in 0..n_tokens {
                let logits = s.engine.logits_ith(i as i32);
                let (id, _timing) = sample_token_within_budget(
                    logits,
                    &recent,
                    &sampling,
                    s.thinking_open_at_start(&sid_key),
                );
                if i == 0 {
                    super::core::mark_first_token_produced(&mut timings, decode_started);
                }
                recent.push(id);
                let is_stop = s.engine.is_generation_stop(&recent[..recent.len() - 1], id);
                pieces.push(if is_stop {
                    Vec::new()
                } else {
                    s.visible_piece(id)
                });
                sampled.push(id);
                if is_stop {
                    // Nothing past a model EOS or turn terminator is worth
                    // sampling, regardless of what the rest of the draft guessed.
                    break;
                }
            }
            timings.sampling_us += verification_started.elapsed().as_micros() as u64;
            if let Some(context) = profiling.as_ref() {
                timings.record_profile_span(
                    context,
                    "sampling",
                    verification_started,
                    Instant::now(),
                    None,
                    None,
                );
            }

            let accept_len = longest_match(&sampled, &draft);
            record_mtp_verification(&mut timings, mtp_verification, draft.len(), accept_len);
            // The `nextn` head keeps one hidden-state row per decoded position;
            // which of them feeds the next round is only known after the
            // verification says how far the draft was good. Without this it pairs
            // the confirmed token with the `h` of a position that was
            // discarded, and the next draft silently degenerates.
            s.engine.mtp_accept(accept_len);

            let keep = (accept_len + 1).min(sampled.len());
            let accepted_ids = &sampled[..keep];
            let done = accepted_ids.last().is_some_and(|id| {
                *id == eos
                    || (sampled.last() == Some(id)
                        && s.engine
                            .is_generation_stop(&recent[..recent.len() - 1], *id))
            });
            // Only accepted positions may advance the session's byte stream.
            // Feeding rejected draft positions would leave their UTF-8 tail
            // in the next request even though those tokens never reached the
            // consumer.
            let mut accepted_texts: Vec<String> = pieces
                .iter()
                .take(keep)
                .map(|piece| {
                    s.sessions
                        .get_mut(&sid_key)
                        .unwrap()
                        .feed_token_piece(piece)
                })
                .collect();
            if done {
                let mut tail = String::new();
                finish_terminal_stream(&mut s.sessions, &sid_key, &mut tail)?;
                if let Some(last) = accepted_texts.last_mut() {
                    last.push_str(&tail);
                }
            }

            // Draft for the next round. On a single machine it feeds the same
            // process; on a sliced pipeline it travels in the reply that already
            // goes up the chain. The remote stream installs it at the head before
            // the next `step`; the legacy direct path carries it to the gateway for
            // injection - always with no extra traversal.
            //
            // The `batch_tokens` being present is what guarantees that this is the
            // tail of a pipeline (the batch composition came from outside, via
            // `tokens`): on a single machine the head is its own tail and there is
            // nowhere to send the draft back.
            let (mtp_tail_reason, mut speculation_disabled_detail) = if mtp_k.is_some() {
                if let Some(reason) = mtp_tail_speculation_reason(
                    s.engine.mtp_ready(),
                    s.engine.mtp_unavailable_reason(),
                ) {
                    (
                        Some(reason),
                        Some(
                            s.engine
                                .mtp_unavailable_reason()
                                .unwrap_or(reason)
                                .to_owned(),
                        ),
                    )
                } else if batch_tokens.is_none() {
                    (
                        Some(SPECULATION_REASON_MTP_INPUT_MISSING),
                        Some("tail received a hidden state without the tokens field".to_owned()),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            let mut speculation_disabled_reason = None;
            let next_draft: Option<Vec<i32>> =
                if !done && batch_tokens.is_some() && mtp_tail_reason.is_none() {
                    // The session has already advanced `n_tokens`; only the first `keep`
                    // positions were confirmed, so the trunk's next free slot
                    // is `n_past - (n_tokens - keep)`.
                    let session_n_past = s.sessions[&sid_key].n_past;
                    let n_past_next = session_n_past - (n_tokens as u32 - keep as u32);
                    let id0 = *accepted_ids.last().unwrap_or(&eos);
                    let p_min = s.mtp_min_probability;
                    let mtp_started = Instant::now();
                    let dr =
                        s.engine
                            .mtp_draft(id0, n_past_next, seq_id, draft.len() as u32, p_min);
                    timings.mtp_draft_us += mtp_started.elapsed().as_micros() as u64;
                    if let Some(context) = profiling.as_ref() {
                        timings.record_profile_span(
                            context,
                            "mtp-draft",
                            mtp_started,
                            Instant::now(),
                            None,
                            None,
                        );
                    }
                    if dr.is_empty() {
                        speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                        speculation_disabled_detail =
                            Some("nextn produced no tokens for the next round".to_owned());
                        None
                    } else {
                        s.meter_drafted(&sid_key, dr.len() as u32, 1);
                        Some(dr)
                    }
                } else {
                    speculation_disabled_reason = mtp_tail_reason;
                    None
                };

            if remote_stream {
                let n_past_after_batch = s.sessions[&sid_key].n_past;
                let new_n_past = rollback_from(n_past_after_batch, n_tokens as u32, keep as u32);
                if new_n_past < n_past_after_batch
                    && !s.engine.kv_remove(seq_id, new_n_past as i32, -1)
                {
                    bail!("remote speculative rollback refused at the tail");
                }
                s.sessions.get_mut(&sid_key).unwrap().n_past = new_n_past;
                s.meter_rollback(&sid_key, keep as u32, n_tokens as u32);
                s.meter_truncate(&sid_key, new_n_past);
            }

            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .recent_tokens
                .extend_from_slice(accepted_ids);
            // See the twin note below: a replica does not end the session. And the end
            // of the turn retains the prompt - see `InferState::retain_prompt`.
            if done && !crate::tp::is_replica() {
                s.retain_prompt(&sid_key);
            }

            let accepted: Vec<Value> = accepted_ids
                .iter()
                .zip(accepted_texts)
                .map(|(id, text)| json!({"token_id": id, "text": text}))
                .collect();
            let encode_started = Instant::now();
            let bytes = wire::encode(
                &json!({
                    "done": done,
                    "accepted": accepted,
                    "accept_count": keep as u32,
                    "batch_width": n_tokens as u32,
                    "speculated": true,
                    "speculation_active": next_draft.is_some(),
                    "speculation_disabled_reason": speculation_disabled_reason,
                    "speculation_disabled_detail": speculation_disabled_detail,
                    "next_draft": next_draft,
                }),
                None,
                wire_dtype,
            )?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            if let Some(context) = profiling.as_ref() {
                timings.record_profile_span(
                    context,
                    "response-encode",
                    encode_started,
                    Instant::now(),
                    None,
                    None,
                );
            }
            return Ok(TimedReply { bytes, timings });
        }

        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_within_budget(
            logits,
            &recent,
            &sampling,
            s.thinking_open_at_start(&sid_key),
        );
        super::core::mark_first_token_produced(&mut timings, decode_started);
        let done = s.engine.is_generation_stop(&recent, next_id);
        let token_text_started = Instant::now();
        let mut text = if done {
            String::new()
        } else {
            let piece = s.visible_piece(next_id);
            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .feed_token_piece(&piece)
        };
        let token_text_us = token_text_started.elapsed().as_micros() as u64;
        if done {
            finish_terminal_stream(&mut s.sessions, &sid_key, &mut text)?;
        }
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        // A TP group's replica does not end the session: whoever decides the end
        // of the generation is rank 0. See `tp::is_replica`. And the end of the
        // turn retains the prompt - see `InferState::retain_prompt`.
        if done && !crate::tp::is_replica() {
            s.retain_prompt(&sid_key);
        }
        timings.sampling_us += logits_access_us
            + sampling_timing.logits_clone_us
            + sampling_timing.repetition_penalty_us
            + sampling_timing.softmax_us
            + sampling_timing.top_k_us
            + sampling_timing.top_p_us
            + sampling_timing.selection_us
            + token_text_us;
        timings.logits_access_us += logits_access_us;
        timings.logits_clone_us += sampling_timing.logits_clone_us;
        timings.repetition_penalty_us += sampling_timing.repetition_penalty_us;
        timings.softmax_us += sampling_timing.softmax_us;
        timings.top_k_us += sampling_timing.top_k_us;
        timings.top_p_us += sampling_timing.top_p_us;
        timings.selection_us += sampling_timing.selection_us;
        timings.token_text_us += token_text_us;
        if let Some(context) = profiling.as_ref() {
            timings.record_profile_span(
                context,
                "sampling",
                logits_access_started,
                Instant::now(),
                None,
                None,
            );
        }

        // The tail of a sliced pipeline also drafts in the round in which the
        // head refused - the first one, when there is no ready draft yet
        // to travel. The `mtp_k` came from the head (via `mtp_k` on the wire): it is
        // the batch width the gateway wants in the next round. The
        // `mtp_accept(0)` confirms the `pending_h` of the row of the token just
        // decoded; without it the next draft would use the row of an earlier
        // position and pair wrongly. A refused whole-model round
        // has no `mtp_k` - nothing to send back, and there is no second machine waiting.
        let (mtp_tail_reason, mut speculation_disabled_detail) = if mtp_k.is_some() {
            if let Some(reason) =
                mtp_tail_speculation_reason(s.engine.mtp_ready(), s.engine.mtp_unavailable_reason())
            {
                (
                    Some(reason),
                    Some(
                        s.engine
                            .mtp_unavailable_reason()
                            .unwrap_or(reason)
                            .to_owned(),
                    ),
                )
            } else if batch_tokens.is_none() {
                (
                    Some(SPECULATION_REASON_MTP_INPUT_MISSING),
                    Some("tail received a hidden state without the tokens field".to_owned()),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        let mut speculation_disabled_reason = None;
        let next_draft: Option<Vec<i32>> = if !done && mtp_tail_reason.is_none() {
            if let Some(k) = mtp_k {
                s.engine.mtp_accept(0);
                let session_n_past = s.sessions[&sid_key].n_past;
                let p_min = s.mtp_min_probability;
                let mtp_started = Instant::now();
                let dr =
                    s.engine
                        .mtp_draft(next_id, session_n_past, seq_id, k.saturating_sub(1), p_min);
                timings.mtp_draft_us += mtp_started.elapsed().as_micros() as u64;
                if let Some(context) = profiling.as_ref() {
                    timings.record_profile_span(
                        context,
                        "mtp-draft",
                        mtp_started,
                        Instant::now(),
                        None,
                        None,
                    );
                }
                if dr.is_empty() {
                    speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                    speculation_disabled_detail =
                        Some("nextn produced no tokens for the next round".to_owned());
                    None
                } else {
                    Some(dr)
                }
            } else {
                None
            }
        } else {
            speculation_disabled_reason = mtp_tail_reason;
            None
        };
        let encode_started = Instant::now();
        let mut reply = json!({"done": done, "token_id": next_id, "text": text});
        reply["speculated"] = json!(false);
        reply["speculation_active"] = json!(next_draft.is_some());
        reply["speculation_disabled_reason"] = json!(speculation_disabled_reason);
        reply["speculation_disabled_detail"] = json!(speculation_disabled_detail);
        if let Some(nd) = &next_draft {
            reply["next_draft"] = json!(nd);
        }
        let bytes = wire::encode(&reply, None, wire_dtype)?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        if let Some(context) = profiling.as_ref() {
            timings.record_profile_span(
                context,
                "response-encode",
                encode_started,
                Instant::now(),
                None,
                None,
            );
        }
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let residual = s.engine.take_residual(n_tokens)?;
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let mut reply_meta = json!({"done": false});
    // Whoever called us needs this to keep threading it toward the tail —
    // see `draft`'s own comment above for why it's carried this way rather
    // than recomputed at each hop.
    if let Some(draft) = &draft {
        reply_meta["draft"] = json!(draft);
    }
    // The composition of the decoded batch (see `batch_tokens`) and the batch
    // width of the next round (see `mtp_k`) travel from the head to the tail -
    // they are what keeps the tail's `nextn` head up to date and drafting the
    // right size.
    if let Some(tokens) = &reply_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    if let Some(k) = mtp_k_echo {
        reply_meta["mtp_k"] = json!(k);
    }
    let encode_started = Instant::now();
    let (bytes, wire_timing) = wire::encode_profiled(
        &reply_meta,
        Some((&residual.values, residual.shape.clone())),
        wire_dtype,
    )?;
    let width = residual.shape.get(1).copied().unwrap_or(0);
    // A speculative batch's rows are not proved: its rejected rows never
    // reach the sequence a verifier re-executes.
    let provable = (n_tokens == 1).then_some((residual.values.as_slice(), width));
    s.meter_output(&sid_key, n_past, n_tokens as u32, &bytes, wire_dtype, provable);
    timings.residual_wire_f16_encode_ns = timings
        .residual_wire_f16_encode_ns
        .saturating_add(wire_timing.f16_conversion_ns);
    timings.residual_wire_f8_conversion_us = timings
        .residual_wire_f8_conversion_us
        .saturating_add(wire_timing.f8_conversion_us);
    timings.residual_wire_frame_serialize_us = timings
        .residual_wire_frame_serialize_us
        .saturating_add(wire_timing.frame_serialize_us);
    timings.residual_wire_encode_calls = timings.residual_wire_encode_calls.saturating_add(1);
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    if let Some(context) = profiling.as_ref() {
        timings.record_profile_span(
            context,
            "response-encode",
            encode_started,
            Instant::now(),
            None,
            None,
        );
    }
    Ok(TimedReply { bytes, timings })
}

/// Records only drafts submitted to target verification. The `next_draft`
/// may be prepared at the end of a round that ends by budget, without
/// ever being verified; counting it in the denominator would distort acceptance.
fn record_mtp_verification(
    timings: &mut NodeTimings,
    mtp_verification: bool,
    draft_len: usize,
    accepted_len: usize,
) {
    if mtp_verification {
        timings.mtp_draft_tokens_proposed = draft_len as u64;
        timings.mtp_draft_tokens_accepted = accepted_len as u64;
        timings.mtp_draft_tokens_rejected = draft_len.saturating_sub(accepted_len) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtp_telemetry_separates_accepted_and_rejected_proposals() {
        let mut timings = NodeTimings::default();

        record_mtp_verification(&mut timings, true, 7, 5);

        assert_eq!(timings.mtp_draft_tokens_proposed, 7);
        assert_eq!(timings.mtp_draft_tokens_accepted, 5);
        assert_eq!(timings.mtp_draft_tokens_rejected, 2);
    }

    #[test]
    fn mtp_telemetry_does_not_count_a_verification_that_is_not_mtp() {
        let mut timings = NodeTimings::default();

        record_mtp_verification(&mut timings, false, 7, 5);

        assert_eq!(timings.mtp_draft_tokens_proposed, 0);
        assert_eq!(timings.mtp_draft_tokens_accepted, 0);
        assert_eq!(timings.mtp_draft_tokens_rejected, 0);
    }
}
