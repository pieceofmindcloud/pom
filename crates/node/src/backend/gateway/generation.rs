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
        return Some("the remote session no longer exists");
    }
    if crate::relay::is_delivery_ambiguous(error) {
        return Some(
            "the response was lost after sending and the peer may have processed the chunk",
        );
    }
    if crate::relay::is_prefill_state_error(error) {
        return Some("the remote KV positions do not match the next chunk");
    }
    if crate::relay::is_transport_relay_failure(error)
        && !crate::relay::is_pre_delivery_error(error)
    {
        return Some("a downstream hop failed after the head accepted the chunk");
    }
    None
}

/// Records the outcome of a request that ends before planning the
/// pipeline.
///
/// Every accepted request must leave exactly one outcome line. Without
/// this, the early exits of this function were silent: the log showed the
/// request arriving, showed the output limit adjustment, and then nothing - neither
/// `Pipeline planejado` nor an error. From the client's side that is a hanging request,
/// and it is what put the agent into exponential backoff repeating the same
/// request in production on 2026-09-08.
fn log_early_outcome(outcome: &str, reason: &str) {
    warn!("⛔ request ended before the plan ({outcome}): {reason}");
}

/// Wakes every idle step selected for this request at once and waits until
/// all of them are ready. This is a control event on the gateway port, not an
/// inference hop, so all peers begin their local load concurrently before the
/// first prefill is sent. The event is idempotent if another request has
/// already woken the same peer.
///
/// A remote peer answers the event at once (`loading` or `ready`) and the
/// gateway repeats it every `IDLE_RELOAD_POLL`. One call held open for the
/// whole load depended on a connection surviving minutes of silence - a model
/// takes longer to load than a tunnel request or an idle QUIC stream lives -
/// and when it broke the request failed while the peer went on loading.
const IDLE_RELOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Deadline of one `reload_model` call. It only has to carry the answer.
const IDLE_RELOAD_CALL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const IDLE_RELOAD_POLL: Duration = Duration::from_secs(1);
#[cfg(test)]
const IDLE_RELOAD_POLL: Duration = Duration::from_millis(20);
/// How long the gateway waits for the woken peers' announcement after they
/// answered `ready`. The plan is rebuilt from it: an idle peer announces no
/// runtime capabilities, so a plan made while it slept would run degraded.
const IDLE_ANNOUNCE_WAIT: Duration = Duration::from_secs(5);

fn idle_steps<'a>(
    plan: &PipelinePlan,
    peers: &'a [crate::network::PeerInfo],
) -> Vec<&'a crate::network::PeerInfo> {
    let mut idle: Vec<&crate::network::PeerInfo> = Vec::new();
    for step in &plan.steps {
        if let Some(peer) = peers
            .iter()
            .find(|peer| peer.node_id == step.node_id && peer.idle)
        {
            if !idle.iter().any(|known| known.node_id == peer.node_id) {
                idle.push(peer);
            }
        }
    }
    idle
}

async fn reload_idle_pipeline(
    state: &GatewayState,
    plan: &PipelinePlan,
    peers: &[crate::network::PeerInfo],
) -> anyhow::Result<bool> {
    reload_idle_pipeline_with_timeout(state, plan, peers, IDLE_RELOAD_TIMEOUT).await
}

/// Asks one remote peer to load until it answers `ready`. A call that does not
/// arrive is tried again - the peer may be busy starting its server - and only
/// the peer saying the load cannot happen ends the wait.
async fn wake_remote_peer(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
    last_error: &std::sync::Mutex<Option<String>>,
) -> anyhow::Result<()> {
    let node_id = &peer.node_id;
    loop {
        let call = state
            .control_post_json(
                peer,
                "/api/internal/reload_model",
                &serde_json::json!({ "event": "reload_model" }),
                IDLE_RELOAD_CALL_TIMEOUT,
            )
            .await;
        let transient = match call {
            Ok(response) if response.status().is_success() => {
                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null);
                if body.get("state").and_then(|value| value.as_str()) != Some("loading") {
                    return Ok(());
                }
                None
            }
            // The tunnel's own answer for a gateway that is not listening.
            Ok(response) if response.status() == reqwest::StatusCode::BAD_GATEWAY => {
                Some("HTTP 502".to_string())
            }
            Ok(response) => {
                let status = response.status();
                let reason = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|body| {
                        body.get("error")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| format!("HTTP {status}"));
                anyhow::bail!("reload_model failed for {node_id}: {reason}");
            }
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(reason) = transient {
            debug!("reload_model for {node_id} still unanswered: {reason}");
            if let Ok(mut slot) = last_error.lock() {
                *slot = Some(format!("{node_id}: {reason}"));
            }
        }
        tokio::time::sleep(IDLE_RELOAD_POLL).await;
    }
}

/// Returns whether any peer had to be woken.
async fn reload_idle_pipeline_with_timeout(
    state: &GatewayState,
    plan: &PipelinePlan,
    peers: &[crate::network::PeerInfo],
    timeout: Duration,
) -> anyhow::Result<bool> {
    let idle = idle_steps(plan, peers);
    if idle.is_empty() {
        return Ok(false);
    }
    let started = Instant::now();
    info!(
        "⏰ idle pipeline: waking {} peer(s) before the prefill ({})",
        idle.len(),
        idle.iter()
            .map(|peer| peer.node_id.chars().take(8).collect::<String>())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let last_error = std::sync::Mutex::new(None);
    let requests = idle.iter().map(|peer| {
        let last_error = &last_error;
        async move {
            if peer.node_id == state.network.node_id {
                let node_id = &peer.node_id;
                return state
                    .runtime
                    .reload_from_idle()
                    .await
                    .map(|_| ())
                    .map_err(|error| {
                        anyhow::anyhow!("reload_model failed for {node_id}: {error}")
                    });
            }
            wake_remote_peer(state, peer, last_error).await
        }
        .boxed()
    });
    let results = tokio::time::timeout(timeout, futures::future::try_join_all(requests))
        .await
        .map_err(|_| {
            let detail = last_error
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .map(|reason| format!(" (last failure: {reason})"))
                .unwrap_or_default();
            anyhow::anyhow!("reload_model of the pipeline exceeded {timeout:?}{detail}")
        })?;
    results?;
    info!(
        "✅ pipeline woken in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(true)
}

/// After a wake, waits until the gossip shows the woken peers as loaded. A
/// peer clears `idle` and announces its runtime capabilities when its model
/// answers, and the plan has to be rebuilt from that announcement. Bounded: if
/// it does not arrive the request goes on with what the admission probe reads.
async fn wait_for_woken_announcements(state: &GatewayState, woken: &[String]) {
    let deadline = Instant::now() + IDLE_ANNOUNCE_WAIT;
    loop {
        let peers = state.all_peers().await;
        let pending = woken.iter().any(|node_id| {
            peers
                .iter()
                .any(|peer| &peer.node_id == node_id && peer.idle)
        });
        if !pending || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
            .expect("test port");
        let port = listener.local_addr().expect("test address").port();
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
            .expect("test port");
        let port = listener.local_addr().expect("test address").port();
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
            .expect("both planned peers must receive the event in parallel");
        barrier.release.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), reload)
            .await
            .expect("a parallel reload cannot stay blocked")
            .expect("reload task")
            .expect("both peers must confirm the reload");

        first_task.abort();
        second_task.abort();
    }

    /// Answers `loading` until the given call, then `ready`; or a fixed 503.
    #[derive(Clone)]
    struct LoadingPeer {
        calls: Arc<AtomicUsize>,
        ready_at: usize,
        refusal: Option<&'static str>,
    }

    async fn loading_reload(
        State(peer): State<LoadingPeer>,
        _body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let call = peer.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(reason) = peer.refusal {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "event": "reload_model", "error": reason })),
            );
        }
        let state = if call >= peer.ready_at {
            "ready"
        } else {
            "loading"
        };
        (
            StatusCode::OK,
            Json(serde_json::json!({ "event": "reload_model", "state": state })),
        )
    }

    async fn spawn_loading_server(peer: LoadingPeer) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test port");
        let port = listener.local_addr().expect("test address").port();
        let router = Router::new()
            .route("/api/internal/reload_model", post(loading_reload))
            .with_state(peer);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, task)
    }

    /// The wake is short calls repeated until the peer is ready, never one
    /// call held open for the whole model load.
    #[tokio::test]
    async fn a_loading_peer_is_asked_again_until_it_is_ready() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (port, server) = spawn_loading_server(LoadingPeer {
            calls: calls.clone(),
            ready_at: 3,
            refusal: None,
        })
        .await;
        let mut peer = crate::gateway::testkit::peer("idle-loading", "loading", port);
        peer.idle = true;
        let state = crate::gateway::testkit::state_with("gateway", vec![peer.clone()]).await;
        let plan = plan_for(&["idle-loading"]);

        let woke =
            reload_idle_pipeline_with_timeout(&state, &plan, &[peer], Duration::from_secs(5))
                .await
                .expect("the peer becomes ready on the third response");
        assert!(woke, "the pipeline was idle and was woken");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn a_peer_that_cannot_load_ends_the_wait_with_its_reason() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (port, server) = spawn_loading_server(LoadingPeer {
            calls: calls.clone(),
            ready_at: 1,
            refusal: Some("the model is waiting for cooldown"),
        })
        .await;
        let mut peer = crate::gateway::testkit::peer("idle-hot", "hot", port);
        peer.idle = true;
        let state = crate::gateway::testkit::state_with("gateway", vec![peer.clone()]).await;
        let plan = plan_for(&["idle-hot"]);

        let error =
            reload_idle_pipeline_with_timeout(&state, &plan, &[peer], Duration::from_secs(5))
                .await
                .expect_err("the peer refusal ends the wait");
        assert!(error.to_string().contains("cooldown"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a refusal is not repeated");
        server.abort();
    }

    #[tokio::test]
    async fn a_pipeline_with_no_idle_peer_is_not_woken() {
        let peer = crate::gateway::testkit::peer("awake", "awake", 1);
        let state = crate::gateway::testkit::state_with("gateway", vec![peer.clone()]).await;
        let plan = plan_for(&["awake"]);
        let woke =
            reload_idle_pipeline_with_timeout(&state, &plan, &[peer], Duration::from_secs(1))
                .await
                .unwrap();
        assert!(!woke);
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
                .expect_err("a stuck peer must return an error with a timeout");
        assert!(
            reached.load(Ordering::SeqCst),
            "the request must reach the stuck peer"
        );
        assert!(
            !error.to_string().is_empty(),
            "the refusal must explain the failure"
        );
        server_task.abort();
    }
}

#[cfg(test)]
mod generation_pump_tests {
    use super::*;

    #[tokio::test]
    async fn client_disconnect_cancels_the_generation_even_without_an_event() {
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
            .expect("the bomb must observe the disconnect")
            .expect("the bomb must not panic");

        assert!(result.is_none());
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn a_prefill_failure_preserves_partial_telemetry_up_to_the_audit() {
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
            .expect("a transport error preserves the partial trace");
        let (client_tx, _client_rx) = mpsc::channel(1);
        let (inner_tx, inner_rx) = mpsc::channel(1);
        let cancellation = crate::prefill::Cancellation::new();
        inner_tx
            .send(GenEvent::FailedWithTelemetry {
                error: "transport failed after progress".into(),
                trace: Some(partial_trace),
            })
            .await
            .unwrap();
        let result = forward_generation_events(client_tx, inner_rx, cancellation)
            .await
            .expect("terminal failure preserved");
        let GenEvent::FailedWithTelemetry {
            trace: Some(trace), ..
        } = result
        else {
            panic!("the terminal event must preserve the partial trace")
        };
        let response_telemetry = audit_telemetry(None, Some(trace)).unwrap();
        assert_eq!(response_telemetry["model"], "test-model");
        assert_eq!(response_telemetry["prefill_ms"], 37);
        assert_eq!(response_telemetry["input_tokens"], 45_647);
        assert_eq!(response_telemetry["cached_tokens"], 13);
    }
}

/// Picks the pipeline with the lowest estimated delivery time. `None` when there
/// is no viable path: then the greedy planner (coverage, RTT and local load)
/// decides as before.
///
/// Candidates: the best path of the dynamic programming and every path that already
/// has this prompt's prefix retained, estimated with the prefill saved.
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
    // The pinned route is the measure of the transport the frame will use; the topology
    // RTT is left for whoever has not been analyzed yet.
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
        "pipeline by delivery time: {} ~{:.0} ms, response {} (prefill {:.0} ms, {:.1} ms/token, queues {:?}, prefix {} tokens)",
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
        request_details,
        probe,
    } = input;
    let mut budget = budget.normalized();
    if cancellation.is_cancelled() {
        log_early_outcome("cancelado", "the client gave up before the start");
        return;
    }
    // Size the prompt up before any peer sees it — see `prompt_token_budget`.
    let validation_started = Instant::now();
    let validation_wall = wall_clock_ms();
    if let Some(cfg) = state.runtime.config().await {
        if let Some(error) = prompt_budget_error(&cfg, &prompt) {
            log_early_outcome("refused", &error);
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        }
    } else {
        // A gateway without a local model had no admission at all: the prompt
        // went through whole, reached the peer's decode and brought down the
        // process with `CUDA error: out of memory` (ksia, 2026-09-08). The
        // capacity announced by the peer that serves this model is the same one
        // `/v1/models` publishes, and it counts as the ceiling here. It sits before any
        // health probe on purpose: an already inadmissible request does not
        // justify a round trip on the network, much less a prefill.
        let (capacity, capacity_pending) = state
            .runtime_capacity_for_model(Some(&model), preferred_head.as_deref())
            .await;
        // Pending capacity is an absence of information, not permission: whoever
        // decides in that case is still the peer.
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
    let session_window = match state
        .runtime_capacity_for_model(Some(&model), preferred_head.as_deref())
        .await
    {
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

    // `qwen3:8b@d-4f2a91c3` chooses **which** deployment serves; the bare name
    // leaves the choice to whoever builds the pipeline. See `deployment::split_selector`.
    let (model, deploy_id) = crate::deployment::split_selector(&model);
    let prefix_model = format!(
        "{}|deployment={}|chat_template={chat_template}|thinking_enabled={thinking_enabled}",
        model,
        deploy_id.unwrap_or("<default>")
    );
    let cache_prompt = cache_key.unwrap_or_else(|| prompt.clone());

    // Admission and planning run once, or twice when the chosen pipeline was
    // asleep (`unload_if_idle`): the first pass finds the idle peers and wakes
    // them, the second plans again over the pipeline that is now loaded. The
    // plan of the first pass is not used for inference - an idle peer
    // announces no runtime capabilities, so that plan would run degraded.
    let mut woke_idle = false;
    let (mut plan, planned_return) = loop {
        let orchestration_started = Instant::now();
        let orchestration_wall = wall_clock_ms();
        // The P2P announcement may arrive after the local load. The admission returns the
        // executable snapshot of `/health` along with each peer, so that the
        // planner does not turn this propagation window into a false hole.
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
        // Planning by delivery time (`fastest`): measured speed of each
        // peer, queue of the whole network, RTT of each edge and the prefix already retained.
        // With no viable path through it, the earlier greedy planner decides.
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
        let Some(plan) = planned else {
            let error = format!(
                "No valid pipeline for model '{model}' (check the layer coverage 0..{total_layers})"
            );
            log_early_outcome("refused", &error);
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        };
        record_gateway_span(
            &mut gateway_spans,
            request_started,
            selection_started,
            selection_wall,
            "pipeline_selection",
        );
        if woke_idle {
            break (plan, planned_return);
        }
        let wake_started = Instant::now();
        let wake_wall = wall_clock_ms();
        let woken: Vec<String> = idle_steps(&plan, &peers)
            .iter()
            .map(|peer| peer.node_id.clone())
            .collect();
        match reload_idle_pipeline(&state, &plan, &peers).await {
            Ok(false) => break (plan, planned_return),
            Ok(true) => {
                wait_for_woken_announcements(&state, &woken).await;
                record_gateway_span(
                    &mut gateway_spans,
                    request_started,
                    wake_started,
                    wake_wall,
                    "pipeline_wake",
                );
                if cancellation.is_cancelled() {
                    log_early_outcome(
                        "cancelado",
                        "the client gave up while the pipeline was waking",
                    );
                    return;
                }
                woke_idle = true;
            }
            Err(error) => {
                log_early_outcome("reload_model", &error.to_string());
                let _ = tx.send(GenEvent::Failed(error.to_string())).await;
                return;
            }
        }
    };
    // The window read before the wake came from peers with no loaded runtime.
    let session_window = if woke_idle {
        match state
            .runtime_capacity_for_model(Some(model), preferred_head.as_deref())
            .await
        {
            (Some(capacity), false) => Some(capacity.n_ctx),
            _ => session_window,
        }
    } else {
        session_window
    };

    // A whole model served by a plugin's inference service takes the prompt
    // this gateway rendered, not hidden states. There is no fallback to the
    // pipeline: the plan said who serves this model. See `plugin_engine`.
    if let Some(code) = crate::plugin_engine::plugin_of_plan(&plan) {
        let refusal = if !images.is_empty() {
            Some("this inference service does not accept images")
        } else {
            None
        };
        if let Some(error) = refusal {
            log_early_outcome("refused", error);
            let _ = tx.send(GenEvent::Failed(error.to_string())).await;
            return;
        }
        crate::plugin_engine::generate(
            &state,
            &plan,
            &code,
            model,
            trace_id.clone(),
            crate::plugin_engine::EngineRequest {
                prompt,
                chat_template,
                thinking_enabled,
                max_tokens: budget.total,
                stops,
                session: crate::plugin_session::session_of(&request_details),
                request_details,
                probe_labels: probe
                    .as_ref()
                    .map(|probe| probe.labels.clone())
                    .unwrap_or_default(),
                probe_prefix_bytes: probe.as_ref().and_then(|probe| probe.retain_prefix_bytes),
            },
            &tx,
            &cancellation,
            request_started,
        )
        .await;
        return;
    }

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
            log_early_outcome("refused", error);
            let _ = tx.send(GenEvent::Failed(error.to_string())).await;
            return;
        }
    }

    // The route is chosen by whoever will dial, not by whoever planned: the plan
    // may have come from a machine on another network, and only this node measured the
    // candidates. Resolving here, once, fixes the address of each hop for the
    // whole session - the KV cache on the other side is per session, and switching
    // address midway is worse than the slow route.
    let route_started = Instant::now();
    let route_wall = wall_clock_ms();
    {
        let mut pinned = std::collections::HashMap::new();
        for step in plan.steps.iter_mut() {
            step.inference_url =
                crate::pipeline::resolve_step_url(step, &state.network.routes, &mut pinned);
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
    // Disk layer (Phase 3): an in-memory miss that matches an automatic
    // snapshot on disk - typical after a restart - restores the KV on
    // all the hops instead of redoing the prefill cold. See
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
                            "♻️  prefix restored from disk: {restored_tokens} tokens on {} hop(s)",
                            plan.steps.len()
                        );
                        disk_restore_hash = Some(entry.key_hash);
                    }
                    Err(error) => {
                        warn!("prefix restore from disk failed ({error}); continuing cold");
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

    // The session cannot switch replica after the first prefill created
    // the KV. The plan is fixed once and this affinity is the explicit reference
    // used to review any future transformation of the route.
    let route_affinity = crate::pipeline::RouteAffinity::from_plan(
        deploy_id.unwrap_or_default(),
        &session_id,
        &plan,
    );
    debug_assert!(route_affinity.matches_plan(&plan));

    // Where the tail delivers the reply. `None` when this machine does not
    // know any address of its own - then the chain goes back to bubbling, which is
    // slow but never ends up without an answer.
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
    // The return mode is chosen together with the path by the delivery-time
    // planner (`fastest::plan_fastest`): ring when the reply
    // arrives sooner coming back over the outbound connections - or when the tail has no
    // way back to here. The ring dispenses with `reply_to`. With the greedy fallback
    // planner, the `return_path_prefers_ring` rule decides.
    // `POM_RETURN_MODE` also applies to the greedy fallback planner.
    let planned_return = crate::fastest::forced_return_mode().or(planned_return);
    let direct = match (direct, planned_return) {
        (Some(_), Some(crate::fastest::ReturnMode::Ring)) => {
            info!(
                "response through the ring ({})",
                if crate::fastest::forced_return_mode().is_some() {
                    "imposed by POM_RETURN_MODE"
                } else {
                    "chosen in the planning"
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
                    info!("response through the ring: {reason}");
                    None
                }
                None => Some(reply),
            }
        }
        (None, _) => None,
    };

    info!(
        "🔀 Pipeline planned: {} hops [{}] session={} max_tokens={} (leg {}) response={}",
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
    // Legs stitched so far. Starts at 1: every reply has one.
    let mut legs: u32 = 1;
    let mut retained_prefix_tokens: Option<u32> = None;
    // How many prompt tokens the head actually did not reprocess. It is not the
    // cache's choice, it is what the pipeline executed - see
    // `PrefillOutcome::cached_tokens`. It comes out in `GenSummary` and from there to
    // `usage.prompt_tokens_details.cached_tokens`.

    // Chunk size, wire precision, and the speculative-decoding knobs all come
    // from the node's configuration, so they're operator knobs rather than
    // constants baked into the protocol.
    // The head tokenizes; the gateway only needs the size to plan chunks.
    // About 4 bytes per token, minus what the prefix cache will reuse.
    let estimated_new_prompt_tokens = ((prompt.len() as f64 / 4.0).ceil() as u32)
        .saturating_sub(if reuse_prefix {
            prefix_lease.cached_tokens
        } else {
            0
        })
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
            // A chunk is decoded as one `llama_batch` on the head; larger than
            // `n_batch` aborts llama.cpp. `prompt_chunk_tokens` is the same
            // clamp as `prompt_budget_error`, and it is the one that counts - not the raw
            // value of the knob.
            cfg.as_ref()
                .map(|config| {
                    if !config.adaptive_prefill_chunks {
                        // The operator fixed the chunk: use it as it is.
                        return prompt_chunk_tokens(config);
                    }
                    state.adaptive_chunk_tuner.choose(
                        &adaptive_chunk_route,
                        prompt_chunk_tokens(config),
                        config.n_batch,
                        estimated_new_prompt_tokens,
                        config.prefill_max_inflight_chunks as usize,
                        if config.moe_cache_mib > 0 {
                            super::budget::MOE_CACHE_MIN_CHUNK
                        } else {
                            super::budget::MIN_CHUNK
                        },
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
            // The parameter is "how many tokens to draft"; the batch width is
            // that plus the already confirmed token.
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
        "MTP speculation decision of the pipeline"
    );

    // Which of the two speculations runs on this request. MTP comes first
    // when both are on: it uses a head trained on the model itself,
    // while lookup only hits on repetitive output. Never both
    // in the same round - the batch is a single one, and mixing the sources would make
    // the acceptance rate impossible to attribute.
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

    // The final event cannot go out before the session is tidied.
    //
    // A chat sends the next turn as soon as the stream closes. While the
    // rewind and the release ran *after* `[DONE]`, the following request
    // arrived in the middle of them - measured: 54 ms before the previous turn's
    // release finished - and caught the cache in a state that nobody had
    // just finished writing. Here the token events go out right away, live, and
    // only the terminal one (`Done`/`Failed`) waits for the tidying.
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
        // Reusing a prefix is an optimization, never a reason for the request to fail.
        // A peer may have evicted the session, restarted or diverged from what
        // this gateway thinks it holds; in that case the first attempt
        // dies with out-of-order positions. The second goes cold, which is
        // exactly the behavior from before this feature existed.
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
                    "prefill failed because of the frame limit ({e}); it is not recoverable by repeating the same chunk"
                );
            } else if !crate::prefill::is_cancelled(e) {
                if let Some(rp) = crate::prefill::resume_point(e) {
                    warn!(
                        "prefill failed before delivering chunk {} (position {}, {e}); resuming at that point and preserving the KV",
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
                        "prefill needs a cold rebuild ({reason}; {e}); clearing the remote sessions once"
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
                            "resumed prefill still requires a cold rebuild ({reason}; {error}); clearing the remote sessions once"
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
                    // The reuse came from a snapshot on disk, not from a session
                    // that stayed alive in memory.
                    o.trace.prefix_reuse_status = Some("disk".to_owned());
                }
                info!(
                    "📥 prompt of {} tokens in {} chunk(s), {} from the cache · {:.1}s to the first word",
                    o.prompt_tokens,
                    o.chunks,
                    o.cached_tokens,
                    request_started.elapsed().as_secs_f64(),
                );
                retained_prefix_tokens = Some(o.retained_tokens);
                o
            }
            Err(e) if crate::prefill::is_cancelled(&e) => {
                info!("client disconnected during the prefill; cancelling the pipeline");
                return;
                }
                Err(e) => {
                let message = if unknown_session_recovery_attempted
                    || crate::relay::is_unknown_session_error(&e)
                {
                    "Prefill failed: a peer lost the session state; the system tried to rebuild the context, but could not finish and the request was not executed. Try sending the message again when the cluster is stable.".to_string()
                } else if cold_recovery_attempted {
                    "Prefill failed: a transport failure left it uncertain whether the peer processed the last chunk. The system tried to rebuild the context once, but could not finish and the request was not executed. Try sending the message again when the cluster is stable.".to_string()
                } else {
                    format!("Prefill failed: {}", crate::prefill::failure_cause(&e))
                };
                warn!("prefill failed: {e:#}");
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
                info!("client disconnected right after the prompt");
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
            "decode path of the pipeline"
        );
        if mtp_enabled && plan.steps.len() >= 2 && !sliced_mtp_requested {
            info!(
                reason = sliced_mtp_disable_reason(&plan),
                "sliced pipeline without MTP: continuing with the available capability"
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
                    warn!("batched remote transport failed: {error}");
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
            // Becomes `false` on the first round in which the head refuses the
            // draft. Asking again would only repeat the refusal, and the request is
            // expensive for a reason that cannot be seen: on the legacy path,
            // `step_speculative` stays on the bubbling ring (2N crossings per
            // token). The remote transport already keeps the loop on the head and arrives
            // here through the callback queue, without asking the gateway for another frame.
            // Sliced MTP is the other half of the same round: on the
            // legacy path the tail returns the draft and the gateway injects it; in the
            // remote stream the head installs it within its own cycle. Sliced
            // MTP exists only with 2+ hops - with one it is the whole model, which already
            // uses `step_speculative`.
            let mut speculating = true;
            // Draft that the tail returned in the previous round, waiting
            // to be injected in the next. `draft_sent` is read before the
            // `take`: it distinguishes round 1 (no draft has traveled yet, and the
            // head's refusal is expected) from the following ones (the draft was
            // sent - being ignored there is a reason to stop).
            let mut stored_draft: Option<Vec<i32>> = None;
            while remaining_generation_steps(budget.total, tokens_generated) > 0 {
                if cancellation.is_cancelled() {
                    return;
                }
                let remote_stream_mode = remote_stream.is_some();
                let draft_sent = speculating
                    && sliced_mtp
                    && !remote_stream_mode
                    && stored_draft.is_some();
                let res = if let Some(stream) = remote_stream.as_mut() {
                    stream.next(&cancellation).await
                } else if speculating {
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
                        let round = if speculating {
                            "speculative round"
                        } else {
                            "decode round"
                        };
                        warn!("Error in the {round} (token {tokens_generated}): {e}");
                        if tokens_generated > 0 {
                            info!("returning {tokens_generated} partial tokens");
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
                                    "Pipeline failed in the {round}: {e}"
                                )))
                                .await;
                        }
                        return;
                    }
                };
                if speculating {
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
                                    .unwrap_or("the tail did not report the cause of the deactivation");
                                info!(
                                    reason,
                                    detail,
                                    "sliced pipeline without an MTP draft: continuing without speculation"
                                );
                                speculating = false;
                            }
                        } else {
                            // Two reasons to give up: a draft already
                            // sent was refused, or the initial round did not
                            // produce the first draft.
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
                                    .unwrap_or("the tail did not report the cause of the deactivation");
                                info!(
                                    reason,
                                    detail,
                                    "sliced pipeline without an MTP draft: continuing without speculation"
                                );
                                speculating = false;
                            } else {
                                stored_draft = step.next_draft;
                            }
                        }
                    } else if !step.speculated {
                        // The head refused to draft - today this only happens if
                        // it is from a version prior to this fix, or if the
                        // repetition draft found nothing to propose.
                        info!("no hop drafted: continuing without speculation");
                        speculating = false;
                    }
                }
                trace.merge(&step.trace);
                if !remote_stream_mode {
                    trace.note_decode_round();
                }

                // A round can confirm more tokens than remain in the budget —
                // stream only what fits, and note it so `step.done` (which
                // may refer to a position we never even looked at) isn't
                // trusted past the cut.
                let remaining =
                    remaining_generation_steps(budget.total, tokens_generated) as usize;
                let take = step.accepted.len().min(remaining);
                let truncated_by_budget = take < step.accepted.len();

                for (token_id, text) in step.accepted.iter().take(take) {
                    next_token = *token_id;
                    tokens_generated += 1;
                    note_leg_boundary(budget, tokens_generated, &mut legs);
                    match send_text_with_stop(&tx, &mut stop_matcher, text).await {
                        StopSend::Disconnected => {
                            info!("client disconnected after {tokens_generated} tokens");
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
                        "✅ Pipeline complete ({}): {} tokens in {:.1}s ({:.1} tok/s)",
                        if speculating {
                            "especulativo"
                        } else {
                            "without speculation"
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
                            "partial rollback refused by some hop after {tokens_generated} tokens: ending the generation instead of risking a contaminated cache"
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
                                    "speculative rollback refused by the hop".to_string(),
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
                    if remote_stream.is_none() {
                        trace.note_decode_round();
                    }
                    if step.accepted.is_empty() {
                        let _ = tx
                            .send(GenEvent::Failed(
                                "Pipeline returned no confirmed token".to_string(),
                            ))
                            .await;
                        return;
                    }
                    let remaining =
                        remaining_generation_steps(budget.total, tokens_generated) as usize;
                    let take = step.accepted.len().min(remaining);
                    let truncated_by_budget = take < step.accepted.len();
                    for (index, (token_id, text)) in step.accepted.iter().take(take).enumerate() {
                        next_token = *token_id;
                        tokens_generated += 1;
                        note_leg_boundary(budget, tokens_generated, &mut legs);
                        match send_text_with_stop(&tx, &mut stop_matcher, text).await {
                            StopSend::Disconnected => {
                                // Client hung up; stop burning GPU time on a dead request.
                                info!("client disconnected after {tokens_generated} tokens");
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
                            "✅ Pipeline complete: {} tokens in {:.1}s ({:.1} tok/s)",
                            tokens_generated,
                            summary.timings.total_ms as f64 / 1000.0,
                            summary.timings.total_tokens_per_sec
                        );
                        let _ = tx.send(GenEvent::Done(summary)).await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("Error at step {tokens_generated} of the pipeline: {e}");
                    if tokens_generated > 0 {
                        // Some output is better than none — the original behaviour.
                        info!("returning {tokens_generated} partial tokens");
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
                                "Pipeline failed at step {tokens_generated}: {e}"
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

    // The lease leaves here in only one way: published if all the hops confirmed
    // that the session stayed parked at the end of the prompt, discarded along with the released
    // session in any other case.
    let mut lease = Some(prefix_lease);
    if let Some(retained_tokens) = retained_prefix_tokens {
        if rewind_sessions(&plan, &session_id, retained_tokens, &state.peer_http).await {
            if let Some(lease) = lease.take() {
                lease.commit(cache_prompt.clone(), retained_tokens);
            }
            // Phase 3: writes the retained prefix to disk, best-effort and outside the
            // response path. The session stays parked (retained) on all the
            // hops until the next turn, so there is time for the `save`.
            if let Some(disk) = state.prefix_disk.clone() {
                let plan = plan.clone();
                let clients = state.peer_http.clone();
                let key = prefix_key.clone();
                let prompt = cache_prompt.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    let hash = prefix_key_hash(&key, &prompt);
                    // Debounce: if the retained prefix grew little since the
                    // last snapshot of this lineage, it only renews the stamp and
                    // does not rewrite (each spill is a KV serialize + fan-out).
                    if let Some(ancestor) =
                        disk.longest_prefix_match(&key.model, &key.hops, &prompt)
                    {
                        if retained_tokens.saturating_sub(ancestor.cached_tokens)
                            < crate::prefix_cache::SLOT_AUTO_MIN_GROWTH_TOKENS
                        {
                            disk.touch(&ancestor.key_hash);
                            debug!(
                                "prefix spill skipped: +{} tokens since {} (< {})",
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
                        Err(error) => debug!("prefix spill to disk: {error}"),
                    }
                });
            }
        } else {
            warn!(
                "could not preserve the prefix of session {}; releasing the KV",
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

/// Forwards the generation events to the HTTP adapter and turns the
/// consumer's disappearance into an explicit cancellation.
///
/// `Sender::closed` must be observed in parallel with the receiving of
/// events. Keeping another clone of the sender in the generator keeps a monitor
/// based only on `closed` from seeing the disconnection, and a remote producer can
/// keep consuming KV even after the SSE has been abandoned.
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

/// Confirms on each peer that the session stayed parked at the end of the prompt, ready
/// for the next turn. A failure on one hop invalidates the whole entry: keeping only
/// part of the chain would create a silent hole.
///
/// On the peer's side this rolls **nothing** back - the generation ran in a separate
/// KV sequence that disappears whole (see `infer_server::handle_rewind`). That is why the
/// deadline here is short: it is bookkeeping, not inference, and the stream's final event
/// waits for it.
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

/// POST of a small JSON body to a session lifecycle route of a
/// peer, through the same transport as the decode: QUIC first when the cluster's
/// preference is QUIC, and HTTP/HTTPS as a fallback.
///
/// Why not just `http.post(...).json(...)`: every request must carry
/// the internal marker and the `connection_key` bearer when configured. This
/// keeps the same authentication contract on HTTP, HTTPS and on the QUIC
/// fallback; without the header, `require_inference_auth` answers `401 missing
/// Authorization header` and leaves the session retained. QUIC can also be accepted
/// by the marker over encrypted transport on the public network. A QUIC
/// transport failure falls back to the HTTP path; a 404/405 means a peer without the
/// HTTP/3 route and also falls back to HTTP.
///
/// `Ok(body)` on any 2xx (deserialized body, `Null` if unreadable);
/// `Err(reason)` when no address answered 2xx.
async fn post_session_lifecycle(
    http: &crate::peer_client::PeerClients,
    step: &crate::pipeline::PipelineStep,
    path: &str,
    body: &serde_json::Value,
    session_id: &str,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    // The route pinned by the route plan also applies to the session
    // lifecycle: a peer reachable only through the tunnel does not answer on the
    // announced addresses, and trying QUIC and TCP on them before used up the whole deadline - the
    // rewind failed (prefix lost) and so did the release (KV stuck on the peer,
    // which then showed up with a queue for the planner).
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
        (Err(direct), Some(tunnel)) if !direct.contains(" answered HTTP ") => {
            crate::relay::post_json_via_tunnel(http, tunnel, step, path, body, session_id, timeout)
                .await
                .map_err(|error| format!("{direct}; libp2p-tunnel: {error}"))
        }
        (result, _) => result,
    }
}

/// QUIC and then HTTP on the peer's announced addresses.
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
        Err(error) => return Err(format!("invalid JSON body: {error}")),
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
                    last_error = Some(format!("quic: HTTP {} (no HTTP/3 route)", response.status));
                    break;
                }
                Ok(response) => {
                    return Err(format!(
                        "{path} on {} answered HTTP {}",
                        step.node_id, response.status
                    ));
                }
                Err(error) => {
                    last_error = Some(format!("quic: {error}"));
                    warn!(
                        "{path} over QUIC on {} failed ({error}), trying TCP",
                        step.node_id
                    );
                }
            }
        }
    }

    let Ok(client) = http.for_peer(&step.node_id) else {
        return Err(last_error.unwrap_or_else(|| "no client for the peer".to_string()));
    };
    for base in http.route_order(&route_key, &candidates) {
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let request = client.post(&url).json(body).timeout(timeout);
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
                last_error = Some(format!("{base}: another node answered"));
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
    Err(last_error.unwrap_or_else(|| "no announced address".to_string()))
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
                    "prefix rewind confirmed: peer={} session={} retained_tokens={} n_past={}",
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
                    "prefix rewind refused: peer={} session={} retained_tokens={} response={}",
                    step.node_id,
                    &session_id[..8.min(session_id.len())],
                    retained_tokens,
                    body,
                );
                all_ok = false;
            }
            Err(error) => {
                warn!(
                    "prefix rewind failed: peer={} session={} retained_tokens={} error={error}",
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
            warn!("session release on {} failed: {error}", step.node_id);
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
                    "session rollback on {} refused: {}",
                    step.node_id,
                    body.get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("no reason given")
                );
                all_ok = false;
            }
            Err(error) => {
                warn!("session rollback on {} failed: {error}", step.node_id);
                all_ok = false;
            }
        }
    }
    all_ok
}

// ── KV cache snapshots of the deployment (`--slot-save-path`) ────────────────

/// Name of the snapshot file each hop writes: the operational name plus the
/// `node_id` of that hop, so a single directory holds each peer's slice
/// without collision. `sanitize_slot_name` on the peer's side accepts `[A-Za-z0-9._-]`, and
/// the `node_id` is a hash hex.
pub(super) fn slot_name_for_step(snapshot_name: &str, node_id: &str) -> String {
    format!("{snapshot_name}__{node_id}")
}

/// All the hops must announce `inference/slot-persist-v1`. A deployment
/// snapshot is all-or-nothing: without the cap on some hop, that peer does not know how to
/// write its slice and the snapshot would be left with a hole.
pub(super) fn plan_supports_slot_persist(plan: &PipelinePlan) -> Result<(), String> {
    if let Some(step) = plan
        .steps
        .iter()
        .find(|step| !step.supports(crate::pipeline::CAP_SLOT_PERSIST))
    {
        return Err(format!(
            "hop {} (layers {}-{}) has no slot_save_path configured; the snapshot requires the capability on all peers",
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
                if error.contains(" answered HTTP ") {
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
                    .unwrap_or("no detail")
            )));
        }
        clients.remember_route_success(&route_key, &base);
        return Ok(payload);
    }
    Err(SlotCallError::Unreachable(last_error.unwrap_or_else(
        || format!("{} did not answer on any inference address", step.node_id),
    )))
}

/// Builds the disk index of the prefix cache, or `None` when
/// `slot_persist_prefix_cache` is off or there is no `slot_save_path`.
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

/// Snapshot name of an automatic prefix: `auto__<key_hash>`. Per hop, the
/// file becomes `auto__<key_hash>__<node_id>.bin` (`slot_name_for_step`).
pub(super) fn auto_slot_name(key_hash: &str) -> String {
    format!("auto__{key_hash}")
}

/// Stable hash of a conversation's prefix: identifies the set of `.bin`.
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

/// Resolves the pipeline that serves a model today, for the operator's snapshot
/// routes. Reuses the same planner as the generation; it is observation, it does not change
/// routing or balancing.
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
        .ok_or_else(|| format!("no peer serves the model '{model}'"))?;
    let deploy_id = peers.iter().find(|p| p.deploy_id == model).map(|_| model);
    crate::pipeline::plan_pipeline(&peers, model, deploy_id, total_layers, &state.loads)
        .await
        .ok_or_else(|| format!("no pipeline covers 0..{total_layers} of the model '{model}'"))
}

/// Writes a KV snapshot of a deployment: each hop persists its slice
/// in the local `slot_save_path`. All-or-nothing - if a hop fails, those that already
/// wrote are deleted and the error describes which hop stopped.
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
                    "snapshot '{snapshot_name}' failed on {}: {error}",
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
        "snapshot '{snapshot_name}' written on {} hop(s), {total_bytes} bytes",
        plan.steps.len()
    );
    Ok(total_bytes)
}

/// Restores a deployment snapshot into a **new** session on all the hops.
/// Each hop has to come back with the same token boundary; any
/// divergence releases the new session on all of them and returns an error.
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
                    .ok_or_else(|| format!("{} did not report restored_tokens", step.node_id))
            });
        match result {
            Ok(tokens) => match agreed {
                Some(prev) if prev != tokens => {
                    release_sessions(plan, new_session_id, clients).await;
                    return Err(format!(
                        "hops diverged in the restore: {} restored {tokens}, another restored {prev}",
                        step.node_id
                    ));
                }
                _ => agreed = Some(tokens),
            },
            Err(error) => {
                warn!(
                    "restore '{snapshot_name}' failed on {}: {error}",
                    step.node_id
                );
                release_sessions(plan, new_session_id, clients).await;
                return Err(error);
            }
        }
    }
    let tokens = agreed.unwrap_or(0);
    info!("snapshot '{snapshot_name}' restored as session {new_session_id} ({tokens} tokens)");
    Ok(tokens)
}

/// Deletes a deployment snapshot. Best effort: a hop that no longer has the
/// file is not an error.
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
            warn!("erase of '{snapshot_name}' on {}: {error}", step.node_id);
        }
    }
}

/// Pessimistic token count for a prompt, used only by the size guard.
pub(super) fn estimate_tokens(prompt: &str) -> u32 {
    prompt.chars().count().div_ceil(CHARS_PER_TOKEN) as u32
}

/// The refusal for a prompt above capacity, from loose numbers.
///
/// It exists separately from [`prompt_budget_error`] because the gateway that **does not**
/// load any model needs the same admission: that was exactly the
/// path of the 2026-09-08 defect, in which a 30-thousand-character prompt
/// went through the gateway without a local config, reached the peer's decode and
/// brought the whole process down with `CUDA error: out of memory`. Refusing here
/// is what keeps the node alive for the other requests.
pub fn prompt_budget_error_of(n_ctx: u32, max_new_tokens: u32, prompt: &str) -> Option<String> {
    let budget = prompt_budget_of(n_ctx, 1, max_new_tokens);
    let estimated = estimate_tokens(prompt);
    (estimated > budget).then(|| {
        format!(
            "Prompt too large: ~{estimated} tokens estimated against a limit of {budget}. Each session gets n_ctx ({n_ctx}) minus max_new_tokens ({max_new_tokens}). Increase n_ctx in Configure → advanced settings, reduce max_new_tokens, or reduce the prompt size."
        )
    })
}

pub fn prompt_budget_error(cfg: &crate::runtime::NodeConfig, prompt: &str) -> Option<String> {
    let error = prompt_budget_error_of(cfg.n_ctx, cfg.max_new_tokens, prompt)?;
    // The node that has the model at home knows more: it adds why `n_batch`
    // is not the limit, which is the confusion the operator makes first.
    Some(format!(
        "{error} The prompt goes in chunks of {} tokens, so n_batch ({}) does not limit the prompt.",
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

    // Representative reproduction from the admins' point of view:
    // Setup: one sender admin and two receiver admins in Network::testkit,
    // without UDP, services or production data.
    // Input: an AuditEvent Begin and Complete accepted by the plugin host.
    // Trigger: the plugin's positive answer must trigger the POM fanout.
    // Mask: when the plugin is not active, the AuditLog fallback already replicated.
    // Symptom: with the plugin active, the two remote admins received no event.
    // This test covers the fanout up to the libp2p queue, not the real POM processes nor
    // the extracted cdylib; it does not claim equivalence with that full E2E execution.
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
            .expect("isolated network"),
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
        let database = Arc::new(crate::db::PomDb::open(&path).expect("isolated database"));
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
                .expect("fanout to the admins in time")
                .expect("replication channel open");
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
                crate::plugins::LOGS_PLUGIN_CODE,
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
                Some("the client disconnected before the end".to_string()),
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
                crate::plugins::LOGS_PLUGIN_CODE,
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
            .ingest_audit_event(crate::plugins::LOGS_PLUGIN_CODE, begin_event.clone())
            .await
        {
            audit.replicate_event(begin_event);
            if plugins
                .ingest_audit_event(crate::plugins::LOGS_PLUGIN_CODE, complete_event.clone())
                .await
            {
                audit.replicate_event(complete_event);
            } else {
                tracing::warn!(request_id = %id, "audit completion did not enter the plugin");
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
    Err("generation ended without a response".to_string())
}
