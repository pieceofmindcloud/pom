//! Whole-model inference by a plugin's engine (an "inference service").
//!
//! A deployment whose `RuntimeParams::inference_service` names a plugin (the
//! Strata plugin, for example) does not run POM's own engine on its target.
//! Three pieces make that a deployment like any other:
//!
//! - **On the target node** ([`run`]): the runtime supervisor calls this
//!   instead of `infer_server::run`. It asks the plugin to load the model
//!   (`engine.load`), serves `/health` on the inference port from the
//!   plugin's state (`engine.status`) with the same fields the llama.cpp
//!   server reports, and asks the plugin to unload (`engine.unload`) when the
//!   generation ends - an undeploy, a redeploy or the node stopping. So the
//!   health watcher, the slice journal, the announce, `/v1/models`, idle
//!   unloading and undeploy work unchanged. The node announces the
//!   capability `engine/plugin-v1:<code>` *instead of* the hidden-state
//!   capabilities: no gateway may plan this peer as a pipeline hop.
//! - **On the gateway** ([`plugin_of_plan`], [`generate`]): a plan whose only
//!   step announces that capability is not run through prefill/step. The
//!   gateway sends the prompt it already rendered - the same text a llama.cpp
//!   head receives, with the same `chat_template` and thinking flags - to the
//!   plugin's generation route and turns the streamed text into the same
//!   `GenEvent`s, so every API adapter, the request audit and retrieval work
//!   as they do for a pipeline. There is no fallback to another engine.
//! - **Between nodes** ([`engine_generate_route`]): when the gateway is
//!   another machine, it posts to `POST /api/internal/plugins/:code/engine/
//!   generate` on the target, signed like every node-to-node call; the target
//!   forwards the body to its plugin's loopback generation server
//!   (`engine.upstream`) and streams the answer back.
//!
//! The generation route of a plugin takes `{prompt, chat_template, thinking,
//! max_tokens, stop, temperature, top_p, top_k, min_p, presence_penalty,
//! frequency_penalty, seed}` and answers one JSON object per line:
//! `{"t": text}`, `{"p": [read, total]}`, `{"done": {finish, prompt_tokens,
//! completion_tokens, cached_tokens}}` or `{"error": message}`.

use crate::gateway::{GatewayState, GenEvent, GenSummary};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// The capability a node announces while a plugin engine serves its model.
pub(crate) const CAPABILITY_PREFIX: &str = "engine/plugin-v1:";
/// A prompt is the whole conversation; the generation route is not the
/// small-JSON peer route.
const MAX_GENERATION_BODY: usize = 32 * 1024 * 1024;
/// How often the target node reads the plugin's engine state.
const STATUS_POLL: Duration = Duration::from_secs(2);
/// One generation may run for a long time (a long prompt, a long answer).
const GENERATION_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
const SAMPLING_KEYS: [&str; 7] = [
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
];

static PLUGINS: OnceLock<Arc<crate::plugins::PluginManager>> = OnceLock::new();

/// The node's plugin manager, for the runtime supervisor, which does not hold
/// the gateway state.
pub(crate) fn install(plugins: Arc<crate::plugins::PluginManager>) {
    let _ = PLUGINS.set(plugins);
}

/// A plugin code as the plugin system writes them.
pub(crate) fn valid_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && code.len() <= 64
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn capability(code: &str) -> String {
    format!("{CAPABILITY_PREFIX}{code}")
}

/// The plugin whose engine serves a peer, from its announced capabilities.
pub(crate) fn plugin_of_capabilities(capabilities: &[String]) -> Option<&str> {
    capabilities
        .iter()
        .find_map(|cap| cap.strip_prefix(CAPABILITY_PREFIX))
        .filter(|code| valid_code(code))
}

/// The plugin engine a plan runs on: every step served by the same one, as
/// whole slices in layer order - a single machine, or the stages of a model
/// split across machines ([`crate::plugin_stage_link`]), whose last step
/// takes the prompt.
pub(crate) fn plugin_of_plan(plan: &crate::pipeline::PipelinePlan) -> Option<String> {
    let first = plan.steps.first()?;
    let code = plugin_of_capabilities(&first.inference_frame_capabilities)?;
    let whole_slices = plan.steps.iter().all(|step| {
        plugin_of_capabilities(&step.inference_frame_capabilities) == Some(code)
            && step.exec_start == step.start_layer
            && step.exec_end == step.end_layer
    });
    let in_order = plan
        .steps
        .windows(2)
        .all(|pair| pair[1].start_layer == pair[0].end_layer + 1);
    (whole_slices && in_order).then(|| code.to_owned())
}

// ── Target node ──────────────────────────────────────────────────────────────

/// `--kv` of an engine with its own KV formats, from the node's cache type.
fn kv_of(cfg: &crate::runtime::NodeConfig) -> &'static str {
    match cfg.cache_type_k.as_str() {
        "q4_0" => "q4_0",
        _ => "int8",
    }
}

async fn nvidia_gpus() -> Vec<u32> {
    crate::sysmon::sample()
        .await
        .gpus
        .into_iter()
        .filter(|gpu| matches!(gpu.vendor, crate::sysmon::GpuVendor::Nvidia))
        .map(|gpu| gpu.index)
        .collect()
}

/// What `engine.load` receives. `link` is set for one stage of a model split
/// across machines ([`crate::plugin_stage_link::prepare`]).
pub(crate) fn load_params(
    cfg: &crate::runtime::NodeConfig,
    gpus: &[u32],
    link: Option<&Value>,
) -> Value {
    let mut params = json!({
        "model_path": cfg.model_path,
        "model_label": cfg.model_label,
        "n_ctx": cfg.n_ctx,
        "max_sessions": cfg.max_sessions,
        "kv": kv_of(cfg),
        "gpus": gpus,
        "settings": cfg.engine_settings,
    });
    if let Some(link) = link {
        params["link"] = link.clone();
    }
    params
}

/// The `/health` this node answers while a plugin engine serves its model:
/// the fields the gateway's admission, the health watcher and the capacity
/// read, with `model_loaded` true only once the plugin says it is ready.
pub(crate) fn health_body(cfg: &crate::runtime::NodeConfig, code: &str, status: &Value) -> Value {
    let ready = status["state"] == "ready";
    let n_ctx = status["n_ctx"]
        .as_u64()
        .filter(|n_ctx| *n_ctx > 0)
        .unwrap_or(u64::from(cfg.n_ctx));
    json!({
        "status": "ok",
        "model_loaded": ready,
        "model_label": cfg.model_label,
        "deploy_id": cfg.deploy_id,
        "start_layer": cfg.start_layer,
        "end_layer": cfg.end_layer,
        "total_layers": cfg.total_layers,
        "original_block_count": cfg.total_layers,
        "selected_path": format!("plugin/{code}"),
        "max_sessions": cfg.max_sessions,
        "n_ctx_total": n_ctx,
        // The plugin engine gives every request the whole window.
        "n_ctx_per_session": n_ctx,
        "capabilities": [capability(code)],
        "engine": {
            "plugin": code,
            "state": status["state"],
            "detail": status["detail"],
            "busy": status["busy"],
        },
    })
}

async fn query(code: &str, operation: &str, payload: Value) -> Option<Value> {
    PLUGINS.get()?.query_audit(code, operation, payload).await
}

/// Runs one generation of a plugin-engine deployment on this node, until the
/// runtime moves to another generation (`stop_rx`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    cfg: crate::runtime::NodeConfig,
    mut stop_rx: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    tls: Option<crate::serve::TlsServer>,
    quic: Option<crate::serve::QuicServerConfig>,
    quic_port: Option<u16>,
) -> anyhow::Result<()> {
    let code = cfg
        .inference_service
        .clone()
        .filter(|code| valid_code(code))
        .ok_or_else(|| anyhow::anyhow!("invalid inference service"))?;
    if PLUGINS.get().is_none() {
        anyhow::bail!("the plugin manager is not available");
    }
    // The port first, as the llama.cpp server does: binding is the part that
    // fails because of the outside world, and loading costs minutes.
    let listener = crate::infer_server::bind_dual_stack(cfg.inference_port).await?;
    let gpus = nvidia_gpus().await;
    // One stage of a split model: a later stage waits here for the earlier
    // stage's machine and keeps the relay to it while it serves.
    let (link, _relay) = if crate::plugin_stage_link::is_stage(&cfg) {
        let stop_check = stop_rx.clone();
        let (link, relay) = crate::plugin_stage_link::prepare(&cfg, &code, move || {
            *stop_check.borrow() != generation
        })
        .await?;
        (Some(link), relay)
    } else {
        (None, None)
    };
    info!(
        plugin = %code,
        model = %cfg.model_label,
        ?gpus,
        "asking the plugin's inference service to load the model"
    );
    let accepted = query(
        &code,
        "engine.load",
        json!({ "params": load_params(&cfg, &gpus, link.as_ref()) }),
    )
    .await;
    if accepted.is_none() {
        anyhow::bail!(
            "the plugin '{code}' is not active on this machine or refused to load {}",
            cfg.model_path
        );
    }

    let status = Arc::new(Mutex::new(json!({"state": "loading"})));
    let (failed_tx, mut failed_rx) = tokio::sync::oneshot::channel::<String>();
    let poll_status = Arc::clone(&status);
    let poll_code = code.clone();
    let mut poll_stop = stop_rx.clone();
    let poller = tokio::spawn(async move {
        let mut failed_tx = Some(failed_tx);
        let mut ready_seen = false;
        let mut last_state = String::new();
        loop {
            if let Some(current) = query(&poll_code, "engine.status", json!({})).await {
                let state = current["state"].as_str().unwrap_or_default().to_owned();
                // Each change of the engine's state is a line of this node's
                // process log, which reaches the admins like llama.cpp's.
                if state != last_state {
                    let detail = current["detail"].as_str().unwrap_or_default();
                    if state == "error" {
                        warn!(plugin = %poll_code, state = %state, detail, "motor do plugin");
                    } else {
                        info!(plugin = %poll_code, state = %state, detail, "motor do plugin");
                    }
                    last_state = state.clone();
                }
                *poll_status
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = current.clone();
                ready_seen |= state == "ready";
                // An error, or an engine that went idle after it had served,
                // ends this generation: the supervisor restarts it within its
                // budget and shows the reason.
                if state == "error" || (ready_seen && state == "idle") {
                    if let Some(sender) = failed_tx.take() {
                        let detail = current["detail"]
                            .as_str()
                            .unwrap_or("the plugin engine stopped")
                            .to_owned();
                        let _ = sender.send(detail);
                    }
                    return;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(STATUS_POLL) => {}
                changed = poll_stop.changed() => {
                    if changed.is_err() || *poll_stop.borrow() != generation {
                        return;
                    }
                }
            }
        }
    });

    let health_cfg = cfg.clone();
    let health_code = code.clone();
    let health_status = Arc::clone(&status);
    let router = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(move || {
                let body = health_body(
                    &health_cfg,
                    &health_code,
                    &health_status
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                );
                async move { axum::Json(body) }
            }),
        )
        .layer(axum::middleware::map_response(
            crate::peer_client::stamp_node_id,
        ));

    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_seen = Arc::clone(&failure);
    let served = crate::serve::serve_with_quic(listener, router, tls, quic, quic_port, async move {
        loop {
            tokio::select! {
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() != generation {
                        break;
                    }
                }
                detail = &mut failed_rx => {
                    if let Ok(detail) = detail {
                        *failure_seen.lock().unwrap_or_else(|error| error.into_inner()) = Some(detail);
                        break;
                    }
                }
            }
        }
    })
    .await;
    poller.abort();
    if query(&code, "engine.unload", json!({})).await.is_none() {
        warn!(plugin = %code, "the plugin did not confirm the unload");
    }
    served?;
    let failure = failure
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match failure {
        Some(detail) => Err(anyhow::anyhow!("{code}: {detail}")),
        None => Ok(()),
    }
}

// ── Gateway ──────────────────────────────────────────────────────────────────

/// What one generation asks of a plugin engine.
pub(crate) struct EngineRequest {
    pub prompt: String,
    pub chat_template: bool,
    pub thinking_enabled: bool,
    pub max_tokens: u32,
    pub stops: Vec<String>,
    /// The client's request, for its sampling fields.
    pub request_details: Value,
}

pub(crate) fn generation_body(request: &EngineRequest) -> Value {
    let mut body = json!({
        "prompt": request.prompt,
        "chat_template": request.chat_template,
        "thinking": request.thinking_enabled,
        "max_tokens": request.max_tokens,
        "stop": request.stops,
    });
    for key in SAMPLING_KEYS {
        let value = &request.request_details[key];
        if value.is_number() {
            body[key] = value.clone();
        }
    }
    body
}

/// `(port, token)` of this node's plugin generation server, while ready.
async fn local_upstream(
    plugins: &crate::plugins::PluginManager,
    code: &str,
) -> Result<(u16, String), String> {
    let answer = plugins
        .query_audit(code, "engine.upstream", json!({}))
        .await
        .ok_or_else(|| format!("the plugin '{code}' is not active on this machine"))?;
    match answer["status"].as_str() {
        Some("ready") => {}
        Some("error") => {
            return Err(answer["error"]
                .as_str()
                .unwrap_or("the plugin engine failed")
                .to_owned())
        }
        _ => return Err("the plugin engine is still loading the model".into()),
    }
    let port = answer["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)
        .ok_or("the plugin gave no port")?;
    let token = answer["token"]
        .as_str()
        .filter(|token| !token.is_empty() && token.len() <= 512 && token.is_ascii())
        .ok_or("the plugin gave no token")?;
    Ok((port, token.to_owned()))
}

fn loopback_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(GENERATION_TIMEOUT)
            .no_proxy()
            .build()
            .unwrap_or_default()
    })
}

async fn post_local(
    plugins: &crate::plugins::PluginManager,
    code: &str,
    body: Vec<u8>,
) -> Result<reqwest::Response, String> {
    let (port, token) = local_upstream(plugins, code).await?;
    loopback_client()
        .post(format!("http://127.0.0.1:{port}/pom/generate"))
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("the plugin engine did not answer: {error}"))
}

async fn open_stream(
    state: &GatewayState,
    node_id: &str,
    code: &str,
    body: &Value,
) -> Result<reqwest::Response, String> {
    let response = if node_id == state.network.node_id {
        let bytes = serde_json::to_vec(body).map_err(|error| error.to_string())?;
        post_local(&state.plugins, code, bytes).await?
    } else {
        let peer = state
            .all_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == node_id)
            .ok_or_else(|| {
                format!("the machine serving this model ({node_id}) is not reachable")
            })?;
        state
            .control_post_json(
                &peer,
                &format!("/api/internal/plugins/{code}/engine/generate"),
                body,
                GENERATION_TIMEOUT,
            )
            .await
            .map_err(|error| format!("the machine serving this model did not answer: {error}"))?
    };
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or(text);
    Err(format!(
        "the plugin engine refused the request ({status}): {message}"
    ))
}

/// The trace of a generation a plugin engine ran: every stage of the plan is
/// a hop the plugin executed. The engine reports the request as a whole (its
/// `done`), not per hop, so every stage counts the same prompt call and decode
/// rounds, and the engine's own prompt and decode times go to the last stage,
/// which serves the request.
pub(crate) fn engine_trace(
    trace_id: String,
    model: &str,
    plan: &crate::pipeline::PipelinePlan,
    code: &str,
    done: &Value,
    generated: u32,
) -> crate::prefill::RequestTrace {
    let steps: Vec<(&str, u32, u32)> = plan
        .steps
        .iter()
        .map(|step| (step.node_id.as_str(), step.start_layer, step.end_layer))
        .collect();
    let mut trace = crate::prefill::RequestTrace::new_with_trace_id(trace_id, model, &steps);
    let timings = &done["timings"];
    let proposed = timings["draft_n"].as_u64().unwrap_or(0);
    let accepted = timings["draft_n_accepted"]
        .as_u64()
        .unwrap_or(0)
        .min(proposed);
    // A decode round yields one token plus the drafts it accepted; the first
    // token comes from the prompt.
    let rounds = u64::from(generated)
        .saturating_sub(accepted)
        .saturating_sub(1)
        .max(u64::from(generated > 0));
    let path = format!("plugin/{code}");
    let last = trace.hops.len().saturating_sub(1);
    for (index, hop) in trace.hops.iter_mut().enumerate() {
        for phase in [&mut hop.prefill, &mut hop.decode] {
            phase.executed_engine = crate::executor::ExecutionEngine::Plugin;
            phase.executed_engines = vec![crate::executor::ExecutionEngine::Plugin];
            phase.selected_path = Some(path.clone());
        }
        hop.prefill.calls = 1;
        hop.prefill.reported_spans = 1;
        hop.decode.calls = rounds.min(u64::from(u32::MAX)) as u32;
        hop.decode.reported_spans = rounds;
        if index == last {
            hop.decode.node_decode_ms = timings["predicted_ms"].as_f64().unwrap_or(0.0);
        }
    }
    for _ in 0..rounds {
        trace.note_decode_round();
    }
    trace.mtp_draft_tokens_proposed = proposed;
    trace.mtp_draft_tokens_accepted = accepted;
    trace.mtp_draft_tokens_rejected = proposed - accepted;
    trace.mtp_acceptance_rate = (proposed > 0).then(|| accepted as f64 / proposed as f64);
    trace
}

fn finish_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "length",
        _ => "stop",
    }
}

/// Runs one generation on the plugin engine of `node_id` and reports it as
/// the pipeline does.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn generate(
    state: &GatewayState,
    plan: &crate::pipeline::PipelinePlan,
    code: &str,
    model: &str,
    trace_id: Option<String>,
    request: EngineRequest,
    tx: &mpsc::Sender<GenEvent>,
    cancellation: &crate::prefill::Cancellation,
    request_started: Instant,
) {
    // The last stage holds the head: it serves the request.
    let Some(step) = plan.steps.last() else {
        let _ = tx.send(GenEvent::Failed("empty plan".into())).await;
        return;
    };
    let node_id = step.node_id.as_str();
    let body = generation_body(&request);
    let response = match open_stream(state, node_id, code, &body).await {
        Ok(response) => response,
        Err(error) => {
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        }
    };
    let mut stream = response.bytes_stream();
    let mut pending: Vec<u8> = Vec::new();
    let mut first_token_at: Option<Instant> = None;
    let mut streamed_any = false;
    let mut pieces = 0u32;
    loop {
        if cancellation.is_cancelled() || tx.is_closed() {
            // Dropping the stream closes the connection; the plugin's server
            // notices and stops the engine.
            return;
        }
        let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        let chunk = match next {
            Err(_) => continue,
            Ok(None) => break,
            Ok(Some(Err(error))) => {
                let _ = tx
                    .send(GenEvent::Failed(format!(
                        "the plugin engine stream broke: {error}"
                    )))
                    .await;
                return;
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let Ok(event) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
                continue;
            };
            if let Some(text) = event["t"].as_str() {
                if text.is_empty() {
                    continue;
                }
                first_token_at.get_or_insert_with(Instant::now);
                streamed_any = true;
                pieces += 1;
                if tx.send(GenEvent::Token(text.to_owned())).await.is_err() {
                    return;
                }
            } else if let Some(done) = event.get("done") {
                let total_ms = request_started.elapsed().as_millis() as u64;
                // The engine's own prompt time, when it reports one (the
                // llama.cpp-style `timings` of Strata's server).
                let engine_prefill_ms = done["timings"]["prompt_ms"]
                    .as_f64()
                    .map(|ms| ms.round() as u64);
                let first = first_token_at.unwrap_or_else(Instant::now);
                let ttft_ms = first.saturating_duration_since(request_started).as_millis() as u64;
                let generated = done["completion_tokens"]
                    .as_u64()
                    .map(|count| count as u32)
                    .unwrap_or(pieces);
                let prompt_tokens = done["prompt_tokens"].as_u64().unwrap_or(0) as u32;
                let cached_tokens = done["cached_tokens"].as_u64().unwrap_or(0) as u32;
                let prefill_ms = engine_prefill_ms.unwrap_or(ttft_ms);
                let mut timings = crate::gateway::timing_metrics(
                    total_ms,
                    ttft_ms,
                    prefill_ms,
                    prompt_tokens.saturating_sub(cached_tokens),
                    generated,
                );
                timings.server_ttft_ms = Some(ttft_ms);
                timings.engine_ttft_ms = engine_prefill_ms;
                let finish = done["finish"].as_str().unwrap_or("stop");
                // The same record a one-hop llama.cpp pipeline leaves: the
                // request audit, the statistics and the logs the admins
                // receive read the source and this trace, whatever the engine.
                let mut trace = engine_trace(
                    trace_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
                    model,
                    plan,
                    code,
                    done,
                    generated,
                );
                trace.input_tokens = prompt_tokens;
                trace.cached_tokens = cached_tokens;
                trace.prefix_match_tokens = cached_tokens;
                trace.prefill_ms = prefill_ms;
                trace.server_ttft_ms = Some(ttft_ms);
                trace.engine_ttft_ms = engine_prefill_ms;
                trace.output_tokens = generated;
                trace.set_generated_tokens(generated);
                trace.complete = true;
                trace.refresh_telemetry(total_ms);
                info!(
                    plugin = %code,
                    node_id = %node_id,
                    prompt_tokens,
                    cached_tokens,
                    generated,
                    total_ms,
                    finish,
                    "geração concluída pelo serviço de inferência do plugin"
                );
                let summary = GenSummary {
                    source: format!("pipeline:{}", plan.hop_chain()),
                    tokens_generated: generated,
                    tokens_per_sec: timings.generation_tokens_per_sec.unwrap_or(0.0),
                    duration_ms: total_ms,
                    timings,
                    partial: false,
                    finish_reason: Some(finish_reason(finish).to_owned()),
                    stop_sequence: done["stop_sequence"].as_str().map(str::to_owned),
                    legs: 1,
                    trace: Some(trace),
                };
                let _ = tx.send(GenEvent::Done(summary)).await;
                return;
            } else if let Some(error) = event["error"].as_str() {
                let _ = tx.send(GenEvent::Failed(error.to_owned())).await;
                return;
            }
        }
    }
    let _ = tx
        .send(GenEvent::Failed(if streamed_any {
            "the plugin engine stopped in the middle of the answer".into()
        } else {
            "the plugin engine closed the request without an answer".into()
        }))
        .await;
}

// ── Deployment ───────────────────────────────────────────────────────────────

/// The plugin services a machine of the network publishes.
async fn services_of(state: &GatewayState, node_id: &str) -> Option<Vec<Value>> {
    if node_id == state.network.node_id {
        let services = crate::plugin_services::node_services(&state.plugins).await;
        return serde_json::to_value(services).ok()?.as_array().cloned();
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)?;
    let response = state
        .control_get(
            &peer,
            "/api/internal/plugins/services",
            Duration::from_secs(5),
        )
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    body["services"].as_array().cloned()
}

/// Why a deployment on a plugin's inference service cannot go ahead, if it
/// cannot. A whole model goes to one machine; layer slices go one per
/// machine, to a service that publishes `stages`. Every machine runs the
/// plugin with an inference service that is not in error.
pub(crate) async fn deploy_block(
    state: &GatewayState,
    service: &str,
    whole: bool,
    targets: &[String],
) -> Option<String> {
    if !valid_code(service) {
        return Some(format!("serviço de inferência inválido: {service}"));
    }
    let named: Vec<&String> = targets.iter().filter(|id| !id.is_empty()).collect();
    let mut unique = named.clone();
    unique.sort();
    unique.dedup();
    if unique.is_empty() {
        return Some("escolha a máquina que roda o serviço de inferência".into());
    }
    if whole && unique.len() != 1 {
        return Some(format!(
            "o serviço de inferência {service} roda o modelo inteiro numa máquina só: escolha uma máquina"
        ));
    }
    if !whole && unique.len() != named.len() {
        return Some(format!(
            "o serviço de inferência {service} roda uma fatia por máquina: escolha máquinas diferentes"
        ));
    }
    for target in unique {
        let Some(services) = services_of(state, target).await else {
            return Some(format!(
                "a máquina {target} não respondeu quais serviços oferece"
            ));
        };
        let offered = services
            .iter()
            .find(|entry| entry["plugin_code"] == service && entry["kind"] == "inference");
        match offered {
            None => {
                return Some(format!(
                    "a máquina {target} não tem o plugin {service} com o serviço de inferência ativo"
                ))
            }
            Some(entry) if entry["status"] == "error" || entry["status"] == "waiting" => {
                return Some(format!(
                    "o serviço de inferência {service} da máquina {target} não está pronto: {}",
                    entry["detail"].as_str().unwrap_or("sem detalhe")
                ))
            }
            Some(entry) if !whole && entry["stages"] != true => {
                return Some(format!(
                    "o serviço de inferência {service} serve o modelo inteiro: escolha \"modelo inteiro\""
                ))
            }
            Some(_) => {}
        }
    }
    None
}

// ── Route between nodes ──────────────────────────────────────────────────────

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// `POST /api/internal/plugins/:code/engine/generate`: a gateway of this
/// network runs one generation on this node's plugin engine. Only a signed
/// call from a live peer gets through.
pub(crate) async fn engine_generate_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    body: Bytes,
) -> Response {
    let Some(principal) = auth.principal() else {
        return refuse(StatusCode::FORBIDDEN, "a signed peer call is required");
    };
    if !valid_code(&code) {
        return refuse(StatusCode::BAD_REQUEST, "invalid plugin");
    }
    if body.len() > MAX_GENERATION_BODY {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    if !state
        .network
        .alive_peers()
        .await
        .iter()
        .any(|peer| peer.node_id == principal.node_id)
    {
        return refuse(StatusCode::FORBIDDEN, "the caller is not a live peer");
    }
    let served = state
        .runtime
        .config()
        .await
        .and_then(|cfg| cfg.inference_service)
        .is_some_and(|service| service == code);
    if !served {
        return refuse(
            StatusCode::CONFLICT,
            "this machine does not serve a model with that inference service",
        );
    }
    let response = match post_local(&state.plugins, &code, body.to_vec()).await {
        Ok(response) => response,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let stream = response.bytes_stream();
    Response::builder()
        .status(status)
        .header("content-type", "application/x-ndjson")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "stream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_codes_and_capabilities_round_trip() {
        assert!(valid_code("strata") && valid_code("code_server"));
        for bad in ["", "Strata", "1x", "a/b", "a-b", &"x".repeat(65)] {
            assert!(!valid_code(bad), "{bad}");
        }
        let caps = vec![
            "transport/libp2p-tunnel-v1".to_owned(),
            capability("strata"),
        ];
        assert_eq!(plugin_of_capabilities(&caps), Some("strata"));
        assert_eq!(plugin_of_capabilities(&["relay/v1-chain".to_owned()]), None);
        assert_eq!(
            plugin_of_capabilities(&[format!("{CAPABILITY_PREFIX}Bad")]),
            None
        );
    }

    fn step(node: &str, start: u32, end: u32, caps: &[String]) -> crate::pipeline::PipelineStep {
        crate::pipeline::PipelineStep {
            node_id: node.into(),
            inference_url: String::new(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: start,
            end_layer: end,
            exec_start: start,
            exec_end: end,
            inference_frame_capabilities: caps.to_vec(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        }
    }

    #[test]
    fn a_split_generation_is_traced_on_every_stage_with_its_rounds_and_drafts() {
        let caps = vec![capability("strata")];
        let plan = crate::pipeline::plan_from_steps(vec![
            step("ksia", 0, 26, &caps),
            step("salem", 27, 47, &caps),
        ]);
        let done = serde_json::json!({"timings": {"draft_n": 310, "draft_n_accepted": 259, "predicted_ms": 11102.0}});
        let mut trace = engine_trace("t".into(), "m", &plan, "strata", &done, 427);
        trace.output_tokens = 427;
        trace.set_generated_tokens(427);
        trace.complete = true;
        trace.refresh_telemetry(12_000);
        assert_eq!(trace.hops.len(), 2);
        assert!(trace.telemetry.missing_peers.is_empty());
        assert_eq!(trace.telemetry.reported_peers, 2);
        assert_eq!(trace.telemetry.status, "complete");
        assert_eq!(trace.telemetry.completeness_percent, 100.0);
        // 427 tokens: the prompt's first, then 167 rounds with their 259 accepted drafts
        assert_eq!(trace.hops[0].decode.reported_spans, 167);
        assert_eq!(trace.hops[1].decode.node_decode_ms, 11102.0);
        assert_eq!(trace.hops[0].decode.node_decode_ms, 0.0);
        assert_eq!(
            trace.hops[1].prefill.executed_engine,
            crate::executor::ExecutionEngine::Plugin
        );
        assert_eq!(
            trace.hops[0].prefill.selected_path.as_deref(),
            Some("plugin/strata")
        );
        assert_eq!(
            (
                trace.mtp_draft_tokens_proposed,
                trace.mtp_draft_tokens_accepted
            ),
            (310, 259)
        );
        assert_eq!(trace.mtp_draft_tokens_rejected, 51);
    }

    #[test]
    fn a_plan_runs_on_a_plugin_engine_when_every_step_is_one_of_its_slices() {
        let caps = vec![capability("strata")];
        let plan = |steps| crate::pipeline::plan_from_steps(steps);
        assert_eq!(
            plugin_of_plan(&plan(vec![step("a", 0, 47, &caps)])).as_deref(),
            Some("strata")
        );
        assert_eq!(
            plugin_of_plan(&plan(vec![
                step("a", 0, 26, &caps),
                step("b", 27, 47, &caps)
            ]))
            .as_deref(),
            Some("strata")
        );
        // a llama.cpp hop, another plugin, a gap or a step entered mid-slice is not
        let llama = vec!["relay/v1-chain".to_owned()];
        assert!(plugin_of_plan(&plan(vec![
            step("a", 0, 26, &caps),
            step("b", 27, 47, &llama)
        ]))
        .is_none());
        let other = vec![capability("other")];
        assert!(plugin_of_plan(&plan(vec![
            step("a", 0, 26, &caps),
            step("b", 27, 47, &other)
        ]))
        .is_none());
        assert!(plugin_of_plan(&plan(vec![
            step("a", 0, 20, &caps),
            step("b", 27, 47, &caps)
        ]))
        .is_none());
        let mut entered = step("b", 20, 47, &caps);
        entered.exec_start = 27;
        assert!(plugin_of_plan(&plan(vec![step("a", 0, 26, &caps), entered])).is_none());
    }

    #[test]
    fn a_stage_receives_its_link() {
        let cfg =
            crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
        assert!(load_params(&cfg, &[0], None).get("link").is_none());
        let link = json!({"token": "t", "prev_port": 4100});
        assert_eq!(
            load_params(&cfg, &[0], Some(&link))["link"]["prev_port"],
            4100
        );
    }

    #[test]
    fn health_says_loaded_only_when_the_plugin_is_ready() {
        let cfg = crate::runtime::NodeConfig {
            model_label: "qwen3.8-flash-next".into(),
            total_layers: 48,
            end_layer: 47,
            n_ctx: 65536,
            deploy_id: "d-1".into(),
            ..crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true)
        };
        let loading = health_body(&cfg, "strata", &json!({"state": "loading"}));
        assert_eq!(loading["model_loaded"], false);
        assert_eq!(loading["n_ctx_per_session"], 65536);
        let ready = health_body(&cfg, "strata", &json!({"state": "ready", "n_ctx": 32768}));
        assert_eq!(ready["model_loaded"], true);
        assert_eq!(ready["n_ctx_per_session"], 32768);
        assert_eq!(ready["end_layer"], 47);
        assert_eq!(ready["total_layers"], 48);
        assert_eq!(ready["capabilities"], json!(["engine/plugin-v1:strata"]));
    }

    #[test]
    fn the_generation_body_carries_the_prompt_flags_and_sampling() {
        let body = generation_body(&EngineRequest {
            prompt: "User: oi".into(),
            chat_template: true,
            thinking_enabled: false,
            max_tokens: 128,
            stops: vec!["</s>".into()],
            request_details: json!({"temperature": 0.2, "top_k": 20, "messages": [], "seed": "x"}),
        });
        assert_eq!(body["prompt"], "User: oi");
        assert_eq!(body["thinking"], false);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_k"], 20);
        assert!(body.get("seed").is_none());
        assert!(body.get("messages").is_none());
    }
}
