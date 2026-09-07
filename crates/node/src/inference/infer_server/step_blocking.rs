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
    // tail). Ver `Speculate` para por que a fonte do rascunho viaja junto.
    speculate: Option<Speculate>,
    // What a previous hop already decided, forwarded via the wire `draft`
    // field. `None` means this round isn't speculative at all (the
    // pre-existing, single-token wire format applies); `Some(vec![])` means
    // it is speculative but nothing matched — still a batch of one, but
    // through the *new* wire format, since the caller committed to it by
    // asking to speculate in the first place.
    incoming_draft: Option<Vec<i32>>,
    // A composição do lote decodificado na cabeça, viajando pela wire (`tokens`)
    // para a cauda. É o que alimenta o `observe` da cabeça `nextn` dentro do
    // `decode_hidden`: a cauda sabe *quais* tokens o tronco decodificou, e sem
    // eles a cabeça de rascunho não se mantém em dia. `None` é peer antigo que
    // não encaminha o campo — aí o `decode_hidden` desliga o MTP com aviso.
    batch_tokens: Option<Vec<i32>>,
    // Largura de lote que o gateway quer na rodada seguinte, vinda da cabeça
    // via `mtp_k` na wire. Só a cauda usa (para rascunhar a medida certa); a
    // cabeça a ecoa a partir do próprio `speculate` (`mtp_k_hint`).
    mtp_k: Option<u32>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    timings.selected_path = s.inference_path;

    let setup_started = Instant::now();
    // A geração deste turno corre numa cópia do KV do prompt: a partir daqui
    // nada mais escreve na sequência do prompt, e é isso que a deixa
    // reutilizável no turno seguinte sem remover nada do cache.
    s.open_generation(&sid_key)?;
    let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
        anyhow::anyhow!("sessão '{}' desconhecida — faça o prefill antes", sid_key)
    })?;
    session.last_used = Instant::now();
    let n_past = session.n_past;
    let seq_id = session.seq_id();
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    let mut head_draft: Option<Vec<i32>> = None;
    // A composição do lote que este hop decodificou, para o reply não-cauda.
    // Inicializada nos dois branches (tensor e cabeça) antes de qualquer
    // leitura, por isso sem valor inicial — ver o uso no reply_meta.
    let reply_tokens;
    // Ecoado no reply não-cauda: na cabeça nasce do `speculate` (mtp_k_hint),
    // nos hops do meio repassa o que chegou da wire. É como a cauda descobre a
    // largura de lote que o gateway quer na rodada seguinte.
    let mut mtp_k_echo: Option<u32> = None;
    // O limite é por decodificação, não por sessão. O mesmo lock serializa
    // esta troca e o decode que a consome.
    if let Err(error) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            bail!("{error}");
        }
    }
    let input_was_hidden = tensor.is_some();
    let selected_moe = s.inference_path.is_moe();
    let mut moe_forwarded = false;
    let tensor = if let Some((hidden, shape)) = tensor {
        let hidden = s.apply_moe_hidden(hidden, shape[0], capture_block, &mut timings)?;
        moe_forwarded = selected_moe
            && s.engine.moe_external_executor_enabled()
            && !s.engine.moe_hook_enabled();
        Some((hidden, shape))
    } else {
        None
    };
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state. `decode_hidden` already
        // handles any `n_tokens` — a speculative batch needs no special
        // handling here beyond what chunked prefill already exercises. A cauda
        // de pipeline fatiado tem o bloco `nextn`: o `decode_hidden` alimenta
        // o `observe` com a composição do lote (`batch_tokens`, vinda da
        // cabeça via `tokens` na wire); sem ela, ele desliga o MTP com aviso.
        n_tokens = shape[0];
        let logits_all = incoming_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_hidden(
            &hidden,
            &shape,
            n_tokens,
            n_past,
            seq_id,
            logits_all,
            batch_tokens.as_deref(),
        )?;
        // `batch_tokens` segue vivo: a cauda usa-o no gate do `next_draft`
        // (verificação) e no caminho plain.
        reply_tokens = batch_tokens.clone();
        // `Option<u32>` é Copy — o `mtp_k` do parâmetro continua vivo para o
        // caminho plain da cauda.
        mtp_k_echo = mtp_k;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
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

        // A largura de lote que o gateway quer na rodada seguinte, extraída do
        // `speculate` que a cabeça recebeu e ecoada na resposta — é como a
        // cauda descobre a medida do rascunho (ver `mtp_k` no doc do parâmetro).
        let mtp_k_hint = speculate
            .as_ref()
            .filter(|s| s.source == DraftSource::Mtp)
            .map(|s| s.k);
        let pode_rascunhar = should_draft(speculate.clone(), s.engine.mtp_ready());
        let batch: Vec<i32> = if let Some(spec) = speculate.filter(|_| pode_rascunhar) {
            let draft: Vec<i32> = if let Some(premade) = spec.draft {
                premade
            } else {
                match spec.source {
                    DraftSource::Mtp => {
                        let p_min = s.mtp_min_probability;
                        s.engine
                            .mtp_draft(tid, n_past, seq_id, spec.k.saturating_sub(1), p_min)
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
        let logits_all = head_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_tokens(&batch, n_past, seq_id, logits_all)?;
        // A composição do lote decodificado na cabeça — o que a cauda precisa
        // para o `observe`. Viaja no reply não-cauda e é ecoada até ela.
        reply_tokens = Some(batch);
        // Ecoa a largura de lote pedida para a cauda rascunhar. `mtp_k` do
        // parâmetro fica vivo para o caminho plain da cauda (o `or` usa uma
        // cópia).
        if let Some(k) = mtp_k_hint.or(mtp_k) {
            mtp_k_echo = Some(k);
        }
    }
    if is_last
        && selected_moe
        && s.engine.moe_external_executor_enabled()
        && !moe_forwarded
        && !s.engine.moe_hook_enabled()
    {
        let residual = s.engine.take_residual(n_tokens)?;
        let _ = s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
        moe_forwarded = true;
    }
    timings.merge_engine_moe(&engine_timing);
    moe_forwarded |= engine_timing.moe_forward_us != 0;
    if is_last {
        record_terminal_engine(&mut timings, s.inference_path, moe_forwarded);
    } else {
        timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);
    }
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    s.sessions.get_mut(&sid_key).unwrap().n_past += n_tokens as u32;

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
                let (id, _timing) = sample_token_profiled(logits, &recent, &sampling);
                recent.push(id);
                let is_stop = s.engine.is_generation_stop(&recent[..recent.len() - 1], id);
                pieces.push(if is_stop {
                    Vec::new()
                } else {
                    s.engine.token_to_piece_bytes(id, false)
                });
                sampled.push(id);
                if is_stop {
                    // Nothing past a model EOS or turn terminator is worth
                    // sampling, regardless of what the rest of the draft guessed.
                    break;
                }
            }
            timings.sampling_us += verification_started.elapsed().as_micros() as u64;

            let accept_len = longest_match(&sampled, &draft);
            // A cabeça `nextn` guarda uma linha de estado oculto por posição
            // decodificada; qual delas alimenta a próxima rodada só se sabe
            // depois de a verificação dizer até onde o rascunho valeu. Sem
            // isto ela pareia o token confirmado com o `h` de uma posição que
            // foi descartada, e o rascunho seguinte degenera em silêncio.
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

            // Rascunho para a rodada seguinte. Numa máquina só ele alimenta
            // o mesmo processo; num pipeline fatiado ele viaja no reply que já
            // sobe pela cadeia. O stream remoto o instala na cabeça antes do
            // próximo `step`; o caminho direto legado o leva ao gateway para
            // injeção - sempre sem travessia extra.
            //
            // O `batch_tokens` presente é o que garante que esta é a cauda de
            // um pipeline (a composição do lote veio de fora, via `tokens`):
            // numa máquina só a cabeça é a própria cauda e não há para onde
            // devolver o rascunho.
            let mut speculation_disabled_reason = None;
            let mtp_tail_reason = mtp_k.is_some().then(|| {
                mtp_tail_speculation_reason(
                    s.engine.mtp_ready(),
                    s.engine.mtp_unavailable_reason(),
                    s.engine.kv_can_shift(),
                )
            });
            let next_draft: Option<Vec<i32>> =
                if !done && batch_tokens.is_some() && mtp_tail_reason.flatten().is_none() {
                    // A sessão já avançou `n_tokens`; só as primeiras `keep`
                    // posições foram confirmadas, então o próximo slot livre do
                    // tronco é `n_past - (n_tokens - keep)`.
                    let session_n_past = s.sessions[&sid_key].n_past;
                    let n_past_next = session_n_past - (n_tokens as u32 - keep as u32);
                    let id0 = *accepted_ids.last().unwrap_or(&eos);
                    let p_min = s.mtp_min_probability;
                    let dr =
                        s.engine
                            .mtp_draft(id0, n_past_next, seq_id, draft.len() as u32, p_min);
                    if dr.is_empty() {
                        speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                        None
                    } else {
                        Some(dr)
                    }
                } else {
                    speculation_disabled_reason = mtp_tail_reason.flatten();
                    None
                };

            if remote_stream {
                let n_past_after_batch = s.sessions[&sid_key].n_past;
                let new_n_past = rollback_from(n_past_after_batch, n_tokens as u32, keep as u32);
                if new_n_past < n_past_after_batch
                    && !s.engine.kv_remove(seq_id, new_n_past as i32, -1)
                {
                    bail!("rollback especulativo remoto recusado na cauda");
                }
                s.sessions.get_mut(&sid_key).unwrap().n_past = new_n_past;
            }

            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .recent_tokens
                .extend_from_slice(accepted_ids);
            // Ver a nota gêmea abaixo: réplica não encerra sessão. E o fim de
            // turno retém o prompt - ver `InferState::retain_prompt`.
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
                    "next_draft": next_draft,
                }),
                None,
                wire_dtype,
            )?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            return Ok(TimedReply { bytes, timings });
        }

        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_profiled(logits, &recent, &sampling);
        let done = s.engine.is_generation_stop(&recent, next_id);
        let token_text_started = Instant::now();
        let mut text = if done {
            String::new()
        } else {
            let piece = s.engine.token_to_piece_bytes(next_id, false);
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
        // A réplica de um grupo TP não encerra sessão: quem decide o fim da
        // geração é o rank 0. Ver `tp::is_replica`. E o fim de turno retém o
        // prompt - ver `InferState::retain_prompt`.
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

        // A cauda de um pipeline fatiado também rascunha na rodada em que a
        // cabeça recusou — a primeira, quando ainda não há rascunho pronto
        // para viajar. O `mtp_k` veio da cabeça (via `mtp_k` na wire): é a
        // largura de lote que o gateway quer na rodada seguinte. O
        // `mtp_accept(0)` confirma o `pending_h` da linha do token recém
        // decodificado; sem ele o draft seguinte usaria a linha de uma posição
        // anterior e parearia errado. Rodada de modelo inteiro recusada não
        // tem `mtp_k` — nada a devolver, e não há segunda máquina esperando.
        let mut speculation_disabled_reason = None;
        let mtp_tail_reason = mtp_k.is_some().then(|| {
            mtp_tail_speculation_reason(
                s.engine.mtp_ready(),
                s.engine.mtp_unavailable_reason(),
                s.engine.kv_can_shift(),
            )
        });
        let next_draft: Option<Vec<i32>> = if !done && mtp_tail_reason.flatten().is_none() {
            if let Some(k) = mtp_k {
                s.engine.mtp_accept(0);
                let session_n_past = s.sessions[&sid_key].n_past;
                let p_min = s.mtp_min_probability;
                let dr =
                    s.engine
                        .mtp_draft(next_id, session_n_past, seq_id, k.saturating_sub(1), p_min);
                if dr.is_empty() {
                    speculation_disabled_reason = Some(SPECULATION_REASON_TAIL_DRAFT_EMPTY);
                    None
                } else {
                    Some(dr)
                }
            } else {
                None
            }
        } else {
            speculation_disabled_reason = mtp_tail_reason.flatten();
            None
        };
        let encode_started = Instant::now();
        let mut reply = json!({"done": done, "token_id": next_id, "text": text});
        reply["speculated"] = json!(false);
        reply["speculation_active"] = json!(next_draft.is_some());
        reply["speculation_disabled_reason"] = json!(speculation_disabled_reason);
        if let Some(nd) = &next_draft {
            reply["next_draft"] = json!(nd);
        }
        let bytes = wire::encode(&reply, None, wire_dtype)?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let mut residual = s.engine.take_residual(n_tokens)?;
    if !input_was_hidden {
        residual.values =
            s.apply_moe_hidden(residual.values, n_tokens, capture_block, &mut timings)?;
    }
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let mut reply_meta = json!({"done": false});
    // Whoever called us needs this to keep threading it toward the tail —
    // see `draft`'s own comment above for why it's carried this way rather
    // than recomputed at each hop.
    if let Some(draft) = &draft {
        reply_meta["draft"] = json!(draft);
    }
    // A composição do lote decodificado (ver `batch_tokens`) e a largura de
    // lote da próxima rodada (ver `mtp_k`) viajam da cabeça para a cauda —
    // são o que mantém a cabeça `nextn` da cauda em dia e rascunhando a
    // medida certa.
    if let Some(tokens) = &reply_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    if let Some(k) = mtp_k_echo {
        reply_meta["mtp_k"] = json!(k);
    }
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual.values, residual.shape.clone())),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}
