use super::*;
use base64::Engine as _;

pub(super) async fn handle_prefill(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let _load_guard = app.loads.acquire(&app.load_node_id).await;
    // Ver a nota gêmea em `handle_step`: o cabeçalho basta para saber se o
    // quadro é um lote.
    if wire::peek_batch_header(&body).is_ok_and(|header| header.item_count > 1) {
        match wire::decode_batch(&body) {
            Ok(batch) => return handle_prefill_batch(app, headers, batch).await,
            Err(error) => return err400(format!("quadro inválido: {error}")),
        }
    }
    // A one-item frame is still a batch frame: the gateway's
    // `BatchCoordinator` deliberately sends it through the same path. TP has
    // one decode id and one collective barrier per request, so keep the
    // complete top-level request under the same gate as a multi-item batch.
    // Fan-out children already hold the gate in `handle_prefill_batch` and
    // mark themselves with `BATCH_CHILD_HEADER`, otherwise they would try to
    // acquire the same non-reentrant mutex again.
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_request_guard = if tp_active && headers.get(BATCH_CHILD_HEADER).is_none() {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let state = app.state.clone();
    let scheduler = app.scheduler.clone();
    let processing_started = Instant::now();
    open_tp_decode(&state, &headers, "/v1/prefill", &body);
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;
    let wire_dtype = match meta.get("dtype").and_then(|value| value.as_str()) {
        Some(value) => match WireDtype::parse(value) {
            Ok(dtype) => dtype,
            Err(error) => return err400(error),
        },
        None => return err400("quadro sem dtype de estado oculto"),
    };

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__session__")
        .to_string();
    let chunk_index = meta
        .get("chunk_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let prefill_sequence_id = meta
        .get("prefill_sequence_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let direct_forward_order = app.direct_forward_order.clone();
    let chunk_size = meta
        .get("chunk_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(512) as u32;
    let reuse_prefix = meta
        .get("reuse_prefix")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cached_tokens = meta
        .get("cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let prefix_match_tokens = meta
        .get("prefix_match_tokens")
        .or_else(|| {
            meta.get("prompt_progress")
                .and_then(|v| v.get("prefix_match_tokens"))
        })
        .and_then(|v| v.as_u64())
        .unwrap_or(cached_tokens as u64) as u32;
    let stable_len = meta
        .get("stable_len")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32);
    let start_pos = meta
        .get("start_pos")
        .and_then(|v| v.as_u64())
        .map(|value| value as u32);
    // Same next_hops-derives-is_last rule as handle_step — see its comment.
    let next_hops: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = next_hops
        .as_ref()
        .map(|hops| hops.is_empty())
        .unwrap_or_else(|| {
            meta.get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    // What *this* request says about chunking — correct as received for a
    // middle/tail hop (which just passes it through); wrong for the head,
    // which always gets `false` here (the sender can't know in advance) and
    // must override it with its own determination before relaying onward —
    // see the relay branch below and `owns_tokens` in `prefill_blocking`.
    let is_final_chunk = meta
        .get("is_final_chunk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let prompt = meta
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let images = match meta.get("images") {
        None => None,
        Some(value) => {
            let Some(values) = value.as_array() else {
                return err400("campo images inválido");
            };
            let mut decoded = Vec::with_capacity(values.len());
            for value in values {
                let Some(encoded) = value.as_str() else {
                    return err400("campo images contém um item inválido");
                };
                match base64::engine::general_purpose::STANDARD.decode(encoded) {
                    Ok(bytes) => decoded.push(bytes),
                    Err(error) => return err400(format!("imagem codificada inválida: {error}")),
                }
            }
            Some(decoded)
        }
    };
    let chat_template = meta.get("chat_template").and_then(|v| v.as_bool());
    // A composição dos tokens deste pedaço, gerada na cabeça (que tokenizou o
    // prompt) e ecoada até a cauda — é o que alimenta o `observe` da cabeça
    // `nextn` no `decode_hidden` (ver `prefill_blocking`). Ausente é peer antigo.
    let chunk_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Scale the hidden state once before moving into spawn_blocking.
    let input_prepare_started = Instant::now();
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };
    let inbound_hidden_state_bytes = match wire::tensor_payload_len(
        tensor
            .as_ref()
            .map(|(values, shape)| (values.as_slice(), shape.clone())),
        wire_dtype,
    ) {
        Ok(bytes) => bytes,
        Err(error) => return err400(error),
    };
    let input_prepare_us = input_prepare_started.elapsed().as_micros() as u64;
    // Neither of `prefill_blocking`'s non-final replies echoes `session_id`
    // back — a relay branch that forwards to the next hop needs its own
    // copy, taken before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    // A sub-faixa que o remetente mandou executar. Recusa alta quando este nó
    // não consegue rodar exatamente aquilo - nunca ajuste silencioso.
    let capture_block = {
        let (slice_start, slice_end) = {
            let st = match lock_inference_state(&state) {
                Ok(st) => st,
                Err(error) => return err500(error),
            };
            (st.start_layer, st.end_layer)
        };
        match resolve_exec_range(&meta, slice_start, slice_end) {
            Ok(block) => block,
            Err(motivo) => return err400(motivo),
        }
    };

    let queued_at = Instant::now();
    let result = match scheduler.submit_prefill(PrefillJob {
        sid_key,
        chunk_index,
        chunk_size,
        reuse_prefix,
        cached_tokens,
        prefix_match_tokens,
        start_pos,
        stable_len,
        is_last,
        is_final_chunk,
        wire_dtype,
        prompt,
        images,
        chat_template,
        tensor,
        capture_block,
        chunk_tokens,
        queued_at,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("fila de inferência cheia (limite {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("scheduler de inferência encerrado");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida")
                || e.to_string().contains("fora de ordem")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (code, e.to_string()).into_response();
        }
        Err(e) => return err500(e),
    };
    reply.timings.frame_decode_us = frame_decode_us;
    reply.timings.input_prepare_us = input_prepare_us;

    // Same reasoning as handle_step: decode once so the relay branch below
    // can inspect what came back, patch in this hop's own processing_ms/
    // node_timings either way, and only then decide whether to relay or
    // just re-encode and return.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor) = match wire::decode(&reply.bytes) {
        Ok(v) => v,
        Err(e) => return err500(e),
    };
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta — ver o bloco gêmeo em `handle_step`. A diferença é o
    // `prompt_progress`: no anel borbulhado a cabeça sobrescrevia
    // `more_chunks`/`n_prompt_tokens` na resposta que passava de volta por ela
    // (`overlay_head_authoritative_fields`). Sem volta, ela manda os dois
    // **adiante**, e quem entrega copia — só a cabeça tokenizou o prompt, e sem
    // isso todo prompt de mais de um pedaço seria truncado no primeiro.
    if let Some(reply_to) = direct_reply_to(&meta) {
        let outbound_hidden_state_bytes = match wire::tensor_payload_len(
            local_tensor
                .as_ref()
                .map(|(values, shape)| (values.as_slice(), shape.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => bytes,
            Err(error) => return err500(error),
        };
        let hop_timings = crate::direct::accumulate(
            &meta,
            crate::direct::hop_entry_with_state(
                &local_meta["processing_ms"],
                &local_meta["node_timings"],
                inbound_bytes,
                inbound_hidden_state_bytes,
                outbound_hidden_state_bytes,
                wire_dtype,
            ),
        );
        let hops = next_hops.unwrap_or_default();

        if hops.is_empty() {
            let mut result = local_meta;
            apply_prompt_progress(&meta, &mut result);
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            let failure = crate::relay::RelayFailure {
                failed_node_id: hops[0].node_id.clone(),
                kind: "protocol".into(),
                message: "hop intermediário não produziu estado oculto".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = json!({
            "session_id": sid_for_forward,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": reuse_prefix_for_forward(&local_meta, reuse_prefix, chunk_index),
            "is_final_chunk": is_final_chunk_for_forward(&local_meta, is_final_chunk),
            "prompt_progress": prompt_progress_for_forward(&meta, &local_meta),
            "hop_timings": hop_timings,
        });
        if let Some(sequence_id) = &prefill_sequence_id {
            forward["prefill_sequence_id"] = json!(sequence_id);
        }
        for field in ["start_pos", "next_pos", "stable_len"] {
            if let Some(value) = local_meta.get(field) {
                forward[field] = value.clone();
            }
        }
        preserve_batch_identity(&meta, &mut forward);
        if let Some(tokens) = local_meta.get("tokens") {
            forward["tokens"] = tokens.clone();
        }
        if chunk_index == 0 {
            if let Some(sequence_id) = &prefill_sequence_id {
                if let Err(error) = direct_forward_order.ensure_sequence(sequence_id) {
                    let failure = crate::relay::RelayFailure {
                        failed_node_id: hops[0].node_id.clone(),
                        kind: "protocol".into(),
                        message: error.to_string(),
                    };
                    tokio::spawn(async move {
                        crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
                    });
                    return direct_ack();
                }
            }
        }
        tokio::spawn(forward_direct_ordered(
            hops,
            "/v1/prefill",
            forward,
            residual,
            shape,
            wire_dtype,
            crate::relay::CHUNK_ATTEMPTS, // uma resposta perdida exige rebuild frio
            reply_to,
            Some(direct_forward_order),
            prefill_sequence_id,
            Some(chunk_index),
        ));
        let progress =
            owns_tokens(&local_meta).then(|| prompt_progress_for_forward(&meta, &local_meta));
        return direct_ack_with_progress(progress);
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode_batch_single(
            &local_meta,
            local_tensor
                .as_ref()
                .map(|(v, s)| (v.as_slice(), s.clone())),
            wire_dtype,
        ) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    // Relay branch: this hop is not the tail, and the sender wants the rest
    // of the chain reached directly instead of relaying back through it.
    let hops = next_hops.expect("should_relay implies next_hops is Some");
    let (residual, shape) = local_tensor.expect("should_relay implies local_tensor is Some");
    let is_final_chunk_for_forward = is_final_chunk_for_forward(&local_meta, is_final_chunk);

    let mut bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/prefill",
        {
            let mut relay_meta = json!({
                "session_id": sid_for_forward,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "reuse_prefix": reuse_prefix_for_forward(&local_meta, reuse_prefix, chunk_index),
                "is_final_chunk": is_final_chunk_for_forward,
                // O próximo hop recebe a própria sub-faixa, pelo mesmo motivo
                // que este recebeu a sua: quem manda é que decide o pedaço.
                "exec_start": hops[0].exec_start,
                "exec_end": hops[0].exec_end,
            });
            if let Some(tokens) = local_meta.get("tokens") {
                relay_meta["tokens"] = tokens.clone();
            }
            for field in ["start_pos", "next_pos", "stable_len"] {
                if let Some(value) = local_meta.get(field) {
                    relay_meta[field] = value.clone();
                }
            }
            if let Some(value) = local_meta.get("prefix_match_tokens") {
                relay_meta["prefix_match_tokens"] = value.clone();
            }
            preserve_batch_identity(&meta, &mut relay_meta);
            relay_meta
        },
        (&residual, shape),
        wire_dtype,
        crate::relay::CHUNK_ATTEMPTS, // uma resposta perdida exige rebuild frio
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };

    overlay_head_authoritative_fields(&local_meta, &mut bubbled);

    match wire::encode_batch_single(&bubbled, None, wire_dtype) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

pub(super) fn owns_tokens(local_meta: &Value) -> bool {
    local_meta
        .get("owns_tokens")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Only the head (`owns_tokens`) can know whether this chunk was the last
/// one — everyone else just passes through whatever `is_final_chunk` it was
/// told, since it has no `prompt_tokens` of its own to check that against.
/// Qual decisão de prefixo o hop seguinte deve obedecer.
///
/// A do peer que tokenizou, quando ele a informou (`reuse_prefix` no próprio
/// meta), não a que o gateway pediu. A cabeça pode ter recomeçado a sessão -
/// prefixo divergente, sessão despejada - e um hop reusando enquanto a cabeça
/// recomeçou deixaria as posições do pipeline desalinhadas. Peer que não
/// informa nada cai no pedido original, que é o comportamento anterior.
pub(crate) fn reuse_prefix_for_forward(
    local_meta: &Value,
    requested: bool,
    chunk_index: u32,
) -> bool {
    if chunk_index != 0 {
        return false;
    }
    local_meta
        .get("reuse_prefix")
        .and_then(|value| value.as_bool())
        .unwrap_or(requested)
}

pub(super) fn is_final_chunk_for_forward(local_meta: &Value, received_is_final_chunk: bool) -> bool {
    if owns_tokens(local_meta) {
        !local_meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    } else {
        received_is_final_chunk
    }
}

/// The verified fix: only the head's own `more_chunks`/`n_prompt_tokens` are
/// authoritative. A bubbled ring response's `more_chunks`/`n_prompt_tokens`
/// came from the *tail* (always `false`/absent there, since it never owns
/// the prompt) unless overwritten — leaving it as-is would silently
/// truncate every multi-chunk prompt on a ring pipeline after one chunk.
pub(crate) fn overlay_head_authoritative_fields(local_meta: &Value, bubbled: &mut Value) {
    if owns_tokens(local_meta) {
        for field in [
            "more_chunks",
            "n_prompt_tokens",
            "start_pos",
            "next_pos",
            // Quantos tokens do prompt não foram reprocessados, e se o prefixo
            // foi de fato aproveitado. Só a cabeça sabe - a cauda responde
            // sobre um prompt que ela nunca tokenizou.
            "reuse_prefix",
            "cached_tokens",
            "prefix_match_tokens",
            "stable_len",
        ] {
            if let Some(value) = local_meta.get(field) {
                bubbled[field] = value.clone();
            }
        }
    }
}

/// Os mesmos dois campos autoritativos, empurrados **adiante** em vez de
/// sobrepostos na volta — é o que a resposta direta precisa, já que a cabeça
/// nunca mais vê o resultado. Quem não é a cabeça repassa intacto o que
/// recebeu.
pub(crate) fn prompt_progress_for_forward(inbound_meta: &Value, local_meta: &Value) -> Value {
    if owns_tokens(local_meta) {
        return json!({
            "more_chunks": local_meta["more_chunks"].clone(),
            "n_prompt_tokens": local_meta["n_prompt_tokens"].clone(),
            "start_pos": local_meta["start_pos"].clone(),
            "next_pos": local_meta["next_pos"].clone(),
            // Ver `overlay_head_authoritative_fields`: só a cabeça sabe.
            "reuse_prefix": local_meta["reuse_prefix"].clone(),
            "cached_tokens": local_meta["cached_tokens"].clone(),
            "prefix_match_tokens": local_meta["prefix_match_tokens"].clone(),
            "stable_len": local_meta["stable_len"].clone(),
        });
    }
    inbound_meta
        .get("prompt_progress")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A ponta da cadeia copia para o resultado o que a cabeça apurou do prompt.
/// Sem isto o gateway leria o `more_chunks: false` que a cauda sempre responde
/// — ela não tokenizou nada — e pararia o prefill no primeiro pedaço.
pub(crate) fn apply_prompt_progress(inbound_meta: &Value, result: &mut Value) {
    let Some(progress) = inbound_meta.get("prompt_progress") else {
        return;
    };
    for field in [
        "more_chunks",
        "n_prompt_tokens",
        "start_pos",
        "next_pos",
        "reuse_prefix",
        "cached_tokens",
        "prefix_match_tokens",
        "stable_len",
    ] {
        if let Some(value) = progress.get(field) {
            if !value.is_null() {
                result[field] = value.clone();
            }
        }
    }
}

/// Preserve the identity assigned by `BatchCoordinator` when a one-item
/// frame takes the regular handler path. `encode_batch_single` cannot infer
/// that identity from the scheduler reply, so dropping these fields makes the
/// coordinator unable to match the response to its waiting request.
pub(crate) fn preserve_batch_identity(request_meta: &Value, response_meta: &mut Value) {
    for field in ["item_id", "batch_id"] {
        if let Some(value) = request_meta.get(field) {
            response_meta[field] = value.clone();
        }
    }
}

/// Recusa um pedaço de prefill maior que o `n_batch` do contexto **antes** de
/// chamar `llama_decode`, que dispara `GGML_ASSERT(n_tokens_all <=
/// cparams.n_batch)` e aborta o processo inteiro. O gateway já clampa o tamanho
/// do pedaço em `prompt_chunk_tokens`; esta guarda cobre um head cujo `n_batch`
/// é menor que o do nó que planejou, e o item isolado que o scheduler deixa
/// passar. O erro nomeia os knobs a ajustar.
pub(super) fn guard_chunk_fits_batch(engine: &LlmEngine, n_tokens: usize, chunk_index: u32) -> Result<()> {
    let n_batch = engine.n_batch();
    if n_tokens as u64 > n_batch as u64 {
        bail!(
            "pedaço {chunk_index} tem {n_tokens} tokens, acima do n_batch={n_batch} deste nó; \
             o llama.cpp abortaria. Reduza prefill_chunk_tokens para <= n_batch, ou aumente \
             n_batch, nos ajustes avançados da implantação"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prefill_blocking(
    state: SharedState,
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    reuse_prefix: bool,
    cached_tokens: u32,
    prefix_match_tokens: u32,
    start_pos: Option<u32>,
    stable_len: Option<u32>,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    images: Option<Vec<Vec<u8>>>,
    chat_template: Option<bool>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    capture_block: Option<u32>,
    // A composição dos tokens deste pedaço, vinda da cabeça via `tokens` na
    // wire. Só tem sentido no branch de estado oculto (a cabeça tokeniza o
    // prompt; hops do meio ecoam): alimenta o `observe` da cabeça `nextn`
    // dentro do `decode_hidden`. `None` é peer antigo que não encaminha o
    // campo — o `decode_hidden` desliga o MTP com aviso.
    chunk_tokens: Option<Vec<i32>>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut is_final_chunk = is_final_chunk;
    let owns_prompt_tokens = prompt.is_some();
    let has_images = images.as_ref().is_some_and(|images| !images.is_empty());
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    // O prefill espera no mesmo mutex do decode: um prompt grande de outra
    // sessão segura este aqui, e é a mesma espera invisível.
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    timings.selected_path = s.inference_path;

    // ── Session setup ────────────────────────────────────────────────────────

    let setup_started = Instant::now();
    // Reusar exige a sessão viva aqui. Ela pode ter sido despejada ou reciclada
    // desde o turno anterior; nesse caso este peer simplesmente recomeça, e é a
    // resposta dele (`reuse_prefix` no meta) que os hops seguintes obedecem.
    let want_reuse = reuse_prefix
        && chunk_index == 0
        && s.sessions
            .get(&sid_key)
            .is_some_and(|session| session.retained);
    s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
    let mut prefix = PrefixDecision::default();
    // Quem não tokeniza recebe a fronteira de quem tokenizou, para partir o
    // prompt na mesma posição - ver `Session::stable_len`.
    if let Some(stable) = stable_len {
        s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
    }

    // First peer: tokenize the prompt on chunk 0.
    // We must not hold a &mut Session borrow while calling engine methods, so
    // compute everything we need, store results, then re-borrow.
    if let Some(ref text) = prompt {
        if chunk_index == 0 && !has_images {
            let use_tmpl = use_chat_template(chat_template, s.chat_template);
            let no_think = s.no_thinking;
            let formatted = if use_tmpl {
                s.engine
                    .apply_chat_template(&[("user", text)], true, no_think)?
            } else {
                text.clone()
            };
            let add_bos = !use_tmpl;
            let tokens = s.engine.tokenize(&formatted, add_bos, true);
            let stable = stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
            prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
        }
    }
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    // ── Rollback / order check ───────────────────────────────────────────────
    let expected_pos = expected_prefill_pos(start_pos, chunk_index, s.sessions[&sid_key].n_past)?;
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if session.n_past > expected_pos {
            warn!(
                "refazendo chunk {chunk_index} da sessão {}… (pos {expected_pos})",
                &sid_key[..8.min(sid_key.len())]
            );
            let seq_id = session.seq_id();
            if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                bail!(
                    "este modelo não aceita descartar posições do meio do cache, \
                     então refazer o pedaço {chunk_index} corromperia a sessão"
                );
            }
            s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
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
    // A partir da fronteira, o prompt deixa de ser reutilizável e passa a viver
    // na cópia de geração junto com os tokens gerados - ver `Session::stable_len`.
    // Um pedaço nunca atravessa a fronteira: ele para nela, e o seguinte começa
    // já na outra sequência.
    let stable = s.sessions[&sid_key].stable_len;
    if expected_pos >= stable && !has_images {
        s.open_generation(&sid_key)?;
    }
    let (n_past, seq_id, tokens_for_chunk) = {
        let session = &s.sessions[&sid_key];
        let toks = if tensor.is_none() && !has_images {
            let start = expected_pos as usize;
            let mut end = (expected_pos + chunk_size) as usize;
            if expected_pos < stable {
                end = end.min(stable as usize);
            }
            session
                .prompt_tokens
                .get(start..end.min(session.prompt_tokens.len()))
                .unwrap_or(&[])
                .to_vec()
        } else {
            vec![]
        };
        (session.n_past, session.seq_id(), toks)
    };

    // Onde parar, quando o remetente pediu menos que a fatia inteira. Antes do
    // decode e sob o mesmo lock, que é o que torna o ajuste válido só para
    // esta decodificação.
    if let Err(e) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            bail!("{e}");
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

    let n_tokens;
    let mut multimodal_n_past = None;
    let decode_started = Instant::now();
    let engine_timing;
    if let Some((hidden, shape)) = tensor {
        n_tokens = shape[0];
        guard_chunk_fits_batch(&s.engine, n_tokens, chunk_index)?;
        engine_timing = s.engine.decode_hidden(
            &hidden,
            &shape,
            n_tokens,
            n_past,
            seq_id,
            !is_last,
            chunk_tokens.as_deref(),
        )?;
    } else if has_images {
        if chunk_index != 0 || tensor.is_some() {
            bail!("imagens só podem acompanhar o primeiro chunk de prefill");
        }
        let text = prompt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("imagem sem prompt"))?;
        let use_tmpl = use_chat_template(chat_template, s.chat_template);
        let formatted = if use_tmpl {
            s.engine
                .apply_chat_template(&[("user", text)], true, s.no_thinking)?
        } else {
            text.to_string()
        };
        let (decoded_tokens, decoded_n_past, timing) = s.engine.decode_multimodal(
            &formatted,
            images.as_deref().unwrap_or_default(),
            n_past,
            seq_id,
            !is_last,
        )?;
        n_tokens = decoded_tokens;
        multimodal_n_past = Some(decoded_n_past);
        engine_timing = timing;
    } else {
        if tokens_for_chunk.is_empty() {
            bail!("chunk {chunk_index} não tem tokens");
        }
        n_tokens = tokens_for_chunk.len();
        guard_chunk_fits_batch(&s.engine, n_tokens, chunk_index)?;
        engine_timing = s
            .engine
            .decode_tokens(&tokens_for_chunk, n_past, seq_id, !is_last)?;
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
    timings.tp = crate::tp::take_decode_telemetry();
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    // ── Session update ───────────────────────────────────────────────────────
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if let Some(decoded_n_past) = multimodal_n_past {
            session.n_past = decoded_n_past;
            session.prompt_tokens = vec![0; n_tokens];
            session.stable_len = decoded_n_past;
        } else {
            session.n_past += n_tokens as u32;
        }
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
    // A fronteira entre prompt e geração, anotada por cada peer no pedaço em
    // que o prompt acaba. É para cá que o fim do turno devolve a sessão, e é o
    // que o `/v1/session/rewind` confere. Hops do meio não tokenizam nada, e
    // por isso recebem `is_final_chunk` da cabeça em vez de deduzi-lo.
    if is_final_chunk {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        session.prompt_len = n_past_now.min(session.stable_len);
    }

    // ── Sampling / residual ──────────────────────────────────────────────────
    let sampling = s.sampling.clone();

    if is_last {
        if !is_final_chunk {
            let encode_started = Instant::now();
            let mut partial =
                partial_prefill_meta(more, total_tokens as usize, expected_pos, n_past_now);
            apply_prefix_decision(
                &mut partial,
                chunk_index,
                prefix,
                reuse_prefix,
                cached_tokens,
                prefix_match_tokens,
                owns_prompt_tokens,
            );
            apply_stable_len(&mut partial, stable);
            let bytes = wire::encode(&partial, None, wire_dtype)?;
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
        // geração é o rank 0. Ver `tp::is_replica`.
        //
        // Encerrar aqui destruía a sessão antes de o rewind do gateway chegar,
        // e o rewind respondia "sessão desconhecida" - o prefixo nunca entrava
        // no cache. O fim do turno agora **retém** o prompt: some só a
        // sequência da geração. Liberar de vez continua sendo ato explícito do
        // gateway (`/v1/session/release`).
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
        let encode_started = Instant::now();
        let n_prompt_tokens = s
            .sessions
            .get(&sid_key)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid_key}' desapareceu antes da resposta"))?
            .prompt_tokens
            .len();
        let mut reply = terminal_prefill_meta(
            n_prompt_tokens,
            expected_pos,
            expected_pos + n_tokens as u32,
            done,
        );
        apply_prefix_decision(
            &mut reply,
            chunk_index,
            prefix,
            reuse_prefix,
            cached_tokens,
            prefix_match_tokens,
            owns_prompt_tokens,
        );
        apply_stable_len(&mut reply, stable);
        reply["token_id"] = json!(next_id);
        reply["text"] = json!(text);
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
    let n_prompt_tokens = s.sessions[&sid_key].prompt_tokens.len();
    let mut reply_meta = json!({
        "done": false,
        "more_chunks": more,
        "n_prompt_tokens": n_prompt_tokens,
        "start_pos": expected_pos,
        "next_pos": n_past_now,
        // Only meaningful when true: whether *this* peer is the one that
        // tokenized the prompt, i.e. whether `more_chunks`/`n_prompt_tokens`
        // above are its own authoritative determination rather than the
        // `false`/`0` placeholder every other peer replies with. Ring
        // mode's relay-and-bubble uses this to know which hop's reply is
        // allowed to overwrite what the tail bubbles back — see
        // `handle_prefill`.
        "owns_tokens": owns_tokens,
    });
    apply_prefix_decision(
        &mut reply_meta,
        chunk_index,
        prefix,
        reuse_prefix,
        cached_tokens,
        prefix_match_tokens,
        owns_prompt_tokens,
    );
    apply_stable_len(&mut reply_meta, stable);
    // A composição real deste pedaço, para a cauda manter a cabeça `nextn` em
    // dia (ver o doc de `chunk_tokens`). A cabeça conhece os tokens; hops do
    // meio ecoam o que receberam.
    if owns_tokens {
        reply_meta["tokens"] = json!(tokens_for_chunk);
    } else if let Some(tokens) = &chunk_tokens {
        reply_meta["tokens"] = json!(tokens);
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

pub(super) struct PreparedPrefill {
    pub(super) sid_key: String,
    pub(super) seq_id: u32,
    pub(super) n_past: u32,
    pub(super) n_tokens: usize,
    pub(super) start_pos: u32,
    pub(super) chunk_index: u32,
    /// O que este peer executou com o prefixo retido - ver `apply_prefix_decision`.
    pub(super) prefix: PrefixDecision,
    /// Decisão recebida da cabeça para ecoar quando este peer não tokeniza.
    pub(super) requested_reuse: bool,
    pub(super) requested_cached_tokens: u32,
    pub(super) requested_prefix_match_tokens: u32,
    /// A fronteira medida - ver `Session::stable_len`.
    pub(super) stable_len: u32,
    pub(super) is_last: bool,
    pub(super) is_final_chunk: bool,
    pub(super) owns_tokens: bool,
    pub(super) tokens_for_chunk: Vec<i32>,
    pub(super) chunk_tokens: Option<Vec<i32>>,
    pub(super) input: PreparedPrefillInput,
    pub(super) wire_dtype: WireDtype,
    pub(super) capture_block: Option<u32>,
    pub(super) queued_at: Instant,
}

pub(super) enum PreparedPrefillInput {
    Tokens(Vec<i32>),
    Hidden {
        values: Vec<f32>,
        shape: TensorShape,
    },
}

pub(super) fn prefill_batch_blocking(
    state: SharedState,
    jobs: Vec<(PrefillJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => {
            let message = error.to_string();
            return jobs
                .into_iter()
                .map(|(_, reply)| (reply, Err(anyhow::anyhow!(message.clone()))))
                .collect();
        }
    };
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let selected_path = s.inference_path;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let PrefillJob {
            sid_key,
            chunk_index,
            chunk_size,
            reuse_prefix,
            cached_tokens,
            prefix_match_tokens,
            start_pos,
            stable_len,
            is_last,
            is_final_chunk,
            wire_dtype,
            prompt,
            images: _images,
            chat_template,
            tensor,
            capture_block,
            chunk_tokens,
            queued_at,
        } = job;

        let result = (|| -> Result<PreparedPrefill> {
            // Mesmo contrato do caminho não-lotado (`prefill_blocking`): reuso
            // só com a sessão viva, e quem tokeniza decide se o prefixo serve.
            let want_reuse = reuse_prefix
                && chunk_index == 0
                && s.sessions
                    .get(&sid_key)
                    .is_some_and(|session| session.retained);
            s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
            let mut prefix = PrefixDecision::default();
            // Ver `prefill_blocking`: quem não tokeniza recebe a fronteira.
            if let Some(stable) = stable_len {
                s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
            }
            if let Some(text) = prompt.as_deref() {
                if chunk_index == 0 {
                    let use_tmpl = use_chat_template(chat_template, s.chat_template);
                    let no_think = s.no_thinking;
                    let formatted = if use_tmpl {
                        s.engine
                            .apply_chat_template(&[("user", text)], true, no_think)?
                    } else {
                        text.to_string()
                    };
                    let tokens = s.engine.tokenize(&formatted, !use_tmpl, true);
                    let stable =
                        stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
                    prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
                }
            }

            let expected_pos =
                expected_prefill_pos(start_pos, chunk_index, s.sessions[&sid_key].n_past)?;
            {
                let session = s.sessions.get(&sid_key).unwrap();
                if session.n_past > expected_pos {
                    let seq_id = session.seq_id();
                    if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                        bail!(
                            "este modelo não aceita descartar posições do meio do cache, \
                             então refazer o pedaço {chunk_index} corromperia a sessão"
                        );
                    }
                    s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
                } else if session.n_past < expected_pos {
                    bail!(
                        "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                        session.n_past
                    );
                }
            }

            // Ver `prefill_blocking`: o pedaço para na fronteira, e daí em
            // diante decodifica na cópia de geração.
            let stable = s.sessions[&sid_key].stable_len;
            if expected_pos >= stable {
                s.open_generation(&sid_key)?;
            }
            let (seq_id, n_past, prompt_tokens, input, n_tokens) = {
                let session = &s.sessions[&sid_key];
                if let Some((hidden, shape)) = tensor {
                    let n_tokens = shape.first().copied().unwrap_or(0);
                    crate::llm::validate_hidden_tensor(
                        hidden.len(),
                        &shape,
                        n_tokens,
                        s.engine.n_embd,
                        s.engine.n_embd_inp,
                        s.engine.n_embd_out,
                        &format!("hidden state de {sid_key}"),
                    )?;
                    (
                        session.seq_id(),
                        session.n_past,
                        Vec::new(),
                        PreparedPrefillInput::Hidden {
                            values: hidden,
                            shape,
                        },
                        n_tokens,
                    )
                } else {
                    let start = expected_pos as usize;
                    let mut end = start.saturating_add(chunk_size as usize);
                    if expected_pos < stable {
                        end = end.min(stable as usize);
                    }
                    let tokens = session
                        .prompt_tokens
                        .get(start..end.min(session.prompt_tokens.len()))
                        .unwrap_or(&[])
                        .to_vec();
                    if tokens.is_empty() {
                        bail!("chunk {chunk_index} não tem tokens");
                    }
                    let n_tokens = tokens.len();
                    (
                        session.seq_id(),
                        session.n_past,
                        tokens.clone(),
                        PreparedPrefillInput::Tokens(tokens),
                        n_tokens,
                    )
                }
            };
            let owns_tokens =
                !prompt_tokens.is_empty() || !s.sessions[&sid_key].prompt_tokens.is_empty();

            Ok(PreparedPrefill {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                start_pos: expected_pos,
                chunk_index,
                prefix,
                requested_reuse: reuse_prefix,
                requested_cached_tokens: cached_tokens,
                requested_prefix_match_tokens: prefix_match_tokens,
                stable_len: stable,
                is_last,
                is_final_chunk,
                owns_tokens,
                tokens_for_chunk: prompt_tokens,
                chunk_tokens,
                input,
                wire_dtype,
                capture_block,
                queued_at,
            })
        })();

        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }

    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    // Onde parar vale para o grafo inteiro, não por item: um lote com pedidos
    // de sub-faixas diferentes não tem uma resposta certa. O agrupamento em
    // lote é conveniência de desempenho; ele não pode decidir isto por conta.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "lote com sub-faixas diferentes: cada uma pararia num bloco, e o grafo é um só"
                )),
            ));
        }
        return replies;
    }
    if let Err(e) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!("{e}"))));
            }
            return replies;
        }
    }

    let mut transform_error = None;
    let mut moe_input_items: HashMap<String, MoeTimings> = HashMap::new();
    for (_, item) in &mut prepared {
        if let PreparedPrefillInput::Hidden { values, .. } = &mut item.input {
            let mut transform_timings = NodeTimings::default();
            match s.apply_moe_hidden(
                std::mem::take(values),
                item.n_tokens,
                capture_block,
                &mut transform_timings,
            ) {
                Ok(transformed) => *values = transformed,
                Err(error) => {
                    transform_error = Some(error);
                    break;
                }
            }
            if !transform_timings.moe.executors.is_empty() {
                moe_input_items.insert(item.sid_key.clone(), transform_timings.moe);
            }
        }
    }
    if let Some(error) = transform_error {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }

    // Um `llama_decode` com mais tokens que `n_batch` aborta o processo. O
    // scheduler já corta o lote em `n_batch`, mas deixa passar um item isolado
    // maior que isso; recusa aqui é um pedido perdido, não o nó inteiro.
    let batch_tokens: usize = prepared.iter().map(|(_, item)| item.n_tokens).sum();
    if let Err(error) = guard_chunk_fits_batch(&s.engine, batch_tokens, prepared[0].1.chunk_index) {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }

    let decode_result = match &prepared[0].1.input {
        PreparedPrefillInput::Tokens(_) => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedPrefillInput::Tokens(tokens) => tokens.clone(),
                        PreparedPrefillInput::Hidden { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedPrefillInput::Hidden { .. } => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedPrefillInput::Hidden { values, .. } => values.clone(),
                        PreparedPrefillInput::Tokens(_) => Vec::new(),
                    },
                    shape: match &item.input {
                        PreparedPrefillInput::Hidden { shape, .. } => shape.clone(),
                        PreparedPrefillInput::Tokens(_) => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };

    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let session = s.sessions.get_mut(&item.sid_key).unwrap();
            session.n_past = session.n_past.saturating_add(item.n_tokens as u32);
            session.last_used = Instant::now();
            let total_tokens = session.prompt_tokens.len() as u32;
            let n_past_now = session.n_past;
            let more = item.owns_tokens && n_past_now < total_tokens;
            let mut is_final_chunk = item.is_final_chunk;
            if item.owns_tokens && !more {
                is_final_chunk = true;
            }
            // Ver `prefill_blocking`: a fronteira prompt/geração desta sessão.
            if is_final_chunk {
                let session = s.sessions.get_mut(&item.sid_key).unwrap();
                session.prompt_len = n_past_now.min(session.stable_len);
            }

            let mut timings = NodeTimings {
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };
            timings.merge_engine_moe(&engine_timings);
            if let Some(moe) = moe_input_items.remove(&item.sid_key) {
                timings.merge_moe(&moe);
                timings.record_engine(crate::executor::ExecutionEngine::Moe);
            }
            timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);

            if item.is_last {
                if !is_final_chunk {
                    let started = Instant::now();
                    let mut meta = partial_prefill_meta(
                        more,
                        total_tokens as usize,
                        item.start_pos,
                        item.start_pos + item.n_tokens as u32,
                    );
                    apply_prefix_decision(
                        &mut meta,
                        item.chunk_index,
                        item.prefix,
                        item.requested_reuse,
                        item.requested_cached_tokens,
                        item.requested_prefix_match_tokens,
                        item.owns_tokens,
                    );
                    apply_stable_len(&mut meta, item.stable_len);
                    let bytes = wire::encode(&meta, None, item.wire_dtype)?;
                    timings.encode_us = started.elapsed().as_micros() as u64;
                    return Ok(TimedReply { bytes, timings });
                }

                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let piece = s.engine.token_to_piece_bytes(next_id, false);
                let mut text = s
                    .sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .feed_token_piece(&piece);
                let done = next_id == s.engine.eos_token();
                if done {
                    finish_terminal_stream(&mut s.sessions, &item.sid_key, &mut text)?;
                }
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                // Ver `prefill_blocking`: o fim do turno retém o prompt em vez
                // de destruir a sessão.
                if done && !crate::tp::is_replica() {
                    s.retain_prompt(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us;
                let token_text_started = Instant::now();
                let mut meta = json!({
                    "done": done,
                    "token_id": next_id,
                    "text": text,
                    "more_chunks": false,
                    "n_prompt_tokens": total_tokens,
                    "start_pos": item.start_pos,
                    "next_pos": item.start_pos + item.n_tokens as u32,
                });
                apply_prefix_decision(
                    &mut meta,
                    item.chunk_index,
                    item.prefix,
                    item.requested_reuse,
                    item.requested_cached_tokens,
                    item.requested_prefix_match_tokens,
                    item.owns_tokens,
                );
                apply_stable_len(&mut meta, item.stable_len);
                let bytes = wire::encode(&meta, None, item.wire_dtype)?;
                timings.token_text_us = token_text_started.elapsed().as_micros() as u64;
                timings.encode_us = 0;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let input_was_hidden = matches!(&item.input, PreparedPrefillInput::Hidden { .. });
            let (mut residual, residual_shape) =
                output.residual_for(&item.sid_key, s.engine.n_embd)?;
            if !input_was_hidden {
                residual =
                    s.apply_moe_hidden(residual, item.n_tokens, item.capture_block, &mut timings)?;
            }
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let mut meta = json!({
                "done": false,
                "more_chunks": more,
                "n_prompt_tokens": total_tokens,
                "start_pos": item.start_pos,
                "next_pos": n_past_now,
                "owns_tokens": item.owns_tokens,
            });
            apply_prefix_decision(
                &mut meta,
                item.chunk_index,
                item.prefix,
                item.requested_reuse,
                item.requested_cached_tokens,
                item.requested_prefix_match_tokens,
                item.owns_tokens,
            );
            apply_stable_len(&mut meta, item.stable_len);
            if item.owns_tokens {
                meta["tokens"] = json!(item.tokens_for_chunk);
            } else if let Some(tokens) = &item.chunk_tokens {
                meta["tokens"] = json!(tokens);
            }
            let started = Instant::now();
            let bytes = wire::encode(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.encode_us = started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

pub(super) fn step_batch_blocking(
    state: SharedState,
    jobs: Vec<(StepJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    if jobs.iter().any(|(job, _)| {
        job.remote_stream
            || job.speculate.is_some()
            || job.incoming_draft.is_some()
            || job.mtp_k.is_some()
    }) {
        return jobs
            .into_iter()
            .map(|(job, reply)| {
                let result = step_blocking(
                    state.clone(),
                    job.sid_key,
                    job.is_last,
                    job.remote_stream,
                    job.wire_dtype,
                    job.token_id,
                    job.tensor,
                    job.capture_block,
                    job.speculate,
                    job.incoming_draft,
                    job.batch_tokens,
                    job.mtp_k,
                    job.queued_at,
                );
                (reply, result)
            })
            .collect();
    }

    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => {
            let message = error.to_string();
            return jobs
                .into_iter()
                .map(|(_, reply)| (reply, Err(anyhow::anyhow!(message.clone()))))
                .collect();
        }
    };
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let selected_path = s.inference_path;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let StepJob {
            sid_key,
            is_last,
            wire_dtype,
            token_id,
            tensor,
            capture_block,
            queued_at,
            ..
        } = job;
        let result = (|| -> Result<PreparedStep> {
            // Tudo daqui em diante é token gerado, então nada mais toca a
            // sequência do prompt - ver `InferState::open_generation`.
            s.open_generation(&sid_key)?;
            let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
                anyhow::anyhow!("sessão '{}' desconhecida - faça o prefill antes", sid_key)
            })?;
            session.last_used = Instant::now();
            let seq_id = session.seq_id();
            let n_past = session.n_past;
            let input = if let Some((hidden, shape)) = tensor {
                let n_tokens = shape.first().copied().unwrap_or(0);
                crate::llm::validate_hidden_tensor(
                    hidden.len(),
                    &shape,
                    n_tokens,
                    s.engine.n_embd,
                    s.engine.n_embd_inp,
                    s.engine.n_embd_out,
                    &format!("hidden state de {sid_key}"),
                )?;
                PreparedStepInput::Hidden {
                    values: hidden,
                    shape,
                }
            } else {
                let token =
                    token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
                s.sessions
                    .get_mut(&sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(token);
                PreparedStepInput::Tokens { token }
            };
            let n_tokens = match &input {
                PreparedStepInput::Tokens { .. } => 1,
                PreparedStepInput::Hidden { shape, .. } => shape[0],
            };
            Ok(PreparedStep {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                is_last,
                wire_dtype,
                capture_block,
                input,
                queued_at,
            })
        })();
        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }
    // Um único grafo não consegue parar em dois blocos diferentes. Recusar o
    // lote é seguro; escolher um dos limites faria uma sessão executar mais
    // ou menos camadas que o remetente pediu.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "lote com sub-faixas diferentes: cada uma pararia num bloco, e o grafo é um só"
                )),
            ));
        }
        return replies;
    }
    if let Err(error) = s.engine.set_capture_layer(capture_block) {
        if capture_block.is_some() {
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(error.to_string()))));
            }
            return replies;
        }
    }
    let mut transform_error = None;
    let mut moe_input_items: HashMap<String, MoeTimings> = HashMap::new();
    for (_, item) in &mut prepared {
        if let PreparedStepInput::Hidden { values, shape } = &mut item.input {
            let mut transform_timings = NodeTimings::default();
            match s.apply_moe_hidden(
                std::mem::take(values),
                shape[0],
                capture_block,
                &mut transform_timings,
            ) {
                Ok(transformed) => *values = transformed,
                Err(error) => {
                    transform_error = Some(error);
                    break;
                }
            }
            if !transform_timings.moe.executors.is_empty() {
                moe_input_items.insert(item.sid_key.clone(), transform_timings.moe);
            }
        }
    }
    if let Some(error) = transform_error {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }
    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    let decode_result = match &prepared[0].1.input {
        PreparedStepInput::Tokens { .. } => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedStepInput::Tokens { token } => vec![*token],
                        PreparedStepInput::Hidden { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedStepInput::Hidden { .. } => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedStepInput::Hidden { values, .. } => values.clone(),
                        PreparedStepInput::Tokens { .. } => Vec::new(),
                    },
                    shape: match &item.input {
                        PreparedStepInput::Hidden { shape, .. } => shape.clone(),
                        PreparedStepInput::Tokens { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };
    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let new_n_past = s.sessions[&item.sid_key]
                .n_past
                .saturating_add(item.n_tokens as u32);
            s.sessions.get_mut(&item.sid_key).unwrap().n_past = new_n_past;
            let mut timings = NodeTimings {
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };
            timings.merge_engine_moe(&engine_timings);
            if let Some(moe) = moe_input_items.remove(&item.sid_key) {
                timings.merge_moe(&moe);
                timings.record_engine(crate::executor::ExecutionEngine::Moe);
            }
            timings.record_engine(crate::executor::ExecutionEngine::LlamaCpp);

            if item.is_last {
                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let token_text_started = Instant::now();
                let piece = s.engine.token_to_piece_bytes(next_id, false);
                let mut text = s
                    .sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .feed_token_piece(&piece);
                let token_text_us = token_text_started.elapsed().as_micros() as u64;
                let done = next_id == s.engine.eos_token();
                if done {
                    finish_terminal_stream(&mut s.sessions, &item.sid_key, &mut text)?;
                }
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                // Fim de turno retém o prompt - ver `InferState::retain_prompt`.
                if done && !crate::tp::is_replica() {
                    s.retain_prompt(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.token_text_us = token_text_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us
                    + token_text_us;
                let encode_started = Instant::now();
                let bytes = wire::encode(
                    &json!({"done": done, "token_id": next_id, "text": text}),
                    None,
                    item.wire_dtype,
                )?;
                timings.encode_us = encode_started.elapsed().as_micros() as u64;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let input_was_hidden = matches!(&item.input, PreparedStepInput::Hidden { .. });
            let (mut residual, residual_shape) =
                output.residual_for(&item.sid_key, s.engine.n_embd)?;
            if !input_was_hidden {
                residual =
                    s.apply_moe_hidden(residual, item.n_tokens, item.capture_block, &mut timings)?;
            }
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let tokens = match &item.input {
                PreparedStepInput::Tokens { token } => vec![*token],
                PreparedStepInput::Hidden { .. } => Vec::new(),
            };
            let mut meta = json!({"done": false});
            if !tokens.is_empty() {
                meta["tokens"] = json!(tokens);
            }
            let encode_started = Instant::now();
            let bytes = wire::encode(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.encode_us = encode_started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}
