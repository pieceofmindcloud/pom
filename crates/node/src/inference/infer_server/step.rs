use super::*;
use crate::prefill::Phase;

pub(super) struct PreparedStep {
    pub(super) sid_key: String,
    pub(super) seq_id: u32,
    pub(super) n_past: u32,
    pub(super) n_tokens: usize,
    pub(super) is_last: bool,
    pub(super) wire_dtype: WireDtype,
    pub(super) capture_block: Option<u32>,
    pub(super) input: PreparedStepInput,
    pub(super) queued_at: Instant,
    pub(super) scheduler_wait_us: u64,
    pub(super) scheduler_wait_reason: Option<&'static str>,
    pub(super) scheduler_session_existing: bool,
    pub(super) pipeline_session: bool,
}

pub(super) enum PreparedStepInput {
    Tokens {
        token: i32,
    },
    Hidden {
        values: Vec<f32>,
        shape: TensorShape,
    },
}

// ── /v1/step ─────────────────────────────────────────────────────────────────

pub(crate) async fn handle_step(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    app.request_activity.mark_now();
    let _load_guard = app.loads.acquire(&app.load_node_id).await;
    // Só o cabeçalho é lido aqui: um quadro de um item - o caso normal de
    // todo salto interno - não pode pagar uma cópia do estado oculto inteiro
    // só para descobrir que não é um lote.
    if wire::peek_batch_header(&body).is_ok_and(|header| header.item_count > 1) {
        match wire::decode_batch(&body) {
            Ok(batch) => return handle_step_batch(app, headers, batch).await,
            Err(error) => return err400(format!("quadro inválido: {error}")),
        }
    }
    // See the matching guard in `handle_prefill`: a one-item batch must not
    // overwrite the decode/barrier state of another TP request while its
    // ranks are still in the collective.
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
    open_tp_decode(&state, &headers, "/v1/step", &body);
    let processing_started = Instant::now();
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (mut meta, tensor, inbound_wire_timing) =
        match wire::decode_inference_request_profiled(&body) {
            Ok(v) => v,
            Err(e) => return err400(format!("quadro inválido: {e}")),
        };
    meta["telemetry_source_peer_id"] = json!(app.load_node_id.clone());
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;
    let wire_dtype = match meta.get("dtype").and_then(|value| value.as_str()) {
        Some(value) => match WireDtype::parse(value) {
            Ok(dtype) => dtype,
            Err(error) => return err400(error),
        },
        None => return err400("quadro sem dtype de estado oculto"),
    };

    let sid_key = match required_session_id(&meta) {
        Ok(session_id) => session_id,
        Err(error) => return err400(error),
    };
    // Present and non-empty only when the sender negotiated relay mode
    // (`PipelinePlan::supports_relay_chain`) — the ordered chain from *our*
    // next hop through the tail. An empty list still parses as `Some(vec![])`
    // and behaves exactly like `None`: this hop is the tail either way.
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
    let remote_stream = meta
        .get("remote_stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pipeline_session = meta
        .get("pipeline_session")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let token_id = meta
        .get("token_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    // Present only on the request to the head — see `step_blocking`'s
    // `speculate` param doc.
    let speculate = Speculate::from_meta(&meta);
    // Present on every non-head request in a speculative round — see
    // `step_blocking`'s `incoming_draft` param doc for why `None` and
    // `Some(vec![])` mean different things here.
    let incoming_draft: Option<Vec<i32>> = meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A composição do lote decodificado na cabeça, ecoada de hop em hop até a
    // cauda — é o que alimenta o `observe` da cabeça `nextn` (ver o doc de
    // `batch_tokens` em `step_blocking`). Ausente é peer antigo.
    let batch_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A largura de lote que o gateway quer na rodada seguinte, gerada na
    // cabeça (do `speculate`) e ecoada até a cauda (ver o doc de `mtp_k`).
    let mtp_k: Option<u32> = meta.get("mtp_k").and_then(|v| v.as_u64()).map(|v| v as u32);
    let profiling = crate::prefill::TelemetryContext::from_meta(
        &meta,
        app.load_node_id.clone(),
        Phase::Decode,
        processing_started,
        None,
    );

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
    // A sub-faixa só é válida se este peer consegue executá-la exatamente.
    // Sem os campos, o contrato antigo continua rodando a fatia inteira.
    let capture_block = {
        let (slice_start, slice_end) = match lock_inference_state(&state) {
            Ok(st) => (st.start_layer, st.end_layer),
            Err(error) => return err500(error),
        };
        match resolve_exec_range(&meta, slice_start, slice_end) {
            Ok(block) => block,
            Err(motivo) => return err400(motivo),
        }
    };
    // Neither of `step_blocking`'s replies echoes `session_id` back — a
    // relay branch that forwards to the next hop needs its own copy, taken
    // before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    let queued_at = Instant::now();
    let result = match scheduler.submit_step(StepJob {
        sid_key,
        is_last,
        remote_stream,
        wire_dtype,
        token_id,
        tensor,
        capture_block,
        speculate,
        incoming_draft,
        batch_tokens,
        mtp_k,
        queued_at,
        scheduler_wait_us: 0,
        scheduler_wait_reason: None,
        scheduler_session_existing: false,
        pipeline_session,
        profiling: profiling.clone(),
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
            let code = if e.to_string().contains("desconhecida") {
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
    reply.timings.residual_wire_f16_decode_ns = reply
        .timings
        .residual_wire_f16_decode_ns
        .saturating_add(inbound_wire_timing.f16_conversion_ns);

    // Decode what step_blocking already encoded so we can tell a residual
    // (needs relaying, if `next_hops` says so) from a final answer (the
    // tail's `{token_id, text, done}`, with no tensor) — and, either way, to
    // patch in this hop's own `processing_ms`/`node_timings` before it goes
    // any further. `handle_prefill` does the identical patch for the same
    // reason.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor, reply_wire_timing) =
        match wire::decode_profiled(&reply.bytes) {
            Ok(v) => v,
            Err(e) => return err500(e),
        };
    reply.timings.residual_wire_f16_decode_ns = reply
        .timings
        .residual_wire_f16_decode_ns
        .saturating_add(reply_wire_timing.f16_conversion_ns);
    if let Some(context) = profiling.as_ref() {
        reply.timings.record_profile_span(
            context,
            "node-processing",
            context.origin,
            Instant::now(),
            None,
            None,
        );
    }
    if local_meta
        .get("token_id")
        .and_then(|value| value.as_u64())
        .is_some()
    {
        reply.timings.first_token_serialized_us = processing_started.elapsed().as_micros() as u64;
        reply.timings.first_token_serialized_at_ms = super::core::wall_clock_ms();
    }
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta: quem terminar entrega ao gateway, e o que sai daqui é
    // só um aceite. Cada hop descreve a si mesmo numa lista que viaja na ida.
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
            // Cauda: o resultado sai daqui direto para quem pediu.
            let mut result = local_meta;
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            // Não somos a cauda e não temos residual: a cadeia não anda. Dizer
            // isso ao gateway agora vale mais que deixá-lo esperar o prazo.
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

        let mut forward = step_forward_meta(&sid_for_forward, &meta, &local_meta, &hops[0]);
        forward["hop_timings"] = json!(hop_timings);
        tokio::spawn(forward_direct(
            hops, "/v1/step", forward, residual, shape, wire_dtype,
            1, // a decode step has never retried — see relay_and_bubble's doc comment
            reply_to,
        ));
        return direct_ack();
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
    let forward_meta = step_forward_meta(&sid_for_forward, &meta, &local_meta, &hops[0]);

    let downstream_started = Instant::now();
    let mut bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/step",
        forward_meta,
        (&residual, shape),
        wire_dtype,
        1, // a decode step has never retried — see relay_and_bubble's doc comment
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };
    if remote_stream {
        bubbled["remote_stream_downstream_wait_ms"] =
            json!(downstream_started.elapsed().as_millis() as u64);
        let accept_count = bubbled
            .get("accept_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let batch_width = bubbled
            .get("batch_width")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if batch_width > 0 && accept_count < batch_width {
            match rollback_session(&state, &sid_for_forward, accept_count, batch_width) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return err500("rollback especulativo remoto recusado no hop");
                }
            }
        }
    }
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

/// Estado do canal persistente: o do servidor de inferência e a configuração
/// usada para reconferir a `connection_key` a cada quadro.
#[derive(Clone)]
pub(super) struct LinkState {
    pub(super) app: InferenceAppState,
    pub(super) auth: Option<crate::auth::AuthState>,
}

/// Serve a ligação persistente da aresta da pipeline. Cada quadro que chega
/// pelo fluxo é despachado pela mesma tabela de rotas em processo que o
/// caminho por requisição usa (`InProcessInference::dispatch`), então canal e
/// requisição executam exatamente o mesmo código de inferência - e passam pela
/// mesma conferência de credencial.
pub(super) async fn handle_link(
    State(state): State<LinkState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Response {
    crate::link::serve(body, move |path, bytes| {
        let state = state.clone();
        let headers = headers.clone();
        async move {
            if let Some(config) = &state.auth {
                crate::auth::validate_internal_node_headers(config, &headers)
                    .await
                    .map_err(|_| anyhow::anyhow!("credencial de peer inválida"))?;
            }
            use crate::relay::InProcessInference;
            state.app.dispatch(&path, bytes).await
        }
    })
}

/// Starts a decode session owned by the remote peer. The response only
/// acknowledges ownership; generated tokens are sent back through the
/// bounded callback inbox in opportunistic batches.
pub(crate) async fn handle_step_stream(
    State(app): State<InferenceAppState>,
    _headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    app.request_activity.mark_now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(error) => return err400(format!("quadro inválido: {error}")),
    };
    if tensor.is_some() {
        return err400("transporte remoto por lotes não aceita estado oculto");
    }
    let Some(mut reply_to) = meta.get("reply_to").cloned() else {
        return err400("transporte remoto por lotes sem reply_to");
    };
    if reply_to.get("ticket").and_then(Value::as_str).is_none() {
        return err400("transporte remoto por lotes sem ticket");
    }
    // The callback is the only response path for this stream. Carry the
    // request identity alongside its ticket so profiling spans from the
    // remote head remain correlated after the control request returns.
    for field in ["trace_id", "request_id", "pipeline_id", "session_id"] {
        if let Some(value) = meta.get(field) {
            reply_to[field] = value.clone();
        }
    }
    let Some(dtype) = meta
        .get("dtype")
        .and_then(Value::as_str)
        .and_then(|value| WireDtype::parse(value).ok())
    else {
        return err400("quadro sem dtype de estado oculto");
    };
    if required_session_id(&meta).is_err() || meta.get("token_id").and_then(Value::as_i64).is_none()
    {
        return err400("transporte remoto por lotes sem sessão ou token inicial");
    }

    let loads = app.loads.clone();
    let load_node_id = app.load_node_id.clone();
    let source_node_id = load_node_id.clone();
    let stream_started = Instant::now();
    // Keep enough completed tokens in flight to absorb short network tails.
    // The queue is bounded, so a persistently slow callback still applies
    // backpressure instead of growing memory without limit.
    let (producer_tx, producer_rx) = mpsc::channel(64);
    let producer = async move {
        let _load_guard = loads.acquire(&load_node_id).await;
        let producer = remote_stream_producer(app, meta, dtype, producer_tx);
        let emitter = remote_stream_emitter(reply_to, producer_rx, stream_started, source_node_id);
        run_remote_stream_tasks(producer, emitter).await;
    };
    tokio::spawn(producer);

    match wire::encode(
        &json!({"stream_accepted": true, "processing_ms": 0, "node_timings": {}}),
        None,
        dtype,
    ) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

/// Keep the producer and emitter coupled for the lifetime of one stream.
///
/// The HTTP control request returns as soon as the stream is accepted, so a
/// client disconnect cannot cancel the task that owns the autoregressive
/// loop. If the emitter notices a gone gateway first, waiting on a plain
/// `join!` would leave that loop running until its next callback or until the
/// inference timeout. Aborting the producer at that boundary releases its
/// scheduler admission and its generation KV promptly. When the producer
/// finishes normally, the emitter is still allowed to drain the bounded
/// queue and deliver the final token.
async fn run_remote_stream_tasks<P, E>(producer: P, emitter: E)
where
    P: std::future::Future<Output = ()> + Send + 'static,
    E: std::future::Future<Output = ()> + Send + 'static,
{
    let mut producer = tokio::spawn(producer);
    let mut emitter = tokio::spawn(emitter);

    tokio::select! {
        _ = &mut producer => {
            let _ = emitter.await;
        }
        _ = &mut emitter => {
            producer.abort();
            let _ = producer.await;
        }
    }
}

pub(super) struct RemoteStreamToken {
    pub(super) token_id: u32,
    pub(super) token_index: u64,
    pub(super) text: String,
    pub(super) done: bool,
    pub(super) processing_ms: u64,
    pub(super) node_timings: Option<Value>,
    /// Contributions from peers after the remote head. The head's own
    /// processing remains in `processing_ms`/`node_timings`; these entries
    /// preserve the rest of the ordered pipeline for the callback batch.
    pub(super) hop_timings: Option<Value>,
    pub(super) speculated: bool,
    pub(super) speculation_active: bool,
    pub(super) speculation_disabled_reason: Option<String>,
    pub(super) speculation_disabled_detail: Option<String>,
    pub(super) next_draft: Option<Vec<i32>>,
    /// Wall time spent waiting for the next hop to finish this token. This
    /// includes that hop's compute and its transport, so it is not reported
    /// as local queue time.
    pub(super) downstream_wait_ms: u64,
    /// Momento monotônico em que o produtor terminou de montar este token.
    /// Não é serializado: serve apenas para separar produção de espera local.
    pub(super) ready_at: Instant,
    /// Momento monotônico em que o token entrou no canal depois de adquirir
    /// o permit. Separar este ponto de `ready_at` torna a contrapressão do
    /// `Sender` observável, sem chamar todo o intervalo de fila.
    pub(super) enqueued_at: Instant,
}

pub(super) enum RemoteStreamItem {
    Token(RemoteStreamToken),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteStreamWaitBreakdown {
    interval_ms: u64,
    producer_ms: u64,
    channel_send_ms: u64,
    queue_ms: u64,
}

fn overlap_ms(start: Instant, end: Instant, window_start: Instant, window_end: Instant) -> u64 {
    let start = start.max(window_start);
    let end = end.min(window_end);
    end.saturating_duration_since(start).as_millis() as u64
}

fn remote_stream_wait_breakdown(
    wait_started: Instant,
    ready_at: Instant,
    enqueued_at: Instant,
    received_at: Instant,
) -> RemoteStreamWaitBreakdown {
    RemoteStreamWaitBreakdown {
        interval_ms: received_at
            .saturating_duration_since(wait_started)
            .as_millis() as u64,
        producer_ms: overlap_ms(wait_started, ready_at, wait_started, received_at),
        channel_send_ms: overlap_ms(ready_at, enqueued_at, wait_started, received_at),
        queue_ms: overlap_ms(enqueued_at, received_at, wait_started, received_at),
    }
}

pub(super) fn remote_stream_install_draft(
    step_meta: &mut Value,
    next_draft: Option<&[i32]>,
    disabled_reason: Option<&str>,
    disabled_detail: Option<&str>,
) -> bool {
    let Some(object) = step_meta.as_object_mut() else {
        return false;
    };
    let Some(speculate) = object.get_mut("speculate").and_then(Value::as_object_mut) else {
        return false;
    };
    if let Some(draft) = next_draft.filter(|draft| !draft.is_empty()) {
        speculate.insert("draft".into(), json!(draft));
        true
    } else {
        object.remove("speculate");
        if let Some(reason) = disabled_reason {
            object.insert("speculation_disabled_reason".into(), json!(reason));
        }
        if let Some(detail) = disabled_detail {
            object.insert("speculation_disabled_detail".into(), json!(detail));
        }
        false
    }
}

pub(super) async fn remote_stream_producer(
    app: InferenceAppState,
    mut step_meta: Value,
    dtype: WireDtype,
    sender: mpsc::Sender<RemoteStreamItem>,
) {
    step_meta.as_object_mut().map(|object| {
        object.remove("reply_to");
    });
    loop {
        let body = match wire::encode(&step_meta, None, dtype) {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                let _ = sender
                    .send(RemoteStreamItem::Error(error.to_string()))
                    .await;
                return;
            }
        };
        let response = handle_step(State(app.clone()), axum::http::HeaderMap::new(), body).await;
        if !response.status().is_success() {
            let body = axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT)
                .await
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|error| error.to_string());
            let _ = sender
                .send(RemoteStreamItem::Error(format!(
                    "step remoto respondeu com erro: {body}"
                )))
                .await;
            return;
        }
        let response_body =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(body) => body,
                Err(error) => {
                    let _ = sender
                        .send(RemoteStreamItem::Error(error.to_string()))
                        .await;
                    return;
                }
            };
        let (reply_meta, tensor) = match wire::decode(&response_body) {
            Ok(value) => value,
            Err(error) => {
                let _ = sender
                    .send(RemoteStreamItem::Error(error.to_string()))
                    .await;
                return;
            }
        };
        if tensor.is_some() {
            let _ = sender
                .send(RemoteStreamItem::Error(
                    "step remoto devolveu estado oculto em vez de token".into(),
                ))
                .await;
            return;
        }

        let processing_ms = reply_meta
            .get("processing_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let node_timings = reply_meta.get("node_timings").cloned();
        let done = reply_meta
            .get("done")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let accepted = reply_meta
            .get("accepted")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some((
                            entry.get("token_id")?.as_u64()? as u32,
                            entry.get("text")?.as_str()?.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let token_index = step_meta
            .get("telemetry_token_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let hop_timings = reply_meta
            .get("hop_timings")
            .and_then(Value::as_array)
            .map(|hops| {
                Value::Array(
                    hops.iter()
                        .map(|hop| {
                            let mut hop = hop.clone();
                            hop["span_count"] = json!(accepted.len().max(1));
                            hop
                        })
                        .collect(),
                )
            });
        let downstream_wait_ms = reply_meta
            .get("remote_stream_downstream_wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let requested_mtp = Speculate::from_meta(&step_meta)
            .is_some_and(|speculate| speculate.source == DraftSource::Mtp);
        let next_draft = reply_meta
            .get("next_draft")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_i64)
                    .map(|value| value as i32)
                    .collect::<Vec<_>>()
            })
            .filter(|draft| !draft.is_empty());
        let speculated = reply_meta
            .get("speculated")
            .and_then(Value::as_bool)
            .unwrap_or(!accepted.is_empty());
        let speculation_active = reply_meta
            .get("speculation_active")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                if requested_mtp {
                    next_draft.is_some()
                } else {
                    speculated
                }
            });
        let speculation_disabled_reason = (!speculation_active && requested_mtp).then(|| {
            reply_meta
                .get("speculation_disabled_reason")
                .and_then(Value::as_str)
                .unwrap_or(SPECULATION_REASON_MODEL_WITHOUT_MTP)
                .to_string()
        });
        let speculation_disabled_detail = (!speculation_active && requested_mtp).then(|| {
            reply_meta
                .get("speculation_disabled_detail")
                .and_then(Value::as_str)
                .or_else(|| {
                    reply_meta
                        .get("speculation_disabled_reason")
                        .and_then(Value::as_str)
                })
                .unwrap_or("nenhum detalhe foi devolvido pela cauda")
                .to_owned()
        });
        if requested_mtp {
            remote_stream_install_draft(
                &mut step_meta,
                next_draft.as_deref(),
                speculation_disabled_reason.as_deref(),
                speculation_disabled_detail.as_deref(),
            );
        }
        if !accepted.is_empty() {
            for (index, (token_id, text)) in accepted.iter().enumerate() {
                let ready_at = Instant::now();
                let Ok(permit) = sender.reserve().await else {
                    return;
                };
                let enqueued_at = Instant::now();
                permit.send(RemoteStreamItem::Token(RemoteStreamToken {
                    token_id: *token_id,
                    token_index: token_index.saturating_add(index as u64),
                    text: text.clone(),
                    done: done && index + 1 == accepted.len(),
                    processing_ms: if index == 0 { processing_ms } else { 0 },
                    node_timings: (index == 0).then(|| node_timings.clone()).flatten(),
                    hop_timings: (index == 0).then(|| hop_timings.clone()).flatten(),
                    speculated,
                    speculation_active,
                    speculation_disabled_reason: speculation_disabled_reason.clone(),
                    speculation_disabled_detail: speculation_disabled_detail.clone(),
                    next_draft: (index == 0).then(|| next_draft.clone()).flatten(),
                    downstream_wait_ms: if index == 0 { downstream_wait_ms } else { 0 },
                    ready_at,
                    enqueued_at,
                }));
            }
            if done {
                return;
            }
            // Every hop in a remote stream already rolled back its own KV
            // before bubbling this reply. The counts remain in the callback
            // for the gateway's telemetry, but no rollback request is sent
            // after the frame.
            if let Some(token_id) = accepted.last().map(|(token_id, _)| *token_id) {
                step_meta["token_id"] = json!(token_id);
            }
            step_meta["telemetry_token_index"] =
                json!(token_index.saturating_add(accepted.len() as u64));
            continue;
        }

        let Some(token_id) = reply_meta.get("token_id").and_then(Value::as_u64) else {
            let _ = sender
                .send(RemoteStreamItem::Error(
                    "step remoto não devolveu token".into(),
                ))
                .await;
            return;
        };
        let ready_at = Instant::now();
        let Ok(permit) = sender.reserve().await else {
            return;
        };
        let enqueued_at = Instant::now();
        permit.send(RemoteStreamItem::Token(RemoteStreamToken {
            token_id: token_id as u32,
            token_index,
            text: reply_meta
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            done,
            processing_ms,
            node_timings,
            hop_timings,
            speculated,
            speculation_active,
            speculation_disabled_reason,
            speculation_disabled_detail,
            next_draft,
            downstream_wait_ms,
            ready_at,
            enqueued_at,
        }));
        if done {
            return;
        }
        step_meta["token_id"] = json!(token_id);
        step_meta["telemetry_token_index"] = json!(token_index.saturating_add(1));
    }
}

pub(super) fn aggregate_remote_node_timings(items: &[RemoteStreamItem]) -> Value {
    let mut merged = serde_json::Map::new();
    for item in items {
        let RemoteStreamItem::Token(token) = item else {
            continue;
        };
        let Some(Value::Object(values)) = &token.node_timings else {
            continue;
        };
        for (key, value) in values {
            if matches!(
                key.as_str(),
                "mtp_draft_tokens_proposed"
                    | "mtp_draft_tokens_accepted"
                    | "mtp_draft_tokens_rejected"
            ) {
                let total = merged
                    .get(key)
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(value.as_u64().unwrap_or(0));
                merged.insert(key.clone(), json!(total));
            } else if key.ends_with("_ms") {
                let total = merged.get(key).and_then(Value::as_f64).unwrap_or(0.0)
                    + value.as_f64().unwrap_or(0.0);
                merged.insert(key.clone(), json!(total));
            } else if key == "engine_mismatch" {
                let mismatch = merged.get(key).and_then(Value::as_bool).unwrap_or(false)
                    || value.as_bool().unwrap_or(false);
                merged.insert(key.clone(), json!(mismatch));
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

fn add_remote_timing_value(target: &mut Value, incoming: &Value) {
    let (Some(target), Value::Object(incoming)) = (target.as_object_mut(), incoming) else {
        return;
    };
    for (key, value) in incoming {
        if key == "node_timings" {
            let nested = target.entry(key.clone()).or_insert_with(|| json!({}));
            add_remote_timing_value(nested, value);
        } else if key == "engine_mismatch" {
            let mismatch = target.get(key).and_then(Value::as_bool).unwrap_or(false)
                || value.as_bool().unwrap_or(false);
            target.insert(key.clone(), json!(mismatch));
        } else if key.ends_with("_ms")
            || matches!(
                key.as_str(),
                "round_trip_ms"
                    | "bytes_sent"
                    | "bytes_received"
                    | "hidden_state_bytes_sent"
                    | "hidden_state_bytes_received"
                    | "span_count"
            )
        {
            let total = target.get(key).and_then(Value::as_f64).unwrap_or(0.0)
                + value.as_f64().unwrap_or(0.0);
            target.insert(key.clone(), json!(total));
        } else if !target.contains_key(key) {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn aggregate_remote_hop_timings(items: &[RemoteStreamItem]) -> Value {
    let mut merged: Vec<Value> = Vec::new();
    for item in items {
        let RemoteStreamItem::Token(token) = item else {
            continue;
        };
        let Some(Value::Array(hops)) = &token.hop_timings else {
            continue;
        };
        for (index, hop) in hops.iter().enumerate() {
            if let Some(current) = merged.get_mut(index) {
                add_remote_timing_value(current, hop);
            } else {
                let mut hop = hop.clone();
                if hop.get("span_count").is_none() {
                    hop["span_count"] = json!(1);
                }
                merged.push(hop);
            }
        }
    }
    Value::Array(merged)
}

pub(super) fn remote_stream_batch(items: &[RemoteStreamItem]) -> (Value, Option<String>, bool) {
    let mut tokens = Vec::new();
    let mut processing_ms = 0_u64;
    let mut done = false;
    let mut error = None;
    let mut status = None;
    let mut next_draft = None;
    let mut downstream_wait_ms = 0_u64;
    for item in items {
        match item {
            RemoteStreamItem::Token(token) => {
                tokens.push(json!({
                    "token_id": token.token_id,
                    "text": token.text,
                    "token_index": token.token_index,
                }));
                processing_ms = processing_ms.saturating_add(token.processing_ms);
                done |= token.done;
                status = Some((
                    token.speculated,
                    token.speculation_active,
                    token.speculation_disabled_reason.clone(),
                    token.speculation_disabled_detail.clone(),
                ));
                if next_draft.is_none() {
                    next_draft = token.next_draft.clone();
                }
                downstream_wait_ms = downstream_wait_ms.max(token.downstream_wait_ms);
            }
            RemoteStreamItem::Error(message) => error = Some(message.clone()),
        }
    }
    let terminal = done || error.is_some();
    let (speculated, speculation_active, speculation_disabled_reason, speculation_disabled_detail) =
        status.unwrap_or((false, false, None, None));
    let batch = json!({
        "accepted": tokens,
        "done": done && error.is_none(),
        "processing_ms": processing_ms,
        "node_timings": aggregate_remote_node_timings(items),
        "hop_timings": aggregate_remote_hop_timings(items),
        "span_count": tokens.len(),
        "speculated": speculated,
        "speculation_active": speculation_active,
        "speculation_disabled_reason": speculation_disabled_reason,
        "speculation_disabled_detail": speculation_disabled_detail,
        "next_draft": next_draft,
        "remote_stream_downstream_wait_ms": downstream_wait_ms,
    });
    (batch, error, terminal)
}

pub(super) async fn remote_stream_emitter(
    reply_to: Value,
    receiver: mpsc::Receiver<RemoteStreamItem>,
    stream_started: Instant,
    source_node_id: String,
) {
    let stream_id = reply_to
        .get("ticket")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let mut frames_sent = 0_u64;
    let mut previous_frame_at = None;
    let delivery = Arc::new(tokio::sync::Mutex::new(
        crate::direct::RemoteStreamDelivery::new(http_client().clone(), reply_to.clone()),
    ));
    emit_remote_stream_batches(reply_to.clone(), receiver, |_reply_to, mut batch| {
        let stream_id = stream_id.clone();
        let frame_index = frames_sent;
        frames_sent = frames_sent.saturating_add(1);
        let now = Instant::now();
        let frame_latency_ms = previous_frame_at
            .map(|previous: Instant| now.saturating_duration_since(previous).as_millis() as u64)
            .unwrap_or_else(|| now.saturating_duration_since(stream_started).as_millis() as u64);
        previous_frame_at = Some(now);
        batch["stream_id"] = json!(stream_id);
        for field in ["trace_id", "request_id", "pipeline_id", "session_id"] {
            if let Some(value) = reply_to.get(field) {
                batch[field] = value.clone();
            }
        }
        batch["remote_stream_frame_index"] = json!(frame_index);
        batch["remote_stream_frames_sent"] = json!(frames_sent);
        batch["remote_stream_lifetime_ms"] =
            json!(now.saturating_duration_since(stream_started).as_millis() as u64);
        batch["remote_stream_frame_latency_ms"] = json!(frame_latency_ms);
        batch["remote_stream_source_node_id"] = json!(&source_node_id);
        if frame_index == 0 {
            batch["remote_stream_first_frame_latency_ms"] = json!(frame_latency_ms);
        }
        let delivery = delivery.clone();
        async move { delivery.lock().await.deliver(batch).await }
    })
    .await;
}

/// Um lote terminal que só carrega o erro. Não tem token: serve para o
/// gateway parar de esperar e nomear a causa.
pub(super) fn remote_stream_error_batch(error: Value) -> Value {
    json!({
        "accepted": [],
        "done": false,
        "processing_ms": 0,
        "node_timings": {},
        "error": error,
    })
}

pub(super) async fn emit_remote_stream_batches<F, Fut>(
    reply_to: Value,
    mut receiver: mpsc::Receiver<RemoteStreamItem>,
    mut deliver: F,
) where
    F: FnMut(Value, Value) -> Fut,
    Fut: Future<Output = crate::direct::Delivery>,
{
    let mut wait_started = Instant::now();
    let mut previous_delivery_ms = 0_u64;
    while let Some(first) = receiver.recv().await {
        let received_at = Instant::now();
        let breakdown = match &first {
            RemoteStreamItem::Token(token) => remote_stream_wait_breakdown(
                wait_started,
                token.ready_at,
                token.enqueued_at,
                received_at,
            ),
            RemoteStreamItem::Error(_) => RemoteStreamWaitBreakdown {
                interval_ms: 0,
                producer_ms: 0,
                channel_send_ms: 0,
                queue_ms: 0,
            },
        };
        let items = crate::prefill::drain_opportunistic_batch(first, &mut receiver);
        let (mut batch, error, terminal) = remote_stream_batch(&items);
        if let Some(session_id) = reply_to.get("session_id") {
            batch["session_id"] = session_id.clone();
        }
        batch["remote_stream_wait_ms"] = json!(breakdown.interval_ms);
        batch["remote_stream_producer_wait_ms"] = json!(breakdown.producer_ms);
        batch["remote_stream_channel_send_wait_ms"] = json!(breakdown.channel_send_ms);
        batch["remote_stream_queue_wait_ms"] = json!(breakdown.queue_ms);
        let has_tokens = batch
            .get("accepted")
            .and_then(Value::as_array)
            .is_some_and(|tokens| !tokens.is_empty());
        if !has_tokens && error.is_some() {
            let _ = deliver(reply_to.clone(), remote_stream_error_batch(json!(error))).await;
            return;
        }
        // The duration is attached to the next batch because the current
        // batch has already been serialized by the delivery future before we
        // know how long its callback took. The first batch is consequently
        // zero and the final callback has no following batch to carry it.
        batch["remote_stream_delivery_ms"] = json!(previous_delivery_ms);
        let delivery_started = Instant::now();
        let delivery_result = deliver(reply_to.clone(), batch).await;
        previous_delivery_ms = delivery_started.elapsed().as_millis() as u64;
        match delivery_result {
            crate::direct::Delivery::Accepted => {}
            // O gateway já desistiu deste stream: não há a quem reportar.
            crate::direct::Delivery::GatewayGone => return,
            // Sem isto o emissor sumia em silêncio e o gateway só percebia
            // quando o prazo do frame vencia - espera
            // por uma falha que este lado já conhecia. O aviso é um lote
            // terminal pequeno, e vai pela mesma escada de endereços, que
            // agora sempre tem pelo menos uma tentativa a pagar.
            crate::direct::Delivery::Failed => {
                warn!(
                    "lote do stream remoto não chegou ao gateway; encerrando o stream com erro \
                     explícito em vez de deixar o prazo vencer"
                );
                let _ = deliver(
                    reply_to.clone(),
                    remote_stream_error_batch(json!(
                        "o lote de decode não chegou ao gateway por nenhum endereço de retorno"
                    )),
                )
                .await;
                return;
            }
        }
        // Preserve tokens that were already generated before an error. The
        // error is terminal, but it must not make the gateway discard the
        // prefix carried by the same opportunistic batch.
        if let Some(error) = error {
            let _ = deliver(reply_to.clone(), remote_stream_error_batch(json!(error))).await;
        }
        if terminal {
            return;
        }
        wait_started = Instant::now();
    }
}

#[cfg(test)]
mod remote_stream_tests {
    use super::*;

    #[tokio::test]
    async fn emissor_para_quando_a_entrega_termina_antes_do_produtor() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        struct DropSignal(Arc<AtomicBool>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let producer_dropped = Arc::new(AtomicBool::new(false));
        let producer_guard = DropSignal(producer_dropped.clone());
        let producer = async move {
            let _guard = producer_guard;
            std::future::pending::<()>().await;
        };

        tokio::time::timeout(
            Duration::from_millis(100),
            run_remote_stream_tasks(producer, async {}),
        )
        .await
        .expect("a entrega encerrada deve cancelar o produtor");
        assert!(producer_dropped.load(Ordering::Acquire));
    }

    fn token(token_id: u32, text: &str, done: bool) -> RemoteStreamItem {
        token_state(token_id, text, done, false, false, None)
    }

    #[test]
    fn espera_do_stream_separa_producao_de_fila_com_instantes_conhecidos() {
        let origin = Instant::now();
        let wait_started = origin + std::time::Duration::from_millis(100);
        let ready_at = origin + std::time::Duration::from_millis(176);
        let enqueued_at = origin + std::time::Duration::from_millis(177);
        let received_at = origin + std::time::Duration::from_millis(178);

        let breakdown =
            remote_stream_wait_breakdown(wait_started, ready_at, enqueued_at, received_at);

        assert_eq!(breakdown.interval_ms, 78);
        assert_eq!(breakdown.producer_ms, 76);
        assert_eq!(breakdown.channel_send_ms, 1);
        assert_eq!(breakdown.queue_ms, 1);
    }

    fn token_state(
        token_id: u32,
        text: &str,
        done: bool,
        speculated: bool,
        speculation_active: bool,
        next_draft: Option<Vec<i32>>,
    ) -> RemoteStreamItem {
        RemoteStreamItem::Token(RemoteStreamToken {
            token_id,
            token_index: token_id as u64,
            text: text.into(),
            done,
            processing_ms: 3,
            node_timings: Some(json!({"decode_ms": 2, "engine_mismatch": false})),
            hop_timings: None,
            speculated,
            speculation_active,
            speculation_disabled_reason: None,
            speculation_disabled_detail: None,
            next_draft,
            downstream_wait_ms: 0,
            ready_at: Instant::now(),
            enqueued_at: Instant::now(),
        })
    }

    #[test]
    fn lote_remoto_preserva_conteudo_ordem_e_tempos() {
        let (batch, error, terminal) =
            remote_stream_batch(&[token(4, "A", false), token(9, "ção", true)]);

        assert_eq!(
            batch["accepted"][0],
            json!({"token_id": 4, "text": "A", "token_index": 4})
        );
        assert_eq!(
            batch["accepted"][1],
            json!({"token_id": 9, "text": "ção", "token_index": 9})
        );
        assert_eq!(batch["processing_ms"], 6);
        assert_eq!(batch["node_timings"]["decode_ms"], 4.0);
        assert_eq!(batch["done"], true);
        assert!(error.is_none());
        assert!(terminal);
    }

    #[tokio::test]
    async fn lote_remoto_preserva_session_id_do_canal_de_retorno() {
        let (items_tx, items_rx) = mpsc::channel(1);
        items_tx
            .send(token(7, "token", true))
            .await
            .expect("token enfileirado");
        drop(items_tx);

        emit_remote_stream_batches(
            json!({"ticket": "stream-session", "session_id": "session-42"}),
            items_rx,
            |_, batch| async move {
                assert_eq!(batch["session_id"], "session-42");
                crate::direct::Delivery::Accepted
            },
        )
        .await;
    }

    #[test]
    fn lote_remoto_preserva_tempos_dos_hops_downstream() {
        let mut first = token_state(4, "A", false, false, false, None);
        if let RemoteStreamItem::Token(token) = &mut first {
            token.hop_timings = Some(json!([
                {
                    "processing_ms": 7,
                    "round_trip_ms": 11,
                    "span_count": 2,
                },
                {
                    "processing_ms": 13,
                    "round_trip_ms": 17,
                    "span_count": 2,
                }
            ]));
        }
        let (batch, error, terminal) =
            remote_stream_batch(&[first, token_state(9, "ção", true, false, false, None)]);

        assert!(error.is_none());
        assert!(terminal);
        assert_eq!(batch["span_count"], 2);
        assert_eq!(batch["hop_timings"][0]["processing_ms"], 7);
        assert_eq!(batch["hop_timings"][0]["span_count"], 2);
        assert_eq!(batch["hop_timings"][1]["processing_ms"], 13);
    }

    #[test]
    fn lote_remoto_soma_contadores_mtp_de_rodadas_agrupadas() {
        let timings = |proposed, accepted, rejected| {
            json!({
                "mtp_draft_tokens_proposed": proposed,
                "mtp_draft_tokens_accepted": accepted,
                "mtp_draft_tokens_rejected": rejected,
            })
        };
        let items = [
            token_with_node_timings(timings(7, 5, 2)),
            token_with_node_timings(timings(3, 2, 1)),
        ];

        let aggregated = aggregate_remote_node_timings(&items);

        assert_eq!(aggregated["mtp_draft_tokens_proposed"], 10);
        assert_eq!(aggregated["mtp_draft_tokens_accepted"], 7);
        assert_eq!(aggregated["mtp_draft_tokens_rejected"], 3);
    }

    fn token_with_node_timings(node_timings: Value) -> RemoteStreamItem {
        RemoteStreamItem::Token(RemoteStreamToken {
            token_id: 0,
            token_index: 0,
            text: String::new(),
            done: false,
            processing_ms: 0,
            node_timings: Some(node_timings),
            hop_timings: None,
            speculated: true,
            speculation_active: true,
            speculation_disabled_reason: None,
            speculation_disabled_detail: None,
            next_draft: None,
            downstream_wait_ms: 0,
            ready_at: Instant::now(),
            enqueued_at: Instant::now(),
        })
    }

    #[test]
    fn lote_remoto_transporta_o_rascunho_no_mesmo_callback() {
        let item = RemoteStreamItem::Token(RemoteStreamToken {
            token_id: 4,
            token_index: 4,
            text: "A".into(),
            done: false,
            processing_ms: 3,
            node_timings: None,
            hop_timings: Some(json!([
                {
                    "processing_ms": 11,
                    "round_trip_ms": 17,
                    "transport": "remote-stream",
                    "node_timings": {"decode_ms": 9}
                }
            ])),
            speculated: false,
            speculation_active: true,
            speculation_disabled_reason: None,
            speculation_disabled_detail: None,
            next_draft: Some(vec![5, 6]),
            downstream_wait_ms: 0,
            ready_at: Instant::now(),
            enqueued_at: Instant::now(),
        });

        let (batch, error, terminal) = remote_stream_batch(&[item]);

        assert_eq!(batch["next_draft"], json!([5, 6]));
        assert_eq!(batch["speculated"], false);
        assert_eq!(batch["speculation_active"], true);
        assert_eq!(batch["hop_timings"][0]["processing_ms"], 11);
        assert!(error.is_none());
        assert!(!terminal);
    }

    #[test]
    fn rascunho_remoto_e_instalado_sem_criar_rodada_extra() {
        let mut step_meta = json!({
            "token_id": 4,
            "speculate": {"k": 4, "m": 0, "source": "mtp"}
        });

        assert!(remote_stream_install_draft(
            &mut step_meta,
            Some(&[5, 6]),
            None,
            None,
        ));
        assert_eq!(step_meta["speculate"]["draft"], json!([5, 6]));

        // The next invocation consumes this installed draft. The transport
        // still has one metadata frame per decode round, with no probe frame
        // inserted solely to carry the draft.
        assert_eq!(
            Speculate::from_meta(&step_meta).unwrap().draft,
            Some(vec![5, 6])
        );
    }

    #[test]
    fn rascunho_remoto_desativado_remove_o_estado_com_razao() {
        let mut step_meta = json!({
            "speculate": {"k": 4, "m": 0, "source": "mtp", "draft": [5, 6]}
        });

        assert!(!remote_stream_install_draft(
            &mut step_meta,
            None,
            Some(SPECULATION_REASON_MODEL_WITHOUT_MTP),
            Some("MTP desligado nesta sessão: teste"),
        ));
        assert!(step_meta.get("speculate").is_none());
        assert_eq!(
            step_meta["speculation_disabled_reason"],
            SPECULATION_REASON_MODEL_WITHOUT_MTP
        );
        assert_eq!(
            step_meta["speculation_disabled_detail"],
            "MTP desligado nesta sessão: teste"
        );
    }

    #[test]
    fn erro_depois_de_tokens_mantem_o_prefixo_e_e_terminal() {
        let (batch, error, terminal) = remote_stream_batch(&[
            token(4, "prefixo", false),
            RemoteStreamItem::Error("falha no peer seguinte".into()),
        ]);

        assert_eq!(batch["accepted"][0]["text"], "prefixo");
        assert_eq!(batch["done"], false);
        assert_eq!(error.as_deref(), Some("falha no peer seguinte"));
        assert!(terminal);
    }

    #[tokio::test]
    async fn oito_tokens_e_rascunho_cruzam_em_um_unico_callback() {
        let (items_tx, items_rx) = mpsc::channel(16);
        items_tx
            .send(token_state(0, "t0", false, false, true, Some(vec![8, 9])))
            .await
            .expect("token enfileirado");
        for id in 1..8 {
            items_tx
                .send(token_state(
                    id,
                    &format!("t{id}"),
                    id == 7,
                    false,
                    true,
                    None,
                ))
                .await
                .expect("token enfileirado");
        }
        drop(items_tx);

        let (deliveries_tx, mut deliveries_rx) = mpsc::channel(8);
        emit_remote_stream_batches(
            json!({"ticket": "test"}),
            items_rx,
            move |_reply_to, batch| {
                let deliveries_tx = deliveries_tx.clone();
                async move {
                    if deliveries_tx.send(batch).await.is_ok() {
                        crate::direct::Delivery::Accepted
                    } else {
                        crate::direct::Delivery::Failed
                    }
                }
            },
        )
        .await;

        let batch = deliveries_rx.recv().await.expect("callback");
        assert_eq!(batch["accepted"].as_array().unwrap().len(), 8);
        assert_eq!(batch["accepted"][0]["token_id"], 0);
        assert_eq!(batch["accepted"][7]["token_id"], 7);
        assert_eq!(batch["next_draft"], json!([8, 9]));
        assert!(
            deliveries_rx.try_recv().is_err(),
            "oito tokens e o rascunho, um callback"
        );
    }

    #[tokio::test]
    async fn emitter_nao_conta_fila_anterior_ao_inicio_da_janela() {
        use std::time::{Duration, Instant};

        let (items_tx, items_rx) = mpsc::channel(1);
        let mut item = token(7, "token", true);
        if let RemoteStreamItem::Token(token) = &mut item {
            let queued_at = Instant::now() - Duration::from_millis(5);
            token.ready_at = queued_at;
            token.enqueued_at = queued_at;
        }
        items_tx.send(item).await.expect("token enfileirado");
        drop(items_tx);

        let (deliveries_tx, mut deliveries_rx) = mpsc::channel(1);
        emit_remote_stream_batches(
            json!({"ticket": "split-wait"}),
            items_rx,
            move |_, batch| {
                let deliveries_tx = deliveries_tx.clone();
                async move {
                    deliveries_tx
                        .send(batch)
                        .await
                        .map(|_| crate::direct::Delivery::Accepted)
                        .unwrap_or(crate::direct::Delivery::Failed)
                }
            },
        )
        .await;

        let batch = deliveries_rx.recv().await.expect("callback");
        assert_eq!(batch["remote_stream_producer_wait_ms"], 0);
        assert_eq!(batch["remote_stream_queue_wait_ms"], 0);
    }

    #[tokio::test]
    async fn falha_de_entrega_do_stream_chega_em_lote_de_erro() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        let (items_tx, items_rx) = mpsc::channel(4);
        items_tx
            .send(token(7, "token antes da falha", true))
            .await
            .expect("token enfileirado");
        drop(items_tx);

        let attempts = Arc::new(AtomicUsize::new(0));
        let (deliveries_tx, mut deliveries_rx) = mpsc::channel(4);
        let started = Instant::now();
        emit_remote_stream_batches(json!({"ticket": "stream-falha"}), items_rx, {
            let attempts = attempts.clone();
            move |_reply_to, batch| {
                let attempts = attempts.clone();
                let deliveries_tx = deliveries_tx.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        crate::direct::Delivery::Failed
                    } else {
                        deliveries_tx
                            .send(batch)
                            .await
                            .map(|_| crate::direct::Delivery::Accepted)
                            .unwrap_or(crate::direct::Delivery::Failed)
                    }
                }
            }
        })
        .await;

        let error_batch = deliveries_rx
            .recv()
            .await
            .expect("a falha deve chegar como lote terminal");
        assert_eq!(
            error_batch["error"],
            json!("o lote de decode não chegou ao gateway por nenhum endereço de retorno")
        );
        assert!(error_batch["accepted"].as_array().unwrap().is_empty());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a falha do stream não pode esperar o deadline: {:?}",
            started.elapsed()
        );
    }
}

/// Monta o metadado que um hop de decode encaminha ao próximo peer.
///
/// A faixa carregada do próximo peer não basta para definir o que ele deve
/// executar: uma mesma fatia pode ser usada por ramos diferentes. Por isso a
/// sub-faixa é escolhida pelo `PipelineStep` do próximo hop e viaja em cada
/// quadro `/v1/step`.
pub(super) fn step_forward_meta(
    session_id: &str,
    request_meta: &Value,
    local_meta: &Value,
    next: &PipelineStep,
) -> Value {
    let mut meta = json!({
        "session_id": session_id,
        "exec_start": next.exec_start,
        "exec_end": next.exec_end,
    });
    preserve_batch_identity(request_meta, &mut meta);
    for field in [
        "draft",
        "tokens",
        "mtp_k",
        "remote_stream",
        "trace_id",
        "request_id",
        "pipeline_id",
        "telemetry_token_index",
        "telemetry_source_peer_id",
    ] {
        if let Some(value) = local_meta.get(field).or_else(|| request_meta.get(field)) {
            meta[field] = value.clone();
        }
    }
    meta
}

/// Relay a residual to `hops[0]` and bubble its reply back: the eventual
/// tail's real answer, plus this hop's own already-known contribution
/// (`local_processing_ms`/`local_node_timings`, computed by the caller from
/// its own `NodeTimings`) prepended onto whatever `hop_timings` bubbled up
/// from further down the chain. Returns the merged `meta` — *not* yet
/// re-encoded — so a caller with phase-specific fields to overlay (prefill's
/// `more_chunks`/`n_prompt_tokens`, see `handle_prefill`) can add them
/// before the final `wire::encode`.
///
/// `base_meta` carries whatever the phase needs beyond what every relay call
/// needs (`is_last`/`scale`/`next_hops`, added here): just `session_id` for
/// `/v1/step`, plus `chunk_index`/`chunk_size`/`is_final_chunk` for
/// `/v1/prefill`. `attempts` follows the same rule `send_chunk`/`step_ring`
/// already do at the gateway: `CHUNK_ATTEMPTS` for a prefill chunk (one
/// attempt, because a lost response makes KV state ambiguous), `1` for a
/// decode step (which has no retry).
///
/// Split out from `handle_step`/`handle_prefill` (both call this with the
/// same `hops`/tensor shape, differing only in `path` and `base_meta`) so
/// it's directly testable against a fake next-hop server — see the
/// `relay_tests` module — without needing a loaded `LlmEngine`.
pub(super) async fn relay_and_bubble(
    http: &crate::peer_client::PeerClients,
    hops: &[PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], TensorShape),
    dtype: WireDtype,
    attempts: u32,
    local_processing_ms: Value,
    local_node_timings: Value,
) -> std::result::Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["exec_start"] = json!(next.exec_start);
    base_meta["exec_end"] = json!(next.exec_end);

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http,
        next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some(tensor),
        dtype,
        dtype.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if sent.tensor.is_some() {
                // A fully-bubbled reply is always the tail's final answer,
                // which never carries a tensor — only a *forward* (not yet
                // relayed) residual does. Getting here means some hop in the
                // chain didn't relay when it should have; surviving loud
                // rather than silently dropping the tensor.
                warn!(
                    "resposta de {} veio com tensor num retransmissão já borbulhada — descartando",
                    next.node_id
                );
            }
            let hop_timings = crate::relay::bubble_hop_timings(&sent);
            let mut bubbled = sent.meta.clone();
            bubbled["hop_timings"] = json!(hop_timings);
            // Ours, not the next hop's: our caller needs these to build our
            // own entry, the same way we just built the next hop's.
            bubbled["processing_ms"] = local_processing_ms;
            bubbled["node_timings"] = local_node_timings;
            Ok(bubbled)
        }
        Err(e) => {
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            Err((StatusCode::BAD_GATEWAY, Json(failure)).into_response())
        }
    }
}
