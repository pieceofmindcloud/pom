use super::*;
use base64::Engine as _;

#[derive(Clone, Copy, Default)]
struct PrefixReuseSnapshot {
    session_present: bool,
    retained: bool,
    contains_media: bool,
    positions_match: bool,
    n_past: u32,
    prompt_cells: u32,
    prompt_tokens: usize,
    prompt_len: u32,
    stable_len: u32,
}

impl PrefixReuseSnapshot {
    fn from_session(session: Option<&Session>) -> Self {
        session.map_or_else(Self::default, |session| Self {
            session_present: true,
            retained: session.retained,
            contains_media: session.contains_media,
            positions_match: session.positions_match_cells(),
            n_past: session.n_past,
            prompt_cells: session.prompt_cells,
            prompt_tokens: session.prompt_tokens.len(),
            prompt_len: session.prompt_len,
            stable_len: session.stable_len,
        })
    }
}

fn prefix_reuse_gate(
    requested: bool,
    chunk_index: u32,
    session: PrefixReuseSnapshot,
) -> (bool, &'static str) {
    let reason = if !requested {
        "gateway-did-not-request"
    } else if chunk_index != 0 {
        "not-first-chunk"
    } else if !session.session_present {
        "peer-session-missing"
    } else if !session.retained {
        "peer-session-not-retained"
    } else if session.contains_media {
        "peer-session-contains-media"
    } else if !session.positions_match {
        "peer-kv-position-mismatch"
    } else {
        "eligible"
    };
    (reason == "eligible", reason)
}

#[cfg(test)]
mod prefix_reuse_gate_tests {
    use super::*;

    #[test]
    fn probe_meta_carries_labels_and_the_head_retain_boundary() {
        let probe = parse_probe(&json!({"probe": ["A", "B"], "retain_prefix_bytes": 96}))
            .unwrap()
            .unwrap();
        assert_eq!(probe.labels, ["A", "B"]);
        assert_eq!(probe.retain_prefix_bytes, Some(96));
        let bare = parse_probe(&json!({"probe": ["Yes", "No"]}))
            .unwrap()
            .unwrap();
        assert_eq!(bare.retain_prefix_bytes, None);

        // A retain boundary without labels: nothing is scored, the boundary stays.
        let boundary = parse_probe(&json!({"retain_prefix_bytes": 512}))
            .unwrap()
            .unwrap();
        assert!(boundary.labels.is_empty());
        assert_eq!(boundary.retain_prefix_bytes, Some(512));
        assert!(parse_probe(&json!({})).unwrap().is_none());
        assert!(parse_probe(&json!({"probe": []})).is_err());
    }

    #[test]
    fn gate_names_missing_and_inconsistent_peer_sessions() {
        assert_eq!(
            prefix_reuse_gate(true, 0, PrefixReuseSnapshot::default()),
            (false, "peer-session-missing")
        );

        let mut session = Session::new(0);
        session.retained = true;
        session.n_past = 12;
        session.prompt_cells = 9;
        let snapshot = PrefixReuseSnapshot::from_session(Some(&session));
        assert_eq!(
            prefix_reuse_gate(true, 0, snapshot),
            (false, "peer-kv-position-mismatch")
        );

        session.prompt_cells = session.n_past;
        let snapshot = PrefixReuseSnapshot::from_session(Some(&session));
        assert_eq!(prefix_reuse_gate(true, 0, snapshot), (true, "eligible"));

        session.contains_media = true;
        let snapshot = PrefixReuseSnapshot::from_session(Some(&session));
        assert_eq!(
            prefix_reuse_gate(true, 0, snapshot),
            (false, "peer-session-contains-media")
        );
    }
}

pub(super) async fn handle_prefill(
    State(app): State<InferenceAppState>,
    handshake: Option<axum::Extension<crate::serve::TlsHandshake>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    // Taken first, whatever path the frame follows below, so the connection's
    // handshake is reported by the request that opened it and by no other.
    let tls_handshake_ms = handshake.map_or(0, |handshake| handshake.take_ms());
    app.request_activity.mark_now();
    let _load_guard = app.loads.acquire(&app.load_node_id).await;
    // See the twin note in `handle_step`: the header is enough to know whether
    // the frame is a batch.
    if wire::peek_batch_header(&body).is_ok_and(|header| header.item_count > 1) {
        match wire::decode_batch(&body) {
            Ok(batch) => return handle_prefill_batch(app, headers, batch).await,
            Err(error) => return err400(format!("invalid frame: {error}")),
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
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor, inbound_wire_timing) = match wire::decode_inference_request_profiled(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("invalid frame: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;
    let wire_dtype = match meta.get("dtype").and_then(|value| value.as_str()) {
        Some(value) => match WireDtype::parse(value) {
            Ok(dtype) => dtype,
            Err(error) => return err400(error),
        },
        None => return err400("frame without a hidden state dtype"),
    };

    let sid_key = match required_session_id(&meta) {
        Ok(session_id) => session_id,
        Err(error) => return err400(error),
    };
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
    let pipeline_session = meta
        .get("pipeline_session")
        .and_then(Value::as_bool)
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
    let next_pos = meta
        .get("next_pos")
        .and_then(|v| v.as_u64())
        .and_then(|value| u32::try_from(value).ok());
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
                return err400("invalid images field");
            };
            let mut decoded = Vec::with_capacity(values.len());
            for value in values {
                let Some(encoded) = value.as_str() else {
                    return err400("images field contains an invalid item");
                };
                match base64::engine::general_purpose::STANDARD.decode(encoded) {
                    Ok(bytes) => decoded.push(bytes),
                    Err(error) => return err400(format!("invalid encoded image: {error}")),
                }
            }
            Some(decoded)
        }
    };
    let chat_template = meta.get("chat_template").and_then(|v| v.as_bool());
    let thinking_enabled = meta.get("thinking_enabled").and_then(|v| v.as_bool());
    let probe = match parse_probe(&meta) {
        Ok(probe) => probe,
        Err(error) => return err400(error),
    };
    // The token composition of this chunk, produced at the head (which tokenized
    // the prompt) and echoed down to the tail - it is what feeds the `observe` of
    // the `nextn` head in `decode_hidden` (see `prefill_blocking`). Absent means
    // an old peer.
    let chunk_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let mrope_positions = match meta.get("mrope_positions") {
        None => None,
        Some(value) => match serde_json::from_value::<Vec<[u32; 4]>>(value.clone()) {
            Ok(positions) => Some(positions),
            Err(error) => return err400(format!("invalid mrope_positions field: {error}")),
        },
    };

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

    // The sub-range the sender asked to run. A loud refusal when this node
    // cannot run exactly that - never a silent adjustment.
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
            Err(reason) => return err400(reason),
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
        next_pos,
        stable_len,
        is_last,
        is_final_chunk,
        wire_dtype,
        prompt,
        images,
        mrope_positions,
        chat_template,
        thinking_enabled,
        probe,
        tensor,
        capture_block,
        chunk_tokens,
        queued_at,
        scheduler_wait_us: 0,
        scheduler_wait_reason: None,
        scheduler_session_existing: false,
        pipeline_session,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("inference queue full (limit {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("inference scheduler shut down");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code =
                if e.to_string().contains("unknown") || e.to_string().contains("out of order") {
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

    // Same reasoning as handle_step: decode once so the relay branch below
    // can inspect what came back, patch in this hop's own processing_ms/
    // node_timings either way, and only then decide whether to relay or
    // just re-encode and return.
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
    if let Some(value) = meta.get("thinking_enabled") {
        local_meta["thinking_enabled"] = value.clone();
    }
    if let Some(value) = meta.get("probe") {
        local_meta["probe"] = value.clone();
    }
    preserve_batch_identity(&meta, &mut local_meta);

    // Direct reply - see the twin block in `handle_step`. The difference is
    // `prompt_progress`: in the bubbled ring the head overwrote
    // `more_chunks`/`n_prompt_tokens` in the reply that passed back through it
    // (`overlay_head_authoritative_fields`). With no way back, it sends both
    // **forward**, and whoever delivers copies them - only the head tokenized the
    // prompt, and without this every prompt longer than one chunk would be
    // truncated at the first.
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
                message: "intermediate hop produced no hidden state".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = json!({
            "session_id": sid_for_forward,
            "pipeline_session": true,
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
        if let Some(value) = local_meta.get("thinking_enabled") {
            forward["thinking_enabled"] = value.clone();
        }
        if let Some(value) = local_meta.get("probe") {
            forward["probe"] = value.clone();
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
        if let Some(positions) = local_meta.get("mrope_positions") {
            forward["mrope_positions"] = positions.clone();
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
            crate::relay::CHUNK_ATTEMPTS, // a lost reply requires a cold rebuild
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
    let ring_overlap = meta
        .get("ring_overlap")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Overlapping ring: the forward of this session's previous chunk has to
    // finish before this one goes on, so the next hop receives the chunks in
    // order. Waiting for it here, after the local compute, is what overlaps
    // this hop's compute on chunk k+1 with the next hop's on chunk k.
    let relay_slot = ring_overlap.then(|| ring_relay_slot(&sid_for_forward));
    let mut relay_guard = match &relay_slot {
        Some(slot) => Some(slot.clone().lock_owned().await),
        None => None,
    };
    let previous = match relay_guard.as_mut().and_then(|guard| guard.take()) {
        Some(pending) => match pending.await {
            // Chunk 0 of a new session: the leftover forward belongs to an
            // abandoned request and says nothing about this one.
            _ if chunk_index == 0 => None,
            Ok(Ok(bubbled)) => Some(bubbled),
            Ok(Err(response)) => {
                forget_ring_relay_slot(&sid_for_forward);
                return response;
            }
            Err(join) => {
                forget_ring_relay_slot(&sid_for_forward);
                return err500(anyhow::anyhow!(
                    "forward of the previous chunk aborted: {join}"
                ));
            }
        },
        None => None,
    };

    let relay_meta = {
        let mut relay_meta = json!({
            "session_id": sid_for_forward,
            "pipeline_session": true,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": reuse_prefix_for_forward(&local_meta, reuse_prefix, chunk_index),
            "is_final_chunk": is_final_chunk_for_forward,
            // The next hop receives its own sub-range, for the same reason
            // this one received its own: the sender is who decides the piece.
            "exec_start": hops[0].exec_start,
            "exec_end": hops[0].exec_end,
        });
        if let Some(tokens) = local_meta.get("tokens") {
            relay_meta["tokens"] = tokens.clone();
        }
        if let Some(positions) = local_meta.get("mrope_positions") {
            relay_meta["mrope_positions"] = positions.clone();
        }
        for field in ["start_pos", "next_pos", "stable_len"] {
            if let Some(value) = local_meta.get(field) {
                relay_meta[field] = value.clone();
            }
        }
        if let Some(value) = local_meta.get("prefix_match_tokens") {
            relay_meta["prefix_match_tokens"] = value.clone();
        }
        if let Some(value) = local_meta.get("thinking_enabled") {
            relay_meta["thinking_enabled"] = value.clone();
        }
        if let Some(value) = local_meta.get("probe") {
            relay_meta["probe"] = value.clone();
        }
        if ring_overlap {
            relay_meta["ring_overlap"] = json!(true);
        }
        preserve_batch_identity(&meta, &mut relay_meta);
        relay_meta
    };

    if let (Some(guard), false) = (relay_guard.as_mut(), is_final_chunk_for_forward) {
        // Intermediate chunk: it goes on to the next hop in the background and
        // the reply leaves right away, with what only this hop knows (position,
        // tokens) and the telemetry of the following hops on the previous chunk.
        // A failure of that forward comes back on the next chunk as a 502, which
        // the gateway treats as a rebuild - never as a resume.
        let processing_ms = local_meta["processing_ms"].clone();
        let node_timings = local_meta["node_timings"].clone();
        **guard = Some(tokio::spawn(async move {
            relay_and_bubble(
                http_client(),
                &hops,
                "/v1/prefill",
                relay_meta,
                (&residual, shape),
                wire_dtype,
                crate::relay::CHUNK_ATTEMPTS,
                processing_ms,
                node_timings,
            )
            .await
        }));
        let mut early = local_meta.clone();
        early["hop_timings"] = previous
            .as_ref()
            .and_then(|bubbled| bubbled.get("hop_timings"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        overlay_head_authoritative_fields(&local_meta, &mut early);
        return match wire::encode_batch_single(&early, None, wire_dtype) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    let bubbled = relay_and_bubble(
        http_client(),
        &hops,
        "/v1/prefill",
        relay_meta,
        (&residual, shape),
        wire_dtype,
        crate::relay::CHUNK_ATTEMPTS, // a lost reply requires a cold rebuild
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await;
    if relay_guard.take().is_some() {
        forget_ring_relay_slot(&sid_for_forward);
    }
    let mut bubbled = match bubbled {
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

/// The background forward of the last intermediate chunk of a session
/// (overlapping ring).
type RingRelay = tokio::task::JoinHandle<std::result::Result<Value, Response>>;
type RingRelaySlot = Arc<tokio::sync::Mutex<Option<RingRelay>>>;

fn ring_relay_slots() -> &'static std::sync::Mutex<HashMap<String, RingRelaySlot>> {
    static SLOTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, RingRelaySlot>>> =
        std::sync::OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

fn ring_relay_slot(session: &str) -> RingRelaySlot {
    let mut slots = ring_relay_slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A session abandoned in the middle of the prompt leaves its slot behind;
    // the ones with no pending forward and nobody waiting are dropped here.
    if slots.len() > 256 {
        slots.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || slot
                    .try_lock()
                    .map(|pending| pending.as_ref().is_some_and(|task| !task.is_finished()))
                    .unwrap_or(true)
        });
    }
    slots.entry(session.to_string()).or_default().clone()
}

fn forget_ring_relay_slot(session: &str) {
    ring_relay_slots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session);
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
/// Which prefix decision the next hop must obey.
///
/// The one of the peer that tokenized, when it reported it (`reuse_prefix` in
/// its own meta), not the one the gateway asked for. The head may have
/// restarted the session - diverging prefix, evicted session - and a hop
/// reusing while the head restarted would leave the pipeline positions
/// misaligned. A peer that reports nothing falls back to the original request,
/// which is the previous behavior.
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

pub(super) fn is_final_chunk_for_forward(
    local_meta: &Value,
    received_is_final_chunk: bool,
) -> bool {
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
            // How many prompt tokens were not reprocessed, and whether the
            // prefix was actually reused. Only the head knows - the tail
            // answers about a prompt it never tokenized.
            "reuse_prefix",
            "cached_tokens",
            "prefix_match_tokens",
            "stable_len",
            "mrope_positions",
        ] {
            if let Some(value) = local_meta.get(field) {
                bubbled[field] = value.clone();
            }
        }
    }
}

/// The same two authoritative fields, pushed **forward** instead of overlaid
/// on the way back - it is what the direct reply needs, since the head never
/// sees the result again. Whoever is not the head passes on untouched what it
/// received.
pub(crate) fn prompt_progress_for_forward(inbound_meta: &Value, local_meta: &Value) -> Value {
    if owns_tokens(local_meta) {
        return json!({
            "more_chunks": local_meta["more_chunks"].clone(),
            "n_prompt_tokens": local_meta["n_prompt_tokens"].clone(),
            "start_pos": local_meta["start_pos"].clone(),
            "next_pos": local_meta["next_pos"].clone(),
            // See `overlay_head_authoritative_fields`: only the head knows.
            "reuse_prefix": local_meta["reuse_prefix"].clone(),
            "cached_tokens": local_meta["cached_tokens"].clone(),
            "prefix_match_tokens": local_meta["prefix_match_tokens"].clone(),
            "stable_len": local_meta["stable_len"].clone(),
            "mrope_positions": local_meta["mrope_positions"].clone(),
        });
    }
    inbound_meta
        .get("prompt_progress")
        .cloned()
        .unwrap_or(Value::Null)
}

/// The end of the chain copies into the result what the head worked out about
/// the prompt. Without this the gateway would read the `more_chunks: false` the
/// tail always answers - it tokenized nothing - and would stop the prefill at
/// the first chunk.
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
        "mrope_positions",
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
    for field in ["item_id", "batch_id", "trace_id"] {
        if let Some(value) = request_meta.get(field) {
            response_meta[field] = value.clone();
        }
    }
}

/// Refuses a prefill chunk larger than the context's `n_batch` **before**
/// calling `llama_decode`, which fires `GGML_ASSERT(n_tokens_all <=
/// cparams.n_batch)` and aborts the whole process. The gateway already clamps
/// the chunk size in `prompt_chunk_tokens`; this guard covers a head whose
/// `n_batch` is smaller than that of the node that planned, and the isolated
/// item the scheduler lets through. The error names the knobs to adjust.
pub(super) fn guard_chunk_fits_batch(
    engine: &LlmEngine,
    n_tokens: usize,
    chunk_index: u32,
) -> Result<()> {
    let n_batch = engine.n_batch();
    if n_tokens as u64 > n_batch as u64 {
        bail!(
            "chunk {chunk_index} has {n_tokens} tokens, above the n_batch={n_batch} of this node; llama.cpp would abort. Reduce prefill_chunk_tokens to <= n_batch, or increase n_batch, in the advanced settings of the deployment"
        );
    }
    Ok(())
}

/// Decide whether the head has another physical prompt chunk to ingest.
///
/// `n_prompt_cells` is the number of embedding/token cells written to KV;
/// `next_pos` is the logical position returned by the engine. They are equal
/// for text, but deliberately differ for an M-RoPE image. A multimodal decode
/// consumes all of its mtmd chunks in one call, so it is complete regardless
/// of that difference.
fn prefill_has_more_chunks(
    owns_tokens: bool,
    n_prompt_cells: u32,
    next_pos: u32,
    multimodal_complete: bool,
) -> bool {
    owns_tokens && !multimodal_complete && next_pos < n_prompt_cells
}

/// Return the logical position after a hidden-state batch whose physical
/// cells carry M-RoPE coordinates. The largest temporal/spatial coordinate is
/// the authoritative logical end; the number of embedding rows is not.
fn logical_end_from_mrope_positions(
    start_pos: u32,
    positions: &[[u32; 4]],
    physical_cells: usize,
    declared_next_pos: Option<u32>,
) -> Result<u32> {
    if positions.len() != physical_cells {
        bail!(
            "hidden state declares {physical_cells} cells, but received {} M-RoPE positions",
            positions.len()
        );
    }
    let next_pos = declared_next_pos.ok_or_else(|| {
        anyhow::anyhow!("M-RoPE hidden state without a logical next_pos produced by the head")
    })?;
    if next_pos < start_pos {
        bail!("M-RoPE next_pos {next_pos} goes back from the start position {start_pos}");
    }
    Ok(next_pos)
}

fn apply_mrope_positions(meta: &mut Value, positions: Option<&[[u32; 4]]>) {
    if let Some(positions) = positions {
        meta["mrope_positions"] = json!(positions);
    }
}

/// A typed decision as a peer sees it: the labels to score (last peer) and,
/// on the head, where the reusable part of the prompt text ends.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HeadProbe {
    pub(super) labels: Vec<String>,
    pub(super) retain_prefix_bytes: Option<usize>,
}

/// Labels of a typed decision (`probe`): 1 to 256 short texts.
pub(super) fn parse_probe(meta: &Value) -> std::result::Result<Option<HeadProbe>, String> {
    let retain_prefix_bytes = meta
        .get("retain_prefix_bytes")
        .and_then(Value::as_u64)
        .map(|bytes| bytes as usize);
    let Some(value) = meta.get("probe").filter(|value| !value.is_null()) else {
        // No labels to score, but the gateway may still say where the
        // reusable part of the prompt ends (retrieved context after it).
        return Ok(retain_prefix_bytes.map(|bytes| HeadProbe {
            labels: Vec::new(),
            retain_prefix_bytes: Some(bytes),
        }));
    };
    let labels: Vec<String> = serde_json::from_value(value.clone())
        .map_err(|_| "probe field must be a list of labels".to_owned())?;
    if labels.is_empty() || labels.len() > 256 {
        return Err("probe field accepts 1 to 256 labels".into());
    }
    if labels
        .iter()
        .any(|label| label.trim().is_empty() || label.len() > 32)
    {
        return Err("probe label empty or longer than 32 bytes".into());
    }
    Ok(Some(HeadProbe {
        labels,
        retain_prefix_bytes,
    }))
}

/// Ids of the first token of each label, with and without a leading space: the
/// model may start the answer either way.
pub(super) fn probe_token_groups(engine: &LlmEngine, labels: &[String]) -> Vec<Vec<i32>> {
    labels
        .iter()
        .map(|label| {
            let mut ids = Vec::new();
            for text in [label.clone(), format!(" {label}")] {
                if let Some(first) = engine.tokenize(&text, false, false).first() {
                    if !ids.contains(first) {
                        ids.push(*first);
                    }
                }
            }
            ids
        })
        .collect()
}

/// Probability of each label at the position right after the prompt.
pub(super) fn probe_reply(engine: &LlmEngine, logits: &[f32], labels: &[String]) -> Value {
    let groups = probe_token_groups(engine, labels);
    let result = crate::llm::probe_candidates(logits, &groups);
    json!({
        "labels": labels,
        "probabilities": result.probabilities,
        "mass": result.mass,
    })
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
    next_pos: Option<u32>,
    stable_len: Option<u32>,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    images: Option<Vec<Vec<u8>>>,
    chat_template: Option<bool>,
    thinking_enabled: Option<bool>,
    probe: Option<HeadProbe>,
    tensor: Option<(Vec<f32>, TensorShape)>,
    capture_block: Option<u32>,
    // The token composition of this chunk, coming from the head via `tokens` on
    // the wire. It only makes sense in the hidden state branch (the head
    // tokenizes the prompt; middle hops echo): it feeds the `observe` of the
    // `nextn` head inside `decode_hidden`. `None` is an old peer that does not
    // forward the field - `decode_hidden` turns MTP off with a warning.
    chunk_tokens: Option<Vec<i32>>,
    mrope_positions: Option<Vec<[u32; 4]>>,
    queued_at: Instant,
    scheduler_wait_us: u64,
    scheduler_wait_reason: Option<&'static str>,
    scheduler_session_existing: bool,
    pipeline_session: bool,
) -> Result<TimedReply> {
    let mut is_final_chunk = is_final_chunk;
    let owns_prompt_tokens = prompt.is_some();
    let has_images = images.as_ref().is_some_and(|images| !images.is_empty());
    let mut timings = NodeTimings {
        session_id: Some(sid_key.clone()),
        queue_us: queued_at.elapsed().as_micros() as u64,
        scheduler_wait_us,
        scheduler_wait_reason,
        scheduler_session_existing,
        pipeline_session,
        ..Default::default()
    };
    // The prefill waits on the same mutex as the decode: a large prompt from
    // another session holds this one back, and it is the same invisible wait.
    let lock_started = Instant::now();
    let mut s = lock_inference_state(&state)?;
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;
    timings.selected_path = s.inference_path;
    s.note_thinking(&sid_key, thinking_enabled);

    // ── Session setup ────────────────────────────────────────────────────────

    let setup_started = Instant::now();
    // Reusing requires the session alive here. It may have been evicted or
    // recycled since the previous turn; in that case this peer simply restarts,
    // and it is its answer (`reuse_prefix` in the meta) that the next hops obey.
    if reuse_prefix && chunk_index == 0 {
        // The session may be parked in host RAM (`--cache-ram`).
        s.unpark_session(&sid_key);
    }
    let reuse_snapshot = PrefixReuseSnapshot::from_session(s.sessions.get(&sid_key));
    let (want_reuse, reuse_gate_reason) =
        prefix_reuse_gate(reuse_prefix, chunk_index, reuse_snapshot);
    if chunk_index == 0 {
        info!(
            "prefix peer eligibility: session={} requested={} eligible={} reason={} retained={} contains_media={} positions_match={} n_past={} prompt_cells={} prompt_tokens={} prompt_len={} stable_len={}",
            &sid_key[..8.min(sid_key.len())],
            reuse_prefix,
            want_reuse,
            reuse_gate_reason,
            reuse_snapshot.retained,
            reuse_snapshot.contains_media,
            reuse_snapshot.positions_match,
            reuse_snapshot.n_past,
            reuse_snapshot.prompt_cells,
            reuse_snapshot.prompt_tokens,
            reuse_snapshot.prompt_len,
            reuse_snapshot.stable_len,
        );
    }
    s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
    let mut prefix = PrefixDecision::default();
    // Whoever does not tokenize receives the boundary from whoever did, to
    // split the prompt at the same position - see `Session::stable_len`.
    if let Some(stable) = stable_len {
        s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
    }

    // First peer: tokenize the prompt on chunk 0.
    // We must not hold a &mut Session borrow while calling engine methods, so
    // compute everything we need, store results, then re-borrow.
    if let Some(ref text) = prompt {
        if chunk_index == 0 && !has_images {
            let use_tmpl = use_chat_template(chat_template, s.chat_template);
            let no_think = s.no_thinking || !thinking_enabled.unwrap_or(!s.no_thinking);
            let formatted = if use_tmpl {
                s.engine
                    .apply_chat_template(&[("user", text)], true, no_think)?
            } else {
                text.clone()
            };
            let add_bos = !use_tmpl;
            let tokens = s.engine.tokenize(&formatted, add_bos, true);
            let new_prompt_tokens = tokens.len();
            check_prompt_fits_session(&s, new_prompt_tokens)?;
            let mut stable = stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
            if let Some(bytes) = probe.as_ref().and_then(|probe| probe.retain_prefix_bytes) {
                stable = stable.min(bounded_stable_len(
                    &s.engine, text, bytes, use_tmpl, no_think, &tokens,
                ) as u32);
            }
            prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
            if chunk_index == 0 {
                info!(
                    "prefix peer comparison: session={} requested={} eligible={} reason={} old_tokens={} new_tokens={} stable_len={} matched_tokens={} reused={} cached_tokens={}",
                    &sid_key[..8.min(sid_key.len())],
                    reuse_prefix,
                    want_reuse,
                    reuse_gate_reason,
                    reuse_snapshot.prompt_tokens,
                    new_prompt_tokens,
                    stable,
                    prefix.matched_tokens,
                    prefix.reused,
                    prefix.cached_tokens,
                );
            }
        }
    }
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    // ── Rollback / order check ───────────────────────────────────────────────
    let expected_pos = expected_prefill_pos(start_pos, chunk_index, s.sessions[&sid_key].n_past)?;
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if session.n_past > expected_pos {
            warn!(
                "redoing chunk {chunk_index} of session {}… (pos {expected_pos})",
                &sid_key[..8.min(sid_key.len())]
            );
            let seq_id = session.seq_id();
            if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                bail!(
                    "this model does not accept discarding positions from the middle of the cache, so redoing chunk {chunk_index} would corrupt the session"
                );
            }
            s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
        } else if session.n_past < expected_pos {
            bail!(
                "chunk out of order: received {chunk_index} (pos {expected_pos}) but the session is at {}",
                session.n_past
            );
        }
    }

    // ── Decode ───────────────────────────────────────────────────────────────
    // Extract the session values we need as plain integers, drop the borrow,
    // then call the engine (which needs &mut s.engine).
    // From the boundary on, the prompt stops being reusable and lives in the
    // generation copy together with the generated tokens - see
    // `Session::stable_len`. A chunk never crosses the boundary: it stops at it,
    // and the next one already starts in the other sequence.
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

    // Where to stop, when the sender asked for less than the whole slice. Before
    // the decode and under the same lock, which is what makes the adjustment
    // valid only for this decode.
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
    let mut multimodal_complete = false;
    let mut mrope_positions = mrope_positions;
    let decode_started = Instant::now();
    let engine_timing;
    if let Some((hidden, shape)) = tensor {
        n_tokens = shape[0];
        guard_chunk_fits_batch(&s.engine, n_tokens, chunk_index)?;
        engine_timing = s.engine.decode_hidden_with_positions(
            &hidden,
            &shape,
            n_tokens,
            n_past,
            seq_id,
            !is_last,
            chunk_tokens.as_deref(),
            mrope_positions.as_deref(),
        )?;
    } else if has_images {
        if chunk_index != 0 || tensor.is_some() {
            bail!("images can only accompany the first prefill chunk");
        }
        let text = prompt
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("image without a prompt"))?;
        let use_tmpl = use_chat_template(chat_template, s.chat_template);
        let no_think = s.no_thinking || !thinking_enabled.unwrap_or(!s.no_thinking);
        let formatted = if use_tmpl {
            s.engine
                .apply_chat_template(&[("user", text)], true, no_think)?
        } else {
            text.to_string()
        };
        let decoded = s.engine.decode_multimodal(
            &formatted,
            images.as_deref().unwrap_or_default(),
            n_past,
            seq_id,
            !is_last,
        )?;
        n_tokens = decoded.n_tokens;
        multimodal_n_past = Some(decoded.n_past);
        mrope_positions = decoded.mrope_positions;
        multimodal_complete = true;
        engine_timing = decoded.timings;
    } else {
        if tokens_for_chunk.is_empty() {
            bail!("chunk {chunk_index} has no tokens");
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
    s.perf.observe_prefill(decode_started.elapsed(), n_tokens);
    timings.tp = crate::tp::take_decode_telemetry();
    timings.backend_call_us += engine_timing.backend_call_us;
    timings.backend_sync_us += engine_timing.backend_sync_us;
    timings.mtp_observe_us += engine_timing.mtp_observe_us;
    timings.merge_engine_capture(&engine_timing);

    // ── Session update ───────────────────────────────────────────────────────
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if let Some(decoded_n_past) = multimodal_n_past {
            session.n_past = decoded_n_past;
            session.prompt_tokens = vec![0; n_tokens];
            session.prompt_cells = n_tokens as u32;
            session.contains_media = true;
            session.stable_len = decoded_n_past;
        } else if let Some(positions) = mrope_positions.as_deref() {
            session.n_past =
                logical_end_from_mrope_positions(n_past, positions, n_tokens, next_pos)?;
            session.record_prompt_sequence_cells(seq_id, n_tokens as u32);
        } else {
            session.n_past += n_tokens as u32;
            session.record_prompt_sequence_cells(seq_id, n_tokens as u32);
        }
        session.last_used = Instant::now();
    }

    let (total_tokens, n_past_now) = {
        let session = &s.sessions[&sid_key];
        (session.prompt_tokens.len() as u32, session.n_past)
    };
    let owns_tokens = total_tokens > 0; // this peer has prompt_tokens → token path
    let more = prefill_has_more_chunks(owns_tokens, total_tokens, n_past_now, multimodal_complete);
    if owns_tokens && !more {
        is_final_chunk = true;
    }
    // The boundary between prompt and generation, recorded by each peer on the
    // chunk where the prompt ends. This is where the end of the turn returns the
    // session to, and it is what `/v1/session/rewind` checks. Middle hops
    // tokenize nothing, so they receive `is_final_chunk` from the head instead
    // of deducing it.
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
        let probe_result = probe
            .as_ref()
            .filter(|probe| !probe.labels.is_empty())
            .map(|probe| {
                let mut reply = probe_reply(&s.engine, logits, &probe.labels);
                // The last peer's forward pass plus the label read.
                reply["probe_ms"] = json!(decode_started.elapsed().as_secs_f64() * 1000.0);
                reply
            });
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
        // The replica of a TP group does not end a session: the one that decides
        // the end of the generation is rank 0. See `tp::is_replica`.
        //
        // Ending here destroyed the session before the gateway's rewind
        // arrived, and the rewind answered "unknown session" - the prefix never
        // entered the cache. The end of the turn now **retains** the prompt:
        // only the generation sequence goes away. Releasing for good remains an
        // explicit act of the gateway (`/v1/session/release`).
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
            .ok_or_else(|| anyhow::anyhow!("session '{sid_key}' disappeared before the response"))?
            .prompt_tokens
            .len();
        let mut reply = terminal_prefill_meta(
            n_prompt_tokens,
            expected_pos,
            expected_pos + n_tokens as u32,
            done,
        );
        if let Some(result) = probe_result {
            reply["probe_result"] = result;
        }
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
        apply_mrope_positions(&mut reply, mrope_positions.as_deref());
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
    apply_mrope_positions(&mut reply_meta, mrope_positions.as_deref());
    // The real composition of this chunk, so the tail keeps the `nextn` head up
    // to date (see the doc of `chunk_tokens`). The head knows the tokens; middle
    // hops echo what they received.
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
    /// What this peer did with the retained prefix - see `apply_prefix_decision`.
    pub(super) prefix: PrefixDecision,
    /// Decision received from the head to echo when this peer does not tokenize.
    pub(super) requested_reuse: bool,
    pub(super) requested_cached_tokens: u32,
    pub(super) requested_prefix_match_tokens: u32,
    /// The measured boundary - see `Session::stable_len`.
    pub(super) stable_len: u32,
    pub(super) is_last: bool,
    pub(super) is_final_chunk: bool,
    pub(super) owns_tokens: bool,
    pub(super) tokens_for_chunk: Vec<i32>,
    pub(super) chunk_tokens: Option<Vec<i32>>,
    pub(super) mrope_positions: Option<Vec<[u32; 4]>>,
    pub(super) next_pos: Option<u32>,
    pub(super) input: PreparedPrefillInput,
    pub(super) wire_dtype: WireDtype,
    pub(super) capture_block: Option<u32>,
    pub(super) queued_at: Instant,
    pub(super) scheduler_wait_us: u64,
    pub(super) scheduler_wait_reason: Option<&'static str>,
    pub(super) scheduler_session_existing: bool,
    pub(super) pipeline_session: bool,
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
            mrope_positions,
            next_pos,
            chat_template,
            thinking_enabled,
            // Requests with a probe never join a batch (`execute_scheduled_batch`).
            probe: _probe,
            tensor,
            capture_block,
            chunk_tokens,
            queued_at,
            scheduler_wait_us,
            scheduler_wait_reason,
            scheduler_session_existing,
            pipeline_session,
        } = job;

        let result = (|| -> Result<PreparedPrefill> {
            // Same contract as the non-batched path (`prefill_blocking`): reuse
            // only with the session alive, and whoever tokenizes decides whether
            // the prefix fits.
            if reuse_prefix && chunk_index == 0 {
                s.unpark_session(&sid_key);
            }
            s.note_thinking(&sid_key, thinking_enabled);
            let reuse_snapshot = PrefixReuseSnapshot::from_session(s.sessions.get(&sid_key));
            let (want_reuse, reuse_gate_reason) =
                prefix_reuse_gate(reuse_prefix, chunk_index, reuse_snapshot);
            if chunk_index == 0 {
                info!(
                "prefix peer eligibility: session={} requested={} eligible={} reason={} retained={} contains_media={} positions_match={} n_past={} prompt_cells={} prompt_tokens={} prompt_len={} stable_len={}",
                    &sid_key[..8.min(sid_key.len())],
                    reuse_prefix,
                want_reuse,
                reuse_gate_reason,
                reuse_snapshot.retained,
                reuse_snapshot.contains_media,
                reuse_snapshot.positions_match,
                    reuse_snapshot.n_past,
                    reuse_snapshot.prompt_cells,
                    reuse_snapshot.prompt_tokens,
                    reuse_snapshot.prompt_len,
                    reuse_snapshot.stable_len,
                );
            }
            s.get_or_create(&sid_key, chunk_index == 0 && !want_reuse)?;
            let mut prefix = PrefixDecision::default();
            // See `prefill_blocking`: whoever does not tokenize receives the boundary.
            if let Some(stable) = stable_len {
                s.sessions.get_mut(&sid_key).unwrap().stable_len = stable;
            }
            if let Some(text) = prompt.as_deref() {
                if chunk_index == 0 {
                    let use_tmpl = use_chat_template(chat_template, s.chat_template);
                    let no_think = s.no_thinking || !thinking_enabled.unwrap_or(!s.no_thinking);
                    let formatted = if use_tmpl {
                        s.engine
                            .apply_chat_template(&[("user", text)], true, no_think)?
                    } else {
                        text.to_string()
                    };
                    let tokens = s.engine.tokenize(&formatted, !use_tmpl, true);
                    let new_prompt_tokens = tokens.len();
                    check_prompt_fits_session(&s, new_prompt_tokens)?;
                    let stable =
                        stable_prefix_len(&s.engine, text, use_tmpl, no_think, &tokens) as u32;
                    prefix = s.adopt_prompt_prefix(&sid_key, tokens, stable, want_reuse)?;
                    if chunk_index == 0 {
                        info!(
                            "prefix peer comparison: session={} requested={} eligible={} reason={} old_tokens={} new_tokens={} stable_len={} matched_tokens={} reused={} cached_tokens={}",
                            &sid_key[..8.min(sid_key.len())],
                            reuse_prefix,
                            want_reuse,
                            reuse_gate_reason,
                            reuse_snapshot.prompt_tokens,
                            new_prompt_tokens,
                            stable,
                            prefix.matched_tokens,
                            prefix.reused,
                            prefix.cached_tokens,
                        );
                    }
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
                            "this model does not accept discarding positions from the middle of the cache, so redoing chunk {chunk_index} would corrupt the session"
                        );
                    }
                    s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
                } else if session.n_past < expected_pos {
                    bail!(
                        "chunk out of order: received {chunk_index} (pos {expected_pos}) but the session is at {}",
                        session.n_past
                    );
                }
            }

            // See `prefill_blocking`: the chunk stops at the boundary, and from
            // there on it decodes in the generation copy.
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
                        &format!("hidden state of {sid_key}"),
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
                        bail!("chunk {chunk_index} has no tokens");
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
                mrope_positions,
                next_pos,
                input,
                wire_dtype,
                capture_block,
                queued_at,
                scheduler_wait_us,
                scheduler_wait_reason,
                scheduler_session_existing,
                pipeline_session,
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
    // Where to stop holds for the whole graph, not per item: a batch with
    // requests for different sub-ranges has no right answer. Batching is a
    // performance convenience; it cannot decide this on its own.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "batch with different sub-ranges: each would stop at a block, and the graph is a single one"
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

    // A `llama_decode` with more tokens than `n_batch` aborts the process. The
    // scheduler already cuts the batch at `n_batch`, but lets through an isolated
    // item larger than that; a refusal here is one lost request, not the whole node.
    let batch_tokens: usize = prepared.iter().map(|(_, item)| item.n_tokens).sum();
    if let Err(error) = guard_chunk_fits_batch(&s.engine, batch_tokens, prepared[0].1.chunk_index) {
        let message = error.to_string();
        for (reply, _) in prepared {
            replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
        }
        return replies;
    }

    let decode_started = Instant::now();
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
                    mrope_positions: item.mrope_positions.clone(),
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
            if let Some(positions) = item.mrope_positions.as_deref() {
                session.n_past = logical_end_from_mrope_positions(
                    session.n_past,
                    positions,
                    item.n_tokens,
                    item.next_pos,
                )?;
                session.record_prompt_sequence_cells(item.seq_id, item.n_tokens as u32);
            } else {
                session.n_past = session.n_past.saturating_add(item.n_tokens as u32);
                session.record_prompt_sequence_cells(item.seq_id, item.n_tokens as u32);
            }
            session.last_used = Instant::now();
            let total_tokens = session.prompt_tokens.len() as u32;
            let n_past_now = session.n_past;
            let more = prefill_has_more_chunks(item.owns_tokens, total_tokens, n_past_now, false);
            let mut is_final_chunk = item.is_final_chunk;
            if item.owns_tokens && !more {
                is_final_chunk = true;
            }
            // See `prefill_blocking`: the prompt/generation boundary of this session.
            if is_final_chunk {
                let session = s.sessions.get_mut(&item.sid_key).unwrap();
                session.prompt_len = n_past_now.min(session.stable_len);
            }

            let mut timings = NodeTimings {
                session_id: Some(item.sid_key.clone()),
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                scheduler_wait_us: item.scheduler_wait_us,
                scheduler_wait_reason: item.scheduler_wait_reason,
                scheduler_session_existing: item.scheduler_session_existing,
                pipeline_session: item.pipeline_session,
                lock_wait_us,
                decode_us: engine_timings.backend_call_us,
                backend_sync_us: engine_timings.backend_sync_us,
                ..Default::default()
            };
            timings.merge_engine_capture(&engine_timings);
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
                        n_past_now,
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
                    apply_mrope_positions(&mut meta, item.mrope_positions.as_deref());
                    let bytes = wire::encode(&meta, None, item.wire_dtype)?;
                    timings.encode_us = started.elapsed().as_micros() as u64;
                    return Ok(TimedReply { bytes, timings });
                }

                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch produced no logits for {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) = sample_token_within_budget(
                    logits,
                    &recent,
                    &s.sampling,
                    s.thinking_open_at_start(&item.sid_key),
                );
                super::core::mark_first_token_produced(&mut timings, decode_started);
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
                // See `prefill_blocking`: the end of the turn retains the prompt
                // instead of destroying the session.
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
                apply_mrope_positions(&mut meta, item.mrope_positions.as_deref());
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
            apply_mrope_positions(&mut meta, item.mrope_positions.as_deref());
            if item.owns_tokens {
                meta["tokens"] = json!(item.tokens_for_chunk);
            } else if let Some(tokens) = &item.chunk_tokens {
                meta["tokens"] = json!(tokens);
            }
            let started = Instant::now();
            let (bytes, wire_timing) =
                wire::encode_profiled(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.residual_wire_f16_encode_ns = timings
                .residual_wire_f16_encode_ns
                .saturating_add(wire_timing.f16_conversion_ns);
            timings.residual_wire_f8_conversion_us = timings
                .residual_wire_f8_conversion_us
                .saturating_add(wire_timing.f8_conversion_us);
            timings.residual_wire_frame_serialize_us = timings
                .residual_wire_frame_serialize_us
                .saturating_add(wire_timing.frame_serialize_us);
            timings.residual_wire_encode_calls =
                timings.residual_wire_encode_calls.saturating_add(1);
            timings.encode_us = started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

#[cfg(test)]
mod multimodal_position_tests {
    use super::*;

    /// Reproduces the arithmetic of the production defect: the image wrote 208
    /// cells to the KV starting at position 26, but in M-RoPE it only occupies
    /// `max(nx, ny) = 25` logical positions, so the following text starts at
    /// 51. Comparing 51 with 208 made the head ask for a second chunk of a
    /// prompt already entirely ingested.
    #[test]
    fn mrope_media_does_not_compare_logical_position_with_physical_cells() {
        let physical_cells = 26 + 208 + 10;
        let logical_position = 26 + 25 + 10;
        assert!(logical_position < physical_cells);
        assert!(!prefill_has_more_chunks(
            true,
            physical_cells,
            logical_position,
            true
        ));
        // Without media the two quantities coincide, and chunking still
        // applies: a text prompt yet to be ingested still asks for another chunk.
        assert!(prefill_has_more_chunks(
            true,
            physical_cells,
            logical_position,
            false
        ));
        assert!(!prefill_has_more_chunks(true, 244, 244, false));
        assert!(!prefill_has_more_chunks(false, 244, 51, false));
    }

    /// The logical end of an M-RoPE hidden state batch is the `next_pos` the
    /// head announced, never the number of embedding rows.
    #[test]
    fn mrope_hidden_state_keeps_logical_end_separate_from_cell_count() {
        let positions: Vec<[u32; 4]> = (0..208)
            .map(|i| [26, 26 + i / 16, 26 + i % 16, 0])
            .collect();

        assert_eq!(
            logical_end_from_mrope_positions(26, &positions, 208, Some(51)).unwrap(),
            51
        );
        // A physical count that diverges from the position table is an
        // inconsistency, not a value to round.
        assert!(logical_end_from_mrope_positions(26, &positions, 207, Some(51)).is_err());
        // Without the head's `next_pos` there is no way to infer the logical
        // end: failing is better than assuming `start + cells`.
        assert!(logical_end_from_mrope_positions(26, &positions, 208, None).is_err());
        // Going back from the start position is always an inconsistency.
        assert!(logical_end_from_mrope_positions(26, &positions, 208, Some(25)).is_err());
    }

    /// A session whose physical count differs from the logical one cannot
    /// serve as a reusable prefix: `prompt_tokens` no longer indexes the KV
    /// cell by cell.
    #[test]
    fn mrope_session_is_not_a_reusable_prefix() {
        let mut multimodal = Session::new(0);
        multimodal.n_past = 51;
        multimodal.prompt_cells = 244;
        multimodal.contains_media = true;
        assert!(!multimodal.positions_match_cells());

        // Even when the projector does not use M-RoPE and the two counts match,
        // `prompt_tokens` is still a filler without the real ids of the media
        // cells: reusing that prefix would compare garbage.
        let mut without_mrope = Session::new(0);
        without_mrope.n_past = 244;
        without_mrope.prompt_cells = 244;
        without_mrope.contains_media = true;
        assert!(!without_mrope.positions_match_cells());

        let mut text = Session::new(0);
        text.n_past = 244;
        text.prompt_cells = 244;
        assert!(text.positions_match_cells());
    }

    /// The M-RoPE positions cross the wire without losing the four-axis shape.
    #[test]
    fn mrope_positions_survive_the_wire() {
        let positions = vec![[26u32, 27, 28, 0], [26, 27, 29, 0]];
        let mut meta = json!({});
        apply_mrope_positions(&mut meta, Some(&positions));
        let decoded: Vec<[u32; 4]> =
            serde_json::from_value(meta["mrope_positions"].clone()).unwrap();
        assert_eq!(decoded, positions);

        let mut empty = json!({});
        apply_mrope_positions(&mut empty, None);
        assert!(empty.get("mrope_positions").is_none());
    }
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
                    job.scheduler_wait_us,
                    job.scheduler_wait_reason,
                    job.scheduler_session_existing,
                    job.pipeline_session,
                    job.profiling,
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
            scheduler_wait_us,
            scheduler_wait_reason,
            scheduler_session_existing,
            pipeline_session,
            ..
        } = job;
        let result = (|| -> Result<PreparedStep> {
            // Everything from here on is a generated token, so nothing else
            // touches the prompt sequence - see `InferState::open_generation`.
            s.open_generation(&sid_key)?;
            let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
                anyhow::anyhow!("unknown session '{}' - run the prefill first", sid_key)
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
                    &format!("hidden state of {sid_key}"),
                )?;
                PreparedStepInput::Hidden {
                    values: hidden,
                    shape,
                }
            } else {
                let token =
                    token_id.ok_or_else(|| anyhow::anyhow!("the first peer needs a token_id"))?;
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
                scheduler_wait_us,
                scheduler_wait_reason,
                scheduler_session_existing,
                pipeline_session,
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
    // A single graph cannot stop at two different blocks. Refusing the batch
    // is safe; picking one of the limits would make a session run more or
    // fewer layers than the sender asked for.
    let capture_block = prepared[0].1.capture_block;
    if prepared
        .iter()
        .any(|(_, item)| item.capture_block != capture_block)
    {
        for (reply, _) in prepared {
            replies.push((
                reply,
                Err(anyhow::anyhow!(
                    "batch with different sub-ranges: each would stop at a block, and the graph is a single one"
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
    let decode_started = Instant::now();
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
                    mrope_positions: None,
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
                session_id: Some(item.sid_key.clone()),
                selected_path,
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                scheduler_wait_us: item.scheduler_wait_us,
                scheduler_wait_reason: item.scheduler_wait_reason,
                scheduler_session_existing: item.scheduler_session_existing,
                pipeline_session: item.pipeline_session,
                lock_wait_us,
                decode_us: engine_timings.backend_call_us,
                backend_sync_us: engine_timings.backend_sync_us,
                ..Default::default()
            };
            timings.merge_engine_capture(&engine_timings);
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
                        anyhow::anyhow!("batch produced no logits for {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) = sample_token_within_budget(
                    logits,
                    &recent,
                    &s.sampling,
                    s.thinking_open_at_start(&item.sid_key),
                );
                super::core::mark_first_token_produced(&mut timings, decode_started);
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
                // End of turn retains the prompt - see `InferState::retain_prompt`.
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
            let (bytes, wire_timing) =
                wire::encode_profiled(&meta, Some((&residual, residual_shape)), item.wire_dtype)?;
            timings.residual_wire_f16_encode_ns = timings
                .residual_wire_f16_encode_ns
                .saturating_add(wire_timing.f16_conversion_ns);
            timings.residual_wire_f8_conversion_us = timings
                .residual_wire_f8_conversion_us
                .saturating_add(wire_timing.f8_conversion_us);
            timings.residual_wire_frame_serialize_us = timings
                .residual_wire_frame_serialize_us
                .saturating_add(wire_timing.frame_serialize_us);
            timings.residual_wire_encode_calls =
                timings.residual_wire_encode_calls.saturating_add(1);
            timings.encode_us = encode_started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}
