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
//! `{"t": answer_text}`, `{"r": reasoning_text}`, `{"p": [read, total]}`,
//! `{"done": {finish, prompt_tokens, completion_tokens, reasoning_tokens, cached_tokens}}` or
//! `{"error": message}`.
//!
//! **Prefill/decode split** ([`prefill_on_peer`]): a whole-model machine may be deployed with the role `prefill`
//! (`RuntimeParams::inference_role`), which it announces as `engine/plugin-role-v1:prefill`; it answers its own
//! requests as any head. Before a named conversation's turn runs on a head with the sessions on (the `shared` role
//! the deploy gives the other machines of the model), the gateway sends the turn to a free prefill
//! machine of the same model with `prefill_only: true`: that machine reads the prompt into a batch slot and saves
//! the conversation's session (or answers that the new part of the prompt is too short to be worth it). The head's
//! turn then names it as `session_source`, and the head fetches that session before it reads its prompt
//! (`plugin_session::prepare_for_body`), so it reads only what the session does not hold and decodes.

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
/// The capability that says a whole-model plugin engine has a prefill/decode role other than both.
pub(crate) const ROLE_CAPABILITY_PREFIX: &str = "engine/plugin-role-v1:";
/// How long the gateway waits for a prefill machine to read a prompt and answer.
const PREFILL_TIMEOUT: Duration = Duration::from_secs(3600);
/// A prompt is the whole conversation; the generation route is not the
/// small-JSON peer route.
pub(crate) const MAX_GENERATION_BODY: usize = crate::openai::PROMPT_BODY_LIMIT;
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

pub(crate) fn role_capability(role: crate::webui::InferenceRole) -> String {
    format!("{ROLE_CAPABILITY_PREFIX}{}", role.as_str())
}

/// The prefill/decode role a peer announces; `Both` when it announces none.
pub(crate) fn role_of_capabilities(capabilities: &[String]) -> crate::webui::InferenceRole {
    use crate::webui::InferenceRole;
    match capabilities
        .iter()
        .find_map(|cap| cap.strip_prefix(ROLE_CAPABILITY_PREFIX))
    {
        Some("prefill") => InferenceRole::Prefill,
        Some("shared") => InferenceRole::Shared,
        _ => InferenceRole::Both,
    }
}

/// The plugin whose engine serves a peer, from its announced capabilities.
pub(crate) fn plugin_of_capabilities(capabilities: &[String]) -> Option<&str> {
    capabilities
        .iter()
        .find_map(|cap| cap.strip_prefix(CAPABILITY_PREFIX))
        .filter(|code| valid_code(code))
}

#[cfg(test)]
mod engine_group_tests {
    use super::*;

    fn peer(id: &str, model: &str, caps: &[&str]) -> crate::network::PeerInfo {
        let mut p = crate::gateway::testkit::peer(id, id, 8080);
        p.model = model.into();
        p.inference_frame_capabilities = caps.iter().map(|c| c.to_string()).collect();
        p
    }

    #[test]
    fn a_pipeline_is_built_on_one_engine_and_llama_comes_first() {
        let plugin = capability("strata");
        let peers = vec![
            peer("gpu1", "m", &[plugin.as_str()]),
            peer("gpu2", "m", &["inference/frame-v1-batch"]),
        ];
        assert_eq!(engine_of_peer(&peers[0]).as_deref(), Some("strata"));
        assert_eq!(engine_of_peer(&peers[1]), None);
        assert_eq!(pipeline_engines(&peers, None), vec![None, Some("strata".to_string())]);
        // a chosen head keeps the pipeline on its own engine
        assert_eq!(pipeline_engines(&peers, Some("gpu1")), vec![Some("strata".to_string())]);
        let llama: Vec<String> = peers_of_engine(&peers, None).into_iter().map(|p| p.node_id).collect();
        assert_eq!(llama, ["gpu2"]);
    }

    #[test]
    fn a_prefill_machine_is_known_by_its_role_capability() {
        use crate::webui::InferenceRole;
        let plugin = capability("strata");
        let prefill = role_capability(InferenceRole::Prefill);
        let shared = role_capability(InferenceRole::Shared);
        assert_eq!(prefill, "engine/plugin-role-v1:prefill");
        assert_eq!(role_of_capabilities(&[plugin, prefill]), InferenceRole::Prefill);
        assert_eq!(role_of_capabilities(&[shared]), InferenceRole::Shared);
        assert_eq!(role_of_capabilities(&[]), InferenceRole::Both);
    }
}

/// The engine a peer runs its models on: its plugin's code, or `None` for llama.cpp (the pipeline engine). A pipeline
/// is built from one engine only: the wire between its stages is that engine's own, so peers of another engine are not
/// hops of it (they can still serve the model whole, as an alternative route).
pub(crate) fn engine_of_peer(peer: &crate::network::PeerInfo) -> Option<String> {
    plugin_of_capabilities(&peer.inference_frame_capabilities).map(str::to_owned)
}

/// The peers of one engine (see [`engine_of_peer`]).
pub(crate) fn peers_of_engine(peers: &[crate::network::PeerInfo], engine: Option<&str>) -> Vec<crate::network::PeerInfo> {
    peers
        .iter()
        .filter(|peer| engine_of_peer(peer).as_deref() == engine)
        .cloned()
        .collect()
}

/// The engines a pipeline may be built on, in the order the planner tries them: the head's engine when a head is
/// already chosen (its pipeline stays on that engine), else every engine that serves a model, llama.cpp first.
pub(crate) fn pipeline_engines(peers: &[crate::network::PeerInfo], head: Option<&str>) -> Vec<Option<String>> {
    if let Some(head) = head {
        return peers
            .iter()
            .find(|peer| peer.node_id == head)
            .map(|peer| vec![engine_of_peer(peer)])
            .unwrap_or_default();
    }
    let mut engines: Vec<Option<String>> = Vec::new();
    for peer in peers.iter().filter(|peer| !peer.model.is_empty()) {
        let engine = engine_of_peer(peer);
        if !engines.contains(&engine) {
            engines.push(engine);
        }
    }
    engines.sort_by_key(|engine| engine.is_some());
    engines
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

/// This node's GPUs for a plugin's engine: `gpus`, the NVIDIA ones by the
/// index the node gives them (what CUDA numbers), and `gpu_devices`, every
/// GPU with its vendor and its position among that vendor's cards - what the
/// vendor's runtime numbers (`HIP_VISIBLE_DEVICES` for an AMD one), since the
/// node counts AMD cards after the NVIDIA ones.
async fn node_gpus() -> (Vec<u32>, Vec<Value>) {
    gpus_of(crate::sysmon::sample().await.gpus)
}

fn gpus_of(gpus: Vec<crate::sysmon::GpuStat>) -> (Vec<u32>, Vec<Value>) {
    let mut nvidia = Vec::new();
    let mut devices = Vec::new();
    let mut counts: std::collections::HashMap<&'static str, u32> = Default::default();
    for gpu in gpus {
        let vendor = match gpu.vendor {
            crate::sysmon::GpuVendor::Nvidia => "nvidia",
            crate::sysmon::GpuVendor::Amd => "amd",
            crate::sysmon::GpuVendor::Apple => "apple",
        };
        if vendor == "nvidia" {
            nvidia.push(gpu.index);
        }
        let ordinal = counts.entry(vendor).or_insert(0);
        devices.push(json!({
            "index": gpu.index,
            "vendor": vendor,
            "ordinal": *ordinal,
            "name": gpu.name,
            "vram_total_mb": gpu.vram_total_mb,
        }));
        *ordinal += 1;
    }
    (nvidia, devices)
}

/// What `engine.load` receives. `link` is set for one stage of a model split
/// across machines ([`crate::plugin_stage_link::prepare`]).
pub(crate) fn load_params(
    cfg: &crate::runtime::NodeConfig,
    gpus: &[u32],
    gpu_devices: &[Value],
    link: Option<&Value>,
) -> Value {
    let mut params = json!({
        "model_path": cfg.model_path,
        "model_label": cfg.model_label,
        "n_ctx": cfg.n_ctx,
        "max_sessions": cfg.max_sessions,
        "kv": kv_of(cfg),
        "gpus": gpus,
        "gpu_devices": gpu_devices,
        "settings": cfg.engine_settings,
    });
    // a whole-model machine's part in a prefill/decode split: the plugin keeps the sessions on for it
    if !cfg.inference_role.is_both() && !crate::plugin_stage_link::is_stage(cfg) {
        params["role"] = json!(cfg.inference_role.as_str());
        if let Some(tokens) = cfg.prefill_min_tokens {
            params["prefill_min_tokens"] = json!(tokens);
        }
    }
    if let Some(link) = link {
        params["link"] = link.clone();
    }
    // The vision projector the deployment chose: the engine reads images with it.
    if let Some(mmproj) = cfg.mmproj_path.as_deref().filter(|path| !path.trim().is_empty()) {
        params["mmproj_path"] = json!(mmproj);
    }
    // the sessions live with the KV snapshots, so the clear of the KV cache empties them too
    if let Some(dir) = cfg.slot_save_path.as_deref().filter(|dir| !dir.trim().is_empty()) {
        params["session_dir"] = json!(std::path::Path::new(dir)
            .join(super::plugin_session::SESSIONS_SUBDIR)
            .display()
            .to_string());
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
    // The route fields the llama.cpp server adds (infer_server `add_route_fields`): the pipeline panel reads the
    // RTT between machines from here, so a plugin engine must say what its node measures as well.
    let book = crate::route_plan::book();
    let mut body = json!({
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
        "capabilities": health_capabilities(cfg, code),
        "engine": {
            "plugin": code,
            "state": status["state"],
            "detail": status["detail"],
            "busy": status["busy"],
        },
    });
    body["route_rtt_ms"] = json!(book.rtt_map(crate::route_plan::Plane::Inference));
    body["gateway_rtt_ms"] = json!(book.rtt_map(crate::route_plan::Plane::Gateway));
    body["route_kinds"] = json!(book.kind_map(crate::route_plan::Plane::Inference));
    body["gateway_route_kinds"] = json!(book.kind_map(crate::route_plan::Plane::Gateway));
    body["relayed_peers"] = json!(book.relayed_nodes());
    body["active_generations"] = json!(super::engine_telemetry::in_flight(code));
    let measured = super::engine_telemetry::perf_for(code);
    body["perf"] = json!({
        "prefill_ms_per_token": measured.prefill_ms_per_token,
        "decode_ms_per_step": measured.decode_ms_per_step,
    });
    body
}

/// The lossy hidden-state formats a plugin engine's machines can exchange: the stage link
/// (`plugin_stage_link`) encodes the engines' tensors with the node's own `wire`, so any of its
/// formats crosses between two slices whatever the plugin is. A whole-model plugin sends no hidden
/// state, and announcing them changes nothing there.
pub(crate) const STAGE_LINK_DTYPES: [&str; 3] = [
    crate::pipeline::CAP_FRAME_DTYPE_F8,
    crate::pipeline::CAP_FRAME_DTYPE_I4G64,
    crate::pipeline::CAP_FRAME_DTYPE_I4G128,
];

fn health_capabilities(cfg: &crate::runtime::NodeConfig, code: &str) -> Vec<String> {
    let mut caps = vec![capability(code)];
    caps.extend(STAGE_LINK_DTYPES.iter().map(|cap| cap.to_string()));
    if !cfg.inference_role.is_both() && !crate::plugin_stage_link::is_stage(cfg) {
        caps.push(role_capability(cfg.inference_role));
    }
    caps
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
    let (gpus, gpu_devices) = node_gpus().await;
    // One stage of a split model: a later stage waits here for the earlier
    // stage's machine and keeps the relay to it while it serves.
    let (link, mut relay) = if crate::plugin_stage_link::is_stage(&cfg) {
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
        json!({ "params": load_params(&cfg, &gpus, &gpu_devices, link.as_ref()) }),
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
    // a later stage whose earlier stage's link secret changed can never link: this generation ends and restarts
    let mut stale_rx = relay.as_mut().and_then(|relay| relay.stale.take());
    let served = crate::serve::serve_with_quic(listener, router, tls, quic, quic_port, async move {
        loop {
            tokio::select! {
                stale = async { stale_rx.as_mut().expect("guarded by is_some").await }, if stale_rx.is_some() => {
                    stale_rx = None;
                    if let Ok(detail) = stale {
                        *failure_seen.lock().unwrap_or_else(|error| error.into_inner()) = Some(detail);
                        break;
                    }
                }
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
    /// The conversation the turn belongs to (`plugin_session`), when the client named one.
    pub session: Option<String>,
    /// The client's request, for its sampling fields.
    pub request_details: Value,
    /// Labels of a typed decision (`/v1/systemone`): the engine answers with their probabilities for the first token.
    pub probe_labels: Vec<String>,
    /// Bytes of the prompt that a typed decision asks again and again (its question): the engine pins the prefix there.
    pub probe_prefix_bytes: Option<usize>,
    /// The turn's images, as `data:image/...;base64,` URLs (see `image_data_url`).
    pub images: Vec<String>,
}

/// An image as a `data:` URL, its type read from the first bytes (JPEG when none is recognized).
pub(crate) fn image_data_url(bytes: &[u8]) -> String {
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/jpeg"
    };
    use base64::Engine as _;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Whether the client chose its thinking itself: the POM's `thinking_enabled`, OpenAI's `reasoning_effort`, `reasoning`
/// or `chat_template_kwargs.enable_thinking`, Anthropic's `thinking`.
fn thinking_asked(details: &Value) -> bool {
    ["thinking_enabled", "reasoning_effort", "reasoning", "thinking"]
        .iter()
        .any(|key| !details[*key].is_null())
        || !details["chat_template_kwargs"]["enable_thinking"].is_null()
}

pub(crate) fn generation_body(request: &EngineRequest) -> Value {
    let mut body = json!({
        "prompt": request.prompt,
        "chat_template": request.chat_template,
        "thinking": request.thinking_enabled,
        "max_tokens": request.max_tokens,
        "stop": request.stops,
    });
    if let Some(session) = &request.session {
        body["session"] = json!(session);
    }
    // a client that chose no thinking level leaves it to the engine's own default (its `reasoning_effort`)
    if !thinking_asked(&request.request_details) {
        body["thinking_default"] = json!(true);
    }
    if !request.images.is_empty() {
        body["images"] = json!(request.images);
    }
    if !request.probe_labels.is_empty() {
        body["probe"] = json!({"labels": request.probe_labels});
        if let Some(bytes) = request.probe_prefix_bytes {
            body["probe"]["prefix_bytes"] = json!(bytes);
        }
    }
    for key in SAMPLING_KEYS {
        let value = &request.request_details[key];
        if value.is_number() {
            body[key] = value.clone();
        }
    }
    body
}

/// `(port, token)` of this node's plugin generation server, while ready.
pub(crate) async fn local_upstream(
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

pub(crate) fn loopback_client() -> &'static reqwest::Client {
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
) -> Result<(reqwest::Response, Option<super::plugin_session::SessionFetch>, Option<u64>), String> {
    // the session this machine fetched for a turn it heads, and how long looking for it took (a remote head reports
    // both in the done line)
    let mut fetched = None;
    let mut lookup_ms = None;
    let response = if node_id == state.network.node_id {
        let (fetch, ms) = super::plugin_session::prepare_timed(state, code, body).await;
        fetched = fetch;
        lookup_ms = Some(ms);
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
        return Ok((response, fetched, lookup_ms));
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
/// Relays the engine's NDJSON answer to the machine that asked for it. At the `done` line it adds the stage links
/// this machine used (its own and the ones below it), so the requester's trace covers every link of the pipeline.
/// The first token moves the links to the decode phase.
fn relay_with_links(
    code: String,
    fetched: Option<Value>,
    lookup_ms: u64,
    upstream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static {
    struct Relay<S> {
        upstream: S,
        code: String,
        fetched: Option<Value>,
        lookup_ms: u64,
        pending: Vec<u8>,
        first_token: bool,
        ended: bool,
    }
    futures::stream::unfold(
        Relay {
            upstream,
            code,
            fetched,
            lookup_ms,
            pending: Vec::new(),
            first_token: false,
            ended: false,
        },
        |mut relay| async move {
            loop {
                if let Some(end) = relay.pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = relay.pending.drain(..=end).collect();
                    let out = rewrite_line(&relay.code, &line, &mut relay.first_token, relay.fetched.as_ref(), relay.lookup_ms);
                    return Some((Ok(Bytes::from(out)), relay));
                }
                if relay.ended {
                    if relay.pending.is_empty() {
                        return None;
                    }
                    let rest = std::mem::take(&mut relay.pending);
                    return Some((Ok(Bytes::from(rest)), relay));
                }
                match relay.upstream.next().await {
                    Some(Ok(chunk)) => relay.pending.extend_from_slice(&chunk),
                    Some(Err(error)) => return Some((Err(error), relay)),
                    None => relay.ended = true,
                }
            }
        },
    )
}

/// One NDJSON line of the engine's answer, with the stage links (and the session this machine fetched for the turn,
/// `session_fetch`, and how long looking for it took, `session_lookup_ms`) added to its `done` event.
fn rewrite_line(code: &str, line: &[u8], first_token: &mut bool, fetched: Option<&Value>, lookup_ms: u64) -> Vec<u8> {
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    let Ok(mut value) = serde_json::from_slice::<Value>(text) else {
        return line.to_vec();
    };
    if (value.get("t").is_some() || value.get("r").is_some()) && !*first_token {
        *first_token = true;
        super::plugin_stage_link::set_phase(code, super::plugin_stage_link::LinkPhase::Decode);
    }
    let Some(done) = value.get_mut("done") else {
        return line.to_vec();
    };
    // The machine that runs the engine measured its speed: its /health publishes it, the card reads it there.
    super::engine_telemetry::record_head_perf(
        code,
        &super::engine_telemetry::EngineReport::from_done(done, 0),
    );
    done["links"] = json!(super::plugin_stage_link::drain_reported_links(code));
    if let Some(fetched) = fetched {
        done["session_fetch"] = fetched.clone();
    }
    done["session_lookup_ms"] = json!(lookup_ms);
    let mut out = serde_json::to_vec(&value).unwrap_or_else(|_| text.to_vec());
    if line.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

fn finish_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "length",
        _ => "stop",
    }
}

/// One generated-text NDJSON object from a plugin engine. Answer text (`t`)
/// and reasoning (`r`) remain distinct from this boundary onward.
fn generated_text_event(event: &Value) -> Option<GenEvent> {
    if let Some(text) = event["t"].as_str().filter(|text| !text.is_empty()) {
        return Some(GenEvent::Token(text.to_owned()));
    }
    event["r"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(|text| GenEvent::Reasoning(text.to_owned()))
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
    // In flight on the serving node until the stream ends: the dashboard's load and the planner's queue read it,
    // as they read the llama.cpp path's (`infer_server`).
    let _in_flight = state.loads.acquire(node_id).await;
    let mut body = generation_body(&request);
    let session_key = request.session.clone();
    let mut prefill_leg: Option<PrefillLeg> = None;
    // Prefill/decode split: a prefill machine of this model reads the prompt first, and the head fetches the
    // session it saved. Any problem leaves the head to read the prompt itself.
    if plan.steps.len() == 1 && request.images.is_empty() && request.probe_labels.is_empty() {
        if let Some(key) = &session_key {
            if let Some(source) = prefill_on_peer(
                state,
                code,
                model,
                node_id,
                key,
                &body,
                cancellation,
            )
            .await
            {
                // its pieces are in the head's slot already; else the head fetches the session it saved
                if source.fetch.is_some() {
                    body["session_pieces"] = json!(true);
                } else {
                    body["session_source"] = json!(source.node_id);
                }
                prefill_leg = Some(source);
            }
            if cancellation.is_cancelled() || tx.is_closed() {
                return;
            }
        }
    }
    // Totals left by an earlier generation that never reached its end belong to nobody now: drop them, and the
    // frames of this one are the prompt's until the first token.
    let _ = super::plugin_stage_link::take_link_stats(code);
    let _ = super::plugin_stage_link::take_channel_stats(code);
    let _ = super::plugin_stage_link::take_own_wait(code, super::plugin_stage_link::LinkPhase::Prefill);
    let _ = super::plugin_stage_link::take_own_wait(code, super::plugin_stage_link::LinkPhase::Decode);
    super::plugin_stage_link::set_phase(code, super::plugin_stage_link::LinkPhase::Prefill);
    let channel = super::plugin_stage_link::channel_of(node_id, &state.network.node_id);
    let leg_started = Instant::now();
    let (response, fetched_here, lookup_here) = match open_stream(state, node_id, code, &body).await {
        Ok(opened) => opened,
        Err(error) => {
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        }
    };
    // The gateway's own leg to the head: the time until the head answers with its headers, the request's bytes.
    super::plugin_stage_link::record_leg(
        code,
        node_id,
        super::plugin_stage_link::LinkPhase::Prefill,
        leg_started.elapsed(),
        body.to_string().len() as u64,
        0,
        0,
        channel,
    );
    // A generation this machine serves is in flight here; the head's stream is counted on the head.
    let mut stream = if node_id == state.network.node_id {
        Box::pin(super::engine_telemetry::track(code, response.bytes_stream()))
            as std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>
    } else {
        Box::pin(response.bytes_stream())
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut received_bytes = 0u64;
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
        received_bytes += chunk.len() as u64;
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let Ok(event) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
                continue;
            };
            if let Some(output) = generated_text_event(&event) {
                if first_token_at.is_none() {
                    super::plugin_stage_link::set_phase(code, super::plugin_stage_link::LinkPhase::Decode);
                }
                first_token_at.get_or_insert_with(Instant::now);
                streamed_any = true;
                pieces += 1;
                if tx.send(output).await.is_err() {
                    return;
                }
            } else if let Some(done) = event.get("done") {
                let total_ms = request_started.elapsed().as_millis() as u64;
                // The plugin's basic data; everything else about the generation is measured here.
                // The turn ran on this machine: its conversation goes on to the peers from a background task
                // that waits for a deferred save (a remote head does this itself).
                // the head that ran the turn holds the conversation's slot: its next turn goes back there
                if let Some(key) = &session_key {
                    super::plugin_session::note_holder(model, key, node_id);
                }
                // and what it read: a new conversation that begins with most of this prompt goes there too
                super::plugin_session::note_prompt_start(model, &request.prompt, node_id);
                if super::plugin_session::PUSH_SYNC && node_id == state.network.node_id {
                    if let Some(key) = super::plugin_session::saved_session(done) {
                        super::plugin_session::spawn_sync(state.clone(), code.to_owned(), key);
                    }
                }
                let report = super::engine_telemetry::EngineReport::from_done(done, pieces);
                let first = first_token_at.unwrap_or_else(Instant::now);
                let ttft_ms = first.saturating_duration_since(request_started).as_millis() as u64;
                let engine_prefill_ms = report.prefill_ms.map(|ms| ms.round() as u64);
                let generated = report.generated;
                let reasoning_tokens = done["reasoning_tokens"]
                    .as_u64()
                    .map(|tokens| tokens.min(u64::from(generated)) as u32)
                    .unwrap_or(0);
                let prompt_tokens = report.prompt_tokens;
                let cached_tokens = report.cached_tokens;
                // a prefill/decode split: the prompt was read on the prefill machine, the head read only the rest
                // after restoring its session, so the prompt phase is both (the session's transfer included)
                // the session the head fetched before it read the rest: here, or in a remote head's done line
                let fetched = fetched_here
                    .clone()
                    .or_else(|| prefill_leg.as_ref().and_then(|leg| leg.fetch.clone()))
                    .or_else(|| super::plugin_session::SessionFetch::from_json(&done["session_fetch"]));
                let fetch_ms = fetched.as_ref().map_or(0, |fetch| fetch.transfer_ms + fetch.save_wait_ms);
                let (prefill_ms, prompt_read) = match &prefill_leg {
                    Some(leg) => (
                        engine_prefill_ms.unwrap_or(0) + leg.elapsed_ms + fetch_ms,
                        prompt_tokens
                            .saturating_sub(cached_tokens)
                            .saturating_add(leg.tokens.min(u64::from(u32::MAX)) as u32),
                    ),
                    None => (engine_prefill_ms.unwrap_or(ttft_ms), prompt_tokens.saturating_sub(cached_tokens)),
                };
                let mut timings = crate::gateway::timing_metrics(
                    total_ms,
                    ttft_ms,
                    prefill_ms,
                    prompt_read,
                    generated,
                );
                timings.server_ttft_ms = Some(ttft_ms);
                if let Some(rate) = timings.prefill_tokens_per_sec {
                    super::plugin_session::note_prefill_rate(rate);
                }
                timings.engine_ttft_ms = engine_prefill_ms;
                let finish = report.finish.clone();
                // The same record a one-hop llama.cpp pipeline leaves: the request audit, the statistics and the
                // logs the admins receive read the source and this trace, whatever the engine.
                // the prefill machine of a split is the first hop of the trace, before the head that decodes
                let shown_plan = match prefill_leg.as_ref().and_then(|leg| leg.step.clone()) {
                    Some(step) => crate::pipeline::plan_from_steps(
                        std::iter::once(step).chain(plan.steps.iter().cloned()).collect(),
                    ),
                    None => plan.clone(),
                };
                let mut trace = super::engine_telemetry::build_trace(
                    trace_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
                    model,
                    &shown_plan,
                    code,
                    &report,
                );
                // The gateway's leg to the head while it decodes: from the first token to the end of the stream. The
                // head's own decode time is the part it served; the rest is the wire. The bytes are the whole stream.
                if let Some(first) = first_token_at {
                    super::plugin_stage_link::record_leg(
                        code,
                        node_id,
                        super::plugin_stage_link::LinkPhase::Decode,
                        first.elapsed(),
                        0,
                        received_bytes,
                        report.decode_ms.unwrap_or(0.0).round() as u64,
                        channel,
                    );
                }
                // Looking for the conversation's session on the head happens before its engine reads the prompt; without
                // its own span the gap between the request and the prefill reads as nothing.
                // Then the engine's bridge, before the engine reads the prompt: the template, a saved session restored, and
                // the admission (a parked conversation copied into its slot), which the engine's own prompt time leaves out.
                let request_epoch_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |now| now.as_millis() as u64)
                    .saturating_sub(request_started.elapsed().as_millis() as u64);
                let mut offset_ms = leg_started.saturating_duration_since(request_started).as_millis() as u64;
                let lookup_ms = lookup_here.or_else(|| done["session_lookup_ms"].as_u64());
                let bridge = &done["pom_timings"];
                let admission_ms = bridge["first_token_ms"]
                    .as_f64()
                    .map(|first| (first - report.prefill_ms.unwrap_or(0.0)).max(0.0));
                // A session fetched from a peer is the lookup's last part: the source's save waited for, then the
                // files copied. Each gets its own span, so a few hundred MB between two machines shows where it went.
                let (save_wait_ms, transfer_ms) =
                    fetched.as_ref().map_or((0, 0), |fetch| (fetch.save_wait_ms, fetch.transfer_ms));
                for (name, ms) in [
                    (
                        "session_lookup",
                        lookup_ms.map(|ms| ms.saturating_sub(save_wait_ms + transfer_ms) as f64),
                    ),
                    ("session_save_wait", (save_wait_ms > 0).then_some(save_wait_ms as f64)),
                    ("session_transfer", fetched.as_ref().map(|_| transfer_ms as f64)),
                    ("bridge_prepare", bridge["prepare_ms"].as_f64()),
                    ("session_restore", bridge["restore_ms"].as_f64()),
                    ("engine_admission", admission_ms),
                ] {
                    let Some(ms) = ms.map(|ms| ms.round() as u64) else { continue };
                    trace.spans.push(crate::prefill::TraceSpan::new(
                        name,
                        node_id.get(..8).unwrap_or(node_id),
                        None,
                        request_epoch_ms + offset_ms,
                        offset_ms,
                        ms,
                        None,
                    ));
                    offset_ms += ms;
                }
                super::engine_telemetry::merge_reported_links(code, done);
                super::engine_telemetry::apply_stage_links(&mut trace, code, engine_prefill_ms.unwrap_or(prefill_ms));
                if let Some(leg) = &prefill_leg {
                    if let Some(hop) = trace.hops.iter_mut().find(|hop| hop.node_id == leg.node_id) {
                        // it read the prompt and decoded nothing; what it sent is the session the head fetched
                        hop.prefill.processing_ms = leg.engine_ms;
                        hop.prefill.round_trip_ms = leg.elapsed_ms;
                        hop.prefill.network_ms = leg.elapsed_ms.saturating_sub(leg.engine_ms);
                        if let Some(fetch) = fetched.as_ref().filter(|fetch| fetch.source == leg.node_id) {
                            hop.prefill.network_ms += fetch.transfer_ms;
                            hop.prefill.round_trip_ms += fetch.transfer_ms + fetch.save_wait_ms;
                            hop.prefill.bytes_sent += fetch.bytes;
                        }
                        hop.decode.calls = 0;
                        hop.decode.reported_spans = 0;
                        hop.decode.processing_ms = 0;
                    }
                }
                // The speed belongs to the machine that runs the engine; a remote head records it on its own /health.
                if node_id == state.network.node_id {
                    super::engine_telemetry::record_head_perf(code, &report);
                }
                if let Some(fetch) = &fetched {
                    trace.total_bytes_sent += fetch.bytes;
                    trace.session_transfer = Some(crate::prefill::SessionTransferTrace::new(
                        &fetch.source,
                        node_id,
                        fetch.tokens,
                        fetch.bytes,
                        fetch.transfer_ms,
                        fetch.save_wait_ms,
                    ));
                }
                trace.input_tokens = prompt_tokens;
                trace.cached_tokens = cached_tokens;
                trace.prefix_match_tokens = cached_tokens;
                trace.prefill_ms = prefill_ms;
                trace.server_ttft_ms = Some(ttft_ms);
                trace.engine_ttft_ms = engine_prefill_ms;
                trace.output_tokens = generated;
                trace.set_generated_tokens(generated);
                trace.complete = true;
                // a typed decision's label probabilities, from the engine's done line (`probe`), as a llama.cpp peer reports
                trace.probe = done.get("probe").filter(|value| !value.is_null()).cloned();
                // A split engine's link keeps up to the deployment's in-flight frames posted (plugin_stage_link),
                // not the llama.cpp prefill's window this field otherwise reports; its tensors cross in the
                // deployment's hidden-state precision.
                if plan.steps.len() > 1 {
                    if let Some(cfg) = state.runtime.config().await {
                        trace.prefill_max_inflight_chunks =
                            super::plugin_stage_link::max_in_flight(&cfg) as u32;
                        for hop in &mut trace.hops {
                            for part in [&mut hop.prefill, &mut hop.decode] {
                                let crossed =
                                    part.hidden_state_bytes_sent + part.hidden_state_bytes_received;
                                if crossed > 0 {
                                    part.hidden_state_dtype = Some(cfg.hidden_state_dtype);
                                }
                            }
                        }
                    }
                }
                trace.refresh_telemetry(total_ms);
                info!(
                    plugin = %code,
                    node_id = %node_id,
                    prompt_tokens,
                    cached_tokens,
                    generated,
                    total_ms,
                    finish,
                    "generation completed by the plugin inference service"
                );
                let summary = GenSummary {
                    source: format!("pipeline:{}", shown_plan.hop_chain()),
                    tokens_generated: generated,
                    reasoning_tokens,
                    tokens_per_sec: timings.generation_tokens_per_sec.unwrap_or(0.0),
                    duration_ms: total_ms,
                    timings,
                    partial: false,
                    error: None,
                    finish_reason: Some(finish_reason(&finish).to_owned()),
                    stop_sequence: report.stop_sequence.clone(),
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

// ── Prefill/decode split ─────────────────────────────────────────────────────

/// The prefill machine for a turn of `model` whose head is `head`: a live peer of the same plugin engine, announced
/// with the role `prefill`, that serves the whole model. The one the client chose (`preferred`) when it is one of
/// them, else the least busy one.
async fn prefill_peer(
    state: &GatewayState,
    code: &str,
    model: &str,
    head: &str,
) -> Option<crate::network::PeerInfo> {
    use crate::webui::InferenceRole;
    let peers = state.all_peers().await;
    let perf = state.admission_health.perf_snapshot();
    let mut load_of = std::collections::HashMap::new();
    for peer in &peers {
        let live = state.loads.load(&peer.node_id).await;
        let reported = perf.get(&peer.node_id).map(|p| p.active_generations).unwrap_or(0);
        load_of.insert(peer.node_id.clone(), live.max(reported));
    }
    let head_role = peers
        .iter()
        .find(|peer| peer.node_id == head)
        .map(|peer| role_of_capabilities(&peer.inference_frame_capabilities))
        .unwrap_or(InferenceRole::Both);
    // the head's load counts this turn (`generate` holds it in flight): more than one is another request on it. A busy
    // head reads its own prompt: it applies a piece only while none of its batch slots decodes, so it would refuse
    // what another machine read (a free machine takes new requests as their head instead, `auto_head`). Only a head
    // with the sessions on (`Shared`) can restore what a prefill machine read; a prefill head reads its own prompt.
    if load_of.get(head).copied().unwrap_or(0) > 1 || head_role != InferenceRole::Shared {
        return None;
    }
    let mut best: Option<(f64, crate::network::PeerInfo)> = None;
    for peer in peers {
        let whole = peer.total_layers > 0 && peer.start_layer == 0 && peer.end_layer + 1 == peer.total_layers;
        let role = role_of_capabilities(&peer.inference_frame_capabilities);
        // an idle prefill machine with the same plugin and the whole model
        if peer.node_id == head
            || peer.model != model
            || !whole
            || engine_of_peer(&peer).as_deref() != Some(code)
            || role != InferenceRole::Prefill
            || load_of.get(&peer.node_id).copied().unwrap_or(0) > 0
        {
            continue;
        }
        // the fastest at reading prompts (unmeasured last)
        let speed = perf
            .get(&peer.node_id)
            .and_then(|p| p.prefill_ms_per_token)
            .unwrap_or(f64::INFINITY);
        if best.as_ref().is_none_or(|(best_speed, _)| speed < *best_speed) {
            best = Some((speed, peer));
        }
    }
    best.map(|(_, peer)| peer)
}

/// What a prefill machine did for a turn: who it is (its step, for the trace), the tokens it read and how long its
/// engine took, and the whole call from this gateway.
pub(crate) struct PrefillLeg {
    pub node_id: String,
    pub step: Option<crate::pipeline::PipelineStep>,
    pub tokens: u64,
    pub engine_ms: u64,
    pub elapsed_ms: u64,
    /// The prompt read in pieces, each applied to the head while the next was read: what the head copied, with only
    /// the wait after the prompt's end as its transfer time (the rest overlapped the read). `None`: one session, which
    /// the head fetches before its turn (`session_source`).
    pub fetch: Option<super::plugin_session::SessionFetch>,
}

/// Runs the prompt of a turn on a prefill machine (`prefill_only`), which saves the conversation's session there.
/// Returns what that machine did when it saved it, so the head fetches it (`session_source`); `None` when there is
/// no prefill machine, the new part of the prompt was too short to be worth the transfer, or anything failed.
#[allow(clippy::too_many_arguments)]
async fn prefill_on_peer(
    state: &GatewayState,
    code: &str,
    model: &str,
    head: &str,
    key: &str,
    body: &Value,
    cancellation: &crate::prefill::Cancellation,
) -> Option<PrefillLeg> {
    let peer = prefill_peer(state, code, model, head).await?;
    // what the head already holds of the conversation: the prefill machine reads the prompt only when the part the
    // head lacks is long enough
    let head_offer = super::plugin_session::offer_of(state, code, key, head).await;
    let holds = match &head_offer {
        Ok(offer) => offer.tokens.max(offer.ram_tokens),
        Err(error) => {
            info!(plugin = %code, session = %key, head = %head, "prefill split: no offer from the head: {error}");
            0
        }
    };
    // a session saved with other engine settings than the head's, or on a layer split the head's engine cannot restore
    // (a whole model's session on a head split across GPUs), is refused there: the head reads the prompt
    if let Ok(head_offer) = &head_offer {
        if let Ok(prefill_offer) = super::plugin_session::offer_of(state, code, key, &peer.node_id).await {
            if !super::plugin_session::layers_compatible(&head_offer.layers, &prefill_offer.layers) {
                warn!(
                    plugin = %code, session = %key, prefill = %peer.node_id, head = %head,
                    prefill_layers = %prefill_offer.layers, head_layers = %head_offer.layers,
                    "prefill split: the head cannot restore a session of the prefill machine's layer split; the head reads the prompt"
                );
                return None;
            }
            if !super::plugin_session::configs_compatible(head_offer.config.as_deref(), prefill_offer.config.as_deref()) {
                warn!(
                    plugin = %code, session = %key, prefill = %peer.node_id, head = %head,
                    prefill_config = prefill_offer.config.as_deref().unwrap_or("?"),
                    head_config = head_offer.config.as_deref().unwrap_or("?"),
                    "prefill split: the prefill machine runs other engine settings than the head; the head reads the prompt"
                );
                return None;
            }
        }
    }
    let mut prefill = body.clone();
    prefill["max_tokens"] = json!(1);
    prefill["prefill_only"] = json!(true);
    prefill["decode_holds"] = json!(holds);
    // read in pieces: the head applies each one while the next is read (a bridge without pieces ignores the flag and
    // answers with one session, which the head fetches as before)
    prefill["pieces"] = json!(true);
    let (piece_tx, mut piece_rx) = mpsc::unbounded_channel::<(String, String)>();
    let pieces = {
        let (state, code, head, source, key) =
            (state.clone(), code.to_owned(), head.to_owned(), peer.node_id.clone(), key.to_owned());
        tokio::spawn(async move {
            let (mut bytes, mut applied) = (0u64, 0u64);
            while let Some((piece, kind)) = piece_rx.recv().await {
                bytes += super::plugin_session::apply_piece_on(&state, &code, &head, &source, &key, &piece, &kind)
                    .await
                    .map_err(|error| format!("{piece}: {error}"))?;
                applied += 1;
            }
            Ok::<(u64, u64), String>((bytes, applied))
        })
    };
    let _busy = state.loads.acquire(&peer.node_id).await;
    let started = Instant::now();
    let response = match open_stream(state, &peer.node_id, code, &prefill).await {
        Ok((response, _, _)) => response,
        Err(error) => {
            warn!(plugin = %code, session = %key, prefill = %peer.node_id, "prefill split: the prefill machine refused: {error}");
            return None;
        }
    };
    let mut stream = response.bytes_stream();
    let mut pending: Vec<u8> = Vec::new();
    let deadline = Instant::now() + PREFILL_TIMEOUT;
    loop {
        if cancellation.is_cancelled() || Instant::now() > deadline {
            return None;
        }
        let chunk = match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
            Err(_) => continue,
            Ok(None) => {
                warn!(plugin = %code, session = %key, prefill = %peer.node_id, "prefill split: the prefill machine closed without an answer");
                return None;
            }
            Ok(Some(Err(error))) => {
                warn!(plugin = %code, session = %key, prefill = %peer.node_id, "prefill split: the stream broke: {error}");
                return None;
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let Ok(event) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
                continue;
            };
            if let Some(error) = event["error"].as_str() {
                warn!(plugin = %code, session = %key, prefill = %peer.node_id, "prefill split: the prefill machine failed: {error}");
                return None;
            }
            if let Some(piece) = event.get("piece") {
                if let (Some(name), Some(kind)) = (piece["key"].as_str(), piece["kind"].as_str()) {
                    let _ = piece_tx.send((name.to_owned(), kind.to_owned()));
                }
                continue;
            }
            let Some(done) = event.get("done") else {
                continue;
            };
            let read = done["prefill"]["tokens"].as_u64().unwrap_or(0);
            let announced = done["prefill"]["pieces"].as_u64().unwrap_or(0);
            if announced > 0 {
                // the prompt is read: the head applies the pieces still in flight, and only that wait is the turn's
                let engine_ms = super::engine_telemetry::EngineReport::from_done(done, 0)
                    .prefill_ms
                    .map(|ms| ms.round() as u64)
                    .unwrap_or(0);
                let elapsed_ms = started.elapsed().as_millis() as u64;
                drop(piece_tx);
                let tail = Instant::now();
                let bytes = match pieces.await {
                    Ok(Ok((bytes, applied))) if applied == announced => bytes,
                    Ok(Ok((_, applied))) => {
                        warn!(plugin = %code, session = %key, prefill = %peer.node_id, applied, announced, "prefill split: the head did not get every piece; the head reads the prompt");
                        return None;
                    }
                    Ok(Err(error)) => {
                        warn!(plugin = %code, session = %key, prefill = %peer.node_id, head = %head, "prefill split: a piece was not applied on the head: {error}; the head reads the prompt");
                        return None;
                    }
                    Err(error) => {
                        warn!(plugin = %code, session = %key, "prefill split: the pieces' task failed: {error}");
                        return None;
                    }
                };
                let tail_ms = tail.elapsed().as_millis() as u64;
                info!(
                    plugin = %code, session = %key, prefill = %peer.node_id, head = %head, tokens = read, holds,
                    pieces = announced, bytes, ms = elapsed_ms, engine_ms, tail_ms,
                    "prefill split: the prompt was read in pieces on the prefill machine; the head holds them"
                );
                return Some(PrefillLeg {
                    node_id: peer.node_id.clone(),
                    step: crate::pipeline::step_for(&peer, 0),
                    tokens: read,
                    engine_ms,
                    elapsed_ms,
                    fetch: Some(super::plugin_session::SessionFetch {
                        source: peer.node_id.clone(),
                        tokens: done["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(1),
                        bytes,
                        transfer_ms: tail_ms,
                        save_wait_ms: 0,
                    }),
                });
            }
            if let Some(reason) = done["prefill"]["skipped"].as_str() {
                info!(plugin = %code, session = %key, prefill = %peer.node_id, holds, "prefill split: skipped ({reason}); the head reads the prompt");
                return None;
            }
            if super::plugin_session::saved_session(done).as_deref() != Some(key) {
                info!(plugin = %code, session = %key, prefill = %peer.node_id, "prefill split: the prefill machine kept no session; the head reads the prompt");
                return None;
            }
            let report = super::engine_telemetry::EngineReport::from_done(done, 0);
            let engine_ms = report.prefill_ms.map(|ms| ms.round() as u64).unwrap_or(0);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            info!(
                plugin = %code, session = %key, prefill = %peer.node_id, head = %head, tokens = read, holds,
                ms = elapsed_ms, engine_ms,
                "prefill split: the prompt was read on the prefill machine; the head fetches its session"
            );
            return Some(PrefillLeg {
                node_id: peer.node_id.clone(),
                step: crate::pipeline::step_for(&peer, 0),
                tokens: read,
                engine_ms,
                elapsed_ms,
                fetch: None,
            });
        }
    }
}

// ── Deployment ───────────────────────────────────────────────────────────────

/// The plugin services a machine of the network publishes.
pub(crate) async fn services_of(state: &GatewayState, node_id: &str) -> Option<Vec<Value>> {
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
/// cannot. A whole model goes to one machine or to several, each with its role
/// (`roles`, in the order of `targets`; the deploy gives `Shared` to the ones
/// left at `Both`, see `InferenceRole::shared`); layer
/// slices go one per machine, to a service that publishes `stages` for the
/// model's `architecture` (its `stage_architectures`, see [`runs_slices`]).
/// Every machine runs the plugin with an inference service that is not in error.
pub(crate) async fn deploy_block(
    state: &GatewayState,
    service: &str,
    whole: bool,
    targets: &[String],
    roles: &[crate::webui::InferenceRole],
    architecture: Option<&str>,
) -> Option<String> {
    if !valid_code(service) {
        return Some(format!("invalid inference service: {service}"));
    }
    let named: Vec<&String> = targets.iter().filter(|id| !id.is_empty()).collect();
    let mut unique = named.clone();
    unique.sort();
    unique.dedup();
    if unique.is_empty() {
        return Some("choose the machine that runs the inference service".into());
    }
    // several machines with the whole model share the conversations as sessions: the deploy gives them a role that
    // keeps the sessions on (`InferenceRole::shared`), every one of them answers, and the gateway balances the
    // prompts between them
    if whole && unique.len() != named.len() {
        return Some(format!(
            "the inference service {service} runs the whole model once per machine: choose different machines"
        ));
    }
    if !whole && unique.len() != named.len() {
        return Some(format!(
            "the inference service {service} runs one slice per machine: choose different machines"
        ));
    }
    // A sliced deployment's prefill machine runs the whole model beside the stages (the planner checks it is the
    // whole model): it needs the service, not its stages.
    let prefill_only = |target: &String| {
        targets
            .iter()
            .zip(roles)
            .filter(|(id, _)| *id == target)
            .all(|(_, role)| *role == crate::webui::InferenceRole::Prefill)
            && roles.len() == targets.len()
    };
    let several = unique.len() > 1;
    for target in unique {
        let Some(services) = services_of(state, target).await else {
            return Some(format!(
                "machine {target} did not answer which services it offers"
            ));
        };
        let offered = services
            .iter()
            .find(|entry| entry["plugin_code"] == service && entry["kind"] == "inference");
        match offered {
            None => {
                return Some(format!(
                    "machine {target} does not have the plugin {service} with the inference service active"
                ))
            }
            Some(entry) if entry["status"] == "error" || entry["status"] == "waiting" => {
                return Some(format!(
                    "the inference service {service} of machine {target} is not ready: {}",
                    entry["detail"].as_str().unwrap_or("no detail")
                ))
            }
            // several machines with the whole model hand conversations over as sessions, which the service turns on
            // through the role it declares among its POM settings
            Some(entry)
                if whole
                    && several
                    && !entry["pom_settings"]
                        .as_array()
                        .is_some_and(|settings| settings.iter().any(|setting| setting == "inference_role")) =>
            {
                return Some(format!(
                    "the inference service {service} runs the whole model on a single machine: choose one machine"
                ))
            }
            Some(entry) if !whole && !prefill_only(target) && !runs_slices(entry, architecture) => {
                return Some(match architecture {
                    Some(architecture) if entry["stages"] == true => format!(
                        "the inference service {service} runs {architecture} models as the whole model only: choose \"whole model\""
                    ),
                    _ => format!("the inference service {service} serves the whole model: choose \"whole model\""),
                })
            }
            Some(_) => {}
        }
    }
    None
}

/// Whether a machine's inference service (an entry of its `plugin_services`) runs a model of `architecture` as
/// layer slices: it publishes `stages`, and its `stage_architectures`, when it lists any, name that architecture. An
/// architecture the catalog could not read is left to the engine, which refuses a model it cannot run.
pub(crate) fn runs_slices(entry: &Value, architecture: Option<&str>) -> bool {
    if entry["stages"] != true {
        return false;
    }
    match (entry["stage_architectures"].as_array(), architecture) {
        (Some(list), Some(architecture)) if !list.is_empty() => {
            list.iter().any(|candidate| candidate.as_str() == Some(architecture))
        }
        _ => true,
    }
}

// ── Route between nodes ──────────────────────────────────────────────────────

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// `POST /api/internal/plugins/:code/engine/generate`: a gateway of this
/// network runs one generation on this node's plugin engine. Only a signed
/// call from a live peer gets through.
/// The checks every internal plugin-engine route makes: a signed call from a live peer, for a plugin this machine
/// serves as its inference service. The refusal is the response.
pub(crate) async fn check_peer_call(
    auth: &crate::auth::InternalNodeAuth,
    state: &GatewayState,
    code: &str,
) -> Result<(), Response> {
    let Some(principal) = auth.principal() else {
        return Err(refuse(StatusCode::FORBIDDEN, "a signed peer call is required"));
    };
    if !valid_code(code) {
        return Err(refuse(StatusCode::BAD_REQUEST, "invalid plugin"));
    }
    if !state
        .network
        .alive_peers()
        .await
        .iter()
        .any(|peer| peer.node_id == principal.node_id)
    {
        return Err(refuse(StatusCode::FORBIDDEN, "the caller is not a live peer"));
    }
    let served = state
        .runtime
        .config()
        .await
        .and_then(|cfg| cfg.inference_service)
        .is_some_and(|service| service == code);
    if !served {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this machine does not serve a model with that inference service",
        ));
    }
    Ok(())
}

pub(crate) async fn engine_generate_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = check_peer_call(&auth, &state, &code).await {
        return response;
    }
    if body.len() > MAX_GENERATION_BODY {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    // The gateway that took the request marked only its own runtime; without
    // this the serving node counts none of its work and unloads mid-session.
    state.runtime.note_request();
    // This machine's stage links of the generation start from nothing; its prompt phase begins now.
    let _ = super::plugin_stage_link::take_link_stats(&code);
    let _ = super::plugin_stage_link::take_channel_stats(&code);
    let _ = super::plugin_stage_link::take_own_wait(&code, super::plugin_stage_link::LinkPhase::Prefill);
    let _ = super::plugin_stage_link::take_own_wait(&code, super::plugin_stage_link::LinkPhase::Decode);
    super::plugin_stage_link::set_phase(&code, super::plugin_stage_link::LinkPhase::Prefill);
    // a remote head's turn: the session it lacks is fetched from a peer here, before the turn reads its prompt
    let turn = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let (fetched, lookup_ms) = super::plugin_session::prepare_timed(&state, &code, &turn).await;
    let fetched = fetched.map(|fetch| fetch.to_json());
    let response = match post_local(&state.plugins, &code, body.to_vec()).await {
        Ok(response) => response,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let stream = relay_with_links(
        code.clone(),
        fetched,
        lookup_ms,
        Box::pin(super::engine_telemetry::track(&code, response.bytes_stream())),
    );
    // This machine ran the turn: when its bridge reports the conversation saved, the turn goes on to the peers.
    let (watch_state, watch_code) = (state.clone(), code.clone());
    let mut pending: Vec<u8> = Vec::new();
    let stream = stream.inspect(move |item| {
        let Ok(bytes) = item else {
            return;
        };
        pending.extend_from_slice(bytes);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            if let Some(key) = serde_json::from_slice::<Value>(&line)
                .ok()
                .and_then(|value| super::plugin_session::saved_session(&value["done"]))
            {
                if super::plugin_session::PUSH_SYNC {
                    super::plugin_session::spawn_sync(watch_state.clone(), watch_code.clone(), key);
                }
            }
        }
    });
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
    fn plugin_reasoning_ndjson_stays_distinct_from_answer_text() {
        assert!(matches!(
            generated_text_event(&json!({"r": "checking"})),
            Some(GenEvent::Reasoning(text)) if text == "checking"
        ));
        assert!(matches!(
            generated_text_event(&json!({"t": "done"})),
            Some(GenEvent::Token(text)) if text == "done"
        ));
        assert!(generated_text_event(&json!({"r": ""})).is_none());
    }

    #[test]
    fn a_service_runs_slices_only_of_its_stage_architectures() {
        let strata = json!({"stages": true, "stage_architectures": ["qwen4exp"]});
        assert!(runs_slices(&strata, Some("qwen4exp")));
        // Qwen3.6 runs on Strata as the whole model only
        assert!(!runs_slices(&strata, Some("qwen35moe")));
        // an architecture the catalog could not read is left to the engine
        assert!(runs_slices(&strata, None));
        // no list: every architecture the service runs also runs in slices
        assert!(runs_slices(&json!({"stages": true}), Some("qwen35moe")));
        assert!(!runs_slices(&json!({"stages": false, "stage_architectures": ["qwen4exp"]}), Some("qwen4exp")));
    }

    #[tokio::test]
    async fn prefill_machines_answer_too_and_several_need_no_role() {
        use crate::webui::InferenceRole;
        let state = crate::gateway::testkit::state_with("me", vec![]).await;
        // every machine of the split answers: prefill machines alone are a deployment (the machines unknown here are
        // the only refusal left)
        for roles in [vec![InferenceRole::Prefill], vec![InferenceRole::Prefill, InferenceRole::Prefill]] {
            let targets: Vec<String> = ["a", "b"][..roles.len()].iter().map(|id| id.to_string()).collect();
            let reason = deploy_block(&state, "strata", true, &targets, &roles, None).await.expect("unknown machines");
            assert!(reason.contains("did not answer which services"), "{reason}");
        }
        // two machines with the whole model and no role are a deployment too (the deploy gives them `Shared`)
        let reason = deploy_block(
            &state,
            "strata",
            true,
            &["a".into(), "b".into()],
            &[InferenceRole::Both, InferenceRole::Both],
            None,
        )
        .await
        .expect("unknown machines");
        assert!(reason.contains("did not answer which services"), "{reason}");
    }

    #[tokio::test]
    async fn a_prompt_goes_to_an_idle_machine_only_when_the_balance_calls_for_it() {
        use crate::webui::InferenceRole;
        let whole = |id: &str, role: InferenceRole| {
            let mut p = crate::gateway::testkit::peer(id, id, 8080);
            p.model = "m".into();
            p.inference_frame_capabilities = vec![capability("strata"), role_capability(role)];
            (p.start_layer, p.end_layer, p.total_layers) = (0, 31, 32);
            p
        };
        let state = crate::gateway::testkit::state_with(
            "me",
            vec![
                whole("head", InferenceRole::Shared),
                whole("fast", InferenceRole::Prefill),
                whole("other", InferenceRole::Shared),
                whole("pre2", InferenceRole::Prefill),
                whole("plain", InferenceRole::Both),
            ],
        )
        .await;
        let picked = |head: &'static str| {
            let state = state.clone();
            async move { prefill_peer(&state, "strata", "m", head).await.map(|p| p.node_id) }
        };
        {
            // this turn is in flight on its head (`generate` holds it)
            let _turn = state.loads.acquire("head").await;
            // a free decode head hands its prompt to an idle prefill machine, never to another decode machine
            assert!(matches!(picked("head").await.as_deref(), Some("fast" | "pre2")));
            let _fast = state.loads.acquire("fast").await;
            let _pre2 = state.loads.acquire("pre2").await;
            assert_eq!(picked("head").await, None, "no idle prefill machine: the free head reads it");
            drop((_fast, _pre2));
            // a busy head reads its own prompt: it would refuse the pieces while a slot decodes
            let _another = state.loads.acquire("head").await;
            assert_eq!(picked("head").await, None);
        }
        // a prefill head reads its own prompt, free or busy
        let _turn = state.loads.acquire("pre2").await;
        assert_eq!(picked("pre2").await, None);
        drop(_turn);
        // a head without the sessions on could not restore what a prefill machine read: it reads its own prompt
        let _plain = state.loads.acquire("plain").await;
        assert_eq!(picked("plain").await, None);
    }

    #[test]
    fn plugin_codes_and_capabilities_round_trip() {
        assert!(valid_code("strata") && valid_code("code_server"));
        for bad in ["", "Strata", "1x", "a/b", "a-b", &"x".repeat(65)] {
            assert!(!valid_code(bad), "{bad}");
        }
        let caps = vec![
            "transport/iroh-tunnel-v1".to_owned(),
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
            endpoint_id: None,
            endpoint_addr_candidates: Vec::new(),
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
        let report = super::super::engine_telemetry::EngineReport::from_done(&done, 427);
        let mut trace = super::super::engine_telemetry::build_trace("t".into(), "m", &plan, "strata", &report);
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
    fn each_machines_own_time_leaves_out_the_round_trip_below_it() {
        use super::super::plugin_stage_link::{record_leg, LinkPhase};
        let caps = vec![capability("strata")];
        let plan = crate::pipeline::plan_from_steps(vec![
            step("a", 0, 19, &caps),
            step("b", 20, 34, &caps),
            step("c", 35, 47, &caps),
        ]);
        let code = "strata-own-time-test";
        // b (answering the head) served 250 ms, of which its own call to a took 100 ms; a served 40 ms on its own
        record_leg(code, "b", LinkPhase::Prefill, std::time::Duration::from_millis(300), 0, 0, 250, "quic");
        record_leg(code, "a", LinkPhase::Prefill, std::time::Duration::from_millis(100), 0, 0, 40, "quic");
        let mut trace = super::super::engine_telemetry::build_trace(
            "t".into(),
            "m",
            &plan,
            code,
            &super::super::engine_telemetry::EngineReport::default(),
        );
        super::super::engine_telemetry::apply_stage_links(&mut trace, code, 500);
        assert_eq!(trace.hops[0].prefill.processing_ms, 40);
        assert_eq!(trace.hops[1].prefill.processing_ms, 150, "250 served, less the 100 ms to a");
        assert_eq!(trace.hops[2].prefill.processing_ms, 200, "the head's 500 ms prompt, less the 300 ms to b");
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
    fn an_amd_card_keeps_its_place_among_the_amd_ones() {
        let gpu = |index: u32, vendor: crate::sysmon::GpuVendor| crate::sysmon::GpuStat {
            index,
            name: format!("gpu{index}"),
            vendor,
            vram_total_mb: Some(16384.0),
            vram_used_mb: None,
            vram_used_pct: None,
            utilization_pct: None,
            power_watts: None,
            temperature_c: None,
        };
        let (nvidia, devices) = gpus_of(vec![
            gpu(0, crate::sysmon::GpuVendor::Nvidia),
            gpu(1, crate::sysmon::GpuVendor::Amd),
        ]);
        assert_eq!(nvidia, vec![0]);
        assert_eq!(devices[1]["vendor"], "amd");
        assert_eq!(devices[1]["index"], 1);
        assert_eq!(devices[1]["ordinal"], 0, "HIP numbers the Radeon 0");
        assert_eq!(devices[0]["ordinal"], 0);
    }

    #[test]
    fn a_stage_receives_its_link() {
        let cfg =
            crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
        assert!(load_params(&cfg, &[0], &[], None).get("link").is_none());
        let link = json!({"token": "t", "prev_port": 4100});
        assert_eq!(
            load_params(&cfg, &[0], &[], Some(&link))["link"]["prev_port"],
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
        // the stage link carries the node's own wire formats, so a slice offers the lossy ones too
        assert_eq!(
            ready["capabilities"],
            json!(["engine/plugin-v1:strata", "inference/frame-v1-dtype-f8", "inference/frame-v1-dtype-i4g64", "inference/frame-v1-dtype-i4g128"])
        );
        // a whole-model prefill machine says its role; the plugin hears it in engine.load
        let prefill = crate::runtime::NodeConfig {
            inference_role: crate::webui::InferenceRole::Prefill,
            prefill_min_tokens: Some(2048),
            ..cfg.clone()
        };
        assert_eq!(
            health_body(&prefill, "strata", &json!({"state": "ready"}))["capabilities"],
            json!(["engine/plugin-v1:strata", "inference/frame-v1-dtype-f8", "inference/frame-v1-dtype-i4g64", "inference/frame-v1-dtype-i4g128", "engine/plugin-role-v1:prefill"])
        );
        assert_eq!(load_params(&prefill, &[0], &[], None)["role"], "prefill");
        assert_eq!(load_params(&prefill, &[0], &[], None)["prefill_min_tokens"], 2048);
        assert!(load_params(&cfg, &[0], &[], None).get("role").is_none());
    }

    #[test]
    fn the_generation_body_carries_the_prompt_flags_and_sampling() {
        let body = generation_body(&EngineRequest {
            prompt: "User: oi".into(),
            chat_template: true,
            thinking_enabled: false,
            max_tokens: 128,
            stops: vec!["</s>".into()],
            session: None,
            request_details: json!({"temperature": 0.2, "top_k": 20, "messages": [], "seed": "x"}),
            probe_labels: vec!["Yes".into(), "No".into()],
            probe_prefix_bytes: Some(12),
            images: vec!["data:image/png;base64,AAAA".into()],
        });
        assert_eq!(body["prompt"], "User: oi");
        assert_eq!(body["probe"]["labels"], json!(["Yes", "No"]));
        assert_eq!(body["probe"]["prefix_bytes"], 12);
        assert_eq!(body["images"][0], "data:image/png;base64,AAAA");
        assert!(image_data_url(&[0xff, 0xd8, 0xff, 0xe0]).starts_with("data:image/jpeg;base64,"));
        assert_eq!(body["thinking"], false);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_k"], 20);
        assert!(body.get("seed").is_none());
        assert!(body.get("messages").is_none());
        // the client chose no thinking level: the engine's own default decides
        assert_eq!(body["thinking_default"], true);
        for asked in [
            json!({"thinking_enabled": true}),
            json!({"reasoning_effort": "low"}),
            json!({"thinking": {"type": "disabled"}}),
            json!({"chat_template_kwargs": {"enable_thinking": false}}),
        ] {
            assert!(thinking_asked(&asked), "{asked}");
        }
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use super::super::plugin_stage_link::{merge_reported_link, LinkPhase, LinkStats, ReportedLink};

    #[test]
    fn the_done_line_carries_the_session_the_head_fetched() {
        let fetch = super::super::plugin_session::SessionFetch {
            source: "prefill-node".to_string(),
            tokens: 7632,
            bytes: 237_139_332,
            transfer_ms: 1900,
            save_wait_ms: 40,
        };
        let mut first = false;
        let done = rewrite_line(
            "strata-fetch-test",
            b"{\"done\":{\"finish\":\"stop\"}}\n",
            &mut first,
            Some(&fetch.to_json()),
            0,
        );
        let value: Value = serde_json::from_slice(&done).unwrap();
        let carried = super::super::plugin_session::SessionFetch::from_json(&value["done"]["session_fetch"]);
        assert_eq!(carried, Some(fetch));
        let trace = crate::prefill::SessionTransferTrace::new("prefill-node", "head", 7632, 237_139_332, 1900, 40);
        assert!((trace.megabytes - 237.139332).abs() < 1e-9);
        assert!((trace.megabytes_per_sec.unwrap() - 124.81).abs() < 0.01);
    }

    #[test]
    fn the_done_line_carries_the_links_and_the_first_token_starts_decode() {
        merge_reported_link(
            "strata-relay-test",
            ReportedLink {
                node: "node-9".to_string(),
                phase: LinkPhase::Prefill,
                stats: LinkStats {
                    calls: 3,
                    round_trip_ms: 30,
                    network_ms: 6,
                    served_ms: 24,
                    bytes_sent: 900,
                    bytes_received: 400,
                    ..Default::default()
                },
                channels: Vec::new(),
            },
        );
        let mut first = false;
        let token = rewrite_line("strata-relay-test", b"{\"t\":\"hi\"}\n", &mut first, None, 0);
        assert!(first, "a text line is the first token");
        assert_eq!(token, b"{\"t\":\"hi\"}\n", "text lines pass through unchanged");
        let done = rewrite_line("strata-relay-test", b"{\"done\":{\"finish\":\"stop\"}}\n", &mut first, None, 0);
        let value: Value = serde_json::from_slice(&done[..done.len() - 1]).unwrap();
        assert_eq!(value["done"]["links"][0]["node"], "node-9");
        assert_eq!(value["done"]["links"][0]["phase"], "prefill");
        assert_eq!(value["done"]["links"][0]["round_trip_ms"], 30);
        assert_eq!(value["done"]["links"][0]["bytes_sent"], 900);
        assert!(done.ends_with(b"\n"));
    }
}
