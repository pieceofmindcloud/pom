use super::*;
use futures::FutureExt;

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn record_gateway_span(
    spans: &mut Vec<crate::prefill::TraceSpan>,
    request_started: Instant,
    span_started: Instant,
    started_at_ms: u64,
    name: &'static str,
) {
    if !crate::telemetry::Profile::current().collects_spans() {
        return;
    }
    spans.push(crate::prefill::TraceSpan::new(
        name,
        "gateway",
        None,
        started_at_ms,
        span_started
            .saturating_duration_since(request_started)
            .as_millis() as u64,
        span_started.elapsed().as_millis() as u64,
        None,
    ));
}

fn prefill_rebuild_reason(error: &anyhow::Error) -> Option<&'static str> {
    if let Some(rebuild) = crate::prefill::rebuild_required(error) {
        return Some(rebuild.reason);
    }
    if crate::relay::is_unknown_session_error(error) {
        return Some("a sessão remota não existe mais");
    }
    if crate::relay::is_delivery_ambiguous(error) {
        return Some("a resposta se perdeu depois do envio e o peer pode ter processado o chunk");
    }
    if crate::relay::is_prefill_state_error(error) {
        return Some("as posições do KV remoto não coincidem com o próximo chunk");
    }
    if crate::relay::is_transport_relay_failure(error)
        && !crate::relay::is_pre_delivery_error(error)
    {
        return Some("um hop downstream falhou depois que a cabeça aceitou o chunk");
    }
    None
}

/// Regista o desfecho de uma requisição que termina antes de planear a
/// pipeline.
///
/// Todo pedido aceite tem de deixar exatamente uma linha de desfecho. Sem
/// isto, as saídas antecipadas desta função eram mudas: o registo mostrava o
/// pedido a chegar, mostrava o ajuste do limite de saída, e depois nada - nem
/// `Pipeline planejado`, nem erro. Do lado do cliente isso é um pedido
/// pendurado, e foi o que pôs o agente em recuo exponencial a repetir o mesmo
/// pedido em produção a 2026-09-08.
fn log_early_outcome(outcome: &str, reason: &str) {
    warn!("⛔ requisição encerrada antes do plano ({outcome}): {reason}");
}

/// Wakes every idle step selected for this request at once. This is a control
/// event on the gateway port, not an inference hop, so all peers begin their
/// local load concurrently before the first prefill is sent. The event is
/// idempotent if another request has already woken the same peer.
const IDLE_RELOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

async fn reload_idle_pipeline(
    state: &GatewayState,
    plan: &PipelinePlan,
    peers: &[crate::network::PeerInfo],
) -> anyhow::Result<()> {
    reload_idle_pipeline_with_timeout(state, plan, peers, IDLE_RELOAD_TIMEOUT).await
}

async fn reload_idle_pipeline_with_timeout(
    state: &GatewayState,
    plan: &PipelinePlan,
    peers: &[crate::network::PeerInfo],
    timeout: Duration,
) -> anyhow::Result<()> {
    if !plan.steps.iter().any(|step| {
        peers
            .iter()
            .any(|peer| peer.node_id == step.node_id && peer.idle)
    }) {
        return Ok(());
    }
    let requests = plan
        .steps
        .iter()
        .filter_map(|step| {
            let peer = peers.iter().find(|peer| peer.node_id == step.node_id)?;
            if !peer.idle {
                return None;
            }
            let node_id = peer.node_id.clone();
            if node_id == state.network.node_id {
                let runtime = state.runtime.clone();
                return Some(Ok(async move {
                    runtime
                        .reload_from_idle()
                        .await
                        .map(|_| ())
                        .map_err(|error| {
                            anyhow::anyhow!("reload_model falhou para {node_id}: {error}")
                        })
                }
                .boxed()));
            }
            let peer = peer.clone();
            Some(Ok(async move {
                let response = state
                    .control_post_json(
                        &peer,
                        "/api/internal/reload_model",
                        &serde_json::json!({ "event": "reload_model" }),
                        timeout,
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("reload_model falhou para {node_id}: {error:#}")
                    })?;
                if response.status().is_success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "reload_model falhou para {node_id}: HTTP {}",
                        response.status()
                    ))
                }
            }
            .boxed()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let results = tokio::time::timeout(timeout, futures::future::try_join_all(requests))
        .await
        .map_err(|_| anyhow::anyhow!("reload_model da pipeline excedeu {timeout:?}"))?;
    results?;
    Ok(())
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use crate::pipeline::PipelineStep;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Clone)]
    struct ReloadBarrier {
        started: Arc<AtomicUsize>,
        all_started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        expected: usize,
    }

    async fn gated_reload(State(barrier): State<ReloadBarrier>, _body: Bytes) -> StatusCode {
        let release = barrier.release.notified();
        if barrier.started.fetch_add(1, Ordering::SeqCst) + 1 == barrier.expected {
            barrier.all_started.notify_one();
        }
        release.await;
        StatusCode::OK
    }

    async fn hanging_reload(State(reached): State<Arc<AtomicBool>>, _body: Bytes) -> StatusCode {
        reached.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await;
        StatusCode::OK
    }

    async fn spawn_gated_server(state: ReloadBarrier) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("porta de teste");
        let port = listener.local_addr().expect("endereço de teste").port();
        let router = Router::new()
            .route("/api/internal/reload_model", post(gated_reload))
            .with_state(state);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, task)
    }

    async fn spawn_hanging_server() -> (u16, tokio::task::JoinHandle<()>, Arc<AtomicBool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("porta de teste");
        let port = listener.local_addr().expect("endereço de teste").port();
        let reached = Arc::new(AtomicBool::new(false));
        let router = Router::new()
            .route("/api/internal/reload_model", post(hanging_reload))
            .with_state(reached.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, task, reached)
    }

    fn plan_for(node_ids: &[&str]) -> PipelinePlan {
        PipelinePlan {
            steps: node_ids
                .iter()
                .map(|node_id| PipelineStep {
                    node_id: (*node_id).into(),
                    inference_url: String::new(),
                    inference_candidates: Vec::new(),
                    quic_addr_candidates: Vec::new(),
                    start_layer: 0,
                    end_layer: 0,
                    exec_start: 0,
                    exec_end: 0,
                    inference_frame_capabilities: Vec::new(),
                    libp2p_peer_id: None,
                    libp2p_addr_candidates: Vec::new(),
                })
                .collect(),
            edges: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reloads_all_planned_idle_peers_concurrently() {
        let barrier = ReloadBarrier {
            started: Arc::new(AtomicUsize::new(0)),
            all_started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            expected: 2,
        };
        let (first_port, first_task) = spawn_gated_server(barrier.clone()).await;
        let (second_port, second_task) = spawn_gated_server(barrier.clone()).await;

        let mut first = crate::gateway::testkit::peer("idle-first", "first", first_port);
        first.idle = true;
        let mut second = crate::gateway::testkit::peer("idle-second", "second", second_port);
        second.idle = true;
        let planned_peers = vec![first.clone(), second.clone()];
        let state = crate::gateway::testkit::state_with("gateway", vec![first, second]).await;
        let plan = plan_for(&["idle-first", "idle-second"]);
        let waiting_for_both = barrier.all_started.notified();
        let reload = tokio::spawn(async move {
            reload_idle_pipeline_with_timeout(&state, &plan, &planned_peers, Duration::from_secs(2))
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), waiting_for_both)
            .await
            .expect("os dois peers planejados devem receber o evento em paralelo");
        barrier.release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), reload)
            .await
            .expect("reload paralelo não pode ficar bloqueado")
            .expect("task de reload")
            .expect("ambos os peers devem confirmar o reload");

        first_task.abort();
        second_task.abort();
    }

    #[tokio::test]
    async fn reload_timeout_returns_instead_of_hanging_the_request() {
        let (port, server_task, reached) = spawn_hanging_server().await;
        let mut peer = crate::gateway::testkit::peer("idle-hung", "hung", port);
        peer.idle = true;
        let state = crate::gateway::testkit::state_with("gateway", vec![peer.clone()]).await;
        let plan = plan_for(&["idle-hung"]);

        let error =
            reload_idle_pipeline_with_timeout(&state, &plan, &[peer], Duration::from_millis(50))
                .await
                .expect_err("um peer preso deve devolver erro com timeout");
        assert!(
            reached.load(Ordering::SeqCst),
            "a requisição deve chegar ao peer preso"
        );
        assert!(
            !error.to_string().is_empty(),
            "a recusa deve explicar a falha"
        );
        server_task.abort();
    }
}

#[cfg(test)]
mod generation_pump_tests {
    use super::*;

    #[tokio::test]
    async fn desconexao_do_cliente_cancela_a_geracao_mesmo_sem_evento() {
        let (client_tx, client_rx) = mpsc::channel(1);
        let (_inner_tx, inner_rx) = mpsc::channel(1);
        let cancellation = crate::prefill::Cancellation::new();
        let pump = tokio::spawn(forward_generation_events(
            client_tx,
            inner_rx,
            cancellation.clone(),
        ));

        drop(client_rx);
        let result = tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .expect("a bomba deve observar a desconexão")
            .expect("a bomba não deve entrar em panic");

        assert!(result.is_none());
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn falha_de_prefill_preserva_telemetria_parcial_ate_a_auditoria() {
        let mut trace = crate::prefill::RequestTrace::from_plan(
            "test-model",
            &crate::pipeline::PipelinePlan {
                steps: Vec::new(),
                edges: Vec::new(),
            },
        );
        trace.prefill_ms = 37;
        trace.input_tokens = 45_647;
        trace.cached_tokens = 13;
        let transport_failure = anyhow::Error::new(crate::prefill::PartialPrefillTrace {
            trace: Box::new(trace),
        });
        let partial_trace = crate::prefill::partial_trace(&transport_failure)
            .expect("erro de transporte preserva trace parcial");
        let (client_tx, _client_rx) = mpsc::channel(1);
        let (inner_tx, inner_rx) = mpsc::channel(1);
        let cancellation = crate::prefill::Cancellation::new();
        inner_tx
            .send(GenEvent::FailedWithTelemetry {
                error: "transporte falhou após progresso".into(),
                trace: Some(partial_trace),
            })
            .await
            .unwrap();
        let result = forward_generation_events(client_tx, inner_rx, cancellation)
            .await
            .expect("falha terminal preservada");
        let GenEvent::FailedWithTelemetry {
            trace: Some(trace), ..
        } = result
        else {
            panic!("o evento terminal deve preservar trace parcial")
        };
        let response_telemetry = audit_telemetry(None, Some(trace)).unwrap();
        assert_eq!(response_telemetry["model"], "test-model");
        assert_eq!(response_telemetry["prefill_ms"], 37);
        assert_eq!(response_telemetry["input_tokens"], 45_647);
        assert_eq!(response_telemetry["cached_tokens"], 13);
    }
}

/// Escolhe a pipeline pelo menor tempo de entrega estimado. `None` quando não
/// há caminho viável: aí o planejador guloso (cobertura, RTT e carga local)
/// decide como antes.
///
/// Candidatos: o melhor caminho da programação dinâmica e cada caminho que já
/// tem o prefixo deste prompt retido, estimado com o prefill economizado.
#[allow(clippy::too_many_arguments)]
async fn plan_by_delivery_time(
    state: &GatewayState,
    peers: &[crate::network::PeerInfo],
    model: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    route_rtt: &std::collections::HashMap<String, f64>,
    preferred_head: Option<&str>,
    prefix_model: &str,
    cache_prompt: &str,
    requested_tokens: u32,
    prefix_eligible: bool,
) -> Option<(crate::pipeline::PipelinePlan, crate::fastest::ReturnMode)> {
    // A rota fixada é a medida do transporte que o frame vai usar; o RTT de
    // topologia fica para quem ainda não foi analisado.
    let mut requester_rtt = route_rtt.clone();
    requester_rtt.extend(crate::route_plan::book().rtt_map(crate::route_plan::Plane::Inference));
    let perf = state.admission_health.perf_snapshot();
    let mut local_load = std::collections::HashMap::new();
    for peer in peers {
        local_load.insert(peer.node_id.clone(), state.loads.load(&peer.node_id).await);
    }
    let inputs = crate::fastest::CostInputs {
        requester: &state.network.node_id,
        requester_rtt_ms: &requester_rtt,
        perf: &perf,
        local_load: &local_load,
        prompt_tokens: (cache_prompt.len() as f64 / 4.0).max(1.0),
        decode_tokens: (requested_tokens as f64).clamp(16.0, 512.0),
        forced_return: crate::fastest::forced_return_mode(),
    };
    let (mut plan, mut estimate) = crate::fastest::plan_fastest(
        peers,
        model,
        deploy_id,
        total_layers,
        &inputs,
        preferred_head,
    )?;

    if prefix_eligible {
        let candidates = crate::fastest::candidates(peers, model, deploy_id);
        for snapshot in state.prefix_cache.snapshots() {
            if snapshot.busy
                || snapshot.key.model != prefix_model
                || !cache_prompt.starts_with(&snapshot.prompt)
            {
                continue;
            }
            let node_ids: Vec<String> = snapshot
                .key
                .hops
                .iter()
                .filter_map(|hop| hop.split('@').next().map(str::to_owned))
                .collect();
            let Some(chain) =
                crate::fastest::resolve_chain(&candidates, &node_ids, total_layers, preferred_head)
            else {
                continue;
            };
            let cached = crate::fastest::estimate_chain(
                &chain,
                &inputs,
                &candidates,
                snapshot.cached_tokens,
            );
            if cached.total_ms < estimate.total_ms {
                if let Some(cached_plan) = crate::fastest::plan_for_chain(&chain) {
                    plan = cached_plan;
                    estimate = cached;
                }
            }
        }
    }

    info!(
        "pipeline por tempo de entrega: {} ~{:.0} ms, resposta {} (prefill {:.0} ms, {:.1} ms/token, filas {:?}, prefixo {} tokens)",
        plan.hop_chain(),
        estimate.total_ms,
        estimate.return_mode.as_str(),
        estimate.prefill_ms,
        estimate.decode_ms_per_token,
        estimate.stages.iter().map(|stage| stage.queue).collect::<Vec<_>>(),
        estimate.cached_tokens,
    );
    Some((plan, estimate.return_mode))
}

pub(super) async fn generate_stream_inner(
    input: GenerationInput,
    tx: mpsc::Sender<GenEvent>,
    cancellation: crate::prefill::Cancellation,
) {
    let request_started = Instant::now();
    let mut gateway_spans = Vec::new();
    let GenerationInput {
        trace_id,
        state,
        inference_token: _inference_token,
        model,
        prompt,
        images,
        chat_template,
        thinking_enabled,
        budget,
        stops,
        cache_key,
        preferred_head,
        request_details: _,
        probe,
    } = input;
    let mut budget = budget.normalized();
    if cancellation.is_cancelled() {
        log_early_outcome("cancelado", "o cliente desistiu antes do início");
        return;
    }
    // Size the prompt up before any peer sees it — see `prompt_token_budget`.
    let validation_started = Instant::now();
    let validation_wall = wall_clock_ms();
    if let Some(cfg) = state.runtime.config().await {
        if let Some(error) = prompt_budget_error(&cfg, &prompt) {
            log_early_outcome("recusado", &error);
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        }
    } else {
        // Um gateway sem modelo local não tinha admissão nenhuma: o prompt
        // atravessava inteiro, chegava ao decode do peer e derrubava o
        // processo com `CUDA error: out of memory` (ksia, 2026-09-08). A
        // capacidade anunciada pelo peer que serve este modelo é a mesma que
        // `/v1/models` publica, e vale como teto aqui. Fica antes de qualquer
        // sondagem de saúde de propósito: um pedido já inadmissível não
        // justifica uma volta na rede, muito menos um prefill.
        let (capacity, capacity_pending) = state.runtime_capacity_for_model(Some(&model)).await;
        // Capacidade pendente é ausência de informação, não permissão: quem
        // decide nesse caso continua sendo o peer.
        if let Some(capacity) = capacity.filter(|_| !capacity_pending) {
            if let Some(error) =
                prompt_budget_error_of(capacity.n_ctx, capacity.max_new_tokens, &prompt)
            {
                let _ = tx.send(GenEvent::Failed(error)).await;
                return;
            }
        }
    }
    // One session's window (`n_ctx_per_session`): the same capacity the
    // admission above uses, applied again to the total after prefill, with the
    // real prompt size (`OutputBudget::within_session_window`).
    let session_window = match state.runtime_capacity_for_model(Some(&model)).await {
        (Some(capacity), false) => Some(capacity.n_ctx),
        _ => state.runtime.config().await.map(|cfg| cfg.n_ctx),
    };
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        validation_started,
        validation_wall,
        "validation",
    );

    let orchestration_started = Instant::now();
    let orchestration_wall = wall_clock_ms();
    // O anúncio P2P pode chegar depois da carga local. A admissão devolve a
    // fotografia executável do `/health` junto com cada peer, para que o
    // planejador não transforme essa janela de propagação num buraco falso.
    let peers = ready_for_pipeline(
        &state.peer_http,
        &state.admission_health,
        &state.network,
        state.all_peers().await,
        ADMISSION_PROBE_TIMEOUT,
    )
    .await;
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        orchestration_started,
        orchestration_wall,
        "orchestration",
    );

    // `qwen3:8b@d-4f2a91c3` escolhe **qual** implantação atende; o nome pelado
    // deixa a escolha para quem monta o pipeline. Ver `deployment::split_selector`.
    let (model, deploy_id) = crate::deployment::split_selector(&model);

    // Total layers for THIS model, as reported by a peer that serves it.
    let total_layers = peers
        .iter()
        .find(|p| match deploy_id {
            Some(id) => p.deploy_id == id,
            None => p.model == model,
        })
        .map(|p| p.total_layers)
        .unwrap_or(32);

    let route_rtt = state
        .network
        .rtts
        .fresh_snapshot()
        .into_iter()
        .map(|measurement| (measurement.target_node_id, measurement.rtt_ms))
        .collect::<std::collections::HashMap<_, _>>();
    let selection_started = Instant::now();
    let selection_wall = wall_clock_ms();
    let prefix_model = format!(
        "{}|deployment={}|chat_template={chat_template}|thinking_enabled={thinking_enabled}",
        model,
        deploy_id.unwrap_or("<default>")
    );
    let cache_prompt = cache_key.unwrap_or_else(|| prompt.clone());
    // Planejamento por tempo de entrega (`fastest`): velocidade medida de cada
    // peer, fila de toda a rede, RTT de cada aresta e o prefixo já retido.
    // Sem nenhum caminho viável por ele, o planejador guloso de antes decide.
    let fastest = plan_by_delivery_time(
        &state,
        &peers,
        model,
        deploy_id,
        total_layers,
        &route_rtt,
        preferred_head.as_deref(),
        &prefix_model,
        &cache_prompt,
        budget.total,
        images.is_empty(),
    )
    .await;
    let (planned, planned_return) = match fastest {
        Some((plan, mode)) => (Some(plan), Some(mode)),
        None => (
            crate::pipeline::plan_pipeline_with_rtt_from_head(
                &peers,
                model,
                deploy_id,
                total_layers,
                &state.loads,
                &route_rtt,
                preferred_head.as_deref(),
            )
            .await,
            None,
        ),
    };
    let Some(mut plan) = planned else {
        let error = format!(
            "Nenhum pipeline válido para o modelo '{model}' (verifique a cobertura de camadas 0..{total_layers})"
        );
        log_early_outcome("recusado", &error);
        let _ = tx.send(GenEvent::Failed(error)).await;
        return;
    };
    if let Err(error) = reload_idle_pipeline(&state, &plan, &peers).await {
        log_early_outcome("reload_model", &error.to_string());
        let _ = tx.send(GenEvent::Failed(error.to_string())).await;
        return;
    }
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        selection_started,
        selection_wall,
        "pipeline_selection",
    );

    // The gateway may be a middle or tail peer. Vision is an admission fact
    // of the selected head, so only reject here when this process owns that
    // head; a remote head performs the same check in its inference runtime.
    if !images.is_empty()
        && plan
            .steps
            .first()
            .is_some_and(|step| step.node_id == state.network.node_id)
    {
        let mmproj_path = state.runtime.config().await.and_then(|cfg| cfg.mmproj_path);
        if let Some(error) = crate::openai::image_vision_error(&images, mmproj_path.as_deref()) {
            log_early_outcome("recusado", error);
            let _ = tx.send(GenEvent::Failed(error.to_string())).await;
            return;
        }
    }

    // A rota e escolhida por quem vai discar, nao por quem planejou: o plano
    // pode ter vindo de uma maquina noutra rede, e so este no mediu os
    // candidatos. Resolver aqui, uma vez, fixa o endereco de cada hop para a
    // sessao inteira - o cache KV do outro lado e por sessao, e trocar de
    // endereco na metade e pior que a rota lenta.
    let route_started = Instant::now();
    let route_wall = wall_clock_ms();
    {
        let mut fixadas = std::collections::HashMap::new();
        for step in plan.steps.iter_mut() {
            step.inference_url =
                crate::pipeline::resolve_step_url(step, &state.network.routes, &mut fixadas);
        }
    }
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        route_started,
        route_wall,
        "route_resolution",
    );
    let prefix_key = crate::prefix_cache::PrefixRouteKey::new(
        prefix_model.clone(),
        plan.steps.iter().map(|step| {
            format!(
                "{}@{}:{}-{}",
                step.node_id, step.inference_url, step.exec_start, step.exec_end
            )
        }),
    );
    let adaptive_chunk_route = format!("{}|{}", prefix_key.model, prefix_key.hops.join("|"));
    // A hit reuses the peer session that stayed parked at the end of the prior
    // prompt. A miss gets a fresh id and is published only after this turn has
    // completed its prefill, so an incomplete session cannot enter the cache.
    let session_started = Instant::now();
    let session_wall = wall_clock_ms();
    let mut prefix_lease = state.prefix_cache.acquire(
        prefix_key.clone(),
        &cache_prompt,
        Uuid::new_v4().to_string(),
    );
    let mut session_id = prefix_lease.session_id().to_string();
    let mut reuse_prefix = prefix_lease.is_hit() && images.is_empty();
    // Camada de disco (Phase 3): um miss em memória que casa um snapshot
    // automático em disco - típico depois de um reinício - restaura o KV em
    // todos os hops em vez de refazer o prefill a frio. Ver
    // `crate::prefix_cache::PrefixDiskIndex`.
    let mut disk_restore_hash: Option<String> = None;
    if !reuse_prefix && images.is_empty() {
        if let Some(disk) = state.prefix_disk.clone() {
            if let Some(entry) =
                disk.longest_prefix_match(&prefix_key.model, &prefix_key.hops, &cache_prompt)
            {
                let restored_sid = format!("disk-{}", Uuid::new_v4());
                match restore_deployment(
                    &plan,
                    &auto_slot_name(&entry.key_hash),
                    &restored_sid,
                    &state.peer_http,
                )
                .await
                {
                    Ok(restored_tokens) => {
                        prefix_lease.discard();
                        prefix_lease = state.prefix_cache.adopt(
                            prefix_key.clone(),
                            &entry.prompt,
                            restored_sid.clone(),
                            restored_tokens,
                        );
                        session_id = restored_sid;
                        reuse_prefix = true;
                        disk.touch(&entry.key_hash);
                        info!(
                            "♻️  prefixo restaurado do disco: {restored_tokens} tokens em {} hop(s)",
                            plan.steps.len()
                        );
                        disk_restore_hash = Some(entry.key_hash);
                    }
                    Err(error) => {
                        warn!("restauração de prefixo do disco falhou ({error}); seguindo a frio");
                        disk.forget(&entry.key_hash);
                    }
                }
            }
        }
    }
    info!(
        "prefix gateway: session={} lease_hit={} reuse_requested={} cached_tokens={} cached_prompt_bytes={} request_prompt_bytes={} miss_reason={}",
        &session_id[..8.min(session_id.len())],
        prefix_lease.is_hit(),
        reuse_prefix,
        prefix_lease.cached_tokens,
        prefix_lease.cached_prompt.len(),
        cache_prompt.len(),
        prefix_lease.miss_reason().unwrap_or("none"),
    );
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        session_started,
        session_wall,
        "session_acquire",
    );

    // A sessão não pode trocar de réplica depois que o primeiro prefill criou
    // o KV. O plano é fixado uma vez e esta afinidade é a referência explícita
    // usada para revisar qualquer transformação futura da rota.
    let route_affinity = crate::pipeline::RouteAffinity::from_plan(
        deploy_id.unwrap_or_default(),
        &session_id,
        &plan,
    );
    debug_assert!(route_affinity.matches_plan(&plan));

    // Para onde a cauda entrega a resposta. `None` quando esta máquina não
    // sabe nenhum endereço de si mesma — aí a cadeia volta a borbulhar, que é
    // lento mas nunca fica sem resposta.
    let direct = crate::direct::DirectReply::new(
        state.reply_inbox.clone(),
        crate::direct::self_reply_urls(
            state.network.advertised_host(),
            state.self_reply_hosts(),
            state.gateway_port,
        ),
        state.network.node_id.clone(),
    )
    .map(|reply| {
        reply
            .with_quic_addresses(state.network.gateway_quic_addr_candidates())
            .with_libp2p(
                state.network.libp2p_peer_id.clone(),
                state.network.libp2p_addrs(),
            )
    });
    // O modo de volta é escolhido junto com o caminho pelo planejador por
    // tempo de entrega (`fastest::plan_fastest`): anel quando a resposta
    // chega antes voltando pelas conexões da ida - ou quando a cauda não tem
    // volta até aqui. O anel dispensa o `reply_to`. Com o planejador guloso de
    // reserva, a regra `return_path_prefers_ring` decide.
    // `POM_RETURN_MODE` vale também para o planejador guloso de reserva.
    let planned_return = crate::fastest::forced_return_mode().or(planned_return);
    let direct = match (direct, planned_return) {
        (Some(_), Some(crate::fastest::ReturnMode::Ring)) => {
            info!(
                "resposta pelo anel ({})",
                if crate::fastest::forced_return_mode().is_some() {
                    "imposta por POM_RETURN_MODE"
                } else {
                    "escolhida no planejamento"
                }
            );
            None
        }
        (Some(reply), Some(crate::fastest::ReturnMode::Direct)) => Some(reply),
        (Some(reply), None) => {
            let chain: Vec<String> = plan.steps.iter().map(|step| step.node_id.clone()).collect();
            match crate::fastest::return_path_prefers_ring(
                &chain,
                &state.network.node_id,
                &state.admission_health.perf_snapshot(),
                &crate::route_plan::book().kind_map(crate::route_plan::Plane::Inference),
            ) {
                Some(reason) => {
                    info!("resposta pelo anel: {reason}");
                    None
                }
                None => Some(reply),
            }
        }
        (None, _) => None,
    };

    info!(
        "🔀 Pipeline planejado: {} hops [{}] session={} max_tokens={} (perna {}) resposta={}",
        plan.steps.len(),
        plan.hop_chain(),
        &session_id[..8],
        budget.total,
        budget.leg,
        if remote_stream_capable(&plan, &state.network.node_id, direct.is_some()) {
            "remote-stream"
        } else if direct.is_some() && plan.supports_direct_reply() {
            "direta"
        } else if plan.supports_relay_chain() {
            "anel"
        } else {
            "hub"
        },
    );

    let mut tokens_generated: u32 = 0;
    // Pernas costuradas até agora. Começa em 1: toda resposta tem uma.
    let mut legs: u32 = 1;
    let mut retained_prefix_tokens: Option<u32> = None;
    // Quantos tokens do prompt a cabeça de fato não reprocessou. Não é a
    // escolha do cache, é o que o pipeline executou - ver
    // `PrefillOutcome::cached_tokens`. Sai no `GenSummary` e daí para
    // `usage.prompt_tokens_details.cached_tokens`.

    // Chunk size, wire precision, and the speculative-decoding knobs all come
    // from the node's configuration, so they're operator knobs rather than
    // constants baked into the protocol.
    // The head tokenizes; the gateway only needs the size to plan chunks.
    // About 4 bytes per token, minus what the prefix cache will reuse.
    let estimated_new_prompt_tokens = ((prompt.len() as f64 / 4.0).ceil() as u32)
        .saturating_sub(if reuse_prefix { prefix_lease.cached_tokens } else { 0 })
        .max(1);
    let (
        chunk_tokens,
        max_inflight_chunks,
        dtype,
        lookup_enabled,
        lookup_k,
        speculative_m,
        mtp_configured,
        mtp_k,
    ) = {
        let cfg = state.runtime.config().await;
        let n_batch = cfg.as_ref().map(|c| c.n_batch).unwrap_or(512);
        (
            // Um pedaço é decodificado como um `llama_batch` no head; maior que
            // `n_batch` aborta o llama.cpp. `prompt_chunk_tokens` é o mesmo
            // clamp de `prompt_budget_error`, e é ele que vale - não o valor
            // cru do knob.
            cfg.as_ref()
                .map(|config| {
                    state.adaptive_chunk_tuner.choose(
                        &adaptive_chunk_route,
                        prompt_chunk_tokens(config),
                        config.n_batch,
                        estimated_new_prompt_tokens,
                        config.prefill_max_inflight_chunks as usize,
                    )
                })
                .unwrap_or(crate::prefill::DEFAULT_CHUNK_TOKENS),
            cfg.as_ref()
                .map(|c| c.prefill_max_inflight_chunks as usize)
                .unwrap_or(crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS),
            cfg.as_ref()
                .map(|c| c.hidden_state_dtype)
                .unwrap_or_default(),
            cfg.as_ref().is_some_and(|c| c.speculative_decoding_enabled),
            speculative_k_of(cfg.as_ref().map(|c| c.speculative_k).unwrap_or(4), n_batch),
            cfg.as_ref()
                .map(|c| c.speculative_lookup_window)
                .unwrap_or(3),
            cfg.as_ref().map(|c| c.mtp_enabled),
            // O parâmetro é "quantos tokens rascunhar"; a largura do lote é
            // isso mais o token já confirmado.
            speculative_k_of(
                cfg.as_ref().map(|c| c.mtp_draft_tokens).unwrap_or(3) + 1,
                n_batch,
            ),
        )
    };

    let plan_supports_mtp = plan.supports_mtp();
    let mtp_enabled = mtp_enabled_for_plan(mtp_configured, plan_supports_mtp);
    info!(
        gateway_runtime_config_present = mtp_configured.is_some(),
        gateway_mtp_enabled = ?mtp_configured,
        plan_supports_mtp,
        mtp_enabled,
        mtp_k,
        pipeline_hops = plan.steps.len(),
        "decisão de especulação MTP do pipeline"
    );

    // Qual das duas especulações roda nesta requisição. O MTP vem primeiro
    // quando os dois estão ligados: ele usa uma cabeça treinada do próprio
    // modelo, enquanto o lookup só acerta em saída repetitiva. Nunca os dois
    // na mesma rodada - o lote é um só, e misturar as fontes tornaria a taxa
    // de aceitação impossível de atribuir.
    let (speculative_enabled, speculative_k, draft_source) = if mtp_enabled {
        (true, mtp_k, crate::prefill::DraftSource::Mtp)
    } else {
        (
            lookup_enabled,
            lookup_k,
            crate::prefill::DraftSource::Lookup,
        )
    };

    let mut stop_matcher = StopMatcher::new(stops);
    let cycle_config = state
        .runtime
        .config()
        .await
        .map(|cfg| crate::cycle::CycleConfig {
            enabled: cfg.cycle_detection_enabled,
            min_window_tokens: cfg.cycle_detection_min_window_tokens as usize,
            max_window_tokens: cfg.cycle_detection_max_window_tokens as usize,
            repetitions: cfg.cycle_detection_repetitions as usize,
        })
        .unwrap_or(state.cycle_config);
    let mut cycle_detector = crate::cycle::CycleDetector::new(cycle_config);

    // O evento final não pode sair antes de a sessão estar arrumada.
    //
    // Um chat manda o turno seguinte assim que o stream fecha. Enquanto o
    // rewind e a liberação rodavam *depois* do `[DONE]`, a requisição seguinte
    // chegava no meio deles - medido: 54 ms antes de a liberação do turno
    // anterior terminar - e pegava o cache num estado que ninguém tinha
    // acabado de escrever. Aqui os eventos de token seguem na hora, ao vivo, e
    // só o terminal (`Done`/`Failed`) espera a arrumação.
    let client_tx = tx;
    let (tx, inner_rx) = mpsc::channel::<GenEvent>(64);
    let pump = tokio::spawn(forward_generation_events(
        client_tx.clone(),
        inner_rx,
        cancellation.clone(),
    ));

    // Whatever ends this generation — EOS, the token limit, the client hanging
    // up, or a peer erroring — every peer has to get its KV slot back. Only the
    // sampling peer used to free anything, and only on EOS, so the first peer
    // leaked a slot per request and died after eight of them.
    async {
        // ── Prefill: the prompt goes in chunk by chunk, and the last peer hands
        // back the first token once the whole prompt is in.
        // Reusar prefixo é otimização, nunca motivo para a requisição falhar.
        // Um peer pode ter despejado a sessão, reiniciado ou divergido do que
        // este gateway acha que ele guarda; nesse caso a primeira tentativa
        // morre com posições fora de ordem. A segunda vai fria, que é
        // exatamente o comportamento de antes desta funcionalidade existir.
        let prefill_attempt = |reuse: bool, resume: Option<crate::prefill::PrefillResumable>| {
            crate::prefill::run_with_options(
                &plan,
                model,
                &session_id,
                &prompt,
                &images,
                chat_template,
                thinking_enabled,
                chunk_tokens,
                dtype,
                &state.peer_http,
                &state.network.routes,
                direct.as_ref(),
                state.tunnel_tx.as_ref(),
                &cancellation,
                reuse,
                max_inflight_chunks,
                resume,
                trace_id.as_deref(),
                probe.as_ref(),
            )
        };
        let mut prefill_result = prefill_attempt(reuse_prefix, None).await;
        let mut prefill_failure_trace = prefill_result
            .as_ref()
            .err()
            .and_then(crate::prefill::partial_trace);
        let mut unknown_session_recovery_attempted = false;
        let mut cold_recovery_attempted = false;
        if let Err(e) = &prefill_result {
            if crate::relay::is_frame_size_error(e) {
                warn!(
                    "prefill falhou por limite de frame ({e}); \
                     não é recuperável repetindo o mesmo chunk"
                );
            } else if !crate::prefill::is_cancelled(e) {
                if let Some(rp) = crate::prefill::resume_point(e) {
                    warn!(
                        "prefill falhou antes de entregar o pedaço {} (posição {}, {e}); retomando nesse ponto e preservando o KV",
                        rp.from_chunk, rp.start_pos
                    );
                    prefill_result = prefill_attempt(reuse_prefix, Some(rp)).await;
                    if let Some(trace) = prefill_result
                        .as_ref()
                        .err()
                        .and_then(crate::prefill::partial_trace)
                    {
                        prefill_failure_trace = Some(trace);
                    }
                } else if let Some(reason) = prefill_rebuild_reason(e) {
                    unknown_session_recovery_attempted =
                        crate::relay::is_unknown_session_error(e);
                    cold_recovery_attempted = true;
                    warn!(
                        "prefill precisa de reconstrução fria ({reason}; {e}); limpando as sessões remotas uma vez"
                    );
                    prefill_result = prefill_attempt(false, None).await;
                    if let Some(trace) = prefill_result
                        .as_ref()
                        .err()
                        .and_then(crate::prefill::partial_trace)
                    {
                        prefill_failure_trace = Some(trace);
                    }
                }
            }
        }
        if !cold_recovery_attempted {
            if let Err(error) = &prefill_result {
                if !crate::prefill::is_cancelled(error) {
                    if let Some(reason) = prefill_rebuild_reason(error) {
                        unknown_session_recovery_attempted =
                            crate::relay::is_unknown_session_error(error);
                        warn!(
                            "prefill retomado ainda exige reconstrução fria ({reason}; {error}); limpando as sessões remotas uma vez"
                        );
                        prefill_result = prefill_attempt(false, None).await;
                        if let Some(trace) = prefill_result
                            .as_ref()
                            .err()
                            .and_then(crate::prefill::partial_trace)
                        {
                            prefill_failure_trace = Some(trace);
                        }
                    }
                }
            }
        }
        let first = match prefill_result {
            Ok(mut o) => {
                o.trace.probe = o.probe.take();
                if let (Some(decision), Some(result)) = (probe.as_ref(), o.trace.probe.as_ref()) {
                    o.trace.jev = Some(decision.trace_summary(
                        result,
                        o.trace.prefill_ms,
                        o.cached_tokens,
                    ));
                }
                o.trace.gateway_prefix_miss_reason =
                    prefix_lease.miss_reason().map(str::to_owned);
                o.trace.prefill_chunk_tokens = chunk_tokens;
                state.adaptive_chunk_tuner.observe(
                    &adaptive_chunk_route,
                    &o.trace,
                    o.prompt_tokens,
                    o.cached_tokens,
                );
                if unknown_session_recovery_attempted {
                    o.trace.prefix_reuse_status = Some("session-lost-cold".to_owned());
                } else if disk_restore_hash.is_some()
                    && o.trace.prefix_reuse_status.as_deref() == Some("full")
                {
                    // O reuso veio de um snapshot em disco, não de uma sessão
                    // que ficou viva em memória.
                    o.trace.prefix_reuse_status = Some("disk".to_owned());
                }
                info!(
                    "📥 prompt de {} tokens em {} pedaço(s), {} do cache · {:.1}s até a primeira palavra",
                    o.prompt_tokens,
                    o.chunks,
                    o.cached_tokens,
                    request_started.elapsed().as_secs_f64(),
                );
                retained_prefix_tokens = Some(o.retained_tokens);
                o
            }
            Err(e) if crate::prefill::is_cancelled(&e) => {
                info!("cliente desconectou durante o prefill; cancelando pipeline");
                return;
                }
                Err(e) => {
                let message = if unknown_session_recovery_attempted
                    || crate::relay::is_unknown_session_error(&e)
                {
                    "Prefill falhou: um peer perdeu o estado da sessão; o sistema tentou reconstruir o contexto, mas não conseguiu concluir e o pedido não foi executado. Tente enviar a mensagem novamente quando o cluster estiver estável.".to_string()
                } else if cold_recovery_attempted {
                    "Prefill falhou: uma falha de transporte deixou incerto se o peer processou o último chunk. O sistema tentou reconstruir o contexto uma vez, mas não conseguiu concluir e o pedido não foi executado. Tente enviar a mensagem novamente quando o cluster estiver estável.".to_string()
                } else {
                    format!("Prefill falhou: {}", crate::prefill::failure_cause(&e))
                };
                warn!("prefill falhou: {e:#}");
                let _ = tx
                    .send(GenEvent::FailedWithTelemetry {
                        error: message,
                        trace: prefill_failure_trace,
                    })
                    .await;
                return;
            }
        };

        let windowed = budget.within_session_window(session_window, first.prompt_tokens);
        if windowed.total < budget.total {
            info!(
                "output limited by the session window: {} of {} tokens (prompt {} of {:?})",
                windowed.total, budget.total, first.prompt_tokens, session_window
            );
            budget = windowed;
        }

        let first_token_at = Instant::now();
        let mut next_token = first.token_id;
        let mut trace = first.trace;
        trace.first_token.gateway_stream_at_ms = Some(wall_clock_ms());
        if crate::telemetry::Profile::current().collects_spans() {
            if let Some(return_ms) = trace.first_token.node_to_gateway_ms {
                let end_offset_ms = request_started.elapsed().as_millis() as u64;
                gateway_spans.push(crate::prefill::TraceSpan::new(
                    "first_token_return",
                    "gateway",
                    None,
                    wall_clock_ms().saturating_sub(return_ms),
                    end_offset_ms.saturating_sub(return_ms),
                    return_ms,
                    None,
                ));
            }
        }
        trace.set_accounted_spans(gateway_spans.clone());
        trace.request_state = match trace.prefix_reuse_status.as_deref() {
            Some("full" | "disk") => crate::prefill::RequestState::Warm,
            Some(_) => crate::prefill::RequestState::Cold,
            None => crate::prefill::RequestState::Unknown,
        };
        trace.server_ttft_ms = Some(
            first_token_at
                .saturating_duration_since(request_started)
                .as_millis() as u64,
        );
        tokens_generated += 1;
        note_leg_boundary(budget, tokens_generated, &mut legs);
        match send_text_with_stop(&tx, &mut stop_matcher, &first.text).await {
            StopSend::Disconnected => {
                info!("cliente desconectou logo após o prompt");
                return;
            }
            StopSend::Stopped(stop_sequence) => {
                send_stopped_summary(
                    &tx,
                    &plan,
                    request_started,
                    first_token_at,
                    tokens_generated,
                    legs,
                    trace,
                    stop_sequence,
                )
                .await;
                return;
            }
            StopSend::Continue => {}
        }
        if !first.done {
            if let Some(cycle) = cycle_detector.observe(first.token_id, &first.text) {
                if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                    return;
                }
                send_cycle_summary(
                    &tx,
                    &plan,
                    request_started,
                    first_token_at,
                    tokens_generated,
                    legs,
                    trace,
                    cycle,
                )
                .await;
                return;
            }
        }
        if generation_finish_reason(budget.total, tokens_generated, first.done)
            == Some("length")
        {
            if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                return;
            }
            send_length_summary(
                &tx,
                &plan,
                request_started,
                first_token_at,
                tokens_generated,
                legs,
                trace,
            )
            .await;
            return;
        }

        if first.done {
            if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                return;
            }
            let summary = summary_for(
                &plan,
                request_started,
                first_token_at,
                tokens_generated,
                legs,
                trace,
                false,
                None,
                None,
            );
            let _ = tx.send(GenEvent::Done(summary)).await;
            return;
        }

        // A remote head can own the autoregressive loop. The control frame
        // below is acknowledged once; all later tokens arrive through its
        // bounded callback stream. A local head never enters this branch, and
        // peers without the advertised capability keep the established
        // /v1/step path.
        let remote_stream_capable = remote_stream_capable(
            &plan,
            &state.network.node_id,
            direct.is_some(),
        );
        let sliced_mtp_requested = mtp_enabled && plan_supports_mtp && plan.steps.len() >= 2;
        let sliced_mtp = sliced_mtp_requested;
        info!(
            remote_stream_capable,
            sliced_mtp_requested,
            sliced_mtp,
            plan_supports_mtp,
            "caminho de decode do pipeline"
        );
        if mtp_enabled && plan.steps.len() >= 2 && !sliced_mtp_requested {
            info!(
                reason = sliced_mtp_disable_reason(&plan),
                "pipeline fatiado sem MTP: seguindo com a capacidade disponível"
            );
        }
        let mut remote_stream = if remote_stream_capable {
            // The remote head owns the autoregressive cycle, MTP included: the
            // tail drafts `nextn` after sampling and returns `next_draft` in
            // the same reply, and the head installs it in its next step. The
            // draft costs one callback more time, but a round then yields up
            // to `mtp_k` tokens instead of one, which is what pays the
            // head-to-tail round trip.
            let speculate = remote_stream_speculation(
                &plan,
                speculative_enabled,
                mtp_enabled,
                speculative_k,
                speculative_m,
                mtp_k,
                draft_source,
            );
            match crate::prefill::step_stream(
                &plan,
                model,
                &session_id,
                next_token,
                tokens_generated as u64,
                dtype,
                &state.peer_http,
                &state.network.routes,
                direct.as_ref().expect("stream capability requires reply address"),
                state.tunnel_tx.as_ref(),
                speculate,
                &cancellation,
                trace_id.as_deref(),
            )
            .await
            {
                Ok(stream) => {
                    trace.merge(&stream.initial_trace);
                    Some(stream)
                }
                Err(error) => {
                    warn!("transporte remoto por lotes falhou: {error}");
                    let summary = summary_for(
                        &plan,
                        request_started,
                        first_token_at,
                        tokens_generated,
                        legs,
                        trace,
                        true,
                        None,
                        None,
                    );
                    let _ = tx.send(GenEvent::Done(summary)).await;
                    return;
                }
            }
        } else {
            None
        };

        if plan.supports_speculation() && speculative_enabled {
            // Vira `false` na primeira rodada em que a cabeça recusar o
            // rascunho. Pedir de novo só repetiria a recusa, e o pedido custa
            // caro por um motivo que não se vê: no caminho legado,
            // `step_speculative` continua no anel borbulhado (2N travessias por
            // token). O transporte remoto já mantém o laço no head e chega
            // aqui pela fila de callbacks, sem pedir outro quadro ao gateway.
            // O MTP fatiado é a outra metade da mesma rodada: no caminho
            // legado a cauda devolve o rascunho e o gateway o injeta; no
            // stream remoto a cabeça o instala dentro do próprio ciclo. Só
            // há MTP fatiado com 2+ hops - com um é o modelo inteiro, que já
            // usa `step_speculative`.
            let mut especulando = true;
            // Rascunho que a cauda devolveu na rodada anterior, esperando
            // para ser injetado na próxima. `draft_sent` é lido antes do
            // `take`: distingue a rodada 1 (nenhum rascunho viajou ainda, e a
            // recusa da cabeça é esperada) das seguintes (o rascunho foi
            // mandado — ignorado aí é motivo para parar).
            let mut stored_draft: Option<Vec<i32>> = None;
            while remaining_generation_steps(budget.total, tokens_generated) > 0 {
                if cancellation.is_cancelled() {
                    return;
                }
                let remote_stream_mode = remote_stream.is_some();
                let draft_sent = especulando
                    && sliced_mtp
                    && !remote_stream_mode
                    && stored_draft.is_some();
                let res = if let Some(stream) = remote_stream.as_mut() {
                    stream.next(&cancellation).await
                } else if especulando {
                    if sliced_mtp {
                        crate::prefill::step(
                            &plan,
                            model,
                            &session_id,
                            next_token,
                            dtype,
                            &state.peer_http,
                            &state.network.routes,
                            direct.as_ref(),
                            state.tunnel_tx.as_ref(),
                            Some(crate::infer_server::Speculate {
                                k: speculative_k,
                                m: speculative_m,
                                source: crate::infer_server::DraftSource::Mtp,
                                draft: stored_draft.take(),
                            }),
                            trace_id.as_deref(),
                        )
                        .await
                    } else {
                        crate::prefill::step_speculative(
                            &plan,
                            model,
                            &session_id,
                            next_token,
                            speculative_k,
                            speculative_m,
                            draft_source,
                            dtype,
                            &state.peer_http,
                            &state.network.routes,
                            state.tunnel_tx.as_ref(),
                            trace_id.as_deref(),
                        )
                        .await
                    }
                } else {
                    crate::prefill::step(
                        &plan,
                        model,
                        &session_id,
                        next_token,
                        dtype,
                        &state.peer_http,
                        &state.network.routes,
                        direct.as_ref(),
                        state.tunnel_tx.as_ref(),
                        None,
                        trace_id.as_deref(),
                    )
                    .await
                };

                let step = match res {
                    Ok(step) => step,
                    Err(e) => {
                        let rodada = if especulando {
                            "rodada especulativa"
                        } else {
                            "rodada de decodificação"
                        };
                        warn!("Erro na {rodada} (token {tokens_generated}): {e}");
                        if tokens_generated > 0 {
                            info!("devolvendo {tokens_generated} tokens parciais");
                            let summary = summary_for(
                                &plan,
                                request_started,
                                first_token_at,
                                tokens_generated,
                                legs,
                                trace,
                                true,
                                None,
                                None,
                            );
                            let _ = tx.send(GenEvent::Done(summary)).await;
                        } else {
                            let _ = tx
                                .send(GenEvent::Failed(format!(
                                    "Pipeline falhou na {rodada}: {e}"
                                )))
                                .await;
                        }
                        return;
                    }
                };
                if especulando {
                    if sliced_mtp {
                        if remote_stream_mode {
                            if !sliced_mtp_round_continues(remote_stream_mode, draft_sent, &step) {
                                let reason = step
                                    .speculation_disabled_reason
                                    .as_deref()
                                    .unwrap_or(
                                        crate::infer_server::SPECULATION_REASON_MODEL_WITHOUT_MTP,
                                    );
                                let detail = step
                                    .speculation_disabled_detail
                                    .as_deref()
                                    .unwrap_or("cauda não informou a causa da desativação");
                                info!(
                                    reason,
                                    detail,
                                    "pipeline fatiado sem rascunho MTP: seguindo sem especulação"
                                );
                                especulando = false;
                            }
                        } else {
                            // Dois motivos para desistir: um rascunho já
                            // enviado foi recusado, ou a rodada inicial não
                            // produziu o primeiro rascunho.
                            if !sliced_mtp_round_continues(remote_stream_mode, draft_sent, &step) {
                                let reason = step
                                    .speculation_disabled_reason
                                    .as_deref()
                                    .unwrap_or(
                                        crate::infer_server::SPECULATION_REASON_MODEL_WITHOUT_MTP,
                                    );
                                let detail = step
                                    .speculation_disabled_detail
                                    .as_deref()
                                    .unwrap_or("cauda não informou a causa da desativação");
                                info!(
                                    reason,
                                    detail,
                                    "pipeline fatiado sem rascunho MTP: seguindo sem especulação"
                                );
                                especulando = false;
                            } else {
                                stored_draft = step.next_draft;
                            }
                        }
                    } else if !step.speculated {
                        // A cabeça recusou rascunhar — hoje só acontece se
                        // ela é de uma versão anterior a este fix, ou se o
                        // rascunho por repetição não achou nada para propor.
                        info!("nenhum hop rascunhou: seguindo sem especulação");
                        especulando = false;
                    }
                }
                trace.merge(&step.trace);

                // A round can confirm more tokens than remain in the budget —
                // stream only what fits, and note it so `step.done` (which
                // may refer to a position we never even looked at) isn't
                // trusted past the cut.
                let remaining =
                    remaining_generation_steps(budget.total, tokens_generated) as usize;
                let take = step.accepted.len().min(remaining);
                let truncated_by_budget = take < step.accepted.len();

                let mut cycle_match = None;
                for (token_id, text) in step.accepted.iter().take(take) {
                    next_token = *token_id;
                    tokens_generated += 1;
                    note_leg_boundary(budget, tokens_generated, &mut legs);
                    match send_text_with_stop(&tx, &mut stop_matcher, text).await {
                        StopSend::Disconnected => {
                            info!("cliente desconectou após {tokens_generated} tokens");
                            return;
                        }
                        StopSend::Stopped(stop_sequence) => {
                            send_stopped_summary(
                                &tx,
                                &plan,
                                request_started,
                                first_token_at,
                                tokens_generated,
                                legs,
                                trace,
                                stop_sequence,
                            )
                            .await;
                            return;
                        }
                        StopSend::Continue => {}
                    }
                    if cycle_match.is_none() {
                        cycle_match = cycle_detector.observe(*token_id, text);
                    }
                    if cycle_match.is_some() {
                        break;
                    }
                }

                if let Some(cycle) = cycle_match {
                    if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                        return;
                    }
                    send_cycle_summary(
                        &tx,
                        &plan,
                        request_started,
                        first_token_at,
                        tokens_generated,
                        legs,
                        trace,
                        cycle,
                    )
                    .await;
                    return;
                }

                let finish_reason = generation_finish_reason(
                    budget.total,
                    tokens_generated,
                    step.done && !truncated_by_budget,
                );
                if finish_reason.is_some() {
                    if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                        return;
                    }
                    let summary = summary_for(
                        &plan,
                        request_started,
                        first_token_at,
                        tokens_generated,
                        legs,
                        trace,
                        false,
                        (finish_reason == Some("length")).then(|| "length".to_string()),
                        None,
                    );
                    info!(
                        "✅ Pipeline completo ({}): {} tokens em {:.1}s ({:.1} tok/s)",
                        if especulando {
                            "especulativo"
                        } else {
                            "sem especulação"
                        },
                        tokens_generated,
                        summary.timings.total_ms as f64 / 1000.0,
                        summary.timings.total_tokens_per_sec
                    );
                    let _ = tx.send(GenEvent::Done(summary)).await;
                    return;
                }

                // Continuing to another round: every hop decoded the full
                // batch, so every hop needs to discard whatever this round
                // didn't confirm before the next one starts. Skipped when
                // finishing above — `release_sessions` right after this
                // block wipes the whole session anyway, making a partial
                // rollback moot.
                if gateway_rollback_needed(
                    remote_stream_mode,
                    step.accept_count,
                    step.batch_width,
                ) {
                    let ok = rollback_sessions(
                        &plan,
                        &session_id,
                        step.accept_count,
                        step.batch_width,
                        &state.peer_http,
                    )
                    .await;
                    if !ok {
                        // A hop kept the rejected tail's KV entries — its
                        // cache still holds attention/recurrent state for
                        // positions the client never confirmed. Continuing
                        // this session risks the model attending to wrong
                        // (rejected) tokens in silence, not just a crash, so
                        // this ends the generation with whatever was already
                        // confirmed rather than trust the cache further —
                        // same shape as the `Err(e)` branch above.
                        warn!(
                            "rollback parcial recusado por algum hop após \
                             {tokens_generated} tokens: encerrando a geração \
                             em vez de arriscar cache contaminado"
                        );
                        if tokens_generated > 0 {
                            let summary = summary_for(
                                &plan,
                                request_started,
                                first_token_at,
                                tokens_generated,
                                legs,
                                trace,
                                true,
                                None,
                                None,
                            );
                            let _ = tx.send(GenEvent::Done(summary)).await;
                        } else {
                            let _ = tx
                                .send(GenEvent::Failed(
                                    "rollback especulativo recusado pelo hop".to_string(),
                                ))
                                .await;
                        }
                        return;
                    }
                }
            }
            return;
        }

        while remaining_generation_steps(budget.total, tokens_generated) > 0 {
            let cycle_started = Instant::now();
            let res = if let Some(stream) = remote_stream.as_mut() {
                stream.next(&cancellation).await
            } else {
                crate::prefill::step(
                    &plan,
                    model,
                    &session_id,
                    next_token,
                    dtype,
                    &state.peer_http,
                    &state.network.routes,
                    direct.as_ref(),
                    state.tunnel_tx.as_ref(),
                    None,
                    trace_id.as_deref(),
                )
                .await
            };

            match res {
                Ok(step) => {
                    trace.merge(&step.trace);
                    if step.accepted.is_empty() {
                        let _ = tx
                            .send(GenEvent::Failed(
                                "Pipeline não devolveu token confirmado".to_string(),
                            ))
                            .await;
                        return;
                    }
                    let remaining =
                        remaining_generation_steps(budget.total, tokens_generated) as usize;
                    let take = step.accepted.len().min(remaining);
                    let truncated_by_budget = take < step.accepted.len();
                    let mut cycle_match = None;
                    for (index, (token_id, text)) in step.accepted.iter().take(take).enumerate() {
                        next_token = *token_id;
                        tokens_generated += 1;
                        note_leg_boundary(budget, tokens_generated, &mut legs);
                        match send_text_with_stop(&tx, &mut stop_matcher, text).await {
                            StopSend::Disconnected => {
                                // Client hung up; stop burning GPU time on a dead request.
                                info!("cliente desconectou após {tokens_generated} tokens");
                                return;
                            }
                            StopSend::Stopped(stop_sequence) => {
                                send_stopped_summary(
                                    &tx,
                                    &plan,
                                    request_started,
                                    first_token_at,
                                    tokens_generated,
                                    legs,
                                    trace,
                                    stop_sequence,
                                )
                                .await;
                                return;
                            }
                            StopSend::Continue => {}
                        }
                        if index == 0 {
                            trace.record_token_cycle(
                                cycle_started.elapsed().as_millis() as u64,
                            );
                        }
                        if !step.done || index + 1 < take {
                            if let Some(cycle) = cycle_detector.observe(*token_id, text) {
                                cycle_match = Some(cycle);
                                break;
                            }
                        }
                    }
                    if let Some(cycle) = cycle_match {
                        if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                            return;
                        }
                        send_cycle_summary(
                            &tx,
                            &plan,
                            request_started,
                            first_token_at,
                            tokens_generated,
                            legs,
                            trace,
                            cycle,
                        )
                        .await;
                        return;
                    }
                    let finish_reason = generation_finish_reason(
                        budget.total,
                        tokens_generated,
                        step.done && !truncated_by_budget,
                    );

                    if finish_reason.is_some() {
                        if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                            return;
                        }
                        let summary = summary_for(
                            &plan,
                            request_started,
                            first_token_at,
                            tokens_generated,
                            legs,
                            trace,
                            false,
                            (finish_reason == Some("length")).then(|| "length".to_string()),
                            None,
                        );
                        info!(
                            "✅ Pipeline completo: {} tokens em {:.1}s ({:.1} tok/s)",
                            tokens_generated,
                            summary.timings.total_ms as f64 / 1000.0,
                            summary.timings.total_tokens_per_sec
                        );
                        let _ = tx.send(GenEvent::Done(summary)).await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("Erro no passo {tokens_generated} do pipeline: {e}");
                    if tokens_generated > 0 {
                        // Some output is better than none — the original behaviour.
                        info!("devolvendo {tokens_generated} tokens parciais");
                        let summary = summary_for(
                            &plan,
                            request_started,
                            first_token_at,
                            tokens_generated,
                            legs,
                            trace,
                            true,
                            None,
                            None,
                        );
                        let _ = tx.send(GenEvent::Done(summary)).await;
                    } else {
                        let _ = tx
                            .send(GenEvent::Failed(format!(
                                "Pipeline falhou no passo {tokens_generated}: {e}"
                            )))
                            .await;
                    }
                    return;
                }
            }
        }
    }
    .await;

    drop(tx);
    let terminal = pump.await.ok().flatten();

    // O lease sai daqui de um jeito só: publicado se todos os hops confirmaram
    // que a sessão ficou parada no fim do prompt, descartado com a sessão
    // liberada em qualquer outro caso.
    let mut lease = Some(prefix_lease);
    if let Some(retained_tokens) = retained_prefix_tokens {
        if rewind_sessions(&plan, &session_id, retained_tokens, &state.peer_http).await {
            if let Some(lease) = lease.take() {
                lease.commit(cache_prompt.clone(), retained_tokens);
            }
            // Phase 3: grava o prefixo retido em disco, best-effort e fora do
            // caminho da resposta. A sessão fica parada (retida) em todos os
            // hops até o próximo turno, então há tempo para o `save`.
            if let Some(disk) = state.prefix_disk.clone() {
                let plan = plan.clone();
                let clients = state.peer_http.clone();
                let key = prefix_key.clone();
                let prompt = cache_prompt.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    let hash = prefix_key_hash(&key, &prompt);
                    // Debounce: se o prefixo retido cresceu pouco desde o
                    // último snapshot desta linhagem, só renova o carimbo e
                    // não regrava (cada spill é um serialize de KV + fan-out).
                    if let Some(ancestor) =
                        disk.longest_prefix_match(&key.model, &key.hops, &prompt)
                    {
                        if retained_tokens.saturating_sub(ancestor.cached_tokens)
                            < crate::prefix_cache::SLOT_AUTO_MIN_GROWTH_TOKENS
                        {
                            disk.touch(&ancestor.key_hash);
                            debug!(
                                "spill de prefixo pulado: +{} tokens desde {} (< {})",
                                retained_tokens.saturating_sub(ancestor.cached_tokens),
                                &ancestor.key_hash[..8.min(ancestor.key_hash.len())],
                                crate::prefix_cache::SLOT_AUTO_MIN_GROWTH_TOKENS,
                            );
                            return;
                        }
                    }
                    match snapshot_deployment(&plan, &auto_slot_name(&hash), &sid, &clients).await {
                        Ok(bytes) => {
                            for stale in disk.record(
                                &key.model,
                                &key.hops,
                                &hash,
                                &prompt,
                                retained_tokens,
                                bytes,
                            ) {
                                erase_snapshot(&plan, &auto_slot_name(&stale), &clients).await;
                            }
                        }
                        Err(error) => debug!("spill de prefixo em disco: {error}"),
                    }
                });
            }
        } else {
            warn!(
                "não foi possível preservar o prefixo da sessão {}; liberando KV",
                &session_id[..8.min(session_id.len())]
            );
        }
    }
    if let Some(lease) = lease.take() {
        lease.discard();
        release_sessions(&plan, &session_id, &state.peer_http).await;
    }

    if let Some(event) = terminal {
        let _ = client_tx.send(event).await;
    }
}

/// Encaminha os eventos da geração para o adaptador HTTP e transforma o
/// desaparecimento do consumidor em cancelamento explícito.
///
/// O `Sender::closed` precisa ser observado em paralelo ao recebimento de
/// eventos. Manter outro clone do sender no gerador impede que um monitor
/// baseado somente em `closed` veja a desconexão, e um produtor remoto pode
/// continuar consumindo KV mesmo depois de o SSE ter sido abandonado.
async fn forward_generation_events(
    client_tx: mpsc::Sender<GenEvent>,
    mut inner_rx: mpsc::Receiver<GenEvent>,
    cancellation: crate::prefill::Cancellation,
) -> Option<GenEvent> {
    let mut terminal = None;
    loop {
        let event = tokio::select! {
            _ = client_tx.closed() => {
                cancellation.cancel();
                break;
            }
            event = inner_rx.recv() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event {
            event @ (GenEvent::Done(_)
            | GenEvent::Failed(_)
            | GenEvent::FailedWithTelemetry { .. }) => {
                terminal = Some(event);
                break;
            }
            event => {
                if client_tx.send(event).await.is_err() {
                    cancellation.cancel();
                    break;
                }
            }
        }
    }
    terminal
}

/// Confirma em cada peer que a sessão ficou parada no fim do prompt, pronta
/// para o turno seguinte. Falha em um hop invalida a entrada inteira: manter só
/// parte da cadeia criaria um buraco silencioso.
///
/// Do lado do peer isto **não** recua nada - a geração correu numa sequência de
/// KV separada e some inteira (ver `infer_server::handle_rewind`). Por isso o
/// prazo aqui é curto: é escrituração, não inferência, e o evento final do
/// stream espera por ela.
fn inference_route_candidates(step: &crate::pipeline::PipelineStep) -> Vec<String> {
    let mut candidates = Vec::with_capacity(step.inference_candidates.len() + 1);
    if !step.inference_url.is_empty() {
        candidates.push(step.inference_url.clone());
    }
    let additional: Vec<String> = step
        .inference_candidates
        .iter()
        .filter(|candidate| !candidates.contains(candidate))
        .cloned()
        .collect();
    candidates.extend(additional);
    candidates
}

/// POST de um corpo JSON pequeno a uma rota de ciclo de vida de sessão de um
/// peer, pelo mesmo transporte que o decode: QUIC primeiro quando a preferência
/// do cluster é QUIC, e HTTP/HTTPS como reserva.
///
/// Por que não só `http.post(...).json(...)`: toda requisição precisa carregar
/// o marcador interno e o bearer da `connection_key` quando configurado. Isso
/// mantém o mesmo contrato de autenticação no HTTP, HTTPS e no fallback de
/// QUIC; sem o header, `require_inference_auth` responde `401 missing
/// Authorization header` e deixa a sessão retida. QUIC também pode ser aceito
/// pelo marcador sobre transporte cifrado na rede pública. Uma falha de
/// transporte QUIC cai para o caminho HTTP; um 404/405 significa peer sem a
/// rota HTTP/3 e também cai para HTTP.
///
/// `Ok(body)` em qualquer 2xx (corpo desserializado, `Null` se ilegível);
/// `Err(motivo)` quando nenhum endereço respondeu 2xx.
async fn post_session_lifecycle(
    http: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    path: &str,
    body: &serde_json::Value,
    session_id: &str,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    // A rota fixada pelo plano de rotas vale também para o ciclo de vida da
    // sessão: um peer que só o túnel alcança não responde nos endereços
    // anunciados, e tentar QUIC e TCP neles antes gastava o prazo inteiro - o
    // rewind falhava (prefixo perdido) e o release também (KV preso no peer,
    // que passava a aparecer com fila para o planejador).
    let tunnel = crate::infer_tunnel::tunnel_sender().filter(|_| step.libp2p_peer_id.is_some());
    let pinned_tunnel = crate::route_plan::book()
        .kind_map(crate::route_plan::Plane::Inference)
        .get(&step.node_id)
        .is_some_and(|kind| kind == "tunnel");
    if let (true, Some(tunnel)) = (pinned_tunnel, tunnel) {
        return crate::relay::post_json_via_tunnel(
            http, tunnel, step, path, body, session_id, timeout,
        )
        .await
        .map_err(|error| error.to_string());
    }
    let result = post_session_lifecycle_direct(http, step, path, body, session_id, timeout).await;
    match (result, tunnel) {
        (Err(direct), Some(tunnel)) if !direct.contains(" respondeu HTTP ") => {
            crate::relay::post_json_via_tunnel(http, tunnel, step, path, body, session_id, timeout)
                .await
                .map_err(|error| format!("{direct}; libp2p-tunnel: {error}"))
        }
        (result, _) => result,
    }
}

/// QUIC e depois HTTP nos endereços anunciados do peer.
async fn post_session_lifecycle_direct(
    http: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    path: &str,
    body: &serde_json::Value,
    session_id: &str,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let route_key = format!("inference:{}", step.node_id);
    let candidates = inference_route_candidates(step);
    let payload = match serde_json::to_vec(body) {
        Ok(payload) => payload,
        Err(error) => return Err(format!("corpo JSON inválido: {error}")),
    };
    let mut last_error = None;

    if http.effective_quic_enabled() && !step.quic_addr_candidates.is_empty() {
        for base in http.route_order(&route_key, &candidates) {
            match http
                .quic_post_typed(
                    &step.node_id,
                    &base,
                    &step.quic_addr_candidates,
                    path,
                    "application/json",
                    payload.clone(),
                    Some(session_id),
                    timeout,
                )
                .await
            {
                Ok(response) if response.status.is_success() => {
                    http.remember_route_success(&route_key, &base);
                    return Ok(
                        serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null)
                    );
                }
                Ok(response) if matches!(response.status.as_u16(), 404 | 405) => {
                    last_error = Some(format!("quic: HTTP {} (sem rota HTTP/3)", response.status));
                    break;
                }
                Ok(response) => {
                    return Err(format!(
                        "{path} em {} respondeu HTTP {}",
                        step.node_id, response.status
                    ));
                }
                Err(error) => {
                    last_error = Some(format!("quic: {error}"));
                    warn!(
                        "{path} por QUIC em {} falhou ({error}), tentando TCP",
                        step.node_id
                    );
                }
            }
        }
    }

    let Ok(cliente) = http.for_peer(&step.node_id) else {
        return Err(last_error.unwrap_or_else(|| "sem cliente para o peer".to_string()));
    };
    for base in http.route_order(&route_key, &candidates) {
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let request = cliente.post(&url).json(body).timeout(timeout);
        match http
            .authorize_inference_request_for_session(request, Some(session_id))
            .await
            .send()
            .await
        {
            Ok(response)
                if crate::peer_client::answered_by_other_node(
                    response.headers(),
                    &step.node_id,
                )
                .is_some() =>
            {
                http.remember_route_failure(&route_key, &base);
                last_error = Some(format!("{base}: respondeu outro nó"));
            }
            Ok(response) if response.status().is_success() => {
                http.remember_route_success(&route_key, &base);
                return Ok(response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null));
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(format!("HTTP {status}: {body}"));
            }
            Err(error) => {
                http.remember_route_failure(&route_key, &base);
                last_error = Some(error.to_string());
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "nenhum endereço anunciado".to_string()))
}

pub(super) async fn rewind_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    retained_tokens: u32,
    clients: &crate::peer_client::PeerClients,
) -> bool {
    let mut all_ok = true;
    for step in &plan.steps {
        let body = serde_json::json!({
            "session_id": session_id,
            "n_tokens": retained_tokens,
        });
        match post_session_lifecycle(
            clients,
            step,
            "/v1/session/rewind",
            &body,
            session_id,
            SESSION_LIFECYCLE_TIMEOUT,
        )
        .await
        {
            Ok(body)
                if body.get("rewound").and_then(|v| v.as_bool()) == Some(true)
                    && body.get("n_past").and_then(|v| v.as_u64())
                        == Some(retained_tokens as u64) =>
            {
                info!(
                    "prefix rewind confirmado: peer={} session={} retained_tokens={} n_past={}",
                    step.node_id,
                    &session_id[..8.min(session_id.len())],
                    retained_tokens,
                    body.get("n_past")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_default(),
                );
            }
            Ok(body) => {
                warn!(
                    "prefix rewind recusado: peer={} session={} retained_tokens={} resposta={}",
                    step.node_id,
                    &session_id[..8.min(session_id.len())],
                    retained_tokens,
                    body,
                );
                all_ok = false;
            }
            Err(error) => {
                warn!(
                    "prefix rewind falhou: peer={} session={} retained_tokens={} erro={error}",
                    step.node_id,
                    &session_id[..8.min(session_id.len())],
                    retained_tokens,
                );
                all_ok = false;
            }
        }
    }
    all_ok
}

/// Hand every peer in the plan its KV slot back. Best effort: a peer that is
/// already gone has nothing to release, and failing here must not turn a good
/// answer into an error.
pub(super) async fn release_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    clients: &crate::peer_client::PeerClients,
) {
    for step in &plan.steps {
        let body = serde_json::json!({ "session_id": session_id });
        if let Err(error) = post_session_lifecycle(
            clients,
            step,
            "/v1/session/release",
            &body,
            session_id,
            SESSION_LIFECYCLE_TIMEOUT,
        )
        .await
        {
            warn!("release de sessão em {} falhou: {error}", step.node_id);
        }
    }
}

/// After a speculative decode round, tell every peer in the plan how much
/// of the batch it just decoded was actually confirmed, so each can discard
/// the rest from its own KV cache — every hop decoded the full batch, not
/// just the tail, so every hop needs this. Same best-effort fan-out shape as
/// `release_sessions`: streaming the accepted tokens to the client does not
/// wait on this, only the *next* speculative round does.
///
/// Returns `false` when any hop's `rolled_back` came back `false` (or the
/// request itself failed) — a 200 response is not success by itself,
/// `handle_rollback` returns 200 either way and puts the real answer in the
/// body. The caller has to treat `false` as fatal for the generation: see
/// its call site in `generate_stream_with_stops`.
pub(super) async fn rollback_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    accept_count: u32,
    batch_width: u32,
    http: &crate::peer_client::PeerClients,
) -> bool {
    let mut all_ok = true;
    for step in &plan.steps {
        let body = serde_json::json!({
            "session_id": session_id,
            "accept_count": accept_count,
            "batch_width": batch_width,
        });
        match post_session_lifecycle(
            http,
            step,
            "/v1/session/rollback",
            &body,
            session_id,
            SESSION_LIFECYCLE_TIMEOUT,
        )
        .await
        {
            Ok(body) if body.get("rolled_back").and_then(|v| v.as_bool()) == Some(true) => {}
            Ok(body) => {
                warn!(
                    "rollback de sessão em {} recusado: {}",
                    step.node_id,
                    body.get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("sem motivo informado")
                );
                all_ok = false;
            }
            Err(error) => {
                warn!("rollback de sessão em {} falhou: {error}", step.node_id);
                all_ok = false;
            }
        }
    }
    all_ok
}

// ── Snapshots de KV cache da implantação (`--slot-save-path`) ────────────────

/// Nome do arquivo de snapshot que cada hop grava: o nome operacional mais o
/// `node_id` daquele hop, para um único diretório guardar a fatia de cada peer
/// sem colisão. `sanitize_slot_name` no lado do peer aceita `[A-Za-z0-9._-]`, e
/// o `node_id` é hex de hash.
pub(super) fn slot_name_for_step(snapshot_name: &str, node_id: &str) -> String {
    format!("{snapshot_name}__{node_id}")
}

/// Todos os hops precisam anunciar `inference/slot-persist-v1`. Um snapshot de
/// implantação é tudo-ou-nada: sem a cap em algum hop, aquele peer não sabe
/// gravar a fatia dele e o snapshot ficaria com um buraco.
pub(super) fn plan_supports_slot_persist(plan: &PipelinePlan) -> Result<(), String> {
    if let Some(step) = plan
        .steps
        .iter()
        .find(|step| !step.supports(crate::pipeline::CAP_SLOT_PERSIST))
    {
        return Err(format!(
            "o hop {} (camadas {}-{}) não tem slot_save_path configurado; \
             o snapshot exige a capability em todos os peers",
            step.node_id, step.start_layer, step.end_layer
        ));
    }
    Ok(())
}

/// Call to `POST /v1/session/slot` (save, restore or erase a hop's KV
/// snapshot), along the same path as the session lifecycle
/// (`post_session_lifecycle`): with the plan's route pinned to the tunnel,
/// straight through it; otherwise the peer's inference addresses and, if none
/// answers at the transport level, the tunnel. A peer only the relay reaches
/// (Docker bridge, NAT) had no other path, and the persistent prefix cache
/// failed on it every turn. An HTTP refusal from the peer is final on any path.
pub(super) async fn slot_call_step(
    step: &crate::pipeline::PipelineStep,
    body: serde_json::Value,
    clients: &crate::peer_client::PeerClients,
) -> Result<serde_json::Value, String> {
    let session_id = body
        .get("session_id")
        .or_else(|| body.get("dest_session_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tunnel = crate::infer_tunnel::tunnel_sender().filter(|_| step.libp2p_peer_id.is_some());
    let pinned_tunnel = crate::route_plan::book()
        .kind_map(crate::route_plan::Plane::Inference)
        .get(&step.node_id)
        .is_some_and(|kind| kind == "tunnel");
    let via_tunnel = |tunnel: &'static crate::infer_tunnel::TunnelOutboundSender| {
        let body = body.clone();
        let session_id = session_id.clone();
        async move {
            crate::relay::post_json_via_tunnel(
                clients,
                tunnel,
                step,
                "/v1/session/slot",
                &body,
                &session_id,
                SLOT_CALL_TIMEOUT,
            )
            .await
            .map_err(|error| {
                let error = error.to_string();
                if error.contains(" respondeu HTTP ") {
                    SlotCallError::Refused(format!(
                        "{} refused the snapshot: {error}",
                        step.node_id
                    ))
                } else {
                    SlotCallError::Unreachable(error)
                }
            })
        }
    };
    if let (true, Some(tunnel)) = (pinned_tunnel, tunnel) {
        return via_tunnel(tunnel)
            .await
            .map_err(SlotCallError::into_message);
    }
    match slot_call_direct(step, &body, &session_id, clients).await {
        Ok(payload) => Ok(payload),
        Err(SlotCallError::Unreachable(direct)) => match tunnel {
            Some(tunnel) => via_tunnel(tunnel).await.map_err(|error| match error {
                SlotCallError::Refused(message) => message,
                SlotCallError::Unreachable(tunnel) => {
                    format!("{direct}; libp2p-tunnel: {tunnel}")
                }
            }),
            None => Err(direct),
        },
        Err(error) => Err(error.into_message()),
    }
}

const SLOT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a snapshot call failed: the peer refused (final) or was not reached on
/// that path (worth trying the next one).
enum SlotCallError {
    Refused(String),
    Unreachable(String),
}

impl SlotCallError {
    fn into_message(self) -> String {
        match self {
            SlotCallError::Refused(message) | SlotCallError::Unreachable(message) => message,
        }
    }
}

/// The peer's announced inference addresses, in route-memory order.
async fn slot_call_direct(
    step: &crate::pipeline::PipelineStep,
    body: &serde_json::Value,
    session_id: &str,
    clients: &crate::peer_client::PeerClients,
) -> Result<serde_json::Value, SlotCallError> {
    let http = clients
        .for_peer(&step.node_id)
        .map_err(|e| SlotCallError::Unreachable(format!("no client for {}: {e}", step.node_id)))?;
    let session_id = (!session_id.is_empty()).then_some(session_id);
    let route_key = format!("inference:{}", step.node_id);
    let mut last_error = None;
    for base in clients.route_order(&route_key, &inference_route_candidates(step)) {
        let url = format!("{}/v1/session/slot", base.trim_end_matches('/'));
        let request = http.post(&url).json(body).timeout(SLOT_CALL_TIMEOUT);
        let response = match clients
            .authorize_inference_request_for_session(request, session_id)
            .await
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                last_error = Some(format!("{} did not answer: {error}", step.node_id));
                continue;
            }
        };
        let status = response.status();
        let payload: serde_json::Value = response.json().await.map_err(|e| {
            SlotCallError::Refused(format!("{} returned an unreadable body: {e}", step.node_id))
        })?;
        if !status.is_success() {
            return Err(SlotCallError::Refused(format!(
                "{} refused the snapshot ({status}): {}",
                step.node_id,
                payload
                    .as_str()
                    .or_else(|| payload.get("error").and_then(|v| v.as_str()))
                    .unwrap_or("sem detalhe")
            )));
        }
        clients.remember_route_success(&route_key, &base);
        return Ok(payload);
    }
    Err(SlotCallError::Unreachable(last_error.unwrap_or_else(
        || format!("{} did not answer on any inference address", step.node_id),
    )))
}

/// Constrói o índice de disco do cache de prefixo, ou `None` quando
/// `slot_persist_prefix_cache` está desligado ou não há `slot_save_path`.
pub(crate) fn build_prefix_disk_index(
    config: &crate::config::Config,
) -> Option<Arc<crate::prefix_cache::PrefixDiskIndex>> {
    if !config.slot_persist_prefix_cache_enabled() {
        return None;
    }
    let path = config.prefix_auto_index_path()?;
    Some(crate::prefix_cache::PrefixDiskIndex::load(
        path,
        config.slot_auto_max_bytes(),
        config.slot_auto_ttl_secs(),
    ))
}

/// Nome de snapshot de um prefixo automático: `auto__<key_hash>`. Por hop, o
/// arquivo vira `auto__<key_hash>__<node_id>.bin` (`slot_name_for_step`).
pub(super) fn auto_slot_name(key_hash: &str) -> String {
    format!("auto__{key_hash}")
}

/// Hash estável do prefixo de uma conversa: identifica o conjunto de `.bin`.
pub(super) fn prefix_key_hash(key: &crate::prefix_cache::PrefixRouteKey, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.model.as_bytes());
    hasher.update([0u8]);
    for hop in &key.hops {
        hasher.update(hop.as_bytes());
        hasher.update([0u8]);
    }
    hasher.update([0u8]);
    hasher.update(prompt.as_bytes());
    hex::encode(&hasher.finalize()[..16])
}

/// Resolve o pipeline que atende um modelo hoje, para as rotas de snapshot do
/// operador. Reusa o mesmo planejador da geração; é observação, não altera
/// roteamento nem balanceamento.
pub(crate) async fn resolve_plan_for_model(
    state: &GatewayState,
    model: &str,
) -> Result<PipelinePlan, String> {
    let peers = state.all_peers().await;
    let total_layers = peers
        .iter()
        .find(|p| p.model == model || p.deploy_id == model)
        .map(|p| p.total_layers)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("nenhum peer serve o modelo '{model}'"))?;
    let deploy_id = peers.iter().find(|p| p.deploy_id == model).map(|_| model);
    crate::pipeline::plan_pipeline(&peers, model, deploy_id, total_layers, &state.loads)
        .await
        .ok_or_else(|| format!("nenhum pipeline cobre 0..{total_layers} do modelo '{model}'"))
}

/// Grava um snapshot de KV de uma implantação: cada hop persiste a fatia dele
/// no `slot_save_path` local. Tudo-ou-nada - se um hop falha, os que já
/// gravaram são apagados e o erro descreve qual hop parou.
pub(crate) async fn snapshot_deployment(
    plan: &PipelinePlan,
    snapshot_name: &str,
    session_id: &str,
    clients: &crate::peer_client::PeerClients,
) -> Result<u64, String> {
    plan_supports_slot_persist(plan)?;
    let mut total_bytes = 0u64;
    let mut done: Vec<&crate::pipeline::PipelineStep> = Vec::new();
    for step in &plan.steps {
        let body = serde_json::json!({
            "action": "save",
            "session_id": session_id,
            "slot_name": slot_name_for_step(snapshot_name, &step.node_id),
        });
        match slot_call_step(step, body, clients).await {
            Ok(payload) => {
                total_bytes += payload
                    .get("bin_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                done.push(step);
            }
            Err(error) => {
                warn!(
                    "snapshot '{snapshot_name}' falhou em {}: {error}",
                    step.node_id
                );
                for prev in &done {
                    let body = serde_json::json!({
                        "action": "erase",
                        "slot_name": slot_name_for_step(snapshot_name, &prev.node_id),
                    });
                    let _ = slot_call_step(prev, body, clients).await;
                }
                return Err(error);
            }
        }
    }
    info!(
        "snapshot '{snapshot_name}' gravado em {} hop(s), {total_bytes} bytes",
        plan.steps.len()
    );
    Ok(total_bytes)
}

/// Restaura um snapshot de implantação numa **nova** sessão em todos os hops.
/// Cada hop tem que voltar com a mesma fronteira de tokens; qualquer
/// divergência libera a nova sessão em todos e devolve erro.
pub(crate) async fn restore_deployment(
    plan: &PipelinePlan,
    snapshot_name: &str,
    new_session_id: &str,
    clients: &crate::peer_client::PeerClients,
) -> Result<u32, String> {
    plan_supports_slot_persist(plan)?;
    let mut agreed: Option<u32> = None;
    for step in &plan.steps {
        let body = serde_json::json!({
            "action": "restore",
            "slot_name": slot_name_for_step(snapshot_name, &step.node_id),
            "dest_session_id": new_session_id,
        });
        let result = slot_call_step(step, body, clients)
            .await
            .and_then(|payload| {
                payload
                    .get("restored_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32)
                    .ok_or_else(|| format!("{} não informou restored_tokens", step.node_id))
            });
        match result {
            Ok(tokens) => match agreed {
                Some(prev) if prev != tokens => {
                    release_sessions(plan, new_session_id, clients).await;
                    return Err(format!(
                        "hops divergiram na restauração: {} restaurou {tokens}, outro restaurou {prev}",
                        step.node_id
                    ));
                }
                _ => agreed = Some(tokens),
            },
            Err(error) => {
                warn!(
                    "restore '{snapshot_name}' falhou em {}: {error}",
                    step.node_id
                );
                release_sessions(plan, new_session_id, clients).await;
                return Err(error);
            }
        }
    }
    let tokens = agreed.unwrap_or(0);
    info!("snapshot '{snapshot_name}' restaurado como sessão {new_session_id} ({tokens} tokens)");
    Ok(tokens)
}

/// Apaga um snapshot de implantação. Melhor esforço: um hop que já não tem o
/// arquivo não é erro.
pub(crate) async fn erase_snapshot(
    plan: &PipelinePlan,
    snapshot_name: &str,
    clients: &crate::peer_client::PeerClients,
) {
    for step in &plan.steps {
        let body = serde_json::json!({
            "action": "erase",
            "slot_name": slot_name_for_step(snapshot_name, &step.node_id),
        });
        if let Err(error) = slot_call_step(step, body, clients).await {
            warn!("erase de '{snapshot_name}' em {}: {error}", step.node_id);
        }
    }
}

/// Pessimistic token count for a prompt, used only by the size guard.
pub(super) fn estimate_tokens(prompt: &str) -> u32 {
    prompt.chars().count().div_ceil(CHARS_PER_TOKEN) as u32
}

/// A recusa por prompt acima da capacidade, a partir de números soltos.
///
/// Existe separada de [`prompt_budget_error`] porque o gateway que **não**
/// carrega modelo nenhum precisa da mesma admissão: era exatamente esse o
/// caminho do defeito de 2026-09-08, em que um prompt de 30 mil caracteres
/// atravessava o gateway sem config local, chegava ao decode do peer e
/// derrubava o processo inteiro com `CUDA error: out of memory`. Recusar aqui
/// é o que mantém o nó vivo para os outros pedidos.
pub fn prompt_budget_error_of(n_ctx: u32, max_new_tokens: u32, prompt: &str) -> Option<String> {
    let budget = prompt_budget_of(n_ctx, 1, max_new_tokens);
    let estimated = estimate_tokens(prompt);
    (estimated > budget).then(|| {
        format!(
            "Prompt grande demais: ~{estimated} tokens estimados contra um limite de \
             {budget}. Cada sessão recebe n_ctx ({n_ctx}) menos max_new_tokens \
             ({max_new_tokens}). Aumente n_ctx em Configurar → ajustes avançados, \
             reduza max_new_tokens, ou reduza o tamanho do prompt."
        )
    })
}

pub fn prompt_budget_error(cfg: &crate::runtime::NodeConfig, prompt: &str) -> Option<String> {
    let error = prompt_budget_error_of(cfg.n_ctx, cfg.max_new_tokens, prompt)?;
    // O nó que tem o modelo em casa sabe mais: acrescenta por que `n_batch`
    // não é o limite, que é a confusão que o operador faz primeiro.
    Some(format!(
        "{error} O prompt vai em pedaços de {} tokens, então n_batch ({}) não limita \
         o prompt.",
        prompt_chunk_tokens(cfg),
        cfg.n_batch,
    ))
}

pub(super) fn rate(tokens: u32, secs: f64) -> f64 {
    if secs > 0.0 {
        tokens as f64 / secs
    } else {
        0.0
    }
}

/// Serialize only trace data that was observed before completion or failure.
fn audit_telemetry(
    summary_trace: Option<crate::prefill::RequestTrace>,
    failure_trace: Option<crate::prefill::RequestTrace>,
) -> Option<serde_json::Value> {
    summary_trace
        .or(failure_trace)
        .and_then(|trace| serde_json::to_value(trace).ok())
}

async fn route_audit_event_to_local_store<F>(
    audit: &crate::reqlog::AuditLog,
    event: crate::reqlog::AuditEvent,
    plugin_ingest: F,
) where
    F: std::future::Future<Output = bool>,
{
    if plugin_ingest.await {
        audit.replicate_event(event);
    } else {
        audit.ingest_local_event(event);
    }
}

#[cfg(test)]
mod audit_replication_tests {
    use super::*;
    use crate::reqlog::{AuditCompletion, AuditEvent, AuditRequest, Caller};
    use libp2p::PeerId;
    use std::collections::{HashMap, HashSet};

    // Reprodução representativa pela visão dos admins:
    // Setup: um admin emissor e dois admins receptores em Network::testkit,
    // sem UDP, serviços ou dados de produção.
    // Entrada: um AuditEvent Begin e Complete aceito pelo host do plugin.
    // Gatilho: a resposta positiva do plugin deve acionar o fanout do POM.
    // Máscara: quando o plugin não está ativo, o fallback AuditLog já replicava.
    // Sintoma: com plugin ativo, os dois admins remotos não recebiam nenhum evento.
    // Este teste cobre o fanout até a fila libp2p, não os processos POM reais nem
    // o cdylib extraído; não afirma equivalência com essa execução E2E completa.
    #[tokio::test]
    async fn plugin_accepted_inference_log_reaches_every_admin_peer() {
        let network = Arc::new(
            crate::network::Network::testkit(
                "admin-source".into(),
                "source-key".into(),
                vec![],
                None,
                None,
                None,
            )
            .await
            .expect("rede isolada"),
        );
        network.set_profile(crate::profile::NodeProfile::Admin);

        let mut expected_peers = HashSet::new();
        for node_id in ["admin-secondary-a", "admin-secondary-b"] {
            let peer_id = PeerId::random();
            expected_peers.insert(peer_id.clone());
            let mut peer = crate::gateway::testkit::peer(node_id, node_id, 8080);
            peer.profile = crate::profile::NodeProfile::Admin;
            peer.libp2p_peer_id = Some(peer_id.to_string());
            network.peers.write().await.insert(node_id.into(), peer);
        }

        let path = std::env::temp_dir().join(format!(
            "pom-plugin-audit-fanout-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let database = Arc::new(crate::db::PomDb::open(&path).expect("banco isolado"));
        let audit = crate::reqlog::AuditLog::new(database);
        let (outbound, mut received) = mpsc::channel(8);
        audit.start_replication(network, outbound);

        let request = AuditRequest {
            id: "request-plugin-replica".into(),
            received_at: "2026-09-23T12:00:00Z".into(),
            route: "/v1/chat/completions".into(),
            model: "test-model".into(),
            caller: Caller::operator(),
            entry_node_id: "admin-source".into(),
            prompt: "isolated replication check".into(),
            request_details: serde_json::json!({"stream": false}),
        };
        let completion = AuditCompletion {
            completed_at: "2026-09-23T12:00:01Z".into(),
            result: Some("done".into()),
            telemetry: None,
            timings: None,
            source: "pipeline:worker-a".into(),
            hops: vec!["worker-a".into()],
            prompt_tokens: Some(4),
            completion_tokens: Some(1),
            duration_ms: Some(1000),
            status: crate::reqlog::AuditStatus::Ok,
            error: None,
        };

        route_audit_event_to_local_store(&audit, AuditEvent::Begin(request.clone()), async {
            true
        })
        .await;
        route_audit_event_to_local_store(
            &audit,
            AuditEvent::Complete {
                id: request.id,
                completion,
            },
            async { true },
        )
        .await;

        let mut received_by_peer: HashMap<PeerId, Vec<AuditEvent>> = HashMap::new();
        while received_by_peer.values().map(Vec::len).sum::<usize>() < 4 {
            let delivery = tokio::time::timeout(Duration::from_secs(2), received.recv())
                .await
                .expect("fanout para os admins no prazo")
                .expect("canal de replicação aberto");
            received_by_peer
                .entry(delivery.peer_id)
                .or_default()
                .push(delivery.event);
        }

        assert_eq!(
            received_by_peer.keys().cloned().collect::<HashSet<_>>(),
            expected_peers
        );
        assert!(received_by_peer.values().all(|events| {
            matches!(
                events.as_slice(),
                [AuditEvent::Begin(_), AuditEvent::Complete { .. }]
            )
        }));

        drop(audit);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}

/// Start a generation and write down what it did, without the generator having
/// to know who asked for it.
///
/// Identity lives with the HTTP route (it is in the `Authorization` header) and
/// the pipeline lives with the generator, so neither one alone can fill in the
/// Logs screen. Forwarding the events through here joins the two without
/// putting a caller argument through every early return in the hot inference
/// path.
pub fn spawn_logged_generation_with_stops(
    route: &'static str,
    caller: crate::reqlog::Caller,
    input: GenerationInput,
    tx: mpsc::Sender<GenEvent>,
) {
    input.state.runtime.note_request();
    let entry_node_id = input.state.network.node_id.clone();
    let prompt_tokens = estimate_tokens(&input.prompt);
    let logged_model = input.model.clone();
    let logged_prompt = input.prompt.clone();
    let request_details = input.request_details.clone();
    let audit = input.state.audit.clone();
    let plugins = input.state.plugins.clone();
    let request_id = input
        .trace_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let received_at = chrono::Utc::now().to_rfc3339();
    let started = Instant::now();
    let begin = crate::reqlog::AuditRequest {
        id: request_id.clone(),
        received_at,
        route: route.to_string(),
        model: logged_model.clone(),
        caller,
        entry_node_id: entry_node_id.clone(),
        prompt: logged_prompt,
        request_details,
    };
    let audit_for_begin = audit.clone();
    let plugins_for_begin = plugins.clone();
    tokio::spawn(async move {
        route_audit_event_to_local_store(
            &audit_for_begin,
            crate::reqlog::AuditEvent::Begin(begin.clone()),
            plugins_for_begin.ingest_audit_event(
                crate::plugins::ENTERPRISE_PLUGIN_CODE,
                crate::reqlog::AuditEvent::Begin(begin),
            ),
        )
        .await;
    });

    let (inner_tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream_with_stops(input, inner_tx));

    tokio::spawn(async move {
        let mut generated: u32 = 0;
        let mut result = String::new();
        // `None` means the client hung up before the generation ended, which is
        // its own outcome and worth seeing in the log.
        let mut outcome: Option<Result<GenSummary, String>> = None;
        let mut failure_trace: Option<crate::prefill::RequestTrace> = None;

        while let Some(event) = inner_rx.recv().await {
            let terminal = match &event {
                GenEvent::Token(text) => {
                    generated += 1;
                    result.push_str(text);
                    None
                }
                GenEvent::Done(summary) => Some(Ok(summary.clone())),
                GenEvent::Failed(e) => Some(Err(e.clone())),
                GenEvent::FailedWithTelemetry { error, trace } => {
                    failure_trace = trace.clone();
                    Some(Err(error.clone()))
                }
            };
            if tx.send(event).await.is_err() {
                break;
            }
            if terminal.is_some() {
                outcome = terminal;
                break;
            }
        }

        let summary_trace = outcome.as_ref().and_then(|outcome| {
            outcome
                .as_ref()
                .ok()
                .and_then(|summary| summary.trace.clone())
        });
        let telemetry = audit_telemetry(summary_trace, failure_trace.clone());
        let timings = outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok().map(|summary| summary.timings.clone()));
        let prompt_tokens = outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok())
            .and_then(|summary| summary.trace.as_ref().map(|trace| trace.input_tokens))
            .or_else(|| failure_trace.as_ref().map(|trace| trace.input_tokens))
            .or(Some(prompt_tokens));
        let (source, hops, completion_tokens, duration_ms, status, error) = match outcome {
            Some(Ok(s)) => (
                s.source.clone(),
                s.source
                    .strip_prefix("pipeline:")
                    .unwrap_or_default()
                    .split('→')
                    .filter(|h| !h.is_empty())
                    .map(|h| h.trim_end_matches(" (partial)").to_string())
                    .collect(),
                Some(s.tokens_generated),
                Some(s.duration_ms),
                audit_status_for_summary(&s),
                None,
            ),
            Some(Err(e)) => (
                "none".to_string(),
                Vec::new(),
                None,
                Some(started.elapsed().as_millis() as u64),
                "error",
                Some(e),
            ),
            None => (
                "none".to_string(),
                Vec::new(),
                Some(generated),
                Some(started.elapsed().as_millis() as u64),
                "abandoned",
                Some("o cliente desconectou antes do fim".to_string()),
            ),
        };

        let audit_status = match status {
            "ok" => crate::reqlog::AuditStatus::Ok,
            "partial" => crate::reqlog::AuditStatus::Partial,
            "error" => crate::reqlog::AuditStatus::Error,
            "abandoned" => crate::reqlog::AuditStatus::Abandoned,
            _ => crate::reqlog::AuditStatus::Incomplete,
        };
        let completion = crate::reqlog::AuditCompletion {
            completed_at: chrono::Utc::now().to_rfc3339(),
            result: (!result.is_empty()).then_some(result),
            telemetry,
            timings,
            source,
            hops,
            prompt_tokens,
            completion_tokens,
            duration_ms,
            status: audit_status,
            error,
        };
        route_audit_event_to_local_store(
            &audit,
            crate::reqlog::AuditEvent::Complete {
                id: request_id.clone(),
                completion: completion.clone(),
            },
            plugins.ingest_audit_event(
                crate::plugins::ENTERPRISE_PLUGIN_CODE,
                crate::reqlog::AuditEvent::Complete {
                    id: request_id,
                    completion,
                },
            ),
        )
        .await;
    });
}

/// Record a request that is rejected before the generation task can be
/// started, such as a prompt that exceeds the configured context budget.
pub(crate) fn record_rejected(
    state: &GatewayState,
    route: &'static str,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    request_details: serde_json::Value,
    error: String,
) {
    let id = Uuid::new_v4().simple().to_string();
    let begin = crate::reqlog::AuditRequest {
        id: id.clone(),
        received_at: chrono::Utc::now().to_rfc3339(),
        route: route.to_string(),
        model,
        caller,
        entry_node_id: state.network.node_id.clone(),
        prompt,
        request_details,
    };
    let completion = crate::reqlog::AuditCompletion {
        completed_at: chrono::Utc::now().to_rfc3339(),
        result: None,
        telemetry: None,
        timings: None,
        source: "none".into(),
        hops: Vec::new(),
        prompt_tokens: None,
        completion_tokens: None,
        duration_ms: Some(0),
        status: crate::reqlog::AuditStatus::Error,
        error: Some(error),
    };
    let audit = state.audit.clone();
    let plugins = state.plugins.clone();
    tokio::spawn(async move {
        let begin_event = crate::reqlog::AuditEvent::Begin(begin.clone());
        let complete_event = crate::reqlog::AuditEvent::Complete {
            id: id.clone(),
            completion: completion.clone(),
        };
        if plugins
            .ingest_audit_event(crate::plugins::ENTERPRISE_PLUGIN_CODE, begin_event.clone())
            .await
        {
            audit.replicate_event(begin_event);
            if plugins
                .ingest_audit_event(
                    crate::plugins::ENTERPRISE_PLUGIN_CODE,
                    complete_event.clone(),
                )
                .await
            {
                audit.replicate_event(complete_event);
            } else {
                tracing::warn!(request_id = %id, "conclusão de auditoria não entrou no plugin");
            }
        } else {
            audit.ingest_local_event(begin_event);
            audit.ingest_local_event(complete_event);
        }
    });
}

/// Run a generation to completion and return the whole text.
pub async fn generate_blocking_with_stops(
    route: &'static str,
    caller: crate::reqlog::Caller,
    input: GenerationInput,
) -> std::result::Result<(String, GenSummary), String> {
    let (tx, mut rx) = mpsc::channel(64);
    spawn_logged_generation_with_stops(route, caller, input, tx);

    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            GenEvent::Token(t) => text.push_str(&t),
            GenEvent::Done(summary) => return Ok((text, summary)),
            GenEvent::Failed(e) => return Err(e),
            GenEvent::FailedWithTelemetry { error, .. } => return Err(error),
        }
    }
    Err("geração terminou sem resposta".to_string())
}
