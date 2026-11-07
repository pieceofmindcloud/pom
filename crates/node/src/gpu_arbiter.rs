//! The GPU arbiter: one physical card shared by an LLM and image components
//! (ComfyUI) favours the LLM. Before an image component op runs on the node
//! that serves it, the gate checks the nodes on the same card (grouped by the
//! card's identity, `gpu_identity.rs`): with room in VRAM, or no LLM on the
//! card, the op runs; with an LLM loaded, quiet for the grace and allowed to
//! yield (`yield_gpu_to_images`, a POM setting, off by default), the LLM is
//! unloaded as after an idle period and the op runs; otherwise the op waits.
//! When a yielded LLM wakes, the image components on its card free their VRAM
//! first (`before_llm_reload`), so a running image pauses at its next network
//! call until the LLM has been quiet again for the grace.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::gateway::GatewayState;

/// An LLM must have had no request in flight for this long before it lends
/// its GPU again (and before a paused image resumes).
pub(crate) const GRACE: Duration = Duration::from_secs(10);
/// VRAM kept free beyond the component's weights.
const MARGIN_MB: u64 = 1024;
/// How long an image op waits for the GPU before it runs anyway.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// One node on the network as the gate sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    pub node_id: String,
    pub gpu_ids: Vec<String>,
    /// Its LLM is loaded (ready); an idle or unloaded one holds no VRAM.
    pub llm_loaded: bool,
    pub in_flight: u32,
    /// Its deployment allows lending the GPU (`yield_gpu_to_images`).
    pub yields: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    Go,
    /// Ask these nodes to unload their LLM, then run.
    Yield(Vec<String>),
    Wait,
}

/// The gate's decision for an op on `gpu` needing `need_mb` of VRAM, with
/// `free_mb` free on that card now. `quiet_for` says how long a node has had
/// no request in flight (`None`: never seen busy).
pub(crate) fn decide(
    gpu: Option<&str>,
    need_mb: u64,
    free_mb: Option<u64>,
    members: &[Member],
    quiet_for: &dyn Fn(&str) -> Option<Duration>,
) -> Verdict {
    let Some(gpu) = gpu else {
        return Verdict::Go;
    };
    let llms: Vec<&Member> = members
        .iter()
        .filter(|m| m.llm_loaded && m.gpu_ids.iter().any(|id| id == gpu))
        .collect();
    if llms.is_empty() {
        return Verdict::Go;
    }
    if free_mb.is_some_and(|free| free >= need_mb + MARGIN_MB) {
        return Verdict::Go;
    }
    let quiet = |m: &Member| m.in_flight == 0 && quiet_for(&m.node_id).is_none_or(|d| d >= GRACE);
    if llms.iter().all(|m| quiet(m) && m.yields) {
        return Verdict::Yield(llms.iter().map(|m| m.node_id.clone()).collect());
    }
    Verdict::Wait
}

// ── Live state ──────────────────────────────────────────────────────────────

static STATE: std::sync::OnceLock<GatewayState> = std::sync::OnceLock::new();
static BUSY: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Instant>>> =
    std::sync::OnceLock::new();

/// Gives the arbiter the gateway state (once, when the gateway starts).
pub(crate) fn install(state: GatewayState) {
    let _ = STATE.set(state);
}

fn busy() -> &'static std::sync::Mutex<HashMap<String, Instant>> {
    BUSY.get_or_init(Default::default)
}

fn note_busy(node_id: &str, in_flight: u32) {
    if in_flight > 0 {
        busy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node_id.to_owned(), Instant::now());
    }
}

fn quiet_for(node_id: &str) -> Option<Duration> {
    busy()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(node_id)
        .map(|at| at.elapsed())
}

/// The cards of a node's `/api/ui/node` answer.
fn gpu_ids_of(node: &Value) -> Vec<(u32, String)> {
    node["gpu_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| Some((g["index"].as_u64()? as u32, g["id"].as_str()?.to_owned())))
        .collect()
}

fn member_of(node_id: &str, node: &Value) -> Member {
    let state = node["status"]["state"].as_str().unwrap_or_default();
    Member {
        node_id: node_id.to_owned(),
        gpu_ids: gpu_ids_of(node).into_iter().map(|(_, id)| id).collect(),
        llm_loaded: state == "ready" && node["config"].is_object(),
        in_flight: node["load"].as_u64().unwrap_or(0) as u32,
        yields: node["config"]["yield_gpu_to_images"]
            .as_bool()
            .unwrap_or(false),
    }
}

/// The node views read in the last two seconds: the diffusion stage makes one
/// op per sampling step, and each step must not ask every peer again.
static RECENT_NODES: crate::components::Recent<Vec<(String, Value)>> =
    crate::components::Recent::new(Duration::from_secs(2));

/// Every live node's `/api/ui/node`, this one included (cached briefly).
async fn nodes(state: &GatewayState) -> Vec<(String, Value)> {
    if let Some(list) = RECENT_NODES.get(Instant::now()) {
        return list;
    }
    let list = read_nodes(state).await;
    RECENT_NODES.put(Instant::now(), list.clone());
    list
}

async fn read_nodes(state: &GatewayState) -> Vec<(String, Value)> {
    let peers = state.all_peers().await;
    let reads = peers.into_iter().map(|peer| async move {
        let response = state
            .control_get(&peer, "/api/ui/node", Duration::from_secs(5))
            .await
            .ok()?;
        let body: Value = response.json().await.ok()?;
        Some((peer.node_id.clone(), body))
    });
    futures::future::join_all(reads)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// This node's first card and its free VRAM, from its own node view.
fn this_gpu(node: &Value) -> (Option<String>, Option<u64>) {
    let ids = gpu_ids_of(node);
    let Some((index, id)) = ids.first().cloned() else {
        return (None, None);
    };
    let free = node["system"]["gpus"].as_array().and_then(|gpus| {
        gpus.iter()
            .find(|g| g["index"].as_u64() == Some(u64::from(index)))
            .and_then(|g| {
                let total = g["vram_total_mb"].as_f64()?;
                let used = g["vram_used_mb"].as_f64()?;
                Some((total - used).max(0.0) as u64)
            })
    });
    (Some(id), free)
}

async fn ask(state: &GatewayState, node_id: &str, path: &str) -> bool {
    if node_id == state.network.node_id {
        return match path {
            YIELD_PATH => state.runtime.yield_for_images().await,
            _ => release_local_images(state).await,
        };
    }
    let Some(peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
    else {
        return false;
    };
    match state
        .control_post_json(
            &peer,
            path,
            &serde_json::json!({}),
            Duration::from_secs(120),
        )
        .await
    {
        Ok(response) => response
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["ok"].as_bool())
            .unwrap_or(false),
        Err(_) => false,
    }
}

const YIELD_PATH: &str = "/api/internal/gpu/yield";
const RELEASE_PATH: &str = "/api/internal/gpu/release-images";

/// The gate before an image component op that needs `need_mb` of VRAM on
/// this node's card. Returns how long it waited.
pub(crate) async fn admit_image(need_mb: u64) -> Duration {
    let started = Instant::now();
    let Some(state) = STATE.get() else {
        return Duration::ZERO;
    };
    if crate::gpu_identity::gpu_ids().await.is_empty() {
        return Duration::ZERO;
    }
    loop {
        let all = nodes(state).await;
        let Some((_, me)) = all.iter().find(|(id, _)| *id == state.network.node_id) else {
            return started.elapsed();
        };
        let (gpu, free) = this_gpu(me);
        let members: Vec<Member> = all.iter().map(|(id, node)| member_of(id, node)).collect();
        for member in &members {
            note_busy(&member.node_id, member.in_flight);
        }
        match decide(gpu.as_deref(), need_mb, free, &members, &quiet_for) {
            Verdict::Go => return started.elapsed(),
            Verdict::Yield(ids) => {
                for id in ids {
                    if ask(state, &id, YIELD_PATH).await {
                        tracing::info!(node = %id, "GPU arbiter: the LLM lent its GPU to an image");
                    }
                }
            }
            Verdict::Wait => {}
        }
        if started.elapsed() > MAX_WAIT {
            tracing::warn!("GPU arbiter: an image waited {MAX_WAIT:?} for the GPU; running anyway");
            return started.elapsed();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Before a yielded LLM loads again: every node on its card that serves image
/// components frees its VRAM (ComfyUI keeps its models in RAM).
pub(crate) async fn before_llm_reload() {
    let Some(state) = STATE.get() else {
        return;
    };
    note_busy(&state.network.node_id, 1);
    let mine: Vec<String> = crate::gpu_identity::gpu_ids()
        .await
        .into_iter()
        .map(|g| g.id)
        .collect();
    if mine.is_empty() {
        return;
    }
    let all = nodes(state).await;
    let served = crate::components::network_components(state).await;
    for (id, node) in all {
        let shares = gpu_ids_of(&node).iter().any(|(_, gpu)| mine.contains(gpu));
        let serves = served.iter().any(|c| c.node_id == id);
        if shares && serves && ask(state, &id, RELEASE_PATH).await {
            tracing::info!(node = %id, "GPU arbiter: image components freed the GPU for the LLM");
        }
    }
}

/// Asks this node's ComfyUI to move its models off the GPU.
async fn release_local_images(state: &GatewayState) -> bool {
    let code = crate::components::IMAGE_PLUGIN;
    let Some(upstream) = state
        .plugins
        .query_audit(code, "ui.upstream", serde_json::json!({}))
        .await
    else {
        return false;
    };
    let (Some(port), token) = (
        upstream["port"].as_u64(),
        upstream["token"].as_str().unwrap_or_default().to_owned(),
    ) else {
        return false;
    };
    state
        .peer_http
        .plain()
        .post(format!("http://127.0.0.1:{port}/pom/gpu/yield"))
        .header("x-pom-plugin-token", token)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

async fn from_live_member(auth: &crate::auth::InternalNodeAuth, state: &GatewayState) -> bool {
    let Some(principal) = auth.principal() else {
        return false;
    };
    state
        .network
        .alive_peers()
        .await
        .iter()
        .any(|peer| peer.node_id == principal.node_id)
}

/// `POST /api/internal/gpu/yield`: lend this node's GPU (unload its LLM when
/// allowed and quiet).
pub(crate) async fn yield_route(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
) -> axum::Json<Value> {
    if !from_live_member(&auth, &state).await {
        return axum::Json(
            serde_json::json!({ "ok": false, "error": "a signed call from a live peer is required" }),
        );
    }
    axum::Json(serde_json::json!({ "ok": state.runtime.yield_for_images().await }))
}

/// `POST /api/internal/gpu/release-images`: this node's ComfyUI frees the GPU.
pub(crate) async fn release_route(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
) -> axum::Json<Value> {
    if !from_live_member(&auth, &state).await {
        return axum::Json(
            serde_json::json!({ "ok": false, "error": "a signed call from a live peer is required" }),
        );
    }
    axum::Json(serde_json::json!({ "ok": release_local_images(&state).await }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm(node: &str, gpu: &str, in_flight: u32, yields: bool) -> Member {
        Member {
            node_id: node.into(),
            gpu_ids: vec![gpu.into()],
            llm_loaded: true,
            in_flight,
            yields,
        }
    }

    fn never(_: &str) -> Option<Duration> {
        None
    }

    #[test]
    fn no_llm_on_the_gpu_goes() {
        assert_eq!(
            decide(Some("G0"), 12_000, Some(1000), &[], &never),
            Verdict::Go
        );
        let idle = Member {
            llm_loaded: false,
            ..llm("a", "G0", 0, true)
        };
        assert_eq!(
            decide(Some("G0"), 12_000, Some(1000), &[idle], &never),
            Verdict::Go
        );
        assert_eq!(
            decide(None, 12_000, Some(0), &[llm("a", "G0", 3, false)], &never),
            Verdict::Go,
            "no identity, no arbitration"
        );
    }

    #[test]
    fn room_in_vram_goes() {
        assert_eq!(
            decide(
                Some("G0"),
                4000,
                Some(6000),
                &[llm("a", "G0", 2, false)],
                &never
            ),
            Verdict::Go
        );
    }

    #[test]
    fn an_idle_yielding_llm_is_asked_to_yield() {
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G0", 0, true)],
                &never
            ),
            Verdict::Yield(vec!["a".into()])
        );
    }

    #[test]
    fn a_busy_llm_makes_the_image_wait() {
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G0", 1, true)],
                &never
            ),
            Verdict::Wait
        );
    }

    #[test]
    fn an_llm_that_does_not_yield_makes_it_wait() {
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G0", 0, false)],
                &never
            ),
            Verdict::Wait
        );
    }

    #[test]
    fn a_recently_busy_llm_waits_for_the_grace() {
        let recent = |_: &str| Some(Duration::from_secs(3));
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G0", 0, true)],
                &recent
            ),
            Verdict::Wait
        );
        let long = |_: &str| Some(Duration::from_secs(30));
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G0", 0, true)],
                &long
            ),
            Verdict::Yield(vec!["a".into()])
        );
    }

    #[test]
    fn another_gpu_is_not_a_member() {
        assert_eq!(
            decide(
                Some("G0"),
                12_000,
                Some(2000),
                &[llm("a", "G1", 5, false)],
                &never
            ),
            Verdict::Go
        );
    }

    #[test]
    fn a_node_view_becomes_a_member() {
        let node = serde_json::json!({
            "status": {"state": "ready"}, "config": {"yield_gpu_to_images": true}, "load": 2,
            "gpu_ids": [{"index": 0, "id": "GPU-a"}],
            "system": {"gpus": [{"index": 0, "vram_total_mb": 16000.0, "vram_used_mb": 12000.0}]}
        });
        let member = member_of("n", &node);
        assert_eq!(
            member,
            Member {
                node_id: "n".into(),
                gpu_ids: vec!["GPU-a".into()],
                llm_loaded: true,
                in_flight: 2,
                yields: true
            }
        );
        assert_eq!(this_gpu(&node), (Some("GPU-a".into()), Some(4000)));
        let idle = serde_json::json!({"status": {"state": "idle"}, "config": {}, "load": 0});
        assert!(!member_of("n", &idle).llm_loaded);
    }
}
