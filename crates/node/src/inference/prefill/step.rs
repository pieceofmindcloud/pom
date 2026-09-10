use super::*;

use super::pipeline::{direct_failure, record_direct_hops};

/// One generated token through the whole pipeline, framed like the prefill.
///
/// `speculate` é o convite a rascunhar que o gateway anexa no caminho fatiado
/// (MTP): `None` mantém a rodada no formato antigo de um token; `Some` faz a
/// cabeça montar o lote — com o rascunho pronto da cauda (`draft`) ou
/// produzindo o próprio. A resposta é sempre a mesma `SpeculativeStep`: uma
/// rodada sem especulação tem `accepted.len() == 1` e `speculated == false`.
#[allow(clippy::too_many_arguments)]
pub async fn step(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: Option<&crate::direct::DirectReply>,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    plan.validate_wire_dtype(dtype)?;
    if let Some(direct) = direct.filter(|_| plan.supports_direct_reply()) {
        return step_direct(
            plan, model, session_id, token_id, dtype, http, routes, direct, tunnel, speculate,
            trace_id,
        )
        .await;
    }
    if plan.supports_relay_chain() {
        return step_ring(
            plan, model, session_id, token_id, dtype, http, routes, tunnel, speculate, trace_id,
        )
        .await;
    }

    // O caminho hub também passa pelo relay comum. Além de manter pinning,
    // credencial e telemetria iguais ao anel, isso permite agrupar chamadas
    // concorrentes que chegam ao mesmo hop.
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, TensorShape)> = None;
    let mut meta = serde_json::Value::Null;
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let mut body_meta = if i == 0 {
            let mut m = json!({
                "session_id": session_id,
                "pipeline_session": true,
                "is_last": is_last,
                "token_id": token_id,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            if let Some(spec) = &speculate {
                m["speculate"] = spec.to_meta();
            }
            m
        } else {
            let mut m = json!({
                "session_id": session_id,
                "pipeline_session": true,
                "is_last": is_last,
                "scale": 1.0,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            // Thread o que a cabeça decidiu na ida — o rascunho, a composição
            // do lote (`tokens`, para o `observe` da cauda) e a largura de
            // lote da próxima rodada (`mtp_k`).
            if let Some(draft) = meta.get("draft") {
                m["draft"] = draft.clone();
            }
            if let Some(tokens) = meta.get("tokens") {
                m["tokens"] = tokens.clone();
            }
            if let Some(mtp_k) = meta.get("mtp_k") {
                m["mtp_k"] = mtp_k.clone();
            }
            m
        };
        if let Some(trace_id) = trace_id {
            body_meta["trace_id"] = json!(trace_id);
        }

        let payload = tensor.take();
        let sent = crate::relay::send_frame_or_tunnel(
            http,
            peer,
            tunnel,
            "/v1/step",
            &body_meta,
            payload
                .as_ref()
                .map(|(values, shape)| (values.as_slice(), shape.clone())),
            dtype,
            dtype.frame_encoding(),
            1,
            &peer.node_id,
            |_issue| {},
        )
        .await
        .map_err(|error| {
            routes.mark_failure(&peer.node_id, &peer.inference_url);
            anyhow!("peer {} não respondeu: {error}", peer.node_id)
        })?;
        trace.record_hop_from_sent(i, Phase::Decode, &sent);
        meta = sent.meta;
        tensor = sent.tensor;
    }

    speculative_step_from_meta(&meta, trace)
}

/// One generated token, relayed hop-to-hop instead of gateway-to-every-hop —
/// see `PipelinePlan::supports_relay_chain`. This process makes exactly one
/// outbound call (to the head); every other hop is reached by the head (and,
/// transitively, each hop after it) relaying directly, and the tail's answer
/// bubbles all the way back through that same chain of calls.
#[allow(clippy::too_many_arguments)]
pub(super) async fn step_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut body_meta = json!({
        "session_id": session_id,
        "pipeline_session": true,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(spec) = &speculate {
        body_meta["speculate"] = spec.to_meta();
    }
    if let Some(trace_id) = trace_id {
        body_meta["trace_id"] = json!(trace_id);
    }

    let sent = crate::relay::send_frame_or_tunnel(
        http,
        head,
        tunnel,
        "/v1/step",
        &body_meta,
        None,
        dtype,
        dtype.frame_encoding(),
        1, // decode has never retried — see relay::send_frame's doc comment
        &head.node_id,
        |_issue| {},
    )
    .await
    .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;

    for (i, entry) in crate::relay::bubble_hop_timings(&sent)
        .into_iter()
        .enumerate()
    {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    speculative_step_from_meta(&sent.meta, trace)
}

/// One generated token, dispatched once and answered once — the decode twin of
/// [`run_direct`].
///
/// This process makes exactly one outbound call, to the head, and then waits on
/// the ticket. The head relays hop to hop and the tail hands the token straight
/// back here, so the token crosses each link once instead of once out and once
/// back. That is the whole difference between `2N` traversals and `N + 1`, and
/// the reason the tokens-per-second stops depending on which machine happens to
/// hold layer 0.
#[allow(clippy::too_many_arguments)]
pub(super) async fn step_direct(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut ticket = direct.ticket_for_hops(plan.steps.len());
    let ticket_id = ticket.id().to_owned();
    let mut body_meta = json!({
        "session_id": session_id,
        "pipeline_session": true,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "next_hops": next_hops,
        "reply_to": direct.reply_to(ticket.id()),
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(spec) = &speculate {
        body_meta["speculate"] = spec.to_meta();
    }
    if let Some(trace_id) = trace_id {
        body_meta["trace_id"] = json!(trace_id);
    }

    // Own the ACK future so it can keep running if the tail's direct result
    // wins the select below. Otherwise the first hop's transport receipt is
    // lost precisely in the latency ordering this telemetry is meant to
    // explain.
    let ack_http = http.clone();
    let ack_head = head.clone();
    let ack_tunnel = tunnel.cloned();
    let ack_body_meta = body_meta.clone();
    let ack = async move {
        crate::relay::send_frame_or_tunnel(
            &ack_http,
            &ack_head,
            ack_tunnel.as_ref(),
            "/v1/step",
            &ack_body_meta,
            None,
            dtype,
            dtype.frame_encoding(),
            1, // decode has never retried - see relay::send_frame's doc comment
            &ack_head.node_id,
            |_issue| {},
        )
        .await
    };
    let ack_direct = direct.clone();
    let ack_ticket_id = ticket_id.clone();
    let meta = crate::direct::await_result_with_ack(ack, &mut ticket, &head.node_id, move |sent| {
        ack_direct.record_transport(&ack_ticket_id, 0, sent);
    })
    .await
    .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;
    if let Some(error) = direct_failure(plan, routes, &meta) {
        return Err(error);
    }

    record_direct_hops(&mut trace, Phase::Decode, &meta);

    speculative_step_from_meta(&meta, trace)
}

/// If the failure carries a `RelayFailure` from further down the chain (a
/// hop's own outbound call to *its* next hop failed), name the peer that
/// actually failed instead of `head_node_id` — the one this process
/// happened to call directly.
pub(super) fn ring_relay_error(head_node_id: &str, e: anyhow::Error) -> anyhow::Error {
    match crate::relay::extract_relay_failure(&e) {
        Some(failure) => anyhow!(
            "pipeline falhou em {} (retransmitido por {head_node_id}): {} — {}",
            failure.failed_node_id,
            failure.kind,
            failure.message
        ),
        None => e,
    }
}

// ── Speculative decoding (prompt-lookup) ────────────────────────────────────

/// Outcome of one speculative round: possibly several confirmed tokens
/// instead of the usual one, in generation order.
#[derive(Debug)]
pub struct SpeculativeStep {
    pub accepted: Vec<(u32, String)>,
    pub done: bool,
    /// How many of the batch's positions the tail confirmed — passed
    /// straight to `gateway::rollback_sessions` afterward.
    pub accept_count: u32,
    /// How many positions the head actually decoded (`1 + draft.len()`).
    pub batch_width: u32,
    /// Whether this round actually was speculative.
    ///
    /// The head is free to decline — `should_draft` says no for an MTP round
    /// with no local `nextn` head and no premade draft yet (round one of a
    /// sliced pipeline) — and it says so by omitting `draft`, which puts the
    /// whole round back on the plain one-token wire format (see
    /// `step_blocking`'s `incoming_draft` doc). `false` means exactly that
    /// happened, and asking the same chain to speculate again would just
    /// repeat the refusal.
    pub speculated: bool,
    /// Whether the remote stream will use a draft in a subsequent round.
    /// This is separate from `speculated`: the first plain token can produce
    /// the draft that makes the next round speculative.
    pub speculation_active: bool,
    /// Stable reason why a requested speculative cycle was disabled.
    pub speculation_disabled_reason: Option<String>,
    /// Detailed evidence from the hop that disabled a requested cycle. This
    /// is deliberately separate from the stable reason used by older peers.
    pub speculation_disabled_detail: Option<String>,
    /// Rascunho que a cauda de um pipeline fatiado produziu para a **próxima**
    /// rodada (bloco `nextn`). No stream remoto, a cabeça o instala
    /// internamente antes de continuar o ciclo. `None` quando esta rodada não
    /// produziu um rascunho ou a geração terminou.
    pub next_draft: Option<Vec<i32>>,
    pub trace: RequestTrace,
}

/// Gateway side of the remote decode stream. The initial call is only a
/// control/acceptance frame; generated tokens arrive through the bounded
/// callback inbox and are decoded in opportunistic batches.
pub(crate) struct RemoteDecodeStream {
    pub(super) ticket: crate::direct::StreamTicket,
    pub(super) model: String,
    pub(super) plan: PipelinePlan,
    pub(crate) initial_trace: RequestTrace,
    pub(super) last_callback_at: Option<Instant>,
}

impl RemoteDecodeStream {
    pub(crate) async fn next(&mut self, cancellation: &Cancellation) -> Result<SpeculativeStep> {
        let meta = cancelable(cancellation, async { Ok(self.ticket.recv().await) })
            .await?
            .ok_or_else(|| anyhow!("transporte remoto encerrou sem lote terminal"))?;
        let callback_at = Instant::now();
        let callback_interval_ms = self
            .last_callback_at
            .replace(callback_at)
            .map(|previous| callback_at.saturating_duration_since(previous).as_millis() as u64);
        if let Some(error) = meta.get("error").and_then(|value| value.as_str()) {
            return Err(anyhow!("peer remoto falhou durante o stream: {error}"));
        }

        let batch_bytes = meta
            .get("stream_batch_bytes")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let span_count = meta
            .get("span_count")
            .and_then(|value| value.as_u64())
            .unwrap_or_else(|| {
                meta.get("accepted")
                    .and_then(|value| value.as_array())
                    .map_or(1, Vec::len) as u64
            });
        let trace_id = meta
            .get("trace_id")
            .and_then(|value| value.as_str())
            .unwrap_or(&self.initial_trace.trace_id);
        let mut trace = RequestTrace::from_plan_with_trace_id(trace_id, &self.model, &self.plan);
        if let Some(interval_ms) = callback_interval_ms {
            trace.record_remote_stream_token_delivery_interval(interval_ms);
        }
        if trace.record_remote_stream_batch(&meta) {
            let entry = json!({
            "round_trip_ms": 0,
            "processing_ms": meta.get("processing_ms").cloned().unwrap_or_else(|| json!(0)),
            "bytes_sent": 0,
            "bytes_received": batch_bytes,
            "client_encode_ms": 0,
            "response_headers_ms": 0,
            "response_body_ms": 0,
            "client_decode_ms": 0,
            "transport": "remote-stream",
            "transport_fallback": false,
            "node_timings": meta.get("node_timings").cloned().unwrap_or_else(|| json!({})),
            "span_count": span_count,
            });
            trace.record_hop_from_json(0, Phase::Decode, &entry);
            if let Some(hops) = meta.get("hop_timings").and_then(|value| value.as_array()) {
                for (index, hop) in hops.iter().enumerate() {
                    trace.record_hop_from_json(index + 1, Phase::Decode, hop);
                    let Some(from_node_id) =
                        trace.hops.get(index).map(|entry| entry.node_id.clone())
                    else {
                        trace.transport_trace_complete = false;
                        continue;
                    };
                    let Some(to_node_id) =
                        trace.hops.get(index + 1).map(|entry| entry.node_id.clone())
                    else {
                        trace.transport_trace_complete = false;
                        continue;
                    };
                    if hop.get("transport").is_some() {
                        trace.record_cycle_transport(
                            Phase::Decode,
                            &from_node_id,
                            &to_node_id,
                            crate::prefill::TransportTrace::from_json(hop),
                        );
                    }
                }
            }
            if let Some(round_trip_ms) = meta
                .get("remote_stream_transport_rtt_ms")
                .and_then(|value| value.as_u64())
            {
                let from_node_id = meta
                    .get("remote_stream_source_node_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                    .or_else(|| trace.hops.last().map(|entry| entry.node_id.clone()));
                if let Some(from_node_id) = from_node_id {
                    trace.record_cycle_return(
                        Phase::Decode,
                        &from_node_id,
                        crate::prefill::TransportTrace::from_json(&json!({
                            "transport": meta
                                .get("remote_stream_transport")
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown"),
                            "round_trip_ms": round_trip_ms,
                        })),
                    );
                }
            }
        }
        speculative_step_from_meta(&meta, trace)
    }
}

/// Starts the new remote decode transport. The remote head owns the generation
/// loop; for a multi-hop plan it relays each decode through the remaining
/// peers internally, while the gateway sees only the bounded callback stream.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn step_stream(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    token_index: u64,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    direct: &crate::direct::DirectReply,
    tunnel: Option<&TunnelOutboundSender>,
    speculate: Option<Speculate>,
    cancellation: &Cancellation,
    trace_id: Option<&str>,
) -> Result<RemoteDecodeStream> {
    plan.validate_wire_dtype(dtype)?;
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let ticket = direct.stream_ticket();
    let request_trace_id = trace_id
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut meta = json!({
        "session_id": session_id,
        "trace_id": request_trace_id,
        "request_id": session_id,
        "pipeline_id": plan.hop_chain(),
        "is_last": next_hops.is_empty(),
        "remote_stream": true,
        "token_id": token_id,
        "telemetry_token_index": token_index,
        "next_hops": next_hops,
        "reply_to": direct.stream_reply_to(ticket.id()),
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(speculate) = speculate {
        meta["speculate"] = speculate.to_meta();
    }

    let stream_opened_at = Instant::now();
    let ack = cancelable(
        cancellation,
        crate::relay::send_frame_or_tunnel(
            http,
            head,
            tunnel,
            REMOTE_STREAM_PATH,
            &meta,
            None,
            dtype,
            dtype.frame_encoding(),
            1,
            &head.node_id,
            |_issue| {},
        ),
    )
    .await
    .map_err(|error| {
        if !is_cancelled(&error) {
            routes.mark_failure(&head.node_id, &head.inference_url);
        }
        error
    })?;
    if ack
        .meta
        .get("stream_accepted")
        .and_then(|value| value.as_bool())
        != Some(true)
    {
        return Err(anyhow!("peer não aceitou o transporte remoto por lotes"));
    }

    let mut initial_trace = RequestTrace::from_plan_with_trace_id(&request_trace_id, model, plan);
    initial_trace.record_remote_stream_open(stream_opened_at.elapsed().as_millis() as u64);
    initial_trace.record_control_transport(
        0,
        Phase::Decode,
        TransportTrace::from_sent(&ack),
        ack.bytes_sent,
        ack.bytes_received,
    );
    initial_trace.record_transport_fallback_from_sent(&ack);
    Ok(RemoteDecodeStream {
        ticket,
        model: model.to_string(),
        plan: plan.clone(),
        initial_trace,
        last_callback_at: None,
    })
}

/// One speculative round: the head drafts up to `k` tokens from its own
/// recent history (searching a trailing window of `m`), the whole batch is
/// decoded through the pipeline in a single pass, and the tail verifies it —
/// see `infer_server.rs`'s `step_blocking` for where the actual drafting and
/// verification happen; this function only orchestrates getting the batch
/// there and the answer back, exactly like `step` does for a single token.
/// Composes with either relay mode transparently: ring when negotiated,
/// hub otherwise — same rule `step`/`run` already follow.
#[allow(clippy::too_many_arguments)]
pub async fn step_speculative(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    plan.validate_wire_dtype(dtype)?;
    if plan.supports_relay_chain() {
        return step_speculative_ring(
            plan, model, session_id, token_id, k, m, source, dtype, http, routes, tunnel, trace_id,
        )
        .await;
    }
    step_speculative_hub(
        plan, model, session_id, token_id, k, m, source, dtype, http, routes, tunnel, trace_id,
    )
    .await
}

/// De onde a cabeça deve tirar o rascunho desta rodada.
///
/// Viaja no quadro (`speculate.source`) porque quem decide é o gateway, que é
/// o único que vê o plano inteiro e as capacidades anunciadas, e quem executa
/// é a cabeça. Ver `infer_server::Speculate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftSource {
    /// `find_draft`: repetição no texto já gerado.
    Lookup,
    /// O bloco `nextn` do próprio modelo (`crate::mtp`).
    Mtp,
}

impl DraftSource {
    pub(super) fn wire(self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn step_speculative_hub(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    let n_peers = plan.steps.len();
    let mut tensor: Option<(Vec<f32>, TensorShape)> = None;
    let mut meta = serde_json::Value::Null;
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));

    for (i, peer) in plan.steps.iter().enumerate() {
        let is_last = i == n_peers - 1;
        let mut body_meta = if i == 0 {
            json!({
                "session_id": session_id,
                "pipeline_session": true,
                "is_last": is_last,
                "token_id": token_id,
                "speculate": {"k": k, "m": m, "source": source.wire()},
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            })
        } else {
            let mut m = json!({
                "session_id": session_id,
                "pipeline_session": true,
                "is_last": is_last,
                "scale": 1.0,
                "exec_start": peer.exec_start,
                "exec_end": peer.exec_end,
            });
            // Keep threading whatever the head decided — see `step_blocking`'s
            // `draft`/`incoming_draft` doc comments for why absent vs. an
            // empty array mean different things here.
            if let Some(draft) = meta.get("draft") {
                m["draft"] = draft.clone();
            }
            if let Some(tokens) = meta.get("tokens") {
                m["tokens"] = tokens.clone();
            }
            if let Some(mtp_k) = meta.get("mtp_k") {
                m["mtp_k"] = mtp_k.clone();
            }
            m
        };
        if let Some(trace_id) = trace_id {
            body_meta["trace_id"] = json!(trace_id);
        }

        let payload = tensor.take();
        let as_ref = payload.as_ref().map(|(v, s)| (v.as_slice(), s.clone()));
        let peer_label = format!("{} (especulativo)", peer.node_id);
        let sent = crate::relay::send_frame_or_tunnel(
            http,
            peer,
            tunnel,
            "/v1/step",
            &body_meta,
            as_ref,
            dtype,
            dtype.frame_encoding(),
            1, // decode has never retried — see relay::send_frame's doc comment
            &peer_label,
            |_issue| {},
        )
        .await
        .map_err(|e| {
            routes.mark_failure(&peer.node_id, &peer.inference_url);
            e
        })?;

        trace.record_hop_from_sent(i, Phase::Decode, &sent);
        meta = sent.meta;
        tensor = sent.tensor;
    }

    speculative_step_from_meta(&meta, trace)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn step_speculative_ring(
    plan: &PipelinePlan,
    model: &str,
    session_id: &str,
    token_id: u32,
    k: u32,
    m: u32,
    source: DraftSource,
    dtype: WireDtype,
    http: &crate::peer_client::PeerClients,
    routes: &crate::route::RouteTable,
    tunnel: Option<&TunnelOutboundSender>,
    trace_id: Option<&str>,
) -> Result<SpeculativeStep> {
    let mut trace = trace_id
        .map(|id| RequestTrace::from_plan_with_trace_id(id, model, plan))
        .unwrap_or_else(|| RequestTrace::from_plan(model, plan));
    let head = &plan.steps[0];
    let next_hops = &plan.steps[1..];
    let mut head_meta = json!({
        "session_id": session_id,
        "pipeline_session": true,
        "is_last": next_hops.is_empty(),
        "token_id": token_id,
        "speculate": {"k": k, "m": m, "source": source.wire()},
        "next_hops": next_hops,
        "exec_start": head.exec_start,
        "exec_end": head.exec_end,
    });
    if let Some(trace_id) = trace_id {
        head_meta["trace_id"] = json!(trace_id);
    }

    let sent = crate::relay::send_frame_or_tunnel(
        http,
        head,
        tunnel,
        "/v1/step",
        &head_meta,
        None,
        dtype,
        dtype.frame_encoding(),
        1,
        &head.node_id,
        |_issue| {},
    )
    .await
    .map_err(|e| {
        routes.mark_failure(&head.node_id, &head.inference_url);
        ring_relay_error(&head.node_id, e)
    })?;

    for (i, entry) in crate::relay::bubble_hop_timings(&sent)
        .into_iter()
        .enumerate()
    {
        trace.record_hop_from_json(i, Phase::Decode, &entry);
    }

    speculative_step_from_meta(&sent.meta, trace)
}

/// Parse the tail's reply to a speculative round — shared by the hub and ring
/// paths, which only differ in how they get to that reply.
///
/// Two shapes are legitimate, and reading only the first one is what killed
/// every generation on a round the head declined to speculate:
///
/// * `{done, accepted, accept_count, batch_width}` — the tail verified a
///   batch, which is what happens when the head drafted;
/// * `{done, token_id, text}` — the plain one-token reply, which is what the
///   protocol *requires* when `draft` never reached the tail because the head
///   declined to draft (`should_draft`).
///
/// `batch_width: 1` on the second shape is not a guess. The tail answers in
/// that shape only when no `draft` arrived, every relay branch threads `draft`
/// through untouched (`handle_step`, `forward_direct`, `step_speculative_hub`),
/// and the head omits it only when it also batched a single token. So nothing
/// beyond the seed token was ever decoded, and there is no KV position to roll
/// back.
pub(super) fn speculative_step_from_meta(
    meta: &serde_json::Value,
    trace: RequestTrace,
) -> Result<SpeculativeStep> {
    let done = meta.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
    // O rascunho da cauda para a próxima rodada (pipeline fatiado) — presente
    // nas duas formas de resposta quando a cauda tem o bloco `nextn`.
    let next_draft: Option<Vec<i32>> = meta
        .get("next_draft")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_i64())
                .map(|v| v as i32)
                .collect()
        })
        .filter(|draft: &Vec<i32>| !draft.is_empty());
    let explicit_speculated = meta.get("speculated").and_then(|v| v.as_bool());
    let explicit_active = meta.get("speculation_active").and_then(|v| v.as_bool());
    let speculation_disabled_reason = meta
        .get("speculation_disabled_reason")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let speculation_disabled_detail = meta
        .get("speculation_disabled_detail")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let accepted: Vec<(u32, String)> = meta
        .get("accepted")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = entry.get("token_id")?.as_u64()? as u32;
                    let text = entry.get("text")?.as_str()?.to_string();
                    Some((id, text))
                })
                .collect()
        })
        .unwrap_or_default();

    if accepted.is_empty() {
        let Some(token_id) = meta.get("token_id").and_then(|v| v.as_u64()) else {
            return Err(anyhow!(
                "último peer não devolveu token nenhum na rodada especulativa"
            ));
        };
        return Ok(SpeculativeStep {
            accepted: vec![(
                token_id as u32,
                meta.get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )],
            done,
            accept_count: 1,
            batch_width: 1,
            speculated: explicit_speculated.unwrap_or(false),
            speculation_active: explicit_active.unwrap_or(next_draft.is_some()),
            speculation_disabled_reason,
            speculation_disabled_detail,
            next_draft,
            trace,
        });
    }

    let accept_count = meta
        .get("accept_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(accepted.len() as u64) as u32;
    let batch_width = meta
        .get("batch_width")
        .and_then(|v| v.as_u64())
        .unwrap_or(accept_count.max(1) as u64) as u32;
    Ok(SpeculativeStep {
        accepted,
        done,
        accept_count,
        batch_width,
        speculated: explicit_speculated.unwrap_or(true),
        speculation_active: explicit_active.unwrap_or(true),
        speculation_disabled_reason,
        speculation_disabled_detail,
        next_draft,
        trace,
    })
}
