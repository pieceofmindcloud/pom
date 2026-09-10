use super::*;

#[allow(clippy::too_many_arguments)]
pub async fn run_with_options(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    images: &[Vec<u8>],
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
    max_inflight_chunks: usize,
    // `Some` descreve um ponto de retomada que o chamador conhece como seguro.
    // O gateway não o usa para uma resposta de transporte ambígua: nesses
    // casos ele recria a sessão a frio desde o pedaço 0. Só hub e resposta
    // direta sabem retomar; o anel ignora e roda do início.
    resume: Option<PrefillResumable>,
    trace_id: Option<&str>,
) -> Result<PrefillOutcome> {
    check_cancelled(cancellation)?;
    plan.validate_wire_dtype(dtype)?;
    let chunk_size = chunk_size.max(1);
    let n_peers = plan.steps.len();
    let overlap_window = PrefillOverlapWindow::new(max_inflight_chunks);
    if n_peers == 0 {
        return Err(anyhow!("pipeline sem peers"));
    }
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return run_direct(
            plan,
            model,
            session_id,
            prompt,
            images,
            chat_template,
            chunk_size,
            dtype,
            http,
            routes,
            direct,
            tunnel,
            cancellation,
            reuse_prefix,
            overlap_window.clone(),
            resume,
            trace_id,
        )
        .await;
    }
    if plan.supports_relay_chain() {
        return run_ring(
            plan,
            model,
            session_id,
            prompt,
            images,
            chat_template,
            chunk_size,
            dtype,
            http,
            routes,
            tunnel,
            cancellation,
            reuse_prefix,
            overlap_window.clone(),
            trace_id,
        )
        .await;
    }

    let started = Instant::now();
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
    let window = overlap_window.max_inflight_chunks();
    trace.prefill_overlap_enabled = window > 1 && n_peers > 1;
    trace.prefill_max_inflight_chunks = window as u32;
    // Retomada explícita: o laço parte deste pedaço/posição em vez do zero, e
    // os totais do pedaço 0 (que não será revisto) vêm do ponto informado.
    let first_chunk = resume.map(|r| r.from_chunk).unwrap_or(0);
    let first_start_pos = resume.map(|r| r.start_pos);
    let resume_cached_tokens = resume.map(|r| r.cached_tokens).unwrap_or(0);

    // Um pedaço a caminho da cadeia: a resposta da cabeça (com o estado oculto)
    // mais o permit da janela, que segura a vaga até o downstream deste pedaço
    // terminar. É `_permit` que limita profundidade e memória.
    struct HeadChunk {
        chunk_index: u32,
        meta: serde_json::Value,
        tensor: Option<(Vec<f32>, TensorShape)>,
        head_trace: RequestTrace,
        is_final: bool,
        start_pos: u32,
        stable_len: Option<u64>,
        head_reused: bool,
        prompt_tokens: u32,
        cached_tokens: u32,
        _permit: OwnedSemaphorePermit,
        _byte_permit: OwnedSemaphorePermit,
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<HeadChunk>(window.max(1));

    // Bomba da cabeça: despacha os pedaços em série - cada um é uma perna de
    // rede + o decode da cabeça - e entrega o estado oculto de cada um à bomba
    // de downstream. A `next_pos` sai sempre da resposta do pedaço anterior,
    // nunca do índice (ver `observed_prefill_start`).
    let head_pump = async {
        let mut idx: u32 = first_chunk;
        let mut start_pos: Option<u32> = first_start_pos;
        // Últimos totais confirmados, para o ponto de retomada quando a chamada
        // à cabeça de um pedaço posterior falha.
        let mut last_prompt_tokens: u32 = resume.map(|r| r.prompt_tokens).unwrap_or(0);
        let mut last_cached_tokens: u32 = resume_cached_tokens;
        loop {
            check_cancelled(cancellation)?;
            let permit = overlap_window.acquire().await?;
            let head_meta = head_prefill_meta(
                session_id,
                idx,
                chunk_size,
                n_peers == 1,
                (idx == 0).then_some(prompt),
                chat_template,
                reuse_prefix && idx == 0,
                start_pos,
                trace_id,
            );
            let head_meta = add_images_to_head_meta(head_meta, (idx == 0).then_some(images));
            let mut head_trace = trace_id
                .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
                .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
            let (meta, tensor) = match send_chunk(
                http,
                routes,
                &plan.steps[0],
                tunnel,
                &head_meta,
                None,
                dtype,
                dtype.frame_encoding(),
                idx,
                &mut head_trace,
                0,
                Phase::Prefill,
                cancellation,
            )
            .await
            {
                Ok(v) => v,
                Err(e)
                    if idx == 0
                        || is_cancelled(&e)
                        || crate::relay::is_unknown_session_error(&e) =>
                {
                    return Err(e)
                }
                Err(e) => {
                    // Os pedaços 0..idx já foram aceitos pela cabeça. Este
                    // ponto só é reutilizado quando o chamador garante que a
                    // retomada é segura; a recuperação de transporte usa o
                    // prefill frio do gateway.
                    return Err(e.context(PrefillResumable {
                        from_chunk: idx,
                        start_pos: start_pos.unwrap_or(0),
                        prompt_tokens: last_prompt_tokens,
                        cached_tokens: last_cached_tokens,
                    }));
                }
            };

            let prompt_tokens = meta
                .get("n_prompt_tokens")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(last_prompt_tokens);
            let (head_reused, cached_tokens) = if idx == 0 {
                (
                    head_reused_prefix(&meta, reuse_prefix),
                    cached_tokens_of(&meta),
                )
            } else {
                (false, last_cached_tokens)
            };
            last_prompt_tokens = prompt_tokens;
            last_cached_tokens = cached_tokens;
            let more = meta
                .get("more_chunks")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let is_final = !more;
            let chunk_start_pos = observed_prefill_start(&meta, idx, head_reused)?;
            let chunk_next_pos = if more {
                required_prefill_position(&meta, "next_pos", idx)?
            } else {
                chunk_start_pos
            };
            let stable_len = meta.get("stable_len").and_then(|value| value.as_u64());
            // Bytes de estado oculto que este pedaço vai segurar em RAM até o
            // downstream dele terminar - `f32` em memória, não o f16 do fio.
            let tensor_bytes = tensor
                .as_ref()
                .map(|(values, _shape)| values.len() * std::mem::size_of::<f32>())
                .unwrap_or(0);
            let byte_permit = overlap_window.reserve_bytes(tensor_bytes).await;

            if tx
                .send(HeadChunk {
                    chunk_index: idx,
                    meta,
                    tensor,
                    head_trace,
                    is_final,
                    start_pos: chunk_start_pos,
                    stable_len,
                    head_reused,
                    prompt_tokens,
                    cached_tokens,
                    _permit: permit,
                    _byte_permit: byte_permit,
                })
                .await
                .is_err()
            {
                break;
            }
            if is_final {
                break;
            }
            start_pos = Some(chunk_next_pos);
            idx += 1;
        }
        Ok::<(), anyhow::Error>(())
    };

    // Bomba de downstream: leva cada estado oculto pelos hops restantes, até
    // `window` pedaços ao mesmo tempo, e colhe os resultados na ordem de envio
    // (`FuturesOrdered`). O pedaço final é o que produz o token e o resultado.
    let drain = async {
        let mut inflight = FuturesOrdered::new();
        let mut head_closed = false;
        let mut prompt_tokens: u32 = 0;
        let mut cached_tokens: u32 = resume_cached_tokens;
        loop {
            check_cancelled(cancellation)?;
            tokio::select! {
                biased;
                maybe = rx.recv(), if !head_closed && inflight.len() < window => {
                    let Some(hc) = maybe else {
                        head_closed = true;
                        continue;
                    };
                    prompt_tokens = hc.prompt_tokens;
                    if hc.chunk_index == 0 {
                        cached_tokens = hc.cached_tokens;
                        trace.record_prefix_decision(&hc.meta, reuse_prefix);
                    }
                    trace.merge(&hc.head_trace);
                    let ci = hc.chunk_index;
                    let is_final = hc.is_final;
                    let chunk_start_pos = hc.start_pos;
                    inflight.push_back(async move {
                        // Segura a vaga e os bytes deste pedaço até o forward
                        // dele terminar - é o que libera a próxima entrada.
                        let _permit = hc._permit;
                        let _byte_permit = hc._byte_permit;
                        let r = forward_hub_chunk(
                            plan,
                            session_id,
                            ci,
                            chunk_size,
                            hc.head_reused,
                            is_final,
                            hc.start_pos,
                            hc.stable_len,
                            hc.meta,
                            hc.tensor,
                            dtype,
                            http,
                            routes,
                            tunnel,
                            cancellation,
                        )
                        .await;
                        (ci, is_final, chunk_start_pos, r)
                    });
                    let depth = inflight.len() as u32;
                    trace.prefill_pipeline_depth_max =
                        trace.prefill_pipeline_depth_max.max(depth);
                    if depth > 1 {
                        trace.prefill_overlapped_chunks += 1;
                    }
                }
                Some((ci, is_final, chunk_start_pos, r)) = inflight.next(), if !inflight.is_empty() => {
                    let (meta, _tensor, downstream_trace) = match r {
                        Ok(v) => v,
                        // Sessão perdida no peer: o KV para retomar não existe
                        // mais, então vai direto para a reconstrução fria.
                        Err(e)
                            if is_cancelled(&e)
                                || crate::relay::is_unknown_session_error(&e) =>
                        {
                            return Err(e)
                        }
                        // Pedaços anteriores já foram aceitos pela cadeia. Um
                        // chamador que tenha provado a retomada pode marcar o
                        // ponto; a recuperação normal de transporte reconstrói
                        // o prompt inteiro para não pedir remoção parcial.
                        Err(e) if ci > 0 => {
                            return Err(e.context(PrefillResumable {
                                from_chunk: ci,
                                start_pos: chunk_start_pos,
                                prompt_tokens,
                                cached_tokens,
                            }));
                        }
                        Err(e) => return Err(e),
                    };
                    trace.merge(&downstream_trace);
                    if is_final {
                        return Ok::<_, anyhow::Error>((meta, ci + 1, prompt_tokens, cached_tokens));
                    }
                }
                else => {
                    return Err(anyhow!("pipeline de prefill fechou sem o pedaço final"));
                }
            }
        }
    };

    let (_, (final_meta, chunks, prompt_tokens, cached_tokens)) =
        tokio::try_join!(head_pump, drain)?;

    let token_id = final_meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
        as u32;
    info!("prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens ({prompt_tokens} tokens)");
    Ok(PrefillOutcome {
        token_id,
        text: final_meta
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        done: final_meta
            .get("done")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        prompt_tokens,
        retained_tokens: retained_tokens_of(&final_meta, prompt_tokens),
        cached_tokens,
        chunks,
        trace: {
            trace.finish_prompt(prompt_tokens, cached_tokens);
            trace.prefill_ms = started.elapsed().as_millis() as u64;
            trace
        },
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_hub_chunk(
    plan: &PipelinePlan,
    session_id: &str,
    chunk_index: u32,
    chunk_size: u32,
    head_reused: bool,
    is_final_chunk: bool,
    chunk_start_pos: u32,
    chunk_stable_len: Option<u64>,
    mut meta: serde_json::Value,
    mut tensor: Option<(Vec<f32>, TensorShape)>,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
) -> Result<(
    serde_json::Value,
    Option<(Vec<f32>, TensorShape)>,
    RequestTrace,
)> {
    let mut trace = meta
        .get("trace_id")
        .and_then(|value| value.as_str())
        .map(|id| RequestTrace::from_plan_with_trace_id(id, "prefill", plan))
        .unwrap_or_else(|| RequestTrace::from_plan("prefill", plan));
    for (i, step) in plan.steps.iter().enumerate().skip(1) {
        check_cancelled(cancellation)?;
        let is_last = i == plan.steps.len() - 1;
        let Some((values, shape)) = tensor.take() else {
            return Err(anyhow!(
                "peer {} não devolveu estado oculto no pedaço {chunk_index}",
                plan.steps[i - 1].node_id
            ));
        };
        let mut body_meta = json!({
            "session_id": session_id,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_last": is_last,
            "is_final_chunk": is_final_chunk,
            "reuse_prefix": head_reused,
            "start_pos": chunk_start_pos,
            "scale": 1.0,
        });
        if let Some(stable) = chunk_stable_len {
            body_meta["stable_len"] = json!(stable);
        }
        if let Some(tokens) = meta.get("tokens") {
            body_meta["tokens"] = tokens.clone();
        }
        copy_mrope_positions(&meta, &mut body_meta);
        if let Some(prefix_match_tokens) = meta.get("prefix_match_tokens") {
            body_meta["prefix_match_tokens"] = prefix_match_tokens.clone();
        }
        if let Some(trace_id) = meta.get("trace_id") {
            body_meta["trace_id"] = trace_id.clone();
        }
        let (next_meta, next_tensor) = send_chunk(
            http,
            routes,
            step,
            tunnel,
            &body_meta,
            Some((&values, shape)),
            dtype,
            step.frame_encoding(),
            chunk_index,
            &mut trace,
            i,
            Phase::Prefill,
            cancellation,
        )
        .await?;
        meta = next_meta;
        tensor = next_tensor;
    }
    Ok((meta, tensor, trace))
}

/// Keeps the physical cells and their logical M-RoPE positions together when
/// a hub forwards a hidden-state chunk to the next slice.
pub(super) fn copy_mrope_positions(meta: &serde_json::Value, body_meta: &mut serde_json::Value) {
    if let Some(positions) = meta.get("mrope_positions") {
        body_meta["mrope_positions"] = positions.clone();
    }
}

/// Same chunk-by-chunk loop as `run`, but each chunk makes exactly one
/// outbound call — to the head — instead of one call per hop. The head
/// relays to its own next hop, which relays to its own, and so on; the
/// eventual answer (and, for the head, its authoritative `more_chunks`/
/// `n_prompt_tokens` — see the overlay fix in `infer_server.rs`'s
/// `handle_prefill`) bubbles back through that same chain of calls.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    images: &[Vec<u8>],
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
    _overlap_window: PrefillOverlapWindow,
    trace_id: Option<&str>,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let mut chunk_index: u32 = 0;
    let mut prompt_tokens: u32 = 0;
    let mut cached_tokens: u32 = 0;
    let mut next_start_pos: Option<u32> = None;
    let started = Instant::now();
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));

    loop {
        check_cancelled(cancellation)?;
        let mut head_meta = head_prefill_meta(
            session_id,
            chunk_index,
            chunk_size,
            next_hops.is_empty(),
            (chunk_index == 0).then_some(prompt),
            chat_template,
            reuse_prefix && chunk_index == 0,
            next_start_pos,
            trace_id,
        );
        head_meta = add_images_to_head_meta(head_meta, (chunk_index == 0).then_some(images));
        head_meta["next_hops"] = json!(next_hops);
        head_meta["exec_start"] = json!(head.exec_start);
        head_meta["exec_end"] = json!(head.exec_end);

        let sent = cancelable(
            cancellation,
            crate::relay::send_frame_or_tunnel(
                http,
                head,
                tunnel,
                "/v1/prefill",
                &head_meta,
                None,
                dtype,
                dtype.frame_encoding(),
                crate::relay::CHUNK_ATTEMPTS, // uma resposta perdida exige rebuild frio
                &head.node_id,
                |_issue| {},
            ),
        )
        .await
        .map_err(|e| {
            if !is_cancelled(&e) {
                routes.mark_failure(&head.node_id, &head.inference_url);
            }
            ring_relay_error(&head.node_id, e)
        })?;

        for (i, entry) in crate::relay::bubble_hop_timings(&sent)
            .into_iter()
            .enumerate()
        {
            trace.record_hop_from_json(i, Phase::Prefill, &entry);
        }

        if let Some(n) = sent.meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
            prompt_tokens = n as u32;
        }
        // No anel a cabeça repassa a própria decisão adiante; aqui só se lê o
        // que ela executou, para a telemetria. Ver `cached_tokens_of`.
        if chunk_index == 0 {
            cached_tokens = cached_tokens_of(&sent.meta);
            trace.record_prefix_decision(&sent.meta, reuse_prefix);
        }
        let more = sent
            .meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_final_chunk = !more;
        let chunk_next_pos = if more {
            required_prefill_position(&sent.meta, "next_pos", chunk_index)?
        } else {
            0
        };

        if is_final_chunk {
            let token_id = sent
                .meta
                .get("token_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
                as u32;
            let chunks = chunk_index + 1;
            info!(
                "prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens ({prompt_tokens} tokens) [anel]"
            );
            return Ok(PrefillOutcome {
                token_id,
                text: sent
                    .meta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                done: sent
                    .meta
                    .get("done")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                prompt_tokens,
                retained_tokens: retained_tokens_of(&sent.meta, prompt_tokens),
                cached_tokens,
                chunks,
                trace: {
                    trace.finish_prompt(prompt_tokens, cached_tokens);
                    trace.prefill_ms = started.elapsed().as_millis() as u64;
                    trace
                },
            });
        }

        next_start_pos = Some(chunk_next_pos);
        chunk_index += 1;
    }
}

/// Same chunk-by-chunk loop as `run_ring`, but the chain does not come back:
/// each chunk is dispatched once, to the head, carrying a `reply_to`, and the
/// tail delivers the answer straight to this process (see [`crate::direct`]).
/// One less crossing of the link per chunk than the ring, and — the point of
/// the whole thing — a cost that no longer depends on whether the head happens
/// to live on the machine that took the request. A prefill chunk is ~5 MB of
/// hidden state, so this is also most of the time to the first word.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_direct(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: &str,
    images: &[Vec<u8>],
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    reuse_prefix: bool,
    overlap_window: PrefillOverlapWindow,
    resume: Option<PrefillResumable>,
    trace_id: Option<&str>,
) -> Result<PrefillOutcome> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];

    let started = Instant::now();
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
    let window = overlap_window.max_inflight_chunks();
    trace.prefill_overlap_enabled = window > 1 && !next_hops.is_empty();
    trace.prefill_max_inflight_chunks = window as u32;
    let prefill_sequence_id = uuid::Uuid::new_v4().to_string();
    // Retomada: ver `run_with_options`.
    let first_chunk = resume.map(|r| r.from_chunk).unwrap_or(0);
    let first_start_pos = resume.map(|r| r.start_pos);
    let resume_cached_tokens = resume.map(|r| r.cached_tokens).unwrap_or(0);

    // Um pedaço despachado à cabeça, aguardando o resultado que a cauda entrega
    // direto neste processo (`await_result_with_ack` sobre o `ticket`).
    struct HeadChunkDirect {
        chunk_index: u32,
        start_pos: u32,
        ticket: crate::direct::Ticket,
        head_sent: crate::relay::SentFrame,
        more_from_head: bool,
        _permit: OwnedSemaphorePermit,
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<HeadChunkDirect>(window.max(1));

    // Bomba da cabeça: despacha cada pedaço à cabeça em série. A `next_pos` do
    // pedaço seguinte sai do `prompt_progress` do ACK da cabeça, publicado
    // adiante justamente para o despacho não esperar o resultado do anterior.
    // Só o gate por contagem vale aqui: na resposta direta a cabeça repassa o
    // estado oculto ao próximo hop e o gateway não retém tensor nenhum, então
    // `reserve_bytes` não é chamado.
    let head_pump = async {
        let mut idx: u32 = first_chunk;
        let mut start_pos: Option<u32> = first_start_pos;
        let mut last_prompt_tokens: u32 = resume.map(|r| r.prompt_tokens).unwrap_or(0);
        loop {
            check_cancelled(cancellation)?;
            let permit = overlap_window.acquire().await?;
            let ticket = direct.ticket_for_hops(plan.steps.len());
            let ticket_id = ticket.id().to_owned();
            let head_sent = match dispatch_direct_prefill(
                plan,
                session_id,
                prompt,
                images,
                chat_template,
                chunk_size,
                dtype,
                idx,
                reuse_prefix && idx == 0,
                start_pos,
                http,
                routes,
                direct,
                tunnel,
                cancellation,
                ticket.id(),
                &prefill_sequence_id,
                trace_id,
            )
            .await
            {
                Ok(v) => v,
                Err(e)
                    if idx == 0
                        || is_cancelled(&e)
                        || crate::relay::is_unknown_session_error(&e) =>
                {
                    return Err(e)
                }
                Err(e) => {
                    return Err(e.context(PrefillResumable {
                        from_chunk: idx,
                        start_pos: start_pos.unwrap_or(0),
                        prompt_tokens: last_prompt_tokens,
                        cached_tokens: resume_cached_tokens,
                    }));
                }
            };
            direct.record_transport(&ticket_id, 0, &head_sent);

            let progress = direct_prefill_progress(&head_sent.meta);
            if let Some(n) = progress.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
                last_prompt_tokens = n as u32;
            }
            let more_from_head = progress
                .get("more_chunks")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let next_pos_from_head = if more_from_head {
                Some(required_prefill_position(progress, "next_pos", idx)?)
            } else {
                None
            };

            if tx
                .send(HeadChunkDirect {
                    chunk_index: idx,
                    start_pos: start_pos.unwrap_or(0),
                    ticket,
                    head_sent,
                    more_from_head,
                    _permit: permit,
                })
                .await
                .is_err()
            {
                break;
            }
            if !more_from_head {
                break;
            }
            start_pos = next_pos_from_head;
            idx += 1;
        }
        Ok::<(), anyhow::Error>(())
    };

    // Bomba de downstream: espera o resultado de cada pedaço na ordem de envio.
    // A cadeia já mantém a ordem do repasse por `prefill_sequence_id`
    // (`DirectForwardSequence` no peer); aqui só se colhe em ordem.
    let drain = async {
        let mut inflight = FuturesOrdered::new();
        let mut head_closed = false;
        let mut prompt_tokens: u32 = 0;
        let mut cached_tokens: u32 = resume_cached_tokens;
        loop {
            check_cancelled(cancellation)?;
            tokio::select! {
                biased;
                maybe = rx.recv(), if !head_closed && inflight.len() < window => {
                    let Some(hc) = maybe else {
                        head_closed = true;
                        continue;
                    };
                    let HeadChunkDirect {
                        chunk_index: ci,
                        start_pos: chunk_start_pos,
                        mut ticket,
                        head_sent,
                        more_from_head,
                        _permit,
                    } = hc;
                    inflight.push_back(async move {
                        let _permit = _permit;
                        let r = cancelable(
                            cancellation,
                            crate::direct::await_result_with_ack(
                                std::future::ready(Ok(head_sent)),
                                &mut ticket,
                                &head.node_id,
                                |_| {},
                            ),
                        )
                        .await;
                        (ci, chunk_start_pos, more_from_head, r)
                    });
                    let depth = inflight.len() as u32;
                    trace.prefill_pipeline_depth_max =
                        trace.prefill_pipeline_depth_max.max(depth);
                    if depth > 1 {
                        trace.prefill_overlapped_chunks += 1;
                    }
                }
                Some((ci, chunk_start_pos, more_from_head, r)) = inflight.next(), if !inflight.is_empty() => {
                    // Pedaços anteriores já foram aceitos pela cadeia: em vez de
                    // refazer o prompt inteiro, marca de onde o gateway retoma.
                    // Sessão perdida no peer não é retomável (o KV sumiu) e cai
                    // no retry frio.
                    let mark_resumable = |e: anyhow::Error| {
                        if ci > 0
                            && !is_cancelled(&e)
                            && !crate::relay::is_unknown_session_error(&e)
                        {
                            e.context(PrefillResumable {
                                from_chunk: ci,
                                start_pos: chunk_start_pos,
                                prompt_tokens,
                                cached_tokens,
                            })
                        } else {
                            e
                        }
                    };
                    let meta = match r {
                        Ok(meta) => meta,
                        Err(e) => {
                            if !is_cancelled(&e) {
                                routes.mark_failure(&head.node_id, &head.inference_url);
                            }
                            return Err(mark_resumable(ring_relay_error(&head.node_id, e)));
                        }
                    };
                    if let Some(error) = direct_failure(plan, routes, &meta) {
                        return Err(mark_resumable(error));
                    }
                    record_direct_hops(&mut trace, Phase::Prefill, &meta);
                    if let Some(n) = meta.get("n_prompt_tokens").and_then(|v| v.as_u64()) {
                        prompt_tokens = n as u32;
                    }
                    if ci == 0 {
                        cached_tokens = cached_tokens_of(&meta);
                        trace.record_prefix_decision(&meta, reuse_prefix);
                    }
                    let more = meta
                        .get("more_chunks")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(more_from_head);
                    if !more {
                        return Ok::<_, anyhow::Error>((meta, ci + 1, prompt_tokens, cached_tokens));
                    }
                }
                else => {
                    return Err(anyhow!(
                        "pipeline de prefill (resposta direta) fechou sem o pedaço final"
                    ));
                }
            }
        }
    };

    let (_, (final_meta, chunks, prompt_tokens, cached_tokens)) =
        tokio::try_join!(head_pump, drain)?;

    let token_id = final_meta
        .get("token_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("último peer não devolveu token ao fim do prompt"))?
        as u32;
    info!(
        "prompt ingerido em {chunks} pedaço(s) de {chunk_size} tokens \
         ({prompt_tokens} tokens) [resposta direta]"
    );
    Ok(PrefillOutcome {
        token_id,
        text: final_meta
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        done: final_meta
            .get("done")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        prompt_tokens,
        retained_tokens: retained_tokens_of(&final_meta, prompt_tokens),
        cached_tokens,
        chunks,
        trace: {
            trace.finish_prompt(prompt_tokens, cached_tokens);
            trace.prefill_ms = started.elapsed().as_millis() as u64;
            trace
        },
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_direct_prefill(
    plan: &PipelinePlan,
    session_id: &str,
    prompt: &str,
    images: &[Vec<u8>],
    chat_template: bool,
    chunk_size: u32,
    dtype: WireDtype,
    chunk_index: u32,
    reuse_prefix: bool,
    start_pos: Option<u32>,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    cancellation: &Cancellation,
    ticket: &str,
    prefill_sequence_id: &str,
    trace_id: Option<&str>,
) -> Result<crate::relay::SentFrame> {
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut head_meta = head_prefill_meta(
        session_id,
        chunk_index,
        chunk_size,
        next_hops.is_empty(),
        (chunk_index == 0).then_some(prompt),
        chat_template,
        reuse_prefix,
        start_pos,
        trace_id,
    );
    head_meta = add_images_to_head_meta(head_meta, (chunk_index == 0).then_some(images));
    head_meta["next_hops"] = json!(next_hops);
    head_meta["reply_to"] = direct.reply_to(ticket);
    head_meta["prefill_sequence_id"] = json!(prefill_sequence_id);
    head_meta["exec_start"] = json!(head.exec_start);
    head_meta["exec_end"] = json!(head.exec_end);
    cancelable(
        cancellation,
        crate::relay::send_frame_or_tunnel(
            http,
            head,
            tunnel,
            "/v1/prefill",
            &head_meta,
            None,
            dtype,
            head.frame_encoding(),
            crate::relay::CHUNK_ATTEMPTS,
            &head.node_id,
            |_issue| {},
        ),
    )
    .await
    .map_err(|e| {
        if !is_cancelled(&e) {
            routes.mark_failure(&head.node_id, &head.inference_url);
        }
        ring_relay_error(&head.node_id, e)
    })
}

/// A cadeia respondeu, mas com a falha de um hop no lugar do resultado.
///
/// Vale mais que o prazo estourado: nomeia o nó que morreu — que quase nunca é
/// aquele para quem despachamos — e marca **esse** para nova medição de rota.
pub(super) fn direct_failure(
    plan: &PipelinePlan,
    routes: &crate::route::RouteTable,
    meta: &serde_json::Value,
) -> Option<anyhow::Error> {
    let failure = crate::direct::result_failure(meta)?;
    if let Some(step) = plan
        .steps
        .iter()
        .find(|s| s.node_id == failure.failed_node_id)
    {
        routes.mark_failure(&step.node_id, &step.inference_url);
    }
    Some(anyhow!(
        "pipeline falhou em {}: {} — {}",
        failure.failed_node_id,
        failure.kind,
        failure.message
    ))
}

/// Dobra a telemetria acumulada na ida para dentro do `RequestTrace`.
///
/// O modo direto recebe uma evidência por travessia em uma mensagem separada.
/// Sem essa lista, o tempo total não pode ser dividido com segurança entre os
/// hops e o trace deve permanecer incompleto em vez de inventar uma atribuição.
pub(super) fn record_direct_hops(trace: &mut RequestTrace, phase: Phase, meta: &serde_json::Value) {
    let mut hops: Vec<serde_json::Value> = meta
        .get("hop_timings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(traversals) = meta
        .get("transport_traversals")
        .and_then(|value| value.as_array())
    {
        if traversals.len() != hops.len() {
            trace.transport_trace_complete = false;
        }
        for traversal in traversals {
            let Some(destination) = traversal.get("hop_index").and_then(|value| value.as_u64())
            else {
                trace.transport_trace_complete = false;
                continue;
            };
            let Some(hop) = hops.get_mut(destination as usize) else {
                trace.transport_trace_complete = false;
                continue;
            };
            for field in [
                "round_trip_ms",
                "bytes_sent",
                "bytes_received",
                "client_encode_ms",
                "response_headers_ms",
                "response_body_ms",
                "client_decode_ms",
                "tls",
                "tls_handshake_ms",
                "transport",
                "transport_fallback",
                "transport_fallback_reason",
            ] {
                if let Some(value) = traversal.get(field) {
                    hop[field] = value.clone();
                }
            }
        }
    } else {
        trace.transport_trace_complete = false;
    }
    for (i, entry) in hops.iter().enumerate() {
        trace.record_hop_from_json(i, phase, entry);
    }
    if let Some(complete) = meta
        .get("transport_trace_complete")
        .and_then(|value| value.as_bool())
    {
        trace.transport_trace_complete &= complete;
    }
    trace.total_bytes_received += meta
        .get("result_bytes")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
}

/// One chunk to one peer, retried on transport failure.
///
/// Retrying is safe because the peer rewinds its KV cache to this chunk's
/// position before redoing it — a repeat can never be counted twice. Thin
/// wrapper over `relay::send_frame`, which also backs the ring-mode relay
/// calls a hop makes to its own next hop (`infer_server.rs`); this function
/// just supplies the `/v1/prefill` path, the retry count, and turns
/// `SendIssue`s into `TraceIssue`s on the gateway's `RequestTrace`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn send_chunk(
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    step: &crate::pipeline::PipelineStep,
    tunnel: Option<&TunnelOutboundSender>,
    meta: &serde_json::Value,
    tensor: Option<(&[f32], TensorShape)>,
    dtype: WireDtype,
    encoding: FrameEncoding,
    chunk_index: u32,
    trace: &mut RequestTrace,
    hop: usize,
    phase: Phase,
    cancellation: &Cancellation,
) -> Result<(serde_json::Value, Option<(Vec<f32>, TensorShape)>)> {
    check_cancelled(cancellation)?;
    let node_id = &step.node_id;
    let peer_label = format!("{node_id} (pedaço {chunk_index})");
    let mut issues = Vec::new();
    let result = cancelable(
        cancellation,
        crate::relay::send_frame_or_tunnel(
            http,
            step,
            tunnel,
            "/v1/prefill",
            meta,
            tensor,
            dtype,
            encoding,
            CHUNK_ATTEMPTS,
            &peer_label,
            |issue| issues.push(issue),
        ),
    )
    .await;

    for issue in issues {
        trace.record_issue(TraceIssue::new(
            Some(hop),
            node_id,
            phase,
            match issue.kind {
                crate::relay::SendIssueKind::Timeout => TraceIssueKind::Timeout,
                crate::relay::SendIssueKind::Transport => TraceIssueKind::Transport,
                crate::relay::SendIssueKind::Http => TraceIssueKind::Http,
                crate::relay::SendIssueKind::Protocol => TraceIssueKind::Protocol,
            },
            issue.attempt,
            issue.message,
        ));
    }

    let sent = result.map_err(|e| {
        // Esperar o ciclo de 5 minutos significaria falhar tudo que chegasse
        // nesse meio tempo. A proxima medicao reescolhe. Uma recuperação bem
        // sucedida via túnel nunca chega aqui — só o insucesso final marca a
        // rota como ruim.
        if !is_cancelled(&e) {
            routes.mark_failure(node_id, &step.inference_url);
        }
        e
    })?;
    trace.record_hop_from_sent(hop, phase, &sent);
    trace.record_transport(hop, phase, TransportTrace::from_sent(&sent));
    Ok((sent.meta, sent.tensor))
}
