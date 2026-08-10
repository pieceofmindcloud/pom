//! Putting a model that is already sliced into service across the cluster.
//!
//! The pieces all existed and none of them was the path: the catalog knows who
//! holds which slice, `transfer` moves a file between two machines, and `launch`
//! binds a range to a peer. What was missing was the decision in the middle —
//! *which* machine should serve *which* slice, and what has to be copied first
//! for that to be possible. This module is only that decision, plus the
//! sequencing to carry it out; it starts no transfer mechanism and no launch
//! mechanism of its own.
//!
//! Two routes, deliberately split the way the slicing screen already splits:
//! `plan` is read-only and cheap enough for the screen to call while the
//! operator is still choosing, and `deploy` recomputes the plan server-side
//! rather than trusting the preview it was handed.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use crate::network::PeerInfo;
use crate::webui::{
    jobs, launch, new_job, set_job, set_job_progress, LaunchRequest, LaunchTarget, RuntimeParams,
    PEER_TIMEOUT,
};

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
}

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// The shard set, named the way the catalog groups it.
    pub group: String,
    #[serde(default)]
    pub params: RuntimeParams,
}

// ── What a machine can tell us about itself ──────────────────────────────────

/// One slice of the chosen set, as it exists on one machine.
#[derive(Debug, Clone)]
struct HeldShard {
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    name: String,
    path: String,
    size_bytes: u64,
}

/// A machine in the cluster, with the slices of this set it already holds.
struct Machine {
    node_id: String,
    hostname: String,
    gateway: String,
    /// Free RAM, used to decide who gets layer 0 — that peer runs the whole
    /// prompt through itself on every request.
    free_mb: u64,
    held: Vec<HeldShard>,
}

impl Machine {
    fn holds(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.held
            .iter()
            .find(|s| s.start_layer == start && s.end_layer == end)
    }
    fn label(&self) -> String {
        if self.hostname.is_empty() {
            self.node_id[..12.min(self.node_id.len())].to_string()
        } else {
            self.hostname.clone()
        }
    }
}

/// Ask every peer what it has, in parallel.
///
/// Reuses each node's own `/api/ui/models`, which is the same answer the
/// cluster catalog screen shows — a machine is the only honest source for where
/// its own files are, and for a path we would otherwise have to guess.
async fn survey(state: &GatewayState, group: &str) -> Vec<Machine> {
    let peers: Vec<PeerInfo> = state.all_peers().await;
    let self_id = state.network.node_id.clone();

    let futures = peers.into_iter().map(|peer| {
        let http = state.http.clone();
        let is_self = peer.node_id == self_id;
        let group = group.to_string();
        async move {
            let base = peer.gateway_url()?;
            let body: serde_json::Value = http
                .get(format!("{base}/api/ui/models"))
                .timeout(PEER_TIMEOUT * 4)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;

            let held = body
                .get("shard_sets")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|set| set.get("group").and_then(|g| g.as_str()) == Some(group.as_str()))
                .flat_map(|set| set.get("shards").and_then(|v| v.as_array()).into_iter().flatten())
                .filter_map(parse_shard)
                .collect();

            Some(Machine {
                node_id: peer.node_id.clone(),
                hostname: if peer.hostname.is_empty() && is_self {
                    body.get("hostname")
                        .and_then(|h| h.as_str())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    peer.hostname.clone()
                },
                gateway: base,
                free_mb: peer.disk_free_mb,
                held,
            })
        }
    });

    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

fn parse_shard(v: &serde_json::Value) -> Option<HeldShard> {
    let range = v.get("shard_range")?.as_array()?;
    Some(HeldShard {
        start_layer: range.first()?.as_u64()? as u32,
        end_layer: range.get(1)?.as_u64()? as u32,
        total_layers: v.get("total_layers").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
        name: v.get("name")?.as_str()?.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
    })
}

// ── The decision ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    name: String,
    size_bytes: u64,
    target_node_id: String,
    target_host: String,
    /// Already on the target: nothing to copy, this one starts immediately.
    ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    source_node_id: Option<String>,
    source_host: Option<String>,
    source_path: Option<String>,
    /// Where it will be on the target once it lands, when known up front.
    target_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    group: String,
    total_layers: u32,
    steps: Vec<Step>,
    /// Every layer from 0 to `total_layers` is covered exactly once.
    complete: bool,
    /// Bytes that have to cross the network before this can serve.
    transfer_bytes: u64,
    /// Set when the plan cannot be carried out at all, and why.
    blocked: Option<String>,
}

/// Decide who serves what.
///
/// Rules, in the order they matter:
///
/// 1. A node serves one range, so the set needs at least as many machines as
///    slices. Fewer machines is a hard stop, not a partial deploy.
/// 2. A machine that already holds a slice is preferred for it. A 10 GB slice
///    takes ~17 minutes on the real link, so every avoided copy is the
///    difference between a deploy that finishes now and one that finishes after
///    lunch.
/// 3. Layer 0 goes to the machine with the most room. Whoever holds it runs the
///    entire prompt of every request through itself, so it is the one position
///    where the choice is not arbitrary.
fn plan(group: &str, machines: &[Machine]) -> Plan {
    // The set as the cluster knows it: every distinct range anyone holds.
    let mut ranges: Vec<(u32, u32, u32, String, u64)> = Vec::new();
    for m in machines {
        for s in &m.held {
            if !ranges.iter().any(|(a, b, ..)| *a == s.start_layer && *b == s.end_layer) {
                ranges.push((
                    s.start_layer,
                    s.end_layer,
                    s.total_layers,
                    s.name.clone(),
                    s.size_bytes,
                ));
            }
        }
    }
    ranges.sort_by_key(|(a, ..)| *a);

    let total_layers = ranges.iter().map(|(_, _, t, ..)| *t).max().unwrap_or(0);

    let mut blocked = None;
    if ranges.is_empty() {
        blocked = Some(format!("nenhuma máquina do cluster tem fatias de {group}"));
    } else if ranges.len() > machines.len() {
        blocked = Some(format!(
            "são {} fatias para {} {} — um nó serve uma faixa por vez",
            ranges.len(),
            machines.len(),
            if machines.len() == 1 { "máquina" } else { "máquinas" }
        ));
    }

    // Coverage is checked against the ranges themselves, not against who has
    // them: an incomplete set is a slicing problem, and saying so here is more
    // use than a pipeline that quietly never plans.
    let mut covered = true;
    let mut next = 0u32;
    for (start, end, ..) in &ranges {
        if *start != next {
            covered = false;
            break;
        }
        next = end + 1;
    }
    if covered && total_layers > 0 && next != total_layers {
        covered = false;
    }

    let mut taken: Vec<&str> = Vec::new();
    let mut steps = Vec::new();

    // Pass 1, in slice order, layer 0 first: give each slice to a machine that
    // already has it, richest machine first so layer 0 lands well.
    let mut pending: Vec<usize> = (0..ranges.len()).collect();
    let mut assigned: HashMap<usize, &Machine> = HashMap::new();

    for &i in &pending.clone() {
        let (start, end, ..) = ranges[i];
        let mut holders: Vec<&Machine> = machines
            .iter()
            .filter(|m| !taken.contains(&m.node_id.as_str()) && m.holds(start, end).is_some())
            .collect();
        holders.sort_by_key(|m| std::cmp::Reverse(m.free_mb));
        if let Some(m) = holders.first() {
            taken.push(&m.node_id);
            assigned.insert(i, m);
            pending.retain(|&j| j != i);
        }
    }

    // Pass 2: whatever is left has to be copied. Again richest first, so the
    // slice that still needs a machine gets the best one available.
    for &i in &pending {
        let mut free: Vec<&Machine> = machines
            .iter()
            .filter(|m| !taken.contains(&m.node_id.as_str()))
            .collect();
        free.sort_by_key(|m| std::cmp::Reverse(m.free_mb));
        if let Some(m) = free.first() {
            taken.push(&m.node_id);
            assigned.insert(i, m);
        }
    }

    let mut transfer_bytes = 0;
    for (i, (start, end, total, name, size)) in ranges.iter().enumerate() {
        // A slice with nowhere to go still belongs in the list. Dropping it made
        // the table disagree with its own summary — "são 2 fatias" above a table
        // showing one.
        let target = assigned.get(&i);
        let ready = target.is_some_and(|t| t.holds(*start, *end).is_some());
        let source = if ready {
            None
        } else {
            machines.iter().find(|m| {
                target.is_none_or(|t| m.node_id != t.node_id) && m.holds(*start, *end).is_some()
            })
        };
        // Only a slice that actually has somewhere to go counts as a copy: a
        // slice with no machine is a blocked plan, not 1,3 GB of traffic.
        if !ready && target.is_some() {
            transfer_bytes += size;
        }

        steps.push(Step {
            start_layer: *start,
            end_layer: *end,
            total_layers: *total,
            name: name.clone(),
            size_bytes: *size,
            target_node_id: target.map(|t| t.node_id.clone()).unwrap_or_default(),
            target_host: target.map(|t| t.label()).unwrap_or_default(),
            ready,
            source_node_id: source.map(|m| m.node_id.clone()),
            source_host: source.map(|m| m.label()),
            source_path: source.and_then(|m| m.holds(*start, *end)).map(|s| s.path.clone()),
            target_path: target.and_then(|t| t.holds(*start, *end)).map(|s| s.path.clone()),
        });
    }

    // A slice nobody holds cannot be deployed however the machines are shuffled.
    if blocked.is_none() {
        if let Some(orphan) = steps
            .iter()
            .find(|s| !s.target_node_id.is_empty() && !s.ready && s.source_node_id.is_none())
        {
            blocked = Some(format!(
                "a fatia L{}-{} não existe em máquina nenhuma do cluster",
                orphan.start_layer, orphan.end_layer
            ));
        }
    }

    Plan {
        group: group.to_string(),
        total_layers,
        complete: covered,
        transfer_bytes,
        steps,
        blocked,
    }
}

async fn plan_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    let machines = survey(&state, &req.group).await;
    Json(plan(&req.group, &machines)).into_response()
}

// ── Carrying it out ──────────────────────────────────────────────────────────

async fn deploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    // Recomputed here rather than taken from the caller: the preview the screen
    // is showing may be a minute old, and a machine that has left in the
    // meantime would otherwise be handed a slice.
    let machines = survey(&state, &req.group).await;
    let plan = plan(&req.group, &machines);

    if let Some(reason) = plan.blocked {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": reason })),
        )
            .into_response();
    }
    if plan.steps.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nada para implantar" })),
        )
            .into_response();
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let title = format!("Implantar {} em {} nós", req.group, plan.steps.len());
    jobs()
        .write()
        .await
        .insert(job_id.clone(), new_job(&job_id, "deploy", title, "preparando"));

    let id = job_id.clone();
    tokio::spawn(async move {
        match run(state, plan, req.params, &id).await {
            Ok(()) => set_job(&id, "done", "pronto", "o modelo está atendendo na rede").await,
            Err(e) => {
                warn!("implantação falhou: {e}");
                set_job(&id, "failed", "falhou", &e).await;
            }
        }
    });

    Json(json!({ "started": true, "job_id": job_id })).into_response()
}

/// Copy what has to be copied, then bind every peer to its range.
async fn run(
    state: GatewayState,
    plan: Plan,
    params: RuntimeParams,
    job_id: &str,
) -> Result<(), String> {
    let key = state.auth.read().await.connection_key.clone();
    let to_move: Vec<&Step> = plan.steps.iter().filter(|s| !s.ready).collect();

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !to_move.is_empty() {
        set_job(
            job_id,
            "running",
            "copiando fatias",
            &format!(
                "{} fatia(s), {:.1} GB no total",
                to_move.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        let mut watching: Vec<(String, String)> = Vec::new(); // (gateway, job id)
        for step in &to_move {
            let (Some(source_node), Some(source_path)) =
                (&step.source_node_id, &step.source_path)
            else {
                return Err(format!(
                    "a fatia L{}-{} não tem de onde vir",
                    step.start_layer, step.end_layer
                ));
            };
            let gateway = gateway_of(&state, &step.target_node_id)
                .await
                .ok_or_else(|| format!("{} não anuncia porta de gateway", step.target_host))?;

            let body: serde_json::Value = state
                .http
                .post(format!("{gateway}/api/transfer/pull"))
                .bearer_auth(&key)
                .json(&json!({
                    "node_id": source_node,
                    "remote_path": source_path,
                    "name": step.name,
                }))
                .send()
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", step.target_host))?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} puxando L{}-{} de {}",
                        step.target_host, step.start_layer, step.end_layer, source_node
                    );
                    watching.push((gateway, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} recusou a cópia: {}",
                        step.target_host,
                        body.get("error").and_then(|e| e.as_str()).unwrap_or("motivo não informado")
                    ))
                }
            }
        }

        wait_for_transfers(&state, &key, &watching, job_id).await?;
    }

    // ── Phase 2: bind every peer to its range ────────────────────────────────
    set_job(job_id, "running", "subindo os nós", "atribuindo as faixas").await;
    set_job_progress(job_id, 92.0).await;

    // Where each file ended up is the target's own business, so ask it rather
    // than assume a path on someone else's filesystem.
    let machines = survey(&state, &plan.group).await;
    let mut targets = Vec::new();
    for step in &plan.steps {
        let path = machines
            .iter()
            .find(|m| m.node_id == step.target_node_id)
            .and_then(|m| m.holds(step.start_layer, step.end_layer))
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                format!(
                    "{} não achou a fatia L{}-{} depois da cópia",
                    step.target_host, step.start_layer, step.end_layer
                )
            })?;

        targets.push(LaunchTarget {
            node_id: step.target_node_id.clone(),
            source_model_path: path,
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            // Already a slice of exactly this range: nothing to cut.
            skip_split: true,
        });
    }

    let primary = plan
        .steps
        .iter()
        .find(|s| s.start_layer == 0)
        .map(|s| s.target_node_id.clone());

    let response = launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            base_name: plan.group.clone(),
            total_layers: plan.total_layers,
            targets,
            primary_node_id: primary,
            params,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(format!("o launch recusou: HTTP {}", response.status()));
    }
    Ok(())
}

async fn gateway_of(state: &GatewayState, node_id: &str) -> Option<String> {
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())
}

/// Poll the machines doing the copying until they are all finished.
///
/// The bar tracks bytes rather than file count, because the slices are wildly
/// unequal — a set of 2 GB and 9,6 GB would otherwise sit at 50% for most of the
/// wait and then finish in a blink.
async fn wait_for_transfers(
    state: &GatewayState,
    key: &str,
    watching: &[(String, String)],
    job_id: &str,
) -> Result<(), String> {
    loop {
        let mut done = 0usize;
        let mut fraction_sum = 0.0;

        for (gateway, id) in watching {
            let body: serde_json::Value = state
                .http
                .get(format!("{gateway}/api/transfer/jobs"))
                .bearer_auth(key)
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("perdi contato com {gateway}: {e}"))?
                .json()
                .await
                .unwrap_or(json!({}));

            let job = body
                .get("jobs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|j| j.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));

            let Some(job) = job else {
                // A job that vanished from the list finished and was pruned.
                done += 1;
                fraction_sum += 1.0;
                continue;
            };

            match job.get("state").and_then(|v| v.as_str()) {
                Some("done") => {
                    done += 1;
                    fraction_sum += 1.0;
                }
                Some("failed") => {
                    return Err(format!(
                        "a cópia falhou: {}",
                        job.get("error").and_then(|e| e.as_str()).unwrap_or("sem detalhe")
                    ))
                }
                _ => {
                    let total = job.get("total_bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let got = job.get("received_bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if total > 0.0 {
                        fraction_sum += (got / total).clamp(0.0, 1.0);
                    }
                }
            }
        }

        // Phase 1 owns the first 90% of the bar; the launch is the rest.
        let pct = fraction_sum / watching.len().max(1) as f64 * 90.0;
        set_job_progress(job_id, pct).await;

        if done == watching.len() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(id: &str, free_mb: u64, held: &[(u32, u32)]) -> Machine {
        Machine {
            node_id: id.to_string(),
            hostname: id.to_string(),
            gateway: format!("http://{id}"),
            free_mb,
            held: held
                .iter()
                .map(|(a, b)| HeldShard {
                    start_layer: *a,
                    end_layer: *b,
                    total_layers: 32,
                    name: format!("m_{a}_{b}.gguf"),
                    path: format!("/models/m_{a}_{b}.gguf"),
                    size_bytes: 1_000_000_000,
                })
                .collect(),
        }
    }

    /// The happy shape: each machine already has one of the slices, so the
    /// deploy is pure launch and copies nothing.
    #[test]
    fn slices_already_in_place_need_no_transfer() {
        let ms = vec![
            machine("a", 100, &[(0, 15)]),
            machine("b", 200, &[(16, 31)]),
        ];
        let p = plan("m", &ms);
        assert!(p.blocked.is_none());
        assert!(p.complete, "0-15 e 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0);
        assert!(p.steps.iter().all(|s| s.ready));
    }

    /// One machine holds both slices: the other has to be given one, and the
    /// plan has to name who sends it.
    #[test]
    fn a_missing_slice_is_planned_as_a_copy() {
        let ms = vec![
            machine("a", 100, &[(0, 15), (16, 31)]),
            machine("b", 200, &[]),
        ];
        let p = plan("m", &ms);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        let copied: Vec<&Step> = p.steps.iter().filter(|s| !s.ready).collect();
        assert_eq!(copied.len(), 1, "só uma fatia precisa viajar");
        assert_eq!(copied[0].source_node_id.as_deref(), Some("a"));
        assert_eq!(copied[0].target_node_id, "b");
        assert_eq!(p.transfer_bytes, 1_000_000_000);
    }

    /// Layer 0 runs every prompt, so it goes to the machine with the most room
    /// — the one place the assignment is not arbitrary.
    #[test]
    fn layer_zero_goes_to_the_roomiest_machine() {
        let ms = vec![
            machine("pequena", 10, &[(0, 15), (16, 31)]),
            machine("grande", 900, &[(0, 15), (16, 31)]),
        ];
        let p = plan("m", &ms);
        let first = p.steps.iter().find(|s| s.start_layer == 0).unwrap();
        assert_eq!(first.target_node_id, "grande");
    }

    /// A node serves one range, so three slices onto two machines is a stop,
    /// not a deploy that half works.
    #[test]
    fn more_slices_than_machines_is_refused() {
        let ms = vec![
            machine("a", 100, &[(0, 9), (10, 19), (20, 31)]),
            machine("b", 100, &[]),
        ];
        let p = plan("m", &ms);
        assert!(p.blocked.is_some());
        assert!(p.blocked.unwrap().contains("3 fatias"));
    }

    /// A gap in the set is reported as incomplete rather than deployed into a
    /// pipeline that can never plan.
    #[test]
    fn a_gap_in_the_set_is_reported() {
        let ms = vec![
            machine("a", 100, &[(0, 15)]),
            machine("b", 100, &[(20, 31)]),
        ];
        let p = plan("m", &ms);
        assert!(!p.complete, "faltam as camadas 16-19");
    }
}
