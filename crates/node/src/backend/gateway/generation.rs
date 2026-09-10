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
            let client = match state.peer_http.for_peer(&node_id) {
                Ok(client) => client,
                Err(error) => return Some(Err(error)),
            };
            let urls = peer.gateway_candidates();
            if urls.is_empty() {
                return Some(Ok(async move {
                    Err(anyhow::anyhow!("peer {node_id} sem endpoint de gateway"))
                }
                .boxed()));
            }
            Some(Ok(async move {
                let mut last_error = None;
                for base in urls {
                    let request = state
                        .control_json_request(
                            client.post(format!("{base}/api/internal/reload_model")),
                            "/api/internal/reload_model",
                            &serde_json::json!({ "event": "reload_model" }),
                        )
                        .await;
                    let response = match request {
                        Ok(request) => request.send().await,
                        Err(error) => {
                            last_error = Some(error.to_string());
                            continue;
                        }
                    };
                    match response {
                        Ok(response) if response.status().is_success() => return Ok(()),
                        Ok(response) => {
                            last_error = Some(format!("HTTP {}", response.status()));
                        }
                        Err(error) => last_error = Some(error.to_string()),
                    }
                }
                Err(anyhow::anyhow!(
                    "reload_model falhou para {node_id}: {}",
                    last_error.unwrap_or_else(|| "nenhum endereço respondeu".into())
                ))
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn hanging_reload(State(_): State<()>, _body: Bytes) -> StatusCode {
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

    async fn spawn_hanging_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("porta de teste");
        let port = listener.local_addr().expect("endereço de teste").port();
        let router = Router::new()
            .route("/api/internal/reload_model", post(hanging_reload))
            .with_state(());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, task)
    }

    fn plan_for(node_ids: &[&str]) -> PipelinePlan {
        PipelinePlan {
            steps: node_ids
                .iter()
                .map(|node_id| PipelineStep {
                    node_id: (*node_id).into(),
                    inference_url: String::new(),
                    inference_candidates: Vec::new(),
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
        let (port, server_task) = spawn_hanging_server().await;
        let mut peer = crate::gateway::testkit::peer("idle-hung", "hung", port);
        peer.idle = true;
        let state = crate::gateway::testkit::state_with("gateway", vec![peer.clone()]).await;
        let plan = plan_for(&["idle-hung"]);

        let error =
            reload_idle_pipeline_with_timeout(&state, &plan, &[peer], Duration::from_millis(50))
                .await
                .expect_err("um peer preso deve devolver erro com timeout");
        assert!(error.to_string().contains("excedeu"));
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
        budget,
        stops,
        cache_key,
        preferred_head,
        request_details: _,
    } = input;
    let budget = budget.normalized();
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
    record_gateway_span(
        &mut gateway_spans,
        request_started,
        validation_started,
        validation_wall,
        "validation",
    );

    let orchestration_started = Instant::now();
    let orchestration_wall = wall_clock_ms();
    let peers = ready_for_pipeline(
        &state.peer_http,
        &state.admission_health,
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
    let Some(mut plan) = crate::pipeline::plan_pipeline_with_rtt_from_head(
        &peers,
        model,
        deploy_id,
        total_layers,
        &state.loads,
        &route_rtt,
        preferred_head.as_deref(),
    )
    .await
    else {
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
        format!(
            "{}|deployment={}|chat_template={chat_template}",
            model,
            deploy_id.unwrap_or("<default>")
        ),
        plan.steps.iter().map(|step| {
            format!(
                "{}@{}:{}-{}",
                step.node_id, step.inference_url, step.exec_start, step.exec_end
            )
        }),
    );
    // A hit reuses the peer session that stayed parked at the end of the prior
    // prompt. A miss gets a fresh id and is published only after this turn has
    // completed its prefill, so an incomplete session cannot enter the cache.
    let session_started = Instant::now();
    let session_wall = wall_clock_ms();
    let cache_prompt = cache_key.unwrap_or_else(|| prompt.clone());
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
    );

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
    let (
        chunk_tokens,
        max_inflight_chunks,
        dtype,
        lookup_enabled,
        lookup_k,
        speculative_m,
        mtp_enabled,
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
                .map(prompt_chunk_tokens)
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
            cfg.as_ref().is_some_and(|c| c.mtp_enabled),
            // O parâmetro é "quantos tokens rascunhar"; a largura do lote é
            // isso mais o token já confirmado.
            speculative_k_of(
                cfg.as_ref().map(|c| c.mtp_draft_tokens).unwrap_or(3) + 1,
                n_batch,
            ),
        )
    };

    // Qual das duas especulações roda nesta requisição. O MTP vem primeiro
    // quando os dois estão ligados: ele usa uma cabeça treinada do próprio
    // modelo, enquanto o lookup só acerta em saída repetitiva. Nunca os dois
    // na mesma rodada - o lote é um só, e misturar as fontes tornaria a taxa
    // de aceitação impossível de atribuir.
    let (speculative_enabled, speculative_k, draft_source) = if mtp_enabled && plan.supports_mtp() {
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
            )
        };
        let mut prefill_result = prefill_attempt(reuse_prefix, None).await;
        let mut unknown_session_recovery_attempted = false;
        if let Err(e) = &prefill_result {
            let original_unknown_session = crate::relay::is_unknown_session_error(e);
            if crate::relay::is_frame_size_error(e) {
                warn!(
                    "prefill falhou por limite de frame ({e}); \
                     não é recuperável repetindo o mesmo chunk"
                );
            } else if !crate::prefill::is_cancelled(e) {
                // Um erro depois de um chunk aceito não revela se o peer
                // avançou o KV antes de a resposta se perder. Não se pode
                // tentar uma remoção parcial: modelos recorrentes recusam-na
                // e a sessão ficaria corrompida. A recuperação segura é um
                // prefill frio, que apaga a sessão inteira em cada peer.
                if let Some(rp) = crate::prefill::resume_point(e) {
                    warn!(
                        "prefill falhou no pedaço {} (posição {}, {e}); reconstruindo a sessão desde o início",
                        rp.from_chunk, rp.start_pos
                    );
                    prefill_result = prefill_attempt(false, None).await;
                }
                // Uma segunda recuperação fria só é necessária para prefixo
                // reutilizado ou sessão desconhecida. Uma falha durante a
                // reconstrução já tem a sessão em estado incerto e deve cair
                // direto no cleanup, sem uma nova tentativa parcial.
                if let Err(e2) = &prefill_result {
                    let unknown_session =
                        original_unknown_session || crate::relay::is_unknown_session_error(e2);
                    if reuse_prefix || unknown_session {
                        unknown_session_recovery_attempted = unknown_session;
                        warn!(
                            "prefill falhou com estado de sessão incompatível ({e2}); \
                             reconstruindo o prefill inteiro a frio uma vez"
                        );
                        prefill_result = prefill_attempt(false, None).await;
                    }
                }
            }
        }
        let first = match prefill_result {
            Ok(mut o) => {
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
                } else {
                    format!("Prefill falhou: {e}")
                };
                let _ = tx
                    .send(GenEvent::Failed(message))
                    .await;
                return;
            }
        };

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
        let sliced_mtp = mtp_enabled && plan.supports_mtp() && plan.steps.len() >= 2;
        if mtp_enabled && plan.steps.len() >= 2 && !sliced_mtp {
            info!(
                reason = sliced_mtp_disable_reason(&plan),
                "pipeline fatiado sem MTP: seguindo com a capacidade disponível"
            );
        }
        let mut remote_stream = if remote_stream_capable {
            // The remote head owns the autoregressive cycle. For a sliced MTP
            // plan it also receives the first MTP request and installs every
            // following draft locally, so the gateway never adds a per-token
            // reinjection round trip.
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
                                info!(
                                    reason,
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
                                info!(
                                    reason,
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
            event @ (GenEvent::Done(_) | GenEvent::Failed(_)) => {
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

pub(super) async fn rewind_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    retained_tokens: u32,
    clients: &crate::peer_client::PeerClients,
) -> bool {
    let mut all_ok = true;
    for step in &plan.steps {
        let Ok(http) = clients.for_peer(&step.node_id) else {
            all_ok = false;
            continue;
        };
        let route_key = format!("inference:{}", step.node_id);
        let mut rewound = false;
        let mut last_error = None;
        for base in clients.route_order(&route_key, &inference_route_candidates(step)) {
            let url = format!("{}/v1/session/rewind", base.trim_end_matches('/'));
            let request = http
                .post(&url)
                .json(&serde_json::json!({
                    "session_id": session_id,
                    "n_tokens": retained_tokens,
                }))
                .timeout(REWIND_TIMEOUT);
            match clients
                .authorize_inference_request_for_session(request, Some(session_id))
                .await
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    match response.json::<serde_json::Value>().await {
                        Ok(body)
                            if body.get("rewound").and_then(|v| v.as_bool()) == Some(true)
                                && body.get("n_past").and_then(|v| v.as_u64())
                                    == Some(retained_tokens as u64) =>
                        {
                            clients.remember_route_success(&route_key, &base);
                            rewound = true;
                        }
                        Ok(body) => {
                            warn!("rewind recusado por {}: {}", step.node_id, body);
                        }
                        Err(error) => {
                            warn!("resposta ilegível no rewind de {}: {error}", step.node_id);
                        }
                    }
                    break;
                }
                Ok(response) => {
                    warn!("rewind em {} respondeu {}", step.node_id, response.status());
                    break;
                }
                Err(error) => {
                    clients.remember_route_failure(&route_key, &base);
                    last_error = Some(error.to_string());
                }
            }
        }
        if !rewound {
            if let Some(error) = last_error {
                warn!(
                    "rewind em {} falhou em todos os endereços: {error}",
                    step.node_id
                );
            }
            all_ok = false;
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
        let Ok(http) = clients.for_peer(&step.node_id) else {
            warn!("sem cliente para liberar sessão em {}", step.node_id);
            continue;
        };
        let candidates = inference_route_candidates(step);
        let route_key = format!("inference:{}", step.node_id);
        let mut last_error = None;
        let mut released = false;
        for base in clients.route_order(&route_key, &candidates) {
            let url = format!("{}/v1/session/release", base.trim_end_matches('/'));
            let request = http
                .post(&url)
                .json(&serde_json::json!({ "session_id": session_id }))
                .timeout(Duration::from_secs(10));
            match clients
                .authorize_inference_request_for_session(request, Some(session_id))
                .await
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    clients.remember_route_success(&route_key, &base);
                    released = true;
                    break;
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    last_error = Some(format!("HTTP {status}: {body}"));
                }
                Err(error) => {
                    clients.remember_route_failure(&route_key, &base);
                    last_error = Some(error.to_string());
                }
            }
        }
        if !released {
            warn!(
                "release de sessão em {} falhou em todos os endereços: {}",
                step.node_id,
                last_error.unwrap_or_else(|| "nenhum endereço anunciado".to_string())
            );
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
        let Ok(cliente) = http.for_peer(&step.node_id) else {
            warn!("rollback: sem cliente para {}", step.node_id);
            all_ok = false;
            continue;
        };
        let route_key = format!("inference:{}", step.node_id);
        let mut rolled_back = false;
        let mut last_error = None;
        for base in http.route_order(&route_key, &inference_route_candidates(step)) {
            let url = format!("{}/v1/session/rollback", base.trim_end_matches('/'));
            let request = cliente
                .post(&url)
                .json(&serde_json::json!({
                    "session_id": session_id,
                    "accept_count": accept_count,
                    "batch_width": batch_width,
                }))
                .timeout(Duration::from_secs(10));
            match http
                .authorize_inference_request_for_session(request, Some(session_id))
                .await
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    match response.json::<serde_json::Value>().await {
                        Ok(body)
                            if body.get("rolled_back").and_then(|v| v.as_bool()) == Some(true) =>
                        {
                            http.remember_route_success(&route_key, &base);
                            rolled_back = true;
                        }
                        Ok(body) => {
                            warn!(
                                "rollback de sessão em {} recusado: {}",
                                step.node_id,
                                body.get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("sem motivo informado")
                            );
                        }
                        Err(error) => {
                            warn!(
                                "rollback de sessão em {}: corpo ilegível: {error}",
                                step.node_id
                            );
                        }
                    }
                    break;
                }
                Ok(response) => {
                    warn!(
                        "rollback de sessão em {} respondeu {}",
                        step.node_id,
                        response.status()
                    );
                    break;
                }
                Err(error) => {
                    http.remember_route_failure(&route_key, &base);
                    last_error = Some(error.to_string());
                }
            }
        }
        if !rolled_back {
            if let Some(error) = last_error {
                warn!(
                    "rollback de sessão em {} falhou em todos os endereços: {error}",
                    step.node_id
                );
            }
            all_ok = false;
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

pub(super) async fn slot_call_step(
    step: &crate::pipeline::PipelineStep,
    body: serde_json::Value,
    clients: &crate::peer_client::PeerClients,
) -> Result<serde_json::Value, String> {
    let http = clients
        .for_peer(&step.node_id)
        .map_err(|e| format!("sem cliente para {}: {e}", step.node_id))?;
    let session_id = body
        .get("session_id")
        .or_else(|| body.get("dest_session_id"))
        .and_then(serde_json::Value::as_str);
    let route_key = format!("inference:{}", step.node_id);
    let mut last_error = None;
    for base in clients.route_order(&route_key, &inference_route_candidates(step)) {
        let url = format!("{}/v1/session/slot", base.trim_end_matches('/'));
        let request = http.post(&url).json(&body).timeout(Duration::from_secs(60));
        let response = match clients
            .authorize_inference_request_for_session(request, session_id)
            .await
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                last_error = Some(format!("{} não respondeu: {error}", step.node_id));
                continue;
            }
        };
        let status = response.status();
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("{} devolveu corpo ilegível: {e}", step.node_id))?;
        if !status.is_success() {
            return Err(format!(
                "{} recusou o snapshot ({status}): {}",
                step.node_id,
                payload
                    .as_str()
                    .or_else(|| payload.get("error").and_then(|v| v.as_str()))
                    .unwrap_or("sem detalhe")
            ));
        }
        clients.remember_route_success(&route_key, &base);
        return Ok(payload);
    }
    Err(last_error.unwrap_or_else(|| {
        format!(
            "{} não respondeu por nenhum endereço de inferência",
            step.node_id
        )
    }))
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
    let request_id = input
        .trace_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let received_at = chrono::Utc::now().to_rfc3339();
    let started = Instant::now();
    audit.begin(crate::reqlog::AuditRequest {
        id: request_id.clone(),
        received_at,
        route: route.to_string(),
        model: logged_model.clone(),
        caller,
        entry_node_id: entry_node_id.clone(),
        prompt: logged_prompt,
        request_details,
    });

    let (inner_tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream_with_stops(input, inner_tx));

    tokio::spawn(async move {
        let mut generated: u32 = 0;
        let mut result = String::new();
        // `None` means the client hung up before the generation ended, which is
        // its own outcome and worth seeing in the log.
        let mut outcome: Option<Result<GenSummary, String>> = None;

        while let Some(event) = inner_rx.recv().await {
            let terminal = match &event {
                GenEvent::Token(text) => {
                    generated += 1;
                    result.push_str(text);
                    None
                }
                GenEvent::Done(summary) => Some(Ok(summary.clone())),
                GenEvent::Failed(e) => Some(Err(e.clone())),
            };
            if tx.send(event).await.is_err() {
                break;
            }
            if terminal.is_some() {
                outcome = terminal;
                break;
            }
        }

        let telemetry = outcome.as_ref().and_then(|outcome| {
            outcome
                .as_ref()
                .ok()
                .and_then(|summary| summary.trace.clone())
                .and_then(|trace| serde_json::to_value(trace).ok())
        });
        let timings = outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok().map(|summary| summary.timings.clone()));
        let prompt_tokens = outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().ok())
            .and_then(|summary| summary.trace.as_ref().map(|trace| trace.input_tokens))
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
        audit.complete(
            request_id,
            crate::reqlog::AuditCompletion {
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
            },
        );
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
    state.audit.begin(crate::reqlog::AuditRequest {
        id: id.clone(),
        received_at: chrono::Utc::now().to_rfc3339(),
        route: route.to_string(),
        model,
        caller,
        entry_node_id: state.network.node_id.clone(),
        prompt,
        request_details,
    });
    state.audit.complete(
        id,
        crate::reqlog::AuditCompletion {
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
        },
    );
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
        }
    }
    Err("geração terminou sem resposta".to_string())
}
