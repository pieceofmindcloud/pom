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
//! operator is still choosing, and `deploy` re-surveys the cluster rather than
//! trusting the preview it was handed.
//!
//! ## The operator's choice is part of the request
//!
//! The planner is a *suggestion*, not the decision. `selection` carries the
//! operator's own answer to the two questions the planner guesses at — which
//! ranges to put on the wire, and which machine takes each one — and everything
//! below validates that answer instead of quietly recomputing its own. Omitting
//! `selection` keeps the old behaviour exactly: the node plans it, and the
//! screen shows what it planned.
//!
//! Three things fall out of the architecture rather than out of taste, and the
//! validation leans on all three:
//!
//! * **The pipeline order is the layer order.** `pipeline.rs` sweeps from layer
//!   0 upwards, so the machine holding layer 0 is the one that receives every
//!   prompt. "Choosing the order" is therefore the same act as choosing who
//!   takes which range — there is no second knob, and pretending otherwise
//!   would be a lie on screen.
//! * **A node may serve multiple ranges.** Each selected range remains a
//!   pipeline step, even when the same machine owns separated ranges. When
//!   `merge` is on, adjacent ranges on the same machine become one *merged
//!   unit*: the machine joins the slices into a single file (`splitter::merge`)
//!   and serves the union. A merged step carries `parts`, one per slice, each
//!   with its own source.
//!   The same exact range on different machines is an allowed replica.
//! * **Overlap is not a preference.** Two different cuts covering the same
//!   layer would make the pipeline ambiguous, so that overlap is refused. The
//!   same exact range on different machines is an explicit parallel route. A
//!   *gap* is different: it makes the global coverage incomplete and therefore
//!   refuses the pipeline before launch.
//!
//! ## A whole model is the degenerate case, not a second path
//!
//! `kind: "whole"` deploys one un-sliced GGUF onto one machine. Everything
//! above still applies, because the survey reads that file as a single held
//! range covering layer 0 to the last one: the coverage arithmetic finds it
//! complete by construction, and the transfer and launch phases are the ones
//! the slice path
//! already uses. What changes is the wording of the refusals — "a fatia L0-31"
//! is not what an operator who picked a whole model is looking at.

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
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
use crate::plan::DEFAULT_RESERVE_PCT;
use crate::webui::{
    jobs, launch, new_job, set_job, set_job_progress, InferenceMode, LaunchRequest, LaunchTarget,
    LayerCut, MergeSource, ModelLayerOptions, RuntimeParams, SliceRange, SliceRangesRequest,
    PEER_TIMEOUT,
};

const MIB: u64 = 1024 * 1024;
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
        .route("/api/ui/deploy/node", post(add_node_endpoint))
        .route("/api/ui/deploy/node/remove", post(remove_node_endpoint))
        .route("/api/ui/undeploy", post(undeploy_endpoint))
        .route(
            "/api/ui/deployments",
            get(list_deployments).post(receive_deployment),
        )
        .route(
            "/api/ui/deployments/:id/activate",
            post(activate_deployment),
        )
        .route("/api/ui/deployments/:id/redeploy", post(redeploy_endpoint))
}

// ── O registro das implantações ──────────────────────────────────────────────

/// Só as implantações que este nó guarda em disco, sem perguntar a ninguém.
///
/// É o que um peer responde quando o coordenador vem juntar a visão do cluster
/// (`?local=1`), e é o que impede a varredura de se chamar em círculo.
fn local_deployments(self_id: &str) -> serde_json::Value {
    let todas = crate::deployment::all();
    json!({ "node_id": self_id, "deployments": todas })
}

#[derive(Debug, Default, Deserialize)]
pub struct ListDeploymentsQuery {
    /// Responder só com os registros deste nó, sem varrer o cluster.
    #[serde(default)]
    pub local: bool,
}

/// Todas as implantações do cluster, com o estado de cada uma.
///
/// O registro de uma implantação vive numa cópia por nó participante, então
/// **nenhuma máquina sozinha sabe a lista inteira**: esta rota pergunta a cada
/// peer e junta, que é o mesmo desenho de `survey`. Cópias do mesmo
/// `deploy_id` são deduplicadas ficando com a de `activated_at` mais recente -
/// um nó que estava fora do ar durante um `activate` tem a cópia velha.
///
/// Duas palavras que **não** são sinônimos, e a tela não pode confundir:
///
/// * `serving` - há peer vivo anunciando este `deploy_id`. É "está no ar".
/// * `is_default` - é esta que atende quem pede só o nome do modelo, sem
///   sufixo `@`. Ver `deployment::active`.
///
/// Uma implantação pode estar no ar sem ser a padrão do seu modelo.
async fn list_deployments(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<ListDeploymentsQuery>,
) -> Response {
    let self_id = state.network.node_id.clone();
    if q.local {
        return Json(local_deployments(&self_id)).into_response();
    }

    let peers = state.all_peers().await;
    let key = state.auth.read().await.connection_key.clone();

    // Por `deploy_id`, ficando com a cópia mais recentemente ativada.
    let mut por_id: HashMap<String, crate::deployment::Deployment> = HashMap::new();
    let mut guardado_por: HashMap<String, Vec<String>> = HashMap::new();

    let absorve = |quem: &str,
                   registros: Vec<crate::deployment::Deployment>,
                   por_id: &mut HashMap<String, crate::deployment::Deployment>,
                   guardado_por: &mut HashMap<String, Vec<String>>| {
        for d in registros {
            guardado_por
                .entry(d.deploy_id.clone())
                .or_default()
                .push(quem.to_string());
            match por_id.get(&d.deploy_id) {
                Some(atual) if atual.activated_at >= d.activated_at => {}
                _ => {
                    por_id.insert(d.deploy_id.clone(), d);
                }
            }
        }
    };

    absorve(
        &self_id,
        crate::deployment::all(),
        &mut por_id,
        &mut guardado_por,
    );

    let buscas = peers.iter().filter(|p| p.node_id != self_id).map(|peer| {
        let http = state.peer_http.for_peer(&peer.node_id);
        let key = key.clone();
        let node_id = peer.node_id.clone();
        let base = peer.gateway_url();
        async move {
            let base = base?;
            let resp = http
                .ok()?
                .get(format!("{base}/api/ui/deployments?local=true"))
                .bearer_auth(&key)
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .ok()?;
            let body: serde_json::Value = resp.json().await.ok()?;
            let registros: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            Some((node_id, registros))
        }
    });

    for (node_id, registros) in futures::future::join_all(buscas)
        .await
        .into_iter()
        .flatten()
    {
        absorve(&node_id, registros, &mut por_id, &mut guardado_por);
    }

    let todas: Vec<crate::deployment::Deployment> = {
        let mut v: Vec<_> = por_id.into_values().collect();
        v.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
        v
    };

    // "No ar" é o que os peers anunciam, não o que o registro diz: o registro
    // sobrevive a um nó cair, e mostrá-lo como servindo seria mentir para quem
    // está tentando entender por que a inferência parou.
    let anunciados: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.deploy_id.is_empty())
        .map(|p| p.deploy_id.clone())
        .collect();

    let linhas: Vec<serde_json::Value> = todas
        .iter()
        .map(|d| {
            let padrao = crate::deployment::active(&todas, &d.model.label)
                .is_some_and(|a| a.deploy_id == d.deploy_id);
            let mut holders = guardado_por.get(&d.deploy_id).cloned().unwrap_or_default();
            holders.sort();
            holders.dedup();
            json!({
                "deployment": d,
                "serving": anunciados.contains(&d.deploy_id),
                "is_default": padrao,
                "holders": holders,
            })
        })
        .collect();

    Json(json!({ "node_id": self_id, "deployments": linhas })).into_response()
}

/// Recebe do coordenador o registro de uma implantação de que este nó faz parte.
///
/// Guardar é o que permite a este nó, depois de um reinício, dizer de que
/// conjunto ele fazia parte - e é o que faz a listagem do cluster sobreviver à
/// queda de quem coordenou.
async fn receive_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(registro): Json<crate::deployment::Deployment>,
) -> Response {
    let self_id = state.network.node_id.clone();
    // Um registro que não menciona esta máquina não é dela para guardar. Sem
    // isto, qualquer peer autenticado encheria o disco dos outros de registros
    // de implantações que não os incluem.
    if registro.node(&self_id).is_none() {
        return bad(json!({
            "error": "este nó não faz parte da implantação"
        }));
    }
    // Aceita mesmo incompleto - uma implantação que não fecha o modelo é um
    // estado real do cluster, e recusar o registro só faria a máquina esquecer
    // de que conjunto ela faz parte. Mas dizer em voz alta, porque uma
    // cobertura furada só se manifestaria muito depois, como "Gap at layer N"
    // na primeira requisição.
    if !registro.covers_the_model() {
        warn!(
            "implantação {} não cobre as {} camadas de {}",
            registro.deploy_id, registro.model.total_layers, registro.model.label
        );
    }
    match crate::deployment::save(&registro) {
        Ok(_) => Json(json!({ "stored": true, "deploy_id": registro.deploy_id })).into_response(),
        Err(e) => bad(json!({ "error": format!("{e:#}") })),
    }
}

/// Camadas que a faixa nova compartilha com as que ja estao no ar.
///
/// Nao e recusa: e o que a tela mostra ao operador para ele saber que esta
/// criando uma rota paralela em vez de preencher um buraco.
pub fn overlapping_layers(existentes: &[(u32, u32)], nova: (u32, u32)) -> Vec<u32> {
    (nova.0..=nova.1)
        .filter(|c| existentes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

/// Posicao desta faixa na ordem das camadas, e quantas faixas ficam no total.
///
/// `peer_index`/`peer_count` nao sao a lista de alvos de uma implantacao: sao
/// a posicao no pipeline, que e a ordem das camadas. Quem soma um no de cada
/// vez precisa dizer isso explicitamente, senao o unico alvo da requisicao
/// vira "peer 1 de 1" e todo no somado se declara o ultimo da divisao.
///
/// Réplicas da mesma faixa ocupam o mesmo estágio lógico, mesmo quando entram
/// pelo endpoint incremental de adicionar uma máquina.
pub fn split_position(existentes: &[(u32, u32)], nova: (u32, u32)) -> (u32, u32) {
    let mut faixas: Vec<(u32, u32)> = existentes.to_vec();
    faixas.push(nova);
    faixas.sort();
    faixas.dedup();
    let index = faixas.iter().position(|f| *f == nova).unwrap_or(0) as u32;
    (index, faixas.len() as u32)
}

/// Camadas que ficam sem ninguem depois de tirar esta maquina.
///
/// Vazio significa que outra maquina cobre a mesma faixa - a rede perde uma
/// rota paralela e segue servindo, e o aviso ao operador e ameno. Nao vazio
/// significa buraco, e ai a confirmacao e explicita.
pub fn uncovered_after_removal(restantes: &[(u32, u32)], removida: (u32, u32)) -> Vec<u32> {
    (removida.0..=removida.1)
        .filter(|c| !restantes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

/// One line of the operator's own answer: this range, on that machine.
#[derive(Debug, Clone, Deserialize)]
pub struct SelectedSlice {
    pub start_layer: u32,
    pub end_layer: u32,
    /// Empty means the operator has not chosen a machine for it yet, which is a
    /// refusal rather than an invitation for the node to pick one.
    #[serde(default)]
    pub target_node_id: String,
    /// Layers this machine puts on its GPU, when it should not use the
    /// cluster-wide number. See `LaunchTarget::n_gpu_layers` for why offload is
    /// decided per machine.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    /// Whether this machine's KV cache lives on its GPU. `None` takes the
    /// cluster's answer.
    ///
    /// Per machine for the same reason offload is: the cache is sized by the
    /// context window, and a box with 16 GB of VRAM and a CPU-only box in one
    /// pipeline cannot both be right about where it fits. One cluster-wide
    /// switch meant the machine with less memory decided for the machine with
    /// more, or the other way round.
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

/// What one machine of a deploy disagrees with the cluster about.
///
/// Everything else in `RuntimeParams` has to agree across the pipeline — the
/// context window and batch set the prompt budget the gateway enforces, the
/// sampling only runs on the last peer — so only what is genuinely a property
/// of the hardware belongs here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerOverrides {
    pub n_gpu_layers: Option<u32>,
    pub offload_kqv: Option<bool>,
}

/// What is being put into service: a set of slices, or one whole model.
///
/// Not two flows. The whole-model case is read into the same `HeldShard` /
/// `Candidate` / `Step` shapes as a slice covering every layer, so the planner,
/// the disk and memory checks, the transfer phase and `launch` are one code
/// path. This only says which files the survey looks at, and which noun the
/// refusals use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployKind {
    /// A set of slices that together cover the model. The default, and what
    /// every caller written before whole-model deploys existed sends.
    #[default]
    Shards,
    /// One un-sliced GGUF, served whole by a single machine.
    Whole,
    /// One model split across a group of machines by tensor parallelism
    /// (caminho C): um primary + K workers. O primary segura o modelo inteiro;
    /// cada worker extrai o próprio shard de tensor da própria cópia local.
    TensorParallel,
}

/// How much of the available cluster the planner should use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStrategy {
    /// Pick the smallest set of peers that can serve the selected model.
    #[default]
    Simple,
    /// Consider every eligible peer to maximize GPU use and spread load.
    Complete,
}

const MAX_DEPLOYMENT_NAME_CHARS: usize = 128;

pub(crate) fn normalize_deployment_name(
    requested: Option<&str>,
    fallback: &str,
) -> Result<String, String> {
    let name = requested
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback.trim());
    if name.is_empty() {
        return Err("o nome da implantação não pode ficar vazio".to_string());
    }
    if name.chars().count() > MAX_DEPLOYMENT_NAME_CHARS {
        return Err(format!(
            "o nome da implantação pode ter no máximo {MAX_DEPLOYMENT_NAME_CHARS} caracteres"
        ));
    }
    Ok(name.to_string())
}

impl DeployKind {
    fn whole(self) -> bool {
        matches!(self, DeployKind::Whole)
    }

    /// How a refusal names the one thing being placed on a machine.
    ///
    /// A whole model has a layer range too — it is 0 to the last one — but
    /// saying "a fatia L0-31" to an operator who picked a whole model names
    /// something that is not on their screen.
    fn unit(self, group: &str, start: u32, end: u32) -> String {
        match self {
            DeployKind::Shards => format!("a fatia L{start}-{end}"),
            DeployKind::Whole => format!("o modelo {group}"),
            DeployKind::TensorParallel => format!("o grupo tensor-parallel {group}"),
        }
    }

    /// The same thing, indefinite, for a sentence about its weight.
    fn indefinite(self) -> &'static str {
        match self {
            DeployKind::Shards => "uma fatia",
            DeployKind::Whole => "um modelo",
            DeployKind::TensorParallel => "um grupo tensor-parallel",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// The shard set, named the way the catalog groups it — or, with
    /// `kind: "whole"`, the model's own name in the catalog.
    pub group: String,
    /// Nome operacional da implantação. `group` continua sendo a identidade
    /// técnica do modelo usada pelo catálogo e pelo roteamento.
    #[serde(default)]
    pub deployment_name: Option<String>,
    #[serde(default)]
    pub strategy: DeployStrategy,
    #[serde(default)]
    pub preferred_node_ids: Vec<String>,
    /// Slices or a whole model. Absent means slices, which is what every
    /// caller written before whole-model deploys existed means.
    #[serde(default)]
    pub kind: DeployKind,
    #[serde(default)]
    pub params: RuntimeParams,
    /// The operator's explicit choice. `None` is the default path: the node
    /// decides, and the screen shows the decision.
    #[serde(default)]
    pub selection: Option<Vec<SelectedSlice>>,
    /// Prefix cut requested from a whole model proposer. The remainder stays
    /// on that proposer as the next pipeline stage.
    #[serde(default)]
    pub whole_slice: Option<WholeSliceSelection>,
    /// Join adjacent slices assigned to the same machine into a single file,
    /// so that run becomes one pipeline step. `false` keeps each selected
    /// range as its own pipeline step.
    #[serde(default)]
    pub merge: bool,
    /// Grupo de tensor parallelism (caminho C). Presente só com
    /// `kind: "tensor_parallel"`: o primary serve o modelo inteiro e os
    /// workers extraem o próprio shard de tensor.
    #[serde(default)]
    pub tp: Option<TpLaunch>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WholeSliceSelection {
    pub source_node_id: String,
    pub target_node_id: String,
    pub end_layer: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WholeLayerOptions {
    pub source_node_id: String,
    pub total_layers: u32,
    pub cuts: Vec<LayerCut>,
    pub granularity: Option<u32>,
    pub rule: String,
    pub reason: Option<String>,
}

/// Especificação de um grupo de tensor parallelism para o deploy.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TpLaunch {
    /// Quantos ranks no grupo (1 primary + size-1 workers).
    pub size: u32,
    /// node_id do primary (rank 0). A URL da coletiva (POST /v1/tp/reduce) é
    /// derivada daqui — da porta de inferência anunciada pelo próprio nó —, não
    /// digitada pelo operador.
    pub primary_node_id: String,
    /// node_ids dos workers (rank 1..size-1), na ordem dos ranks.
    pub worker_node_ids: Vec<String>,
}

fn effective_params_for_kind(kind: DeployKind, params: &RuntimeParams) -> RuntimeParams {
    let mut effective = params.clone();
    if kind == DeployKind::TensorParallel {
        // llama.cpp's common_fit_params explicitly refuses split-mode tensor.
        // Keep the plan honest and make the runtime safety belt independent of
        // whether the caller is a current UI or an older API client.
        effective.fit_memory = false;
    }
    effective
}

// ── What a machine can tell us about itself ──────────────────────────────────

/// One slice of the chosen set, as it exists on one machine.
#[derive(Debug, Clone)]
struct HeldShard {
    start_layer: u32,
    end_layer: u32,
    /// How many layers the *original* model had, when the slice says so.
    ///
    /// `None` is the normal state of an old slice: only `split_model.py` writes
    /// `pom.original_block_count`, so anything cut before it — the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf` — carries no total at all. It stays
    /// an `Option` all the way to `model_size` because a missing total and a
    /// total of zero are different facts, and collapsing them is what let a
    /// deploy start and die at the far end (`runtime::NodeConfig::validate`).
    total_layers: Option<u32>,
    name: String,
    path: String,
    size_bytes: u64,
    /// sha256 da parte, quando ela é endereçada por conteúdo.
    ///
    /// É esta a identidade que o `launch` manda para o destino: com ela o nó
    /// resolve o caminho local sozinho, e duas fatias de modelos diferentes que
    /// por acaso cobrem a mesma faixa deixam de ser a mesma coisa aos olhos de
    /// `Machine::holds`. `None` num modelo inteiro, que não é parte de rodada
    /// nenhuma.
    sha256: Option<String>,
    /// Identidade da rodada de fatiamento que produziu a parte.
    pom_id: Option<String>,
    /// Planner-only range backed by a whole GGUF that still needs local
    /// materialization before it can be transferred or loaded.
    virtual_slice: bool,
    /// Rank TP desta parte, quando a rodada foi fatiada para tensor parallelism.
    tp_rank: Option<u32>,
    /// Número total de ranks desta rodada TP.
    tp_size: Option<u32>,
    /// Whether the actual GGUF metadata identifies this model as MoE.
    is_moe: bool,
    /// Evidência do modelo lida no GGUF, quando o peer conseguiu apurá-la.
    model_capabilities: Option<crate::gguf::ModelCapabilityEvidence>,
}

/// A machine in the cluster, with the slices of this set it already holds.
struct Machine {
    node_id: String,
    hostname: String,
    /// URL do gateway HTTP do peer (para o coordenador falar com ele).
    gateway_url: Option<String>,
    /// URL do servidor de inferência do peer (rank 0 de um grupo TP).
    inference_url: Option<String>,
    /// Todos os endereços anunciados para o servidor de inferência, na ordem
    /// em que este coordenador os conhece.
    inference_candidates: Vec<String>,
    /// Endereços UDP anunciados pelo peer, para reidratar o túnel libp2p.
    addr_candidates: Vec<String>,
    /// Identidade e capacidade do transporte alternativo do servidor de
    /// inferência, preservadas para o grupo TP.
    libp2p_peer_id: Option<String>,
    supports_tunnel: bool,
    /// Capacidades runtime anunciadas pelo peer selecionado.
    inference_capabilities: Option<Vec<String>>,
    /// Memory the weights would have to fit in: free VRAM on a machine with a
    /// discrete GPU, available RAM anywhere else. `0` means the machine did not
    /// answer, and nothing is inferred from that.
    mem_free_mb: u64,
    gpu_free_mb: u64,
    ram_free_mb: u64,
    /// Room for the file itself, which is what a copy needs and what
    /// `transfer.rs` checks before it writes a byte.
    disk_free_mb: u64,
    rtt_ms: Option<f64>,
    load: u32,
    held: Vec<HeldShard>,
}

#[derive(Default, Clone)]
struct PairwiseRtt {
    values: std::collections::HashMap<(String, String), f64>,
}

impl PairwiseRtt {
    fn remember(&mut self, from_node_id: &str, measurement: &crate::route::RttMeasurement) {
        self.values.insert(
            (from_node_id.to_string(), measurement.target_node_id.clone()),
            measurement.rtt_ms,
        );
    }

    fn get(&self, from_node_id: &str, to_node_id: &str) -> Option<f64> {
        self.values
            .get(&(from_node_id.to_string(), to_node_id.to_string()))
            .copied()
    }
}

struct Survey {
    machines: Vec<Machine>,
    pairwise_rtt: PairwiseRtt,
    metadata_pending: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RttReport {
    source_node_id: String,
    measurements: Vec<crate::route::RttMeasurement>,
}

impl Machine {
    fn holds(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.held
            .iter()
            .find(|s| s.start_layer == start && s.end_layer == end)
    }
    fn holds_ready(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.held
            .iter()
            .find(|s| s.start_layer == start && s.end_layer == end && !s.virtual_slice)
    }
    fn holds_tp(&self, start: u32, end: u32, rank: u32, size: u32) -> Option<&HeldShard> {
        self.held.iter().find(|s| {
            s.start_layer == start
                && s.end_layer == end
                && s.tp_rank == Some(rank)
                && s.tp_size == Some(size)
        })
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
/// its own files are, and for a path we would otherwise have to guess — and its
/// `/api/ui/node`, which is the only honest source for how much room it has.
/// Porta de teste para `survey`, que e privado: a suite precisa provar que o
/// levantamento fala com os peers pelo cliente pinado. Ver
/// `webui::catalogo_sob_tls_tests`.
#[cfg(test)]
pub(crate) async fn survey_for_tests(state: &GatewayState, group: &str) -> usize {
    survey(state, group, DeployKind::Shards, None, None)
        .await
        .machines
        .len()
}

/// Whether a peer's partial catalog contains the model family being planned.
///
/// `metadata_complete` is deliberately not enough to answer this question:
/// it describes the peer's whole catalog, while deployment only needs the
/// selected ranges. The group presence also keeps a slow, unrelated model
/// from turning into a refusal for this deployment.
fn catalog_contains_group(body: &serde_json::Value, group: &str, kind: DeployKind) -> bool {
    let in_shards = |set: &serde_json::Value| {
        set.get("group").and_then(|value| value.as_str()) == Some(group)
            || set.get("model_label").and_then(|value| value.as_str()) == Some(group)
    };

    match kind {
        DeployKind::Shards => body
            .get("shard_sets")
            .and_then(|value| value.as_array())
            .is_some_and(|sets| {
                sets.iter()
                    .any(|set| set.get("group").and_then(|value| value.as_str()) == Some(group))
            }),
        DeployKind::Whole => {
            body.get("models")
                .and_then(|value| value.as_array())
                .is_some_and(|models| {
                    models.iter().any(|model| {
                        model.get("name").and_then(|value| value.as_str()) == Some(group)
                    })
                })
                || body
                    .get("shard_sets")
                    .and_then(|value| value.as_array())
                    .is_some_and(|sets| sets.iter().any(in_shards))
        }
        DeployKind::TensorParallel => {
            body.get("shard_sets")
                .and_then(|value| value.as_array())
                .is_some_and(|sets| sets.iter().any(in_shards))
                || body
                    .get("models")
                    .and_then(|value| value.as_array())
                    .is_some_and(|models| {
                        models.iter().any(|model| {
                            model.get("name").and_then(|value| value.as_str()) == Some(group)
                        })
                    })
        }
    }
}

fn selected_metadata_pending_reason(
    kind: DeployKind,
    group: &str,
    missing: &[(u32, u32)],
) -> String {
    match (kind, missing.first()) {
        (DeployKind::Whole, _) => format!(
            "a seleção de {group} ainda aguarda os metadados do GGUF; aguarde a leitura desta seleção"
        ),
        (DeployKind::Shards, Some((start, end))) => format!(
            "a seleção da fatia L{start}-{end} ainda aguarda os metadados do GGUF; aguarde a leitura desta seleção"
        ),
        (DeployKind::TensorParallel, Some((start, end))) => format!(
            "a seleção tensor-parallel L{start}-{end} ainda aguarda os metadados do GGUF; aguarde a leitura desta seleção"
        ),
        (_, None) => format!(
            "a seleção de {group} ainda aguarda os metadados do GGUF; aguarde a leitura desta seleção"
        ),
    }
}

fn selected_metadata_pending_reason_for(
    kind: DeployKind,
    group: &str,
    selected_ranges: &[(u32, u32)],
    tp: Option<&TpLaunch>,
    machines: &[Machine],
) -> Option<String> {
    if kind == DeployKind::TensorParallel && has_tp_parts(machines) {
        if let Some(tp) = tp {
            let total_layers = model_size(&candidates(machines))
                .value()
                .or_else(|| selected_ranges.iter().map(|(_, end)| end + 1).max());
            if let Some(total_layers) = total_layers {
                let end_layer = total_layers.saturating_sub(1);
                let missing_ranks = (0..tp.size)
                    .filter(|rank| {
                        !machines.iter().any(|machine| {
                            machine.held.iter().any(|shard| {
                                shard.start_layer == 0
                                    && shard.end_layer == end_layer
                                    && shard.tp_rank == Some(*rank)
                                    && shard.tp_size == Some(tp.size)
                                    && !shard.virtual_slice
                            })
                        })
                    })
                    .map(|rank| rank + 1)
                    .collect::<Vec<_>>();
                if !missing_ranks.is_empty() {
                    return Some(format!(
                        "a seleção tensor-parallel de {group} ainda aguarda os metadados do GGUF dos ranks {}; aguarde a leitura desta seleção",
                        missing_ranks
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        return None;
    }

    let missing = selected_ranges
        .iter()
        .copied()
        .filter(|(start, end)| {
            !machines
                .iter()
                .any(|machine| machine.holds_ready(*start, *end).is_some())
        })
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| selected_metadata_pending_reason(kind, group, &missing))
}

async fn survey(
    state: &GatewayState,
    group: &str,
    kind: DeployKind,
    selection: Option<&[SelectedSlice]>,
    tp: Option<&TpLaunch>,
) -> Survey {
    let selected_ranges = selection.map(|selected| {
        selected
            .iter()
            .map(|slice| (slice.start_layer, slice.end_layer))
            .collect::<Vec<_>>()
    });
    let peers: Vec<PeerInfo> = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let mut route_rtt: std::collections::HashMap<String, f64> = state
        .network
        .routes
        .snapshot()
        .into_iter()
        .map(|(node_id, route)| (node_id, route.rtt_ms))
        .collect();
    // A medição libp2p deste gateway é a fonte correta para os cards das
    // máquinas. A RouteTable HTTP fica somente como fallback para peers
    // antigos ou enquanto a primeira sonda libp2p ainda não respondeu.
    for measurement in state.network.rtts.snapshot() {
        route_rtt.insert(measurement.target_node_id, measurement.rtt_ms);
    }
    let mut pairwise_rtt = PairwiseRtt::default();
    for measurement in state.network.rtts.snapshot() {
        pairwise_rtt.remember(&self_id, &measurement);
    }
    for (node_id, rtt_ms) in &route_rtt {
        if pairwise_rtt.get(&self_id, node_id).is_none() {
            pairwise_rtt
                .values
                .insert((self_id.clone(), node_id.clone()), *rtt_ms);
        }
    }
    let loads = state.loads.clone();
    let connection_key = state.auth.read().await.connection_key.clone();

    let futures = peers.into_iter().map(|peer| {
        // Pinado no node_id do destino: sem isto, com o cluster em TLS o
        // levantamento inteiro falhava calado e a tela de implantar dizia
        // "nenhuma máquina do cluster tem fatias de <modelo>".
        let http = state.peer_http.for_peer(&peer.node_id);
        let is_self = peer.node_id == self_id;
        let rtt_ms = if is_self {
            Some(0.0)
        } else {
            route_rtt.get(&peer.node_id).copied()
        };
        let loads = loads.clone();
        let group = group.to_string();
        let connection_key = connection_key.clone();
        async move {
            let base = peer.gateway_url()?;
            let http = http.ok()?;

            let (models, node, rtt) = futures::future::join3(
                http.get(format!("{base}/api/ui/models"))
                    .timeout(PEER_TIMEOUT * 4)
                    .send(),
                http.get(format!("{base}/api/ui/node"))
                    .timeout(PEER_TIMEOUT)
                    .send(),
                http.get(format!("{base}/api/ui/network/rtt"))
                    .timeout(PEER_TIMEOUT)
                    .bearer_auth(&connection_key)
                    .send(),
            )
            .await;

            let body: serde_json::Value = models.ok()?.json().await.ok()?;
            let metadata_pending_candidate = body
                .get("metadata_complete")
                .and_then(|value| value.as_bool())
                == Some(false)
                && catalog_contains_group(&body, &group, kind);
            let node: serde_json::Value = match node {
                Ok(r) => r.json().await.unwrap_or(json!({})),
                Err(_) => json!({}),
            };
            let addr_candidates = node
                .get("addr_candidates")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| {
                    if peer.addr_candidates.is_empty() {
                        vec![peer.addr.clone()]
                    } else {
                        peer.addr_candidates.clone()
                    }
                });
            let inference_port = node
                .get("inference_port")
                .and_then(|value| value.as_u64())
                .and_then(|value| u16::try_from(value).ok())
                .or(peer.inference_port);
            let inference_candidates = inference_port
                .map(|port| {
                    addr_candidates
                        .iter()
                        .map(|addr| {
                            format!(
                                "{}://{}:{}",
                                crate::peer_client::peer_scheme(),
                                crate::network::ip_host(addr),
                                port
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| peer.inference_candidates());
            let libp2p_peer_id = node
                .get("libp2p_peer_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .or_else(|| peer.libp2p_peer_id.clone());
            let supports_tunnel = node
                .get("inference_frame_capabilities")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .any(|value| value.as_str() == Some(crate::pipeline::CAP_LIBP2P_TUNNEL))
                })
                .unwrap_or_else(|| {
                    peer.inference_frame_capabilities
                        .iter()
                        .any(|value| value == crate::pipeline::CAP_LIBP2P_TUNNEL)
                });
            let inference_capabilities = node
                .get("inference_frame_capabilities")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .or_else(|| {
                    (!peer.inference_frame_capabilities.is_empty())
                        .then(|| peer.inference_frame_capabilities.clone())
                });
            let rtt_report = match rtt {
                Ok(response) if response.status().is_success() => {
                    response.json::<RttReport>().await.ok()
                }
                _ => None,
            };
            let (gpu_free_mb, ram_free_mb) = memory_capacity(node.get("system"));

            let mut held: Vec<HeldShard> = match kind {
                DeployKind::Shards => body
                    .get("shard_sets")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|set| set.get("group").and_then(|g| g.as_str()) == Some(group.as_str()))
                    .flat_map(|set| {
                        let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                        set.get("shards")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .map(move |shard| (shard, pom_id))
                    })
                    .filter_map(|(shard, pom_id)| parse_shard(shard, pom_id))
                    .collect(),
                DeployKind::Whole => {
                    let mut models = body
                        .get("models")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|m| parse_whole(m, &group))
                        .collect::<Vec<_>>();
                    let mut slices = body
                        .get("shard_sets")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter(|set| {
                            set.get("group").and_then(|value| value.as_str())
                                == Some(group.as_str())
                                || set.get("model_label").and_then(|value| value.as_str())
                                    == Some(group.as_str())
                        })
                        .flat_map(|set| {
                            let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                            set.get("shards")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                                .map(move |shard| (shard, pom_id))
                        })
                        .filter_map(|(shard, pom_id)| parse_shard(shard, pom_id))
                        .collect::<Vec<_>>();
                    models.append(&mut slices);
                    models
                }
                DeployKind::TensorParallel => {
                    let parts: Vec<HeldShard> = body
                        .get("shard_sets")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter(|set| {
                            set.get("group").and_then(|g| g.as_str()) == Some(group.as_str())
                                || set.get("model_label").and_then(|g| g.as_str())
                                    == Some(group.as_str())
                        })
                        .flat_map(|set| {
                            let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                            set.get("tp_parts")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                                .map(move |shard| (shard, pom_id))
                        })
                        .filter_map(|(shard, pom_id)| parse_shard(shard, pom_id))
                        .collect();
                    if parts.is_empty() {
                        body.get("models")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|m| parse_whole(m, &group))
                            .collect()
                    } else {
                        parts
                    }
                }
            };
            // Directory order is whatever the filesystem felt like; the screen
            // lists these as "L0–4 · L5–31" and reads as noise unsorted.
            held.sort_by_key(|s| (s.start_layer, s.end_layer, s.tp_rank.unwrap_or(0)));
            // Two copies of the same file on one machine are one range, not
            // two: `candidates` would otherwise list the machine twice as a
            // holder of it. The whole-model path meets this the moment the same
            // GGUF is reachable through two scanned folders — the captain's
            // `~/.ollama/models` is a symlink into the LM Studio library.
            held.dedup_by_key(|s| (s.start_layer, s.end_layer, s.tp_rank, s.tp_size));

            Some((
                Machine {
                    node_id: peer.node_id.clone(),
                    gateway_url: peer.gateway_url(),
                    inference_url: inference_candidates.first().cloned(),
                    inference_candidates,
                    addr_candidates,
                    libp2p_peer_id,
                    supports_tunnel,
                    inference_capabilities,
                    hostname: if peer.hostname.is_empty() && is_self {
                        body.get("hostname")
                            .and_then(|h| h.as_str())
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    mem_free_mb: free_memory_mb(node.get("system")),
                    gpu_free_mb,
                    ram_free_mb,
                    // The peer's own reading beats the one it gossiped minutes ago,
                    // but the gossip is still there when it did not answer.
                    disk_free_mb: node
                        .pointer("/system/disk/free_mb")
                        .and_then(|v| v.as_f64())
                        .map(|v| v as u64)
                        .unwrap_or(peer.disk_free_mb),
                    rtt_ms,
                    load: loads.load(&peer.node_id).await,
                    held,
                },
                rtt_report,
                metadata_pending_candidate,
            ))
        }
    });

    let mut machines = Vec::new();
    let mut metadata_pending_candidate = false;
    for result in futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
    {
        metadata_pending_candidate |= result.2;
        if let Some(report) = &result.1 {
            if report.source_node_id == result.0.node_id {
                for measurement in &report.measurements {
                    pairwise_rtt.remember(&report.source_node_id, measurement);
                }
            }
        }
        machines.push(result.0);
    }

    let metadata_pending = selected_ranges.as_deref().and_then(|selected| {
        if !metadata_pending_candidate {
            return None;
        }
        selected_metadata_pending_reason_for(kind, group, selected, tp, &machines)
    });

    Survey {
        machines,
        pairwise_rtt,
        metadata_pending,
    }
}

/// Where the weights of a slice would actually have to fit.
///
/// A machine with a discrete GPU loads them into VRAM (`--n-gpu-layers 99`), so
/// its RAM figure says nothing useful. Unified memory has no separate pool to
/// report, so RAM *is* the answer there — see the note `sysmon` puts on screen.
fn free_memory_mb(system: Option<&serde_json::Value>) -> u64 {
    let (gpu, ram) = memory_capacity(system);
    if gpu > 0 {
        gpu
    } else {
        ram
    }
}

fn memory_capacity(system: Option<&serde_json::Value>) -> (u64, u64) {
    let Some(system) = system else { return (0, 0) };
    let gpu = system
        .get("gpus")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let total = g.get("vram_total_mb")?.as_f64()?;
            let used = g
                .get("vram_used_mb")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            Some((total - used).max(0.0))
        })
        .sum::<f64>() as u64;
    let ram = system
        .pointer("/ram/available_mb")
        .and_then(|v| v.as_f64())
        .map(|v| v as u64)
        .unwrap_or(0);
    (gpu, ram)
}

fn parse_shard(v: &serde_json::Value, pom_id: Option<&str>) -> Option<HeldShard> {
    let range = v.get("shard_range")?.as_array()?;
    Some(HeldShard {
        start_layer: range.first()?.as_u64()? as u32,
        end_layer: range.get(1)?.as_u64()? as u32,
        total_layers: v
            .get("total_layers")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
            .filter(|t| *t > 0),
        name: v.get("name")?.as_str()?.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
        sha256: v
            .get("sha256")
            .and_then(|h| h.as_str())
            .filter(|h| crate::pom::is_sha256_hex(h))
            .map(|h| h.to_string()),
        pom_id: pom_id.map(str::to_string),
        virtual_slice: false,
        tp_rank: v
            .get("tp_rank")
            .and_then(|rank| rank.as_u64())
            .map(|rank| rank as u32),
        tp_size: v
            .get("tp_size")
            .and_then(|size| size.as_u64())
            .map(|size| size as u32),
        is_moe: v
            .get("is_moe")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        model_capabilities: v
            .get("capabilities")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    })
}

/// One whole model of the catalog, read as the range it covers: all of them.
///
/// This is the whole of the whole-model special case. Everything downstream —
/// the coverage arithmetic, the one-range-per-machine rule, the copy planning,
/// `launch` — then works on a `HeldShard` like any other, and a deploy of one
/// un-sliced GGUF is simply a plan with a single step whose range happens to be
/// the entire model.
///
/// Two files are skipped rather than half-described:
///
/// * a slice (`shard_range` set) — those belong to the other `kind`, and the
///   catalog serves both lists from the same scan;
/// * a model whose GGUF header did not yield a layer count. `total_layers` is
///   the number every peer is configured with, and `runtime::NodeConfig::validate`
///   refuses a zero at the far end — after `launch` has already stopped
///   inference on the target machine.
fn parse_whole(v: &serde_json::Value, group: &str) -> Option<HeldShard> {
    if !v.get("shard_range").map(|r| r.is_null()).unwrap_or(true) {
        return None;
    }
    let name = v.get("name")?.as_str()?;
    if name != group {
        return None;
    }
    let total = v
        .get("total_layers")
        .and_then(|t| t.as_u64())
        .filter(|t| *t > 0)? as u32;
    Some(HeldShard {
        start_layer: 0,
        end_layer: total - 1,
        total_layers: Some(total),
        name: name.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
        // Modelo inteiro não é parte de rodada nenhuma: não tem hash de parte.
        sha256: None,
        pom_id: None,
        virtual_slice: false,
        tp_rank: None,
        tp_size: None,
        is_moe: v
            .get("is_moe")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        model_capabilities: v
            .get("capabilities")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    })
}

// ── The decision ─────────────────────────────────────────────────────────────

/// A range that exists somewhere in the cluster, selected or not.
///
/// The same model is often sliced more than once — 0-4 / 5-31 on one machine and
/// 0-10 / 11-31 on another — and every one of those cuts is a legitimate thing
/// to deploy. The screen needs all of them to offer the choice, so the plan
/// carries the whole menu and not only the part it picked.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    start_layer: u32,
    end_layer: u32,
    /// Only what the slice itself declares. `null` on screen means "this file
    /// does not say how big the model is", which is a fact about the file.
    total_layers: Option<u32>,
    name: String,
    size_bytes: u64,
    /// Every machine that already has this exact range.
    holders: Vec<String>,
    /// Part of the plan being shown.
    selected: bool,
}

/// A machine that could take a range, as the screen has to describe it.
#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    node_id: String,
    label: String,
    mem_free_mb: u64,
    gpu_free_mb: u64,
    ram_free_mb: u64,
    disk_free_mb: u64,
    rtt_ms: Option<f64>,
    load: u32,
    /// Ranges of this set it already holds.
    holds: Vec<[u32; 2]>,
    /// Range it takes in the plan being shown, if any.
    taking: Option<[u32; 2]>,
}

/// One machine offered as the target of one slice, with the reason it works.
#[derive(Debug, Clone, Serialize)]
pub struct Alternative {
    node_id: String,
    label: String,
    /// Already has the file: nothing to copy.
    has: bool,
    /// Nothing about this machine stops the assignment.
    ok: bool,
    /// What it costs, or why it does not work.
    note: Option<String>,
}

/// One slice feeding a merged step: the range, its file, and where it comes
/// from. A merged step serves the union of its parts, joined into one file on
/// the target.
#[derive(Debug, Clone, Serialize)]
pub struct StepPart {
    start_layer: u32,
    end_layer: u32,
    name: String,
    size_bytes: u64,
    /// Already on the target: nothing to copy for this part.
    ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    source_node_id: Option<String>,
    source_host: Option<String>,
    source_path: Option<String>,
    /// Hash da parte na origem. É por ele que a cópia é pedida: assim o
    /// destino grava em `<shard_dir>/<modelo>/<sha>.gguf` em vez de largar o
    /// arquivo solto, e sabe conferir o que chegou.
    source_sha256: Option<String>,
    /// True when the source is a virtual range that still has to be cut out
    /// of its whole GGUF before it can be transferred or launched.
    source_needs_split: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// 1-based place in the pipeline. Position 1 receives the prompt.
    position: u32,
    start_layer: u32,
    end_layer: u32,
    /// The model size this step will be launched with — the plan's answer, not
    /// the file's, so every step of one deploy carries the same number.
    total_layers: Option<u32>,
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
    /// Hash da parte na origem. É por ele que a cópia é pedida: assim o
    /// destino grava em `<shard_dir>/<modelo>/<sha>.gguf` em vez de largar o
    /// arquivo solto, e sabe conferir o que chegou.
    source_sha256: Option<String>,
    /// True when the source is a virtual range backed by a whole GGUF.
    source_needs_split: bool,
    /// Where it will be on the target once it lands, when known up front.
    target_path: Option<String>,
    /// Entry point of the pipeline — whoever holds layer 0.
    primary: bool,
    /// Per-machine GPU offload, when the operator set one. `None` means this
    /// peer takes `params.n_gpu_layers`.
    n_gpu_layers: Option<u32>,
    /// Per-machine KV cache placement. `None` takes `params.offload_kqv`.
    offload_kqv: Option<bool>,
    /// Every machine the operator could move this range to.
    options: Vec<Alternative>,
    /// Set when this assignment cannot be carried out, and why.
    problem: Option<String>,
    /// Set when it can, but the operator should read it first.
    note: Option<String>,
    /// True when this step serves the union of `parts`, joined into one file
    /// on the target, instead of a single slice already in place.
    merged: bool,
    /// The slices this step is assembled from, in layer order. Empty for a
    /// plain step.
    parts: Vec<StepPart>,
}

/// Um rank de uma rodada TP já fatiada. Diferente de um `Step`, cada rank
/// cobre o modelo inteiro e deve ser iniciado no peer correspondente da
/// coletiva, sem passar novamente pelo extrator de shards.
#[derive(Debug, Clone, Serialize)]
pub struct TpTarget {
    rank: u32,
    size: u32,
    start_layer: u32,
    end_layer: u32,
    total_layers: Option<u32>,
    name: String,
    size_bytes: u64,
    target_node_id: String,
    target_host: String,
    ready: bool,
    source_node_id: Option<String>,
    source_host: Option<String>,
    source_path: Option<String>,
    source_sha256: Option<String>,
    target_path: Option<String>,
    problem: Option<String>,
    n_gpu_layers: Option<u32>,
    offload_kqv: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    group: String,
    deployment_name: String,
    strategy: DeployStrategy,
    /// Echoed so the screen never has to guess which question it asked. A whole
    /// model plan has exactly one step covering every layer.
    kind: DeployKind,
    /// True only when the surveyed GGUF metadata reports more than one expert.
    /// The UI must never infer this from a filename or architecture label.
    is_moe: bool,
    /// Whether this plan can honor the requested inference mode.
    inference_mode_supported: bool,
    /// Explicit reason when the selected execution mode cannot be honored.
    inference_mode_reason: Option<String>,
    /// Capacidades condicionais apuradas para os alvos deste plano.
    capabilities: crate::capabilities::DeploymentCapabilities,
    /// Real cut points from the whole GGUF proposer, when this model can be
    /// split locally for a destination that lacks the selected slice.
    layer_options: Option<WholeLayerOptions>,
    /// Grupo de tensor parallelism, quando kind == tensor_parallel. O launch
    /// usa isto para gerar os alvos dos workers.
    tp: Option<TpLaunch>,
    /// How many layers the original model has. `None` means nobody could say —
    /// which is a refusal (`blocked`), never a zero passed down the line.
    total_layers: Option<u32>,
    /// True when that number was read off the coverage of the slices instead of
    /// out of their metadata. The deploy still runs; the screen has to say so,
    /// because a set of slices that stops at L31 looks exactly the same whether
    /// the model has 32 layers or 40 and the tail is missing from the cluster.
    total_layers_inferred: bool,
    steps: Vec<Step>,
    /// Alvos por rank quando o grupo foi obtido de `tp_parts`. Fica vazio no
    /// caminho de fallback que extrai os ranks de modelos inteiros.
    tp_targets: Vec<TpTarget>,
    /// Every layer from 0 to `total_layers` is covered exactly once.
    complete: bool,
    /// Bytes that have to cross the network before this can serve.
    transfer_bytes: u64,
    /// Set when the plan cannot be carried out at all, and why.
    blocked: Option<String>,
    /// True when the node chose this, false when it is validating the
    /// operator's own selection.
    auto: bool,
    /// True when the operator asked each machine's adjacent slices to be
    /// joined into one file — the plan reflects it in its steps.
    merge: bool,
    /// Every range anyone holds, so the screen can offer the ones left out.
    available: Vec<Candidate>,
    /// Every machine, so the screen can offer them as targets.
    machines: Vec<MachineView>,
    /// Layer ranges the selection does not cover.
    missing: Vec<[u32; 2]>,
    /// Layer ranges the selection covers more than once. Never empty together
    /// with `blocked: None`.
    overlap: Vec<[u32; 2]>,
    /// Things worth reading that do not stop the deploy.
    warnings: Vec<String>,
    /// The execution settings this deploy would launch with, defaults filled
    /// in. Echoed rather than left to the screen so the form starts from the
    /// node's own numbers instead of a second copy of them in TypeScript.
    params: RuntimeParams,
    /// Prompt tokens one request will be allowed, under `params`. This is the
    /// number that decides whether a harness fits — see `gateway::prompt_budget`.
    prompt_tokens: u32,
    /// Prompt tokens per round trip through the pipeline, under `params`.
    chunk_tokens: u32,
    /// Rotas candidatas entre os hops escolhidos. A latência pode ser nula
    /// quando o gateway ainda não mediu o peer.
    route_options: Vec<RouteOption>,
    load_distribution: Vec<LoadDistribution>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteOption {
    from_node_id: String,
    to_node_id: String,
    rtt_ms: Option<f64>,
    selected: bool,
    note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadDistribution {
    node_id: String,
    current: u32,
    projected: u32,
    selected: bool,
}

#[derive(Debug, Clone)]
struct RequestedModelIdentity {
    proof: Option<crate::deployment::DeploymentIdentity>,
    /// Caminhos só valem como prova no peer que os anunciou. Eles existem
    /// mesmo quando o hash não pôde ser calculado, para a reserva aprovada.
    paths_by_peer: HashMap<String, String>,
}

fn deployment_identity_matches(
    desired: &RequestedModelIdentity,
    current: &crate::deployment::DeploymentModel,
    peer_id: &str,
    current_path: Option<&str>,
) -> bool {
    let known_current = current.identity.clone().or_else(|| {
        current
            .pom_id
            .clone()
            .map(|pom_id| crate::deployment::DeploymentIdentity::PomId { pom_id })
    });
    if desired.proof.is_some() && desired.proof == known_current {
        return true;
    }
    desired
        .paths_by_peer
        .get(peer_id)
        .zip(current_path)
        .is_some_and(|(desired_path, current_path)| desired_path == current_path)
}

fn node_is_idle(snapshot: &serde_json::Value) -> bool {
    snapshot
        .get("config")
        .is_some_and(|config| config.is_null())
        && snapshot
            .pointer("/status/state")
            .and_then(|state| state.as_str())
            == Some("disabled")
        && snapshot
            .get("active_deployment")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .is_empty()
        && snapshot
            .get("active_model")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .is_empty()
}

fn node_state_detail(snapshot: &serde_json::Value) -> String {
    format!(
        "estado={}, modelo={}, implantação={}",
        snapshot
            .pointer("/status/state")
            .and_then(|value| value.as_str())
            .unwrap_or("desconhecido"),
        snapshot
            .get("active_model")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("nenhum"),
        snapshot
            .get("active_deployment")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("nenhuma"),
    )
}

const DEPLOY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEPLOY_IDLE_POLL: Duration = Duration::from_millis(250);

/// Every distinct range anyone holds, in layer order.
fn candidates(machines: &[Machine]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for m in machines {
        for s in &m.held {
            match out
                .iter_mut()
                .find(|c| c.start_layer == s.start_layer && c.end_layer == s.end_layer)
            {
                Some(c) if !c.holders.contains(&m.node_id) => c.holders.push(m.node_id.clone()),
                Some(_) => {}
                None => out.push(Candidate {
                    start_layer: s.start_layer,
                    end_layer: s.end_layer,
                    total_layers: s.total_layers,
                    name: s.name.clone(),
                    size_bytes: s.size_bytes,
                    holders: vec![m.node_id.clone()],
                    selected: false,
                }),
            }
        }
    }
    out.sort_by_key(|c| (c.start_layer, c.end_layer));
    out
}

/// How many layers the model being deployed has, and how we know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSize {
    /// A slice said so, in `pom.original_block_count`.
    Declared(u32),
    /// No slice said so, but the ranges reach this far from layer 0, which is
    /// the same reading `catalog::build_shard_sets` makes for one machine.
    Inferred(u32),
    /// Nothing to read and nothing to deduce: no slice declares a total and
    /// none of them starts at layer 0.
    Unknown,
}

impl ModelSize {
    fn value(self) -> Option<u32> {
        match self {
            ModelSize::Declared(n) | ModelSize::Inferred(n) => Some(n),
            ModelSize::Unknown => None,
        }
    }
    fn inferred(self) -> bool {
        matches!(self, ModelSize::Inferred(_))
    }
}

/// Decide the model's layer count from every range the cluster holds.
///
/// Declared beats deduced, and the deduction is deliberately cluster-wide: one
/// machine holding only L16–31 can conclude nothing on its own, while the two
/// machines together plainly reach layer 32. Overlapping cuts of the same model
/// (0–4 / 5–31 next to 0–15 / 16–31) are why this walks the reach instead of
/// summing lengths.
fn model_size(candidates: &[Candidate]) -> ModelSize {
    if let Some(declared) = candidates.iter().filter_map(|c| c.total_layers).max() {
        return ModelSize::Declared(declared);
    }
    let mut reach = 0u32;
    loop {
        let next = candidates
            .iter()
            .filter(|c| c.start_layer <= reach && c.end_layer >= reach)
            .map(|c| c.end_layer + 1)
            .max();
        match next {
            Some(n) => reach = n,
            None => break,
        }
    }
    if reach > 0 {
        ModelSize::Inferred(reach)
    } else {
        ModelSize::Unknown
    }
}

/// The slices that carry no layer total, named the way the operator sees them
/// on screen, so a refusal points at files instead of at a concept.
fn nameless(candidates: &[Candidate]) -> String {
    const SHOWN: usize = 4;
    let missing: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.total_layers.is_none())
        .collect();
    let mut out = missing
        .iter()
        .take(SHOWN)
        .map(|c| format!("L{}-{} em {}", c.start_layer, c.end_layer, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    if missing.len() > SHOWN {
        out.push_str(&format!(" e mais {}", missing.len() - SHOWN));
    }
    out
}

/// The chain the node would pick on its own: start at layer 0 and always take
/// the longest range that begins where the last one ended.
///
/// Longest-first is not an aesthetic choice. The chain with the fewest slices
/// is the one most likely to fit the cluster, and taking ranges only where the
/// previous one ended makes overlap impossible by
/// construction — which matters the moment a model has been sliced twice with
/// different cuts. When nothing continues the chain the sweep jumps to the next
/// range that starts later, which leaves a hole and says so.
fn auto_chain(candidates: &[Candidate], total: Option<u32>) -> Vec<usize> {
    let total = total.unwrap_or(0);
    let mut chain = Vec::new();
    let mut next = 0u32;
    loop {
        let pick = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.start_layer == next)
            .max_by_key(|(_, c)| (c.end_layer, c.holders.len()))
            .map(|(i, _)| i)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.start_layer > next)
                    .min_by_key(|(_, c)| (c.start_layer, std::cmp::Reverse(c.end_layer)))
                    .map(|(i, _)| i)
            });
        let Some(i) = pick else { break };
        chain.push(i);
        next = candidates[i].end_layer.saturating_add(1);
        if total > 0 && next >= total {
            break;
        }
    }
    chain
}

/// Complete mode favors the finest existing cut so more eligible peers can
/// participate. It never invents a new boundary and still refuses overlap in
/// the common coverage pass below.
fn auto_chain_complete(candidates: &[Candidate], total: Option<u32>) -> Vec<usize> {
    let total = total.unwrap_or(0);
    let mut chain = Vec::new();
    let mut next = 0u32;
    loop {
        let pick = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.start_layer == next)
            .min_by_key(|(_, c)| (c.end_layer, std::cmp::Reverse(c.holders.len())))
            .map(|(i, _)| i)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.start_layer > next)
                    .min_by_key(|(_, c)| (c.start_layer, c.end_layer))
                    .map(|(i, _)| i)
            });
        let Some(i) = pick else { break };
        chain.push(i);
        next = candidates[i].end_layer.saturating_add(1);
        if total > 0 && next >= total {
            break;
        }
    }
    chain
}

/// Hand each range in the chain to a machine.
///
/// A machine that already holds a slice is preferred because a copy is costly,
/// but a peer without that file is still a valid target when its disk and
/// combined GPU/RAM capacity can receive it. Complete mode then breaks ties by
/// the measured RTT, current in-flight load and available capacity. This keeps
/// the planner data-driven: node ids are only values from the live survey.
fn target_capacity_mb(machine: &Machine) -> u64 {
    machine
        .mem_free_mb
        .max(machine.gpu_free_mb.saturating_add(machine.ram_free_mb))
}

fn target_capacity_class(candidate: &Candidate, machine: &Machine) -> u8 {
    if machine
        .holds_ready(candidate.start_layer, candidate.end_layer)
        .is_some()
    {
        return 0;
    }
    let need = needed_mb(candidate.size_bytes);
    let capacity = target_capacity_mb(machine);
    if capacity == 0 {
        return 3;
    }
    if capacity >= need {
        if machine.gpu_free_mb >= need {
            1
        } else {
            2
        }
    } else {
        4
    }
}

fn auto_assign(
    chain: &[usize],
    candidates: &[Candidate],
    machines: &[Machine],
    strategy: DeployStrategy,
    preferred_node_ids: &[String],
) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    let mut targets = vec![String::new(); chain.len()];

    for (slot, &ci) in chain.iter().enumerate() {
        let c = &candidates[ci];
        let mut possible: Vec<&Machine> = machines.iter().collect();
        possible.sort_by_key(|m| {
            (
                taken.contains(&m.node_id),
                target_capacity_class(c, m),
                !c.holders.contains(&m.node_id),
                !preferred_node_ids.iter().any(|id| id == &m.node_id),
                if strategy == DeployStrategy::Complete {
                    m.rtt_ms
                        .map(|rtt| (rtt.max(0.0) * 1000.0) as u64)
                        .unwrap_or(u64::MAX)
                } else {
                    0
                },
                if strategy == DeployStrategy::Complete {
                    m.load
                } else {
                    0
                },
                std::cmp::Reverse(m.mem_free_mb),
            )
        });
        if let Some(m) = possible.first() {
            if !taken.contains(&m.node_id) {
                taken.push(m.node_id.clone());
            }
            targets[slot] = m.node_id.clone();
        }
    }

    targets
}

/// Roughly what the weights need once loaded, in MB, with the same headroom the
/// slicing planner reserves.
fn needed_mb(size_bytes: u64) -> u64 {
    size_bytes / MIB
}

fn usable_mb(free_mb: u64) -> u64 {
    (free_mb as f64 * (1.0 - DEFAULT_RESERVE_PCT / 100.0)) as u64
}

/// Decide who serves what — or check what the operator decided.
#[cfg(test)]
fn plan(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
) -> Plan {
    plan_with_options(
        group,
        machines,
        selection,
        params,
        merge,
        kind,
        tp,
        DeployStrategy::Simple,
        group,
        &[],
    )
}

#[cfg(test)]
fn plan_with_options(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
    strategy: DeployStrategy,
    deployment_name: &str,
    preferred_node_ids: &[String],
) -> Plan {
    plan_with_rtt_options(
        group,
        machines,
        selection,
        params,
        merge,
        kind,
        tp,
        strategy,
        deployment_name,
        preferred_node_ids,
        &PairwiseRtt::default(),
        None,
    )
}

fn combine_known(values: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut found = false;
    for value in values {
        found = true;
        if value == Some(false) {
            return Some(false);
        }
        if value.is_none() {
            return None;
        }
    }
    found.then_some(true)
}

fn selected_model_capability(
    steps: &[Step],
    machines: &[Machine],
    select: impl Fn(&crate::gguf::ModelCapabilityEvidence) -> Option<bool>,
) -> Option<bool> {
    let mut values = Vec::new();
    for step in steps {
        let ranges = if step.parts.is_empty() {
            vec![(step.start_layer, step.end_layer)]
        } else {
            step.parts
                .iter()
                .map(|part| (part.start_layer, part.end_layer))
                .collect()
        };
        for (start_layer, end_layer) in ranges {
            let evidence = machines
                .iter()
                .find(|machine| machine.node_id == step.target_node_id)
                .and_then(|machine| machine.holds(start_layer, end_layer))
                .or_else(|| {
                    machines
                        .iter()
                        .find_map(|machine| machine.holds(start_layer, end_layer))
                })
                .and_then(|shard| shard.model_capabilities.as_ref())
                .and_then(&select);
            values.push(evidence);
        }
    }
    combine_known(values.into_iter())
}

fn selected_final_model_capability(
    steps: &[Step],
    machines: &[Machine],
    select: impl Fn(&crate::gguf::ModelCapabilityEvidence) -> Option<bool>,
) -> Option<bool> {
    let step = steps.last()?;
    let (start_layer, end_layer) = step
        .parts
        .last()
        .map(|part| (part.start_layer, part.end_layer))
        .unwrap_or((step.start_layer, step.end_layer));
    machines
        .iter()
        .find(|machine| machine.node_id == step.target_node_id)
        .and_then(|machine| machine.holds(start_layer, end_layer))
        .or_else(|| {
            machines
                .iter()
                .find_map(|machine| machine.holds(start_layer, end_layer))
        })
        .and_then(|shard| shard.model_capabilities.as_ref())
        .and_then(select)
}

fn selected_runtime_capability(
    steps: &[Step],
    machines: &[Machine],
    capability: &str,
) -> Option<bool> {
    combine_known(steps.iter().map(|step| {
        machines
            .iter()
            .find(|machine| machine.node_id == step.target_node_id)
            .and_then(|machine| machine.inference_capabilities.as_ref())
            .map(|caps| caps.iter().any(|candidate| candidate == capability))
    }))
}

fn selected_runtime_capability_for_step(
    step: &Step,
    machines: &[Machine],
    capability: &str,
) -> Option<bool> {
    machines
        .iter()
        .find(|machine| machine.node_id == step.target_node_id)
        .and_then(|machine| machine.inference_capabilities.as_ref())
        .map(|caps| caps.iter().any(|candidate| candidate == capability))
}

fn runtime_capability_for_ui(
    value: Option<bool>,
    source: &str,
    supported_reason: &str,
    unsupported_reason: &str,
    unknown_reason: &str,
) -> crate::capabilities::Capability {
    match value {
        Some(true) => crate::capabilities::supported(source, supported_reason),
        Some(false) => {
            let mut capability = crate::capabilities::supported(source, unsupported_reason);
            capability.warning = Some(format!(
                "Limitação do runtime: {unsupported_reason}; a opção permanece acionável e pode não ser executada."
            ));
            capability
        }
        None => {
            let mut capability = crate::capabilities::unknown(source, unknown_reason);
            capability.warning = Some(
                "O runtime não confirmou esta capacidade; ligá-la é por conta e risco do usuário."
                    .to_string(),
            );
            capability
        }
    }
}

fn capability_from_bool(
    value: Option<bool>,
    source: &str,
    supported_reason: &str,
    unsupported_reason: &str,
    unknown_reason: &str,
) -> crate::capabilities::Capability {
    match value {
        Some(true) => crate::capabilities::supported(source, supported_reason),
        Some(false) => crate::capabilities::unsupported(source, unsupported_reason),
        None => crate::capabilities::unknown(source, unknown_reason),
    }
}

fn no_embeddings_topology(
    steps: &[Step],
    total_layers: Option<u32>,
    kind: DeployKind,
) -> crate::capabilities::NoEmbeddingsEvaluation {
    let mut ranges = Vec::new();
    if kind == DeployKind::Shards || (kind == DeployKind::Whole && steps.len() > 1) {
        for step in steps {
            let range = (step.start_layer, step.end_layer);
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    } else if !steps.is_empty() {
        ranges.push((steps[0].start_layer, steps[0].end_layer));
    }
    let count = if ranges.is_empty() {
        None
    } else {
        Some(ranges.len() as u32)
    };
    let peers = steps
        .iter()
        .map(|step| {
            let index = ranges
                .iter()
                .position(|range| *range == (step.start_layer, step.end_layer))
                .map(|value| value as u32);
            crate::capabilities::PeerTopology {
                node_id: step.target_node_id.clone(),
                start_layer: Some(step.start_layer),
                end_layer: Some(step.end_layer),
                total_layers,
                peer_index: index,
                peer_count: count,
            }
        })
        .collect::<Vec<_>>();
    crate::capabilities::evaluate_no_embeddings(&peers)
}

fn capabilities_for_plan(
    steps: &[Step],
    machines: &[Machine],
    total_layers: Option<u32>,
    kind: DeployKind,
) -> (
    crate::capabilities::DeploymentCapabilities,
    crate::capabilities::NoEmbeddingsEvaluation,
) {
    let no_embeddings = no_embeddings_topology(steps, total_layers, kind);
    let no_thinking = capability_from_bool(
        selected_model_capability(steps, machines, |evidence| evidence.thinking),
        "GGUF tokenizer.chat_template + tokenizer.ggml.tokens",
        "o template e o vocabulário do modelo permitem o controle de thinking",
        "o template e o vocabulário do modelo não oferecem o controle de thinking",
        "o template ou o vocabulário do modelo não pôde ser confirmado",
    );
    let lookup = selected_runtime_capability(steps, machines, "speculate/v1-lookup");
    let speculative_decoding = runtime_capability_for_ui(
        lookup,
        "capacidades runtime dos peers selecionados",
        "todos os peers selecionados anunciam lookup especulativo",
        "pelo menos um peer selecionado não anuncia lookup especulativo",
        "as capacidades runtime de pelo menos um peer não puderam ser lidas",
    );
    let model_mtp = selected_final_model_capability(steps, machines, |evidence| evidence.mtp);
    let runtime_mtp = combine_known(
        [
            lookup,
            selected_runtime_capability(steps, machines, "speculate/v1-mtp-sliced"),
            steps.last().and_then(|step| {
                selected_runtime_capability_for_step(step, machines, "speculate/v1-mtp")
            }),
        ]
        .into_iter(),
    );
    let mtp = match model_mtp {
        Some(false) => crate::capabilities::unsupported(
            "arquivo GGUF selecionado",
            "o GGUF selecionado não traz previsão nextn comprovada",
        ),
        Some(true) => {
            let mut capability = crate::capabilities::supported(
                "arquivo GGUF selecionado",
                "o GGUF selecionado traz metadata e tensores nextn comprovados",
            );
            match runtime_mtp {
                Some(true) => {}
                Some(false) => {
                    capability.warning = Some(
                        "Limitação do runtime: um ou mais peers não anunciam o protocolo MTP necessário; a opção permanece acionável e pode não ser executada."
                            .to_string(),
                    );
                }
                None => {
                    capability.warning = Some(
                        "O runtime dos peers não confirmou o protocolo MTP; ligá-lo é por conta e risco do usuário."
                            .to_string(),
                    );
                }
            }
            capability
        }
        None => {
            let mut capability = crate::capabilities::unknown(
                "arquivo GGUF selecionado",
                "metadata ou tensores nextn do GGUF selecionado não puderam ser confirmados",
            );
            if runtime_mtp.is_some() {
                capability.warning = Some(
                    "O runtime foi consultado, mas não substitui a prova do modelo; ligá-lo é por conta e risco do usuário."
                        .to_string(),
                );
            }
            capability
        }
    };
    let mut no_embeddings = no_embeddings.capability.clone();
    let unsafe_peers = no_embeddings_topology(steps, total_layers, kind)
        .peers
        .into_iter()
        .filter(|peer| peer.safe == Some(false))
        .map(|peer| format!("{}: {}", peer.node_id, peer.reason))
        .collect::<Vec<_>>();
    let unknown_peers = no_embeddings_topology(steps, total_layers, kind)
        .peers
        .into_iter()
        .filter(|peer| peer.safe.is_none())
        .map(|peer| format!("{}: {}", peer.node_id, peer.reason))
        .collect::<Vec<_>>();
    if !unsafe_peers.is_empty() {
        no_embeddings.reason = format!(
            "controle visível, mas estes peers precisam manter embeddings: {}",
            unsafe_peers.join("; ")
        );
    } else if !unknown_peers.is_empty() {
        no_embeddings.reason = format!(
            "controle visível, mas a topologia não pôde ser confirmada em: {}",
            unknown_peers.join("; ")
        );
    }
    (
        crate::capabilities::DeploymentCapabilities {
            fit_memory: if kind == DeployKind::TensorParallel {
                crate::capabilities::unsupported(
                    "estratégia Tensor Parallelism",
                    "o ajuste automático de memória não é compatível com Tensor Parallelism",
                )
            } else {
                crate::capabilities::supported(
                    "estratégia do plano",
                    "esta estratégia permite o ajuste automático de memória",
                )
            },
            no_thinking,
            no_embeddings,
            speculative_decoding,
            mtp,
        },
        no_embeddings_topology(steps, total_layers, kind),
    )
}

fn requested_capability_block(
    params: &RuntimeParams,
    capabilities: &crate::capabilities::DeploymentCapabilities,
    no_embeddings: &crate::capabilities::NoEmbeddingsEvaluation,
) -> Option<String> {
    crate::capabilities::validate_no_embeddings(no_embeddings, params.no_embeddings)
        .err()
        .or_else(|| {
            if params.fit_memory
                && capabilities.fit_memory.state == crate::capabilities::CapabilityState::Unknown
            {
                Some(format!(
                    "Ajustar à memória não pode ser confirmado: {}",
                    capabilities.fit_memory.reason
                ))
            } else {
                None
            }
        })
}

fn plan_with_rtt_options(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
    strategy: DeployStrategy,
    deployment_name: &str,
    preferred_node_ids: &[String],
    pairwise_rtt: &PairwiseRtt,
    layer_options: Option<WholeLayerOptions>,
) -> Plan {
    let mut available = candidates(machines);
    let size = model_size(&available);
    let total_layers = size.value();
    let tp_pre_sliced = kind == DeployKind::TensorParallel && has_tp_parts(machines);
    let is_moe = machines
        .iter()
        .flat_map(|machine| &machine.held)
        .any(|shard| shard.is_moe);
    let inference_mode_reason = if params.inference_mode == InferenceMode::Moe && !is_moe {
        Some("MoE exige metadata real de experts no GGUF deste modelo.".to_string())
    } else if kind == DeployKind::TensorParallel && is_moe {
        Some("Tensor Parallelism não suporta MoE nesta versão.".to_string())
    } else {
        None
    };
    let inference_mode_supported = inference_mode_reason.is_none();

    // What we are planning: (index into `available`, target node id).
    let auto = selection.is_none();
    // (index into `available`, target node id, that machine's overrides)
    let mut chosen: Vec<(usize, String, PeerOverrides)> = Vec::new();
    let mut blocked: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();

    if let Some(reason) = &inference_mode_reason {
        blocked = Some(reason.clone());
    }

    match selection {
        None => {
            let chain = match strategy {
                DeployStrategy::Simple => auto_chain(&available, total_layers),
                DeployStrategy::Complete => auto_chain_complete(&available, total_layers),
            };
            let targets = auto_assign(&chain, &available, machines, strategy, preferred_node_ids);
            chosen = chain
                .into_iter()
                .zip(targets)
                .map(|(i, t)| (i, t, PeerOverrides::default()))
                .collect();
        }
        Some(picked) => {
            for s in picked {
                let Some(i) = available
                    .iter()
                    .position(|c| c.start_layer == s.start_layer && c.end_layer == s.end_layer)
                else {
                    blocked.get_or_insert(format!(
                        "{} não existe em máquina nenhuma do cluster",
                        kind.unit(group, s.start_layer, s.end_layer)
                    ));
                    continue;
                };
                if chosen.iter().any(|(j, target, ..)| {
                    let existing = &available[*j];
                    existing.start_layer == s.start_layer
                        && existing.end_layer == s.end_layer
                        && target == &s.target_node_id
                }) {
                    // Repeating the exact same row is an idempotent UI action.
                    // It must not turn a valid replica on another peer into a
                    // refusal, and it must not create two launch steps on one
                    // peer for the same file.
                    continue;
                }
                chosen.push((
                    i,
                    s.target_node_id.clone(),
                    PeerOverrides {
                        n_gpu_layers: s.n_gpu_layers,
                        offload_kqv: s.offload_kqv,
                    },
                ));
            }
            chosen.sort_by_key(|(i, ..)| (available[*i].start_layer, available[*i].end_layer));
        }
    }

    // ── Tensor parallelism: o primary é quem o operador escolheu ─────────────
    //
    // O modelo inteiro é um candidato só; o alvo dele é o primary (rank 0), não
    // a máquina que o planejador automático sugeriria. As checagens do grupo
    // (tamanho, workers no cluster, modelo inteiro presente) recusam o plano
    // antes da checagem de shardabilidade, que lê o GGUF e roda no endpoint
    // `/api/ui/tp/check`.
    if kind == DeployKind::TensorParallel {
        if let Some(tp) = &tp {
            if let Some(reason) = tp_group_checks(tp, machines, group) {
                blocked.get_or_insert(reason);
            }
            // A seleção da tela pode conter o modelo inteiro uma vez para o
            // primary e outra vez para um worker. Worker é rank da coletiva,
            // não etapa adicional do pipeline. Mantemos somente a linha do
            // primary, preservando seus overrides, e `launch_targets` cria os
            // workers exatamente uma vez abaixo.
            if let Some(primary_index) = chosen
                .iter()
                .position(|(_, target, _)| target == &tp.primary_node_id)
            {
                let (candidate, _, overrides) = chosen.remove(primary_index);
                chosen.clear();
                chosen.push((candidate, tp.primary_node_id.clone(), overrides));
            } else if let Some((candidate, _, overrides)) = chosen.first().cloned() {
                chosen.clear();
                chosen.push((candidate, tp.primary_node_id.clone(), overrides));
            }
        }
    }

    for (i, ..) in &chosen {
        available[*i].selected = true;
    }

    if available.is_empty() {
        blocked.get_or_insert(if kind.whole() {
            // Both readings of an empty answer, because from here they are the
            // same fact: the catalog skips a whole model whose header did not
            // yield a layer count, and a model nobody has never appears either.
            format!(
                "nenhuma máquina do cluster tem o modelo inteiro {group} com o número de \
                 camadas legível no GGUF"
            )
        } else {
            format!("nenhuma máquina do cluster tem fatias de {group}")
        });
    }

    // ── How big is the model ────────────────────────────────────────────────
    //
    // Refused here, in front of the operator, instead of being carried down as
    // a zero that only `runtime::NodeConfig::validate` objects to — by which
    // point `launch` has already stopped inference on every target machine.
    //
    // Ahead of "nenhuma fatia selecionada" on purpose: a set nobody can size is
    // unusable however the operator ticks the boxes, and finding that out one
    // click later is the same dead end one step further in.
    match size {
        ModelSize::Unknown if !available.is_empty() => {
            blocked.get_or_insert(format!(
                "nenhuma fatia de {group} diz de quantas camadas é o modelo ({}), \
                 e nenhuma delas começa na camada 0 para que o total possa ser deduzido. \
                 Refatie o modelo inteiro pela tela de Fatiar, que grava esse dado em cada \
                 fatia, ou traga para o cluster a fatia que começa na camada 0.",
                nameless(&available)
            ));
        }
        ModelSize::Inferred(n) => warnings.push(format!(
            "o total de camadas não está gravado nas fatias ({}), então foi deduzido da \
             cobertura delas: {n} camadas. Se o modelo original tiver mais, as camadas que \
             faltam não estão em máquina nenhuma e a resposta sai errada. Refatie pela tela \
             de Fatiar para gravar o total.",
            nameless(&available)
        )),
        _ => {}
    }

    if !available.is_empty() && chosen.is_empty() {
        blocked.get_or_insert(
            if kind.whole() {
                "nenhum modelo selecionado"
            } else {
                "nenhuma fatia selecionada"
            }
            .to_string(),
        );
    }

    // ── Coverage: gaps and overlap refuse ───────────────────────────────────
    let mut missing: Vec<[u32; 2]> = Vec::new();
    let mut overlap: Vec<[u32; 2]> = Vec::new();
    let mut next = 0u32;
    let mut covered_exact: Option<(u32, u32)> = None;
    for (i, ..) in &chosen {
        let c = &available[*i];
        if covered_exact == Some((c.start_layer, c.end_layer)) {
            continue;
        }
        covered_exact = Some((c.start_layer, c.end_layer));
        if c.start_layer > next {
            missing.push([next, c.start_layer - 1]);
        } else if c.start_layer < next {
            overlap.push([c.start_layer, next.min(c.end_layer + 1) - 1]);
        }
        next = next.max(c.end_layer.saturating_add(1));
    }
    if let Some(total) = total_layers.filter(|t| next < *t) {
        missing.push([next, total - 1]);
    }
    let complete = missing.is_empty() && overlap.is_empty() && !chosen.is_empty();

    if let Some(o) = overlap.first() {
        // Two peers running the same layers would run them twice and the answer
        // would be wrong without anything failing, so this is a stop.
        blocked.get_or_insert(format!(
            "as camadas L{}-{} estão em duas fatias escolhidas ao mesmo tempo",
            o[0], o[1]
        ));
    }
    for m in &missing {
        let message = if m[0] == m[1] {
            format!("a camada L{} não está em nenhuma fatia escolhida", m[0])
        } else {
            format!(
                "as camadas L{}-{} não estão em nenhuma fatia escolhida",
                m[0], m[1]
            )
        };
        blocked.get_or_insert(message.clone());
        warnings.push(message);
    }

    // ── Preserve the selected pipeline steps, optionally merging neighbours ─
    //
    // A node may serve several selected ranges. `merge` says that adjacent
    // ranges on the same machine should become one step that joins them into a
    // single file on that machine. Separated ranges remain independent steps,
    // because contiguity is not a validity rule for the global pipeline.
    //
    // "Sem máquina" is not a machine. Each unplaced range keeps its own group
    // and its own line, so it is named separately in the plan.
    let mut groups: Vec<Vec<(usize, String, PeerOverrides)>> = Vec::new();
    for entry in &chosen {
        let can_merge_with_previous = merge
            && !entry.1.is_empty()
            && groups.last().is_some_and(|group| {
                group.last().is_some_and(|previous| {
                    previous.1 == entry.1
                        && available[entry.0].start_layer
                            == available[previous.0].end_layer.saturating_add(1)
                })
            });
        if can_merge_with_previous {
            groups
                .last_mut()
                .expect("grupo anterior")
                .push(entry.clone());
        } else {
            groups.push(vec![entry.clone()]);
        }
    }
    // `chosen` is already in layer order and groups are formed in that order,
    // so the pipeline order is preserved without regrouping by machine.

    let mut transfer_bytes = 0u64;
    let mut steps: Vec<Step> = Vec::new();

    for grp in groups {
        let target_id = &grp[0].1;
        let target = machines.iter().find(|m| &m.node_id == target_id);
        let first = &available[grp[0].0];
        let last = &available[grp[grp.len() - 1].0];
        let start_layer = first.start_layer;
        let end_layer = last.end_layer;

        // Groups contain only adjacent ranges for the merge path. A separated
        // range has its own group and remains an ordinary pipeline step.
        let merged = merge && grp.len() > 1;

        if merged {
            let mut parts: Vec<StepPart> = Vec::new();
            for (i, _, _) in &grp {
                let c = &available[*i];
                let part_ready =
                    target.is_some_and(|t| t.holds_ready(c.start_layer, c.end_layer).is_some());
                let source = if part_ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        (Some(&m.node_id) != target.map(|t| &t.node_id)
                            || m.holds(c.start_layer, c.end_layer)
                                .is_some_and(|shard| shard.virtual_slice))
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
                };
                if !part_ready
                    && target.is_some()
                    && source.is_none_or(|source| source.node_id != *target_id)
                {
                    transfer_bytes += c.size_bytes;
                }
                parts.push(StepPart {
                    start_layer: c.start_layer,
                    end_layer: c.end_layer,
                    name: c.name.clone(),
                    size_bytes: c.size_bytes,
                    ready: part_ready,
                    source_node_id: source.map(|m| m.node_id.clone()),
                    source_host: source.map(|m| m.label()),
                    source_path: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .map(|s| s.path.clone()),
                    source_sha256: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .and_then(|s| s.sha256.clone()),
                    source_needs_split: source.is_some_and(|m| {
                        m.holds(c.start_layer, c.end_layer)
                            .is_some_and(|shard| shard.virtual_slice)
                    }),
                });
            }

            let size_bytes = parts.iter().map(|p| p.size_bytes).sum();
            let ready = parts.iter().all(|p| p.ready);

            let mut problem = None;
            let mut note = None;

            if target_id.is_empty() {
                problem = Some(format!(
                    "a faixa L{}-{} está sem máquina",
                    start_layer, end_layer
                ));
            } else if target.is_none() {
                problem = Some(format!("a máquina {target_id} não está no cluster"));
            } else if !ready {
                if let Some(orphan) = parts
                    .iter()
                    .find(|p| !p.ready && p.source_node_id.is_none())
                {
                    problem = Some(format!(
                        "a fatia L{}-{} não existe em máquina nenhuma do cluster",
                        orphan.start_layer, orphan.end_layer
                    ));
                }
            }

            if let (None, Some(t)) = (&problem, target) {
                // The join writes the union file on top of the parts, which
                // stay on disk — so free space must cover the union plus
                // whatever still has to be pulled. A fully-ready join is not
                // exempt: it still needs room to write the union.
                let missing: u64 = parts
                    .iter()
                    .filter(|p| !p.ready)
                    .map(|p| p.size_bytes)
                    .sum();
                let disk_need = needed_mb(size_bytes + missing);
                let mem_need = needed_mb(size_bytes);
                if t.disk_free_mb > 0 && disk_need > t.disk_free_mb {
                    problem = Some(format!(
                        "{} tem {} GB livres em disco e juntar essas fatias precisa de {} GB",
                        t.label(),
                        gb(t.disk_free_mb),
                        gb(disk_need)
                    ));
                } else if target_capacity_mb(t) > 0 && mem_need > usable_mb(target_capacity_mb(t)) {
                    note = Some(format!(
                        "{} tem {} GB de memória livre para o modelo junto de {} GB",
                        t.label(),
                        gb(t.mem_free_mb),
                        gb(mem_need)
                    ));
                } else if !ready {
                    note = Some(format!("{} GB pela rede antes de subir", gb(mem_need)));
                }
            }

            if let Some(p) = &problem {
                blocked.get_or_insert(p.clone());
            }
            if let Some(n) = &note {
                if n.contains("memória") {
                    warnings.push(n.clone());
                }
            }

            steps.push(Step {
                // Numbered after the sort below: the place in the pipeline is
                // the place in the layer order, not the order the grouping
                // happened to walk.
                position: 0,
                start_layer,
                end_layer,
                total_layers,
                // O que o operador lê. A identidade de verdade é o hash da
                // parte, que viaja em `part_sha256`; um "qwen3-8b L0-15" aqui
                // é rótulo de tela, não chave de nada.
                name: format!("{group} L{start_layer}-{end_layer}"),
                size_bytes,
                target_node_id: target_id.clone(),
                target_host: target.map(|t| t.label()).unwrap_or_default(),
                ready,
                source_node_id: None,
                source_host: None,
                source_path: None,
                source_sha256: None,
                source_needs_split: false,
                target_path: None,
                primary: start_layer == 0,
                n_gpu_layers: grp[0].2.n_gpu_layers,
                offload_kqv: grp[0].2.offload_kqv,
                options: merged_alternatives(&parts, machines),
                problem,
                note,
                merged: true,
                parts,
            });
        } else {
            // Plain steps: one range, one file. A separated range has its own
            // group and remains an ordinary step in the pipeline.
            for (i, target_id, over) in &grp {
                let c = &available[*i];
                let target = machines.iter().find(|m| &m.node_id == target_id);
                let ready =
                    target.is_some_and(|t| t.holds_ready(c.start_layer, c.end_layer).is_some());
                let source = if ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        (Some(&m.node_id) != target.map(|t| &t.node_id)
                            || m.holds(c.start_layer, c.end_layer)
                                .is_some_and(|shard| shard.virtual_slice))
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
                };
                if !tp_pre_sliced
                    && !ready
                    && target.is_some()
                    && source.is_none_or(|source| source.node_id != *target_id)
                {
                    transfer_bytes += c.size_bytes;
                }

                let mut problem = None;
                let mut note = None;

                let unit = kind.unit(group, c.start_layer, c.end_layer);
                if target_id.is_empty() {
                    problem = Some(format!("{unit} está sem máquina"));
                } else if target.is_none() {
                    problem = Some(format!("a máquina {target_id} não está no cluster"));
                } else if !ready && source.is_none() {
                    problem = Some(format!("{unit} não existe em máquina nenhuma do cluster"));
                }

                if let (None, Some(t)) = (&problem, target) {
                    let need = needed_mb(c.size_bytes);
                    if !ready && t.disk_free_mb > 0 && need > t.disk_free_mb {
                        problem = Some(format!(
                            "{} tem {} GB livres em disco e {unit} pesa {} GB",
                            t.label(),
                            gb(t.disk_free_mb),
                            gb(need)
                        ));
                    } else if target_capacity_mb(t) > 0 && need > usable_mb(target_capacity_mb(t)) {
                        note = Some(format!(
                            "{} tem {} GB de memória livre para {} de {} GB",
                            t.label(),
                            gb(t.mem_free_mb),
                            kind.indefinite(),
                            gb(need)
                        ));
                    } else if !ready {
                        // The pull direction is the one the sender did not
                        // choose, so `transfer::safe_model_path` keeps it inside
                        // the sender's slice folder. A whole model is routinely
                        // outside it — an LM Studio library, an Ollama store —
                        // and that copy is refused at the far end, which is a
                        // thing to read before pressing the button rather than
                        // after the job fails.
                        note = Some(if kind.whole() {
                            format!(
                                "{} GB pela rede antes de subir - a máquina de origem só envia \
                                 sozinha o que estiver na pasta de fatias dela; de fora dela, \
                                 mande o arquivo pela tela de Transferências primeiro",
                                gb(need)
                            )
                        } else {
                            format!("{} GB pela rede antes de subir", gb(need))
                        });
                    }
                }

                if let Some(p) = &problem {
                    blocked.get_or_insert(p.clone());
                }
                if let Some(n) = &note {
                    if n.contains("memória") {
                        warnings.push(n.clone());
                    }
                }

                steps.push(Step {
                    position: 0,
                    start_layer: c.start_layer,
                    end_layer: c.end_layer,
                    total_layers,
                    name: c.name.clone(),
                    size_bytes: c.size_bytes,
                    target_node_id: target_id.clone(),
                    target_host: target.map(|t| t.label()).unwrap_or_default(),
                    ready,
                    source_node_id: source.map(|m| m.node_id.clone()),
                    source_host: source.map(|m| m.label()),
                    source_path: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .map(|s| s.path.clone()),
                    source_sha256: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .and_then(|s| s.sha256.clone()),
                    source_needs_split: source.is_some_and(|m| {
                        m.holds(c.start_layer, c.end_layer)
                            .is_some_and(|shard| shard.virtual_slice)
                    }),
                    target_path: target
                        .and_then(|t| t.holds_ready(c.start_layer, c.end_layer))
                        .map(|s| s.path.clone()),
                    primary: c.start_layer == 0,
                    n_gpu_layers: over.n_gpu_layers,
                    offload_kqv: over.offload_kqv,
                    options: alternatives(c, machines, kind),
                    problem,
                    note,
                    merged: false,
                    parts: Vec::new(),
                });
            }
        }
    }

    // ── The order the answer comes back in ──────────────────────────────────
    //
    // The pipeline order *is* the layer order, so `position` is only honest in
    // layer order, and `run` reads the launch order straight off this list.
    // Groups are formed in layer order, and this final sort keeps the contract
    // explicit even if a future grouping change walks a different collection.
    steps.sort_by_key(|s| (s.start_layer, s.end_layer));
    for (i, s) in steps.iter_mut().enumerate() {
        s.position = i as u32 + 1;
    }

    let mut tp_targets = Vec::new();
    if tp_pre_sliced {
        if let Some(tp) = &tp {
            let overrides = steps
                .first()
                .map(|step| (step.n_gpu_layers, step.offload_kqv));
            let (resolved, bytes, reason) = tp_targets_for(
                tp,
                machines,
                group,
                total_layers,
                overrides.and_then(|value| value.0),
                overrides.and_then(|value| value.1),
            );
            tp_targets = resolved;
            transfer_bytes = transfer_bytes.saturating_add(bytes);
            if let Some(reason) = reason {
                blocked.get_or_insert(reason);
            }
            if let (Some(step), Some(rank0)) = (
                steps.iter_mut().find(|step| step.primary),
                tp_targets.first(),
            ) {
                step.ready = rank0.ready;
                step.source_node_id = rank0.source_node_id.clone();
                step.source_host = rank0.source_host.clone();
                step.source_path = rank0.source_path.clone();
                step.source_sha256 = rank0.source_sha256.clone();
                step.target_path = rank0.target_path.clone();
                step.size_bytes = rank0.size_bytes;
                step.problem = rank0.problem.clone();
            }
        }
    }

    // The auto planner runs out of machines before it runs out of slices, and
    // that reads better as a count than as one message per missing range.
    if auto && steps.iter().any(|s| s.target_node_id.is_empty()) {
        blocked = Some(format!(
            "são {} fatias para {} {} - algumas fatias ficaram sem máquina",
            steps.len(),
            machines.len(),
            if machines.len() == 1 {
                "máquina"
            } else {
                "máquinas"
            }
        ));
    }

    let machine_views = machines
        .iter()
        .map(|m| MachineView {
            node_id: m.node_id.clone(),
            label: m.label(),
            mem_free_mb: m.mem_free_mb,
            gpu_free_mb: m.gpu_free_mb,
            ram_free_mb: m.ram_free_mb,
            disk_free_mb: m.disk_free_mb,
            rtt_ms: m.rtt_ms,
            load: m.load,
            holds: m
                .held
                .iter()
                .map(|s| [s.start_layer, s.end_layer])
                .collect(),
            // A merged machine serves the union of its ranges as one file, so
            // the table names that union rather than the first slice.
            taking: steps
                .iter()
                .find(|s| s.target_node_id == m.node_id)
                .map(|s| [s.start_layer, s.end_layer]),
        })
        .collect();

    let selected_ids: std::collections::HashSet<&str> = steps
        .iter()
        .map(|step| step.target_node_id.as_str())
        .collect();
    let mut projected: std::collections::HashMap<&str, u32> = machines
        .iter()
        .map(|m| (m.node_id.as_str(), m.load))
        .collect();
    for step in &steps {
        if let Some(value) = projected.get_mut(step.target_node_id.as_str()) {
            *value = value.saturating_add(1);
        }
    }
    let load_distribution = machines
        .iter()
        .map(|machine| LoadDistribution {
            node_id: machine.node_id.clone(),
            current: machine.load,
            projected: projected
                .get(machine.node_id.as_str())
                .copied()
                .unwrap_or(machine.load),
            selected: selected_ids.contains(machine.node_id.as_str()),
        })
        .collect();

    // Build edges between consecutive logical layer stages, not between every
    // pair of machines in the plan. Two exact copies of L0-14 are one stage
    // with two source choices; treating them as L0-14 -> L0-14 would invent a
    // backward pipeline hop. A route option is only useful when its target is
    // one of the selected machines serving the immediately next stage. A peer
    // that merely has an inference endpoint, or serves a later stage, is not a
    // bridge for this handoff.
    let mut stage_ranges: Vec<(u32, u32)> = Vec::new();
    for step in &steps {
        let range = (step.start_layer, step.end_layer);
        if !stage_ranges.contains(&range) {
            stage_ranges.push(range);
        }
    }
    let (capabilities, no_embeddings_evaluation) =
        capabilities_for_plan(&steps, machines, total_layers, kind);
    if let Some(reason) =
        requested_capability_block(params, &capabilities, &no_embeddings_evaluation)
    {
        if blocked.is_none() {
            blocked = Some(reason);
        } else {
            warnings.push(reason);
        }
    }

    let mut route_options = Vec::new();
    for pair in stage_ranges.windows(2) {
        let from_range = pair[0];
        let to_range = pair[1];
        let from_nodes: Vec<String> = steps
            .iter()
            .filter(|step| {
                (step.start_layer, step.end_layer) == from_range && !step.target_node_id.is_empty()
            })
            .map(|step| step.target_node_id.clone())
            .collect();
        let to_nodes: Vec<String> = steps
            .iter()
            .filter(|step| {
                (step.start_layer, step.end_layer) == to_range && !step.target_node_id.is_empty()
            })
            .map(|step| step.target_node_id.clone())
            .collect();
        let selected_from = from_nodes.first();
        let selected_to = to_nodes.first();

        for from in &from_nodes {
            for to in &to_nodes {
                if from == to {
                    continue;
                }
                let Some(machine) = machines.iter().find(|machine| &machine.node_id == to) else {
                    continue;
                };
                if machine.inference_url.is_none() {
                    continue;
                }
                let selected = selected_from == Some(from) && selected_to == Some(to);
                let rtt_ms = pairwise_rtt.get(from, to);
                route_options.push(RouteOption {
                    from_node_id: from.clone(),
                    to_node_id: to.clone(),
                    rtt_ms,
                    selected,
                    note: if selected {
                        "rota escolhida".to_string()
                    } else if rtt_ms.is_some() {
                        "rota alternativa medida no peer de origem".to_string()
                    } else {
                        "rota alternativa sem medição direcional de RTT".to_string()
                    },
                });
            }
        }
    }

    Plan {
        group: group.to_string(),
        deployment_name: deployment_name.to_string(),
        strategy,
        kind,
        tp,
        is_moe,
        inference_mode_supported,
        inference_mode_reason,
        capabilities,
        layer_options,
        total_layers,
        total_layers_inferred: size.inferred(),
        complete,
        transfer_bytes,
        steps,
        tp_targets,
        blocked,
        auto,
        merge,
        available,
        machines: machine_views,
        missing,
        overlap,
        warnings,
        prompt_tokens: crate::gateway::prompt_budget_of(
            params.n_ctx,
            params.max_sessions,
            params.max_new_tokens,
        ),
        chunk_tokens: crate::gateway::prompt_chunk_of(params.prefill_chunk_tokens, params.n_batch),
        params: params.clone(),
        route_options,
        load_distribution,
    }
}

fn gb(mb: u64) -> String {
    format!("{:.1}", mb as f64 / 1024.0).replace('.', ",")
}

/// Every machine as a possible target for one range, with the reason.
///
/// Occupancy is deliberately *not* part of `ok`: a machine already holding
/// another range is exactly the one an operator wants to swap with, and hiding
/// it would make the swap impossible. Picking a busy machine is caught by the
/// one-range-per-node rule instead, which is the honest place for it.
fn alternatives(c: &Candidate, machines: &[Machine], kind: DeployKind) -> Vec<Alternative> {
    let need = needed_mb(c.size_bytes);
    machines
        .iter()
        .map(|m| {
            let has = m.holds_ready(c.start_layer, c.end_layer).is_some();
            let no_disk = !has && m.disk_free_mb > 0 && need > m.disk_free_mb;
            let tight = target_capacity_mb(m) > 0 && need > usable_mb(target_capacity_mb(m));
            Alternative {
                node_id: m.node_id.clone(),
                label: m.label(),
                has,
                ok: !no_disk,
                note: if no_disk {
                    Some(format!("só {} GB livres em disco", gb(m.disk_free_mb)))
                } else if tight {
                    Some(format!("{} GB de memória livre", gb(m.mem_free_mb)))
                } else if has {
                    Some(
                        if kind.whole() {
                            "já tem o modelo"
                        } else {
                            "já tem a fatia"
                        }
                        .to_string(),
                    )
                } else {
                    Some(format!("receberia {} GB", gb(need)))
                },
            }
        })
        .collect()
}

/// Every machine as a possible home for a merged step.
///
/// A machine "has" the merged unit when it already holds every part — the
/// merge then runs locally with nothing crossing the network. Otherwise it
/// would receive the parts, so the cost is the union's size.
fn merged_alternatives(parts: &[StepPart], machines: &[Machine]) -> Vec<Alternative> {
    let need = needed_mb(parts.iter().map(|p| p.size_bytes).sum());
    machines
        .iter()
        .map(|m| {
            let has = parts
                .iter()
                .all(|p| m.holds_ready(p.start_layer, p.end_layer).is_some());
            let no_disk = !has && m.disk_free_mb > 0 && need > m.disk_free_mb;
            let tight = target_capacity_mb(m) > 0 && need > usable_mb(target_capacity_mb(m));
            Alternative {
                node_id: m.node_id.clone(),
                label: m.label(),
                has,
                ok: !no_disk,
                note: if no_disk {
                    Some(format!("só {} GB livres em disco", gb(m.disk_free_mb)))
                } else if tight {
                    Some(format!("{} GB de memória livre", gb(m.mem_free_mb)))
                } else if has {
                    Some("já tem todas as fatias".to_string())
                } else {
                    Some(format!("receberia {} GB", gb(need)))
                },
            }
        })
        .collect()
}

/// Checagem remota de shardabilidade: pergunta ao primary (o dono do modelo
/// inteiro) se o GGUF dele dá para partir em `tp_size` ranks, sem escrever nada.
///
/// `Some(motivo)` = o plano deve ser recusado. A checagem falha fechada: se o
/// primary não responder (ou for um binário antigo sem a rota), o plano é
/// recusado com um motivo honesto, em vez de deixar o job falhar na metade do
/// apply.
async fn tp_shardability_check(
    state: &GatewayState,
    machines: &[Machine],
    tp: &TpLaunch,
) -> Option<String> {
    let primary = machines.iter().find(|m| m.node_id == tp.primary_node_id)?;
    let Some(path) = primary.held.first().map(|s| s.path.clone()) else {
        return Some(format!(
            "o primary {} não tem o modelo inteiro",
            primary.label()
        ));
    };
    let Some(base) = primary.gateway_url.clone() else {
        return Some(format!(
            "não foi possível falar com o primary {}",
            primary.label()
        ));
    };
    let Ok(http) = state.peer_http.for_peer(&primary.node_id) else {
        return Some(format!(
            "não foi possível falar com o primary {}",
            primary.label()
        ));
    };

    // `/api/ui/tp/check` é autenticada como todo `/api/ui/*` de operador: sem a
    // chave, o primary responde 401 e a checagem nunca passa - em nenhuma
    // máquina, porque isto não depende de quem é o primary. `for_peer` resolve
    // só o TLS (cliente pinado por node_id); credencial é por requisição, como
    // no resto do arquivo.
    let key = state.auth.read().await.connection_key.clone();
    let resp = match http
        .post(format!("{base}/api/ui/tp/check"))
        .timeout(PEER_TIMEOUT * 4)
        .bearer_auth(&key)
        .json(&json!({ "model_path": path, "tp_size": tp.size }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Some(format!(
                "não foi possível validar o shard em {}: {e}",
                primary.label()
            ))
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            return Some(format!(
                "{} respondeu mal à checagem de shard",
                primary.label()
            ))
        }
    };
    tp_check_verdict(&body, &primary.label())
}

/// Como se lê a resposta de `/api/ui/tp/check`.
///
/// Separado da chamada para ser testável sem peer vivo - e porque o ramo sem
/// `ok` já mentiu uma vez. `/api/ui/tp/check` é rota autenticada
/// (`webui::tp_check` recebe `ConnectionAuth`), e um peer que recusa devolve
/// `{"error":"missing Authorization header"}`: JSON válido, sem `ok`. Chamar
/// isso de "binário antigo" mandou o operador conferir a versão do binário nas
/// duas máquinas enquanto o defeito real era a credencial que
/// `tp_shardability_check` nem enviava (17/08, com Mac e ksia no mesmo commit).
/// Quem recusa costuma dizer por quê; repetir a razão dele é sempre melhor que
/// adivinhar a nossa.
fn tp_check_verdict(body: &serde_json::Value, label: &str) -> Option<String> {
    match body.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => None,
        Some(false) => Some(
            body.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("o modelo não é shardável no caminho C")
                .to_string(),
        ),
        None => Some(match body.get("error").and_then(|v| v.as_str()) {
            Some(err) => format!("{label} recusou a checagem de shard: {err}"),
            None => format!("{label} não tem a checagem de shard (binário antigo?)"),
        }),
    }
}

async fn plan_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    let deployment_name =
        match normalize_deployment_name(req.deployment_name.as_deref(), &req.group) {
            Ok(name) => name,
            Err(error) => return bad(json!({ "error": error })),
        };
    let survey = survey(
        &state,
        &req.group,
        req.kind,
        req.selection.as_deref(),
        req.tp.as_ref(),
    )
    .await;
    let mut machines = survey.machines;
    let pairwise_rtt = survey.pairwise_rtt;
    let layer_options = if req.kind == DeployKind::Whole {
        whole_layer_options_for_plan(&state, &machines, &req.group).await
    } else {
        None
    };
    if let (Some(selection), Some(options)) = (&req.whole_slice, &layer_options) {
        if let Err(error) = add_virtual_whole_slices(&mut machines, &req.group, selection, options)
        {
            return bad(json!({ "error": error }));
        }
    }
    let params = effective_params_for_kind(req.kind, &req.params);
    let mut p = plan_with_rtt_options(
        &req.group,
        &machines,
        req.selection.as_deref(),
        &params,
        req.merge,
        req.kind,
        req.tp.clone(),
        req.strategy,
        &deployment_name,
        &req.preferred_node_ids,
        &pairwise_rtt,
        layer_options,
    );
    if req.kind == DeployKind::TensorParallel && p.blocked.is_none() && !has_tp_parts(&machines) {
        if let Some(tp) = &req.tp {
            if let Some(reason) = tp_shardability_check(&state, &machines, tp).await {
                p.blocked = Some(reason);
            }
        }
    }
    if let Some(reason) = survey.metadata_pending {
        p.blocked = Some(reason);
    }
    Json(p).into_response()
}

// ── Carrying it out ──────────────────────────────────────────────────────────

async fn deploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    // The cluster is re-surveyed rather than trusted from the preview: the
    // screen's copy may be a minute old, and a machine that has left in the
    // meantime would otherwise be handed a slice. What is *not* recomputed is
    // the operator's choice — it is validated against the fresh survey and
    // refused if it no longer holds.
    let survey = survey(
        &state,
        &req.group,
        req.kind,
        req.selection.as_deref(),
        req.tp.as_ref(),
    )
    .await;
    if let Some(reason) = survey.metadata_pending {
        return bad(json!({ "error": reason }));
    }
    let mut machines = survey.machines;
    let pairwise_rtt = survey.pairwise_rtt;
    let layer_options = if req.kind == DeployKind::Whole {
        whole_layer_options_for_plan(&state, &machines, &req.group).await
    } else {
        None
    };
    if let (Some(selection), Some(options)) = (&req.whole_slice, &layer_options) {
        if let Err(error) = add_virtual_whole_slices(&mut machines, &req.group, selection, options)
        {
            return bad(json!({ "error": error }));
        }
    }
    let deployment_name =
        match normalize_deployment_name(req.deployment_name.as_deref(), &req.group) {
            Ok(name) => name,
            Err(error) => return bad(json!({ "error": error })),
        };
    let params = effective_params_for_kind(req.kind, &req.params);
    let mut plan = plan_with_rtt_options(
        &req.group,
        &machines,
        req.selection.as_deref(),
        &params,
        req.merge,
        req.kind,
        req.tp.clone(),
        req.strategy,
        &deployment_name,
        &req.preferred_node_ids,
        &pairwise_rtt,
        layer_options,
    );

    // A checagem de shardabilidade lê o GGUF no primary; o deploy re-faz tudo
    // sobre a varredura fresca, então também re-valida aqui — não confia no
    // preview que a tela mostrou.
    if req.kind == DeployKind::TensorParallel && plan.blocked.is_none() && !has_tp_parts(&machines)
    {
        if let Some(tp) = &req.tp {
            if let Some(reason) = tp_shardability_check(&state, &machines, tp).await {
                plan.blocked = Some(reason);
            }
        }
    }

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

    // A identidade é resolvida antes de criar o job e antes de tocar em um
    // peer. Se o GGUF não puder ser provado por hash, ou a fatia não trouxer
    // pom_id, o operador recebe a recusa na própria ação de implantar.
    let identity = match resolve_requested_model_identity(&state, &plan, &machines).await {
        Ok(identity) => identity,
        Err(error) => return bad(json!({ "error": error })),
    };

    let job_id = uuid::Uuid::new_v4().to_string();
    // A whole model is one machine, and "em 1 nós" is the kind of seam that
    // makes an operator wonder what else was written for the other case.
    let title = format!(
        "Implantar {} em {} {}",
        deployment_name,
        plan.steps.len(),
        if plan.steps.len() == 1 { "nó" } else { "nós" }
    );
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "deploy", title, "preparando"),
    );

    // Cunhado antes de subir nada: é o nome desta implantação, e ele vai junto
    // com cada `apply` para que o nó saiba de que conjunto passou a fazer parte.
    let deploy_id = crate::deployment::new_deploy_id();
    let id = job_id.clone();
    tokio::spawn(async move {
        match run(state, plan, params, identity, &deploy_id, &id).await {
            Ok(()) => set_job(&id, "done", "pronto", "o modelo está atendendo na rede").await,
            Err(e) => {
                warn!("implantação falhou: {e}");
                set_job(&id, "failed", "falhou", &e).await;
            }
        }
    });

    Json(json!({ "started": true, "job_id": job_id })).into_response()
}

/// The plan's steps as `launch` wants them, resolved against a fresh survey.
///
/// Split out of `run` because this is the whole of phase 2's decision-making
/// and the only part of the deploy that can be checked without a cluster: what
/// each machine is told to load, and from which of *its own* paths. The survey
/// it reads is deliberately the one taken after the copies, so a file that
/// landed somewhere unexpected is still found where it actually is.
///
/// A whole model needs nothing special here and gets nothing: it is a step
/// whose range happens to be the entire model, so it comes out as a plain
/// target with `skip_split` — which is also what makes the receiving peer a
/// `single_peer`, running without hidden-state extraction.
/// Validações estáticas de um grupo de tensor parallelism, antes da checagem
/// de shardabilidade (que lê o GGUF). Devolve `Some(motivo)` quando o deploy não
/// pode nem começar: tamanho inválido, primary/workers fora do cluster, alguém
/// sem o modelo inteiro, ou primary sem URL de inferência derivável.
fn tp_group_checks(tp: &TpLaunch, machines: &[Machine], group: &str) -> Option<String> {
    if tp.size < 2 {
        return Some("um grupo tensor-parallel precisa de pelo menos 2 ranks".to_string());
    }
    let Some(primary) = machines.iter().find(|m| m.node_id == tp.primary_node_id) else {
        return Some(format!(
            "o primary {} não está no cluster",
            tp.primary_node_id
        ));
    };
    if primary.inference_url.is_none() {
        return Some(format!(
            "não foi possível descobrir a URL de inferência do primary {} (ele precisa anunciar a porta de inferência)",
            primary.label()
        ));
    }

    let expected_workers = (tp.size - 1) as usize;
    if tp.worker_node_ids.len() != expected_workers {
        return Some(format!(
            "são {} ranks, então o grupo precisa de {} worker{} (você marcou {})",
            tp.size,
            expected_workers,
            if expected_workers == 1 { "" } else { "s" },
            tp.worker_node_ids.len(),
        ));
    }

    let pre_sliced = has_tp_parts(machines);

    let mut seen = std::collections::HashSet::new();
    for worker_id in &tp.worker_node_ids {
        if worker_id == &tp.primary_node_id {
            return Some("um worker não pode ser o próprio primary".to_string());
        }
        if !seen.insert(worker_id.clone()) {
            return Some(format!("o worker {worker_id} foi marcado duas vezes"));
        }
        let Some(worker) = machines.iter().find(|m| &m.node_id == worker_id) else {
            return Some(format!("o worker {worker_id} não está no cluster"));
        };
        if worker.inference_url.is_none() {
            return Some(format!(
                "não foi possível descobrir a URL de inferência do worker {}",
                worker.label()
            ));
        }
    }

    if pre_sliced {
        let total = model_size(&candidates(machines)).value().unwrap_or(0);
        let end = total.saturating_sub(1);
        for rank in 0..tp.size {
            if !machines
                .iter()
                .any(|machine| machine.holds_tp(0, end, rank, tp.size).is_some())
            {
                return Some(format!(
                    "rank {}of{} do grupo TP {group} não existe em máquina nenhuma do cluster",
                    rank + 1,
                    tp.size
                ));
            }
        }
        return None;
    }

    if !primary.held.iter().any(|s| s.start_layer == 0) {
        return Some(format!(
            "o primary {} não tem o modelo inteiro {group}",
            primary.label()
        ));
    }
    for worker_id in &tp.worker_node_ids {
        let worker = machines
            .iter()
            .find(|m| &m.node_id == worker_id)
            .expect("worker validado");
        if !worker.held.iter().any(|s| s.start_layer == 0) {
            return Some(format!(
                "o worker {} não tem o modelo inteiro {group} para extrair o shard",
                worker.label()
            ));
        }
    }
    None
}

fn has_tp_parts(machines: &[Machine]) -> bool {
    machines
        .iter()
        .any(|machine| machine.held.iter().any(|shard| shard.tp_size.is_some()))
}

/// Resolve o arquivo de cada rank TP pré-fatiado. O alvo pode já ter o rank;
/// nesse caso não há transferência. Se ainda não tiver, escolhemos qualquer
/// peer que possua exatamente o mesmo `tp_rank` e carregamos o hash e caminho
/// desse peer para a transferência por conteúdo.
fn tp_targets_for(
    tp: &TpLaunch,
    machines: &[Machine],
    group: &str,
    total_layers: Option<u32>,
    n_gpu_layers: Option<u32>,
    offload_kqv: Option<bool>,
) -> (Vec<TpTarget>, u64, Option<String>) {
    let total = total_layers.unwrap_or(0);
    let end_layer = total.saturating_sub(1);
    let mut transfer_bytes: u64 = 0;
    let mut blocked = None;
    let mut targets = Vec::with_capacity(tp.size as usize);

    for rank in 0..tp.size {
        let target_node_id = if rank == 0 {
            tp.primary_node_id.clone()
        } else {
            tp.worker_node_ids
                .get(rank as usize - 1)
                .cloned()
                .unwrap_or_default()
        };
        let target = machines.iter().find(|m| m.node_id == target_node_id);
        let held = target.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size));
        let source = if held.is_none() {
            machines.iter().find(|m| {
                m.node_id != target_node_id && m.holds_tp(0, end_layer, rank, tp.size).is_some()
            })
        } else {
            None
        };
        let file = held.or_else(|| source.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size)));
        let ready = held.is_some();
        let problem = if target_node_id.is_empty() {
            Some(format!("rank {}of{} está sem máquina", rank + 1, tp.size))
        } else if target.is_none() {
            Some(format!("a máquina {target_node_id} não está no cluster"))
        } else if file.is_none() {
            Some(format!(
                "rank {}of{} do grupo TP {group} não existe em máquina nenhuma do cluster",
                rank + 1,
                tp.size
            ))
        } else {
            None
        };
        if let Some(reason) = &problem {
            blocked.get_or_insert(reason.clone());
        }
        if !ready && file.is_some() {
            transfer_bytes = transfer_bytes.saturating_add(file.map(|s| s.size_bytes).unwrap_or(0));
        }

        let source_shard = source.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size));
        let target_shard = held;
        let shard = target_shard.or(source_shard).or(file);
        targets.push(TpTarget {
            rank,
            size: tp.size,
            start_layer: 0,
            end_layer,
            total_layers,
            name: format!("{group} · {}of{}", rank + 1, tp.size),
            size_bytes: shard.map(|s| s.size_bytes).unwrap_or(0),
            target_node_id,
            target_host: target.map(|m| m.label()).unwrap_or_default(),
            ready,
            source_node_id: source.map(|m| m.node_id.clone()),
            source_host: source.map(|m| m.label()),
            source_path: source_shard.map(|s| s.path.clone()),
            source_sha256: source_shard.and_then(|s| s.sha256.clone()),
            target_path: target_shard.map(|s| s.path.clone()),
            problem,
            n_gpu_layers,
            offload_kqv,
        });
    }

    (targets, transfer_bytes, blocked)
}

fn tp_identity(
    size: u32,
    group_id: &str,
    rank: u32,
    role: &str,
    members: &[crate::network::TpMember],
) -> crate::network::TpIdentity {
    let rank0 = members.first();
    crate::network::TpIdentity {
        role: role.into(),
        group_id: group_id.into(),
        size,
        rank,
        rank0_url: rank0.map(|m| m.url.clone()),
        // O certificado da coletiva é pinado por este id; sem ele o cliente
        // padrão recusa o autoassinado do cluster e o all-reduce falha calado.
        rank0_node_id: rank0.map(|m| m.node_id.clone()),
        // Todo membro recebe a lista inteira: o rank 0 usa para transmitir o
        // decode, e os demais para saber quem são.
        members: members.to_vec(),
    }
}

/// Os membros do grupo TP em ordem de rank (0 = primary), com a URL de
/// inferência de cada um.
///
/// Recusa cedo quem não anuncia porta de inferência: sem ela o rank 0 não teria
/// como disparar o decode daquele rank, e o grupo travaria na primeira barreira
/// em vez de falhar aqui.
fn tp_members(
    tp: &TpLaunch,
    machines: &[Machine],
) -> Result<Vec<crate::network::TpMember>, String> {
    let de = |node_id: &str| -> Result<crate::network::TpMember, String> {
        let m = machines
            .iter()
            .find(|m| m.node_id == node_id)
            .ok_or_else(|| format!("{node_id} não está no cluster"))?;
        let url = m.inference_url.clone().ok_or_else(|| {
            format!(
                "não foi possível descobrir a URL de inferência de {} (ele precisa anunciar a porta de inferência)",
                m.label()
            )
        })?;
        let mut url_candidates = if m.inference_candidates.is_empty() {
            vec![url.clone()]
        } else {
            m.inference_candidates.clone()
        };
        if !url_candidates.iter().any(|candidate| candidate == &url) {
            url_candidates.insert(0, url.clone());
        }
        Ok(crate::network::TpMember {
            node_id: node_id.to_string(),
            url,
            url_candidates,
            addr_candidates: if m.addr_candidates.is_empty() {
                Vec::new()
            } else {
                m.addr_candidates.clone()
            },
            libp2p_peer_id: m.libp2p_peer_id.clone(),
            supports_tunnel: m.supports_tunnel,
        })
    };
    let mut out = vec![de(&tp.primary_node_id)?];
    for w in &tp.worker_node_ids {
        out.push(de(w)?);
    }
    Ok(out)
}

fn launch_targets(
    steps: &[Step],
    machines: &[Machine],
    kind: DeployKind,
    tp: Option<&TpLaunch>,
    tp_targets: &[TpTarget],
) -> Result<Vec<LaunchTarget>, String> {
    // A URL da coletiva (rank 0) é derivada do primary, não digitada: é a porta
    // de inferência que o próprio nó anuncia. `tp.is_some()` ⟺ `rank0_url.is_some()`.
    let rank0_url = tp
        .map(|t| {
            machines
                .iter()
                .find(|m| m.node_id == t.primary_node_id)
                .and_then(|m| m.inference_url.clone())
                .ok_or_else(|| {
                    format!(
                        "não foi possível descobrir a URL de inferência do primary {}",
                        t.primary_node_id
                    )
                })
        })
        .transpose()?;
    let _ = rank0_url;
    let membros = match tp {
        Some(t) => tp_members(t, machines)?,
        None => Vec::new(),
    };

    if let Some(tp) = tp.filter(|_| !tp_targets.is_empty()) {
        let mut targets = Vec::with_capacity(tp_targets.len());
        for target in tp_targets {
            let machine = machines
                .iter()
                .find(|machine| machine.node_id == target.target_node_id)
                .ok_or_else(|| format!("{} não está no cluster", target.target_node_id))?;
            let held = machine
                .holds_tp(
                    target.start_layer,
                    target.end_layer,
                    target.rank,
                    target.size,
                )
                .ok_or_else(|| {
                    format!(
                        "{} não achou o rank {}of{} depois da cópia",
                        machine.label(),
                        target.rank + 1,
                        target.size
                    )
                })?;
            targets.push(LaunchTarget {
                node_id: target.target_node_id.clone(),
                source_model_path: held.path.clone(),
                start_layer: target.start_layer,
                end_layer: target.end_layer,
                skip_split: true,
                part_sha256: held.sha256.clone(),
                merge: None,
                n_gpu_layers: target.n_gpu_layers,
                offload_kqv: target.offload_kqv,
                no_embeddings: None,
                peer_index: None,
                peer_count: None,
                tp: Some(tp_identity(
                    tp.size,
                    &target.name,
                    target.rank,
                    if target.rank == 0 {
                        "primary"
                    } else {
                        "worker"
                    },
                    &membros,
                )),
            });
        }
        return Ok(targets);
    }

    // Réplicas da mesma faixa ocupam o mesmo estágio lógico do pipeline. Cada
    // cópia precisa receber o mesmo peer_index/peer_count para exportar o
    // residual exatamente como a rota principal daquele estágio.
    let mut pipeline_ranges: Vec<(u32, u32)> = Vec::new();
    for step in steps {
        let range = (step.start_layer, step.end_layer);
        if !pipeline_ranges.contains(&range) {
            pipeline_ranges.push(range);
        }
    }
    let pipeline_peer_count = pipeline_ranges.len() as u32;
    let pipeline_fields = |step: &Step| {
        if kind != DeployKind::Shards && !(kind == DeployKind::Whole && steps.len() > 1) {
            return (None, None);
        }
        let index = pipeline_ranges
            .iter()
            .position(|range| *range == (step.start_layer, step.end_layer))
            .unwrap_or(0) as u32;
        (Some(index), Some(pipeline_peer_count))
    };

    let mut targets = Vec::new();
    for step in steps {
        let target_machine = machines.iter().find(|m| m.node_id == step.target_node_id);

        if step.merged {
            // Each part's path on the target is resolved the same way; the
            // apply merges them into one file there before serving.
            let mut merge: Vec<MergeSource> = Vec::new();
            for part in &step.parts {
                let path = target_machine
                    .and_then(|m| m.holds_ready(part.start_layer, part.end_layer))
                    .map(|s| s.path.clone())
                    .ok_or_else(|| {
                        format!(
                            "{} não achou a fatia L{}-{} depois da cópia",
                            step.target_host, part.start_layer, part.end_layer
                        )
                    })?;
                merge.push(MergeSource {
                    source_model_path: path,
                    start_layer: part.start_layer,
                    end_layer: part.end_layer,
                });
            }
            let anchor = merge
                .first()
                .map(|m| m.source_model_path.clone())
                .ok_or_else(|| format!("{} não tem fatias para juntar", step.target_host))?;
            let (peer_index, peer_count) = pipeline_fields(step);

            targets.push(LaunchTarget {
                node_id: step.target_node_id.clone(),
                // Unused when `merge` is set: the apply validates the parts and
                // never reads this. Present only because `LaunchTarget` requires
                // the field; the first part names a real file for the record.
                source_model_path: anchor,
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                // Nothing to cut: the merge produces the exact range.
                skip_split: true,
                // A junção produz um arquivo novo, cujo hash ninguém conhece
                // antes de juntar: quem junta é que o nomeia.
                part_sha256: None,
                merge: Some(merge),
                n_gpu_layers: step.n_gpu_layers,
                offload_kqv: step.offload_kqv,
                no_embeddings: None,
                peer_index,
                peer_count,
                tp: tp.map(|t| tp_identity(t.size, &step.name, 0, "primary", &membros)),
            });
            continue;
        }

        let held = target_machine
            .and_then(|m| m.holds_ready(step.start_layer, step.end_layer))
            .ok_or_else(|| {
                format!(
                    "{} não achou {} depois da cópia",
                    step.target_host,
                    kind.unit(&step.name, step.start_layer, step.end_layer)
                )
            })?;
        let (peer_index, peer_count) = pipeline_fields(step);

        targets.push(LaunchTarget {
            node_id: step.target_node_id.clone(),
            source_model_path: held.path.clone(),
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            // Already a file of exactly this range: nothing to cut. A whole
            // model is the same statement about the whole range.
            skip_split: true,
            part_sha256: held.sha256.clone(),
            merge: None,
            n_gpu_layers: step.n_gpu_layers,
            offload_kqv: step.offload_kqv,
            no_embeddings: None,
            peer_index,
            peer_count,
            tp: tp.map(|t| tp_identity(t.size, &step.name, 0, "primary", &membros)),
        });
    }

    // Grupo de tensor parallelism: além do primary (o step do modelo inteiro),
    // cada worker vira um alvo que extrai o próprio shard de tensor.
    if let Some(tp) = tp {
        let head = steps.first().ok_or("plano TP sem step")?;
        for (i, worker_id) in tp.worker_node_ids.iter().enumerate() {
            let rank = (i + 1) as u32;
            let worker_machine = machines
                .iter()
                .find(|m| m.node_id == *worker_id)
                .ok_or_else(|| format!("worker {worker_id} não está no cluster"))?;
            let path = worker_machine
                .holds_ready(head.start_layer, head.end_layer)
                .map(|s| s.path.clone())
                .ok_or_else(|| {
                    format!(
                        "{} não tem o modelo para extrair o shard de tensor",
                        worker_machine.hostname
                    )
                })?;
            targets.push(LaunchTarget {
                node_id: worker_id.clone(),
                source_model_path: path,
                start_layer: head.start_layer,
                end_layer: head.end_layer,
                skip_split: true,
                // O shard de tensor deste rank ainda vai ser extraído aqui; o
                // hash sai do corte.
                part_sha256: None,
                merge: None,
                n_gpu_layers: head.n_gpu_layers,
                offload_kqv: head.offload_kqv,
                no_embeddings: None,
                peer_index: None,
                peer_count: None,
                tp: Some(tp_identity(tp.size, &head.name, rank, "worker", &membros)),
            });
        }
    }

    Ok(targets)
}

fn planned_model_paths(plan: &Plan) -> Vec<(String, String)> {
    let mut paths = Vec::new();
    let mut add = |node_id: &str, path: Option<&String>| {
        if !node_id.is_empty() {
            if let Some(path) = path.filter(|path| !path.is_empty()) {
                if !paths
                    .iter()
                    .any(|(node, existing)| node == node_id && existing == path)
                {
                    paths.push((node_id.to_string(), path.clone()));
                }
            }
        }
    };
    for step in &plan.steps {
        add(&step.target_node_id, step.target_path.as_ref());
        add(
            &step.source_node_id.clone().unwrap_or_default(),
            step.source_path.as_ref(),
        );
        for part in &step.parts {
            add(
                &part.source_node_id.clone().unwrap_or_default(),
                part.source_path.as_ref(),
            );
        }
    }
    for target in &plan.tp_targets {
        add(&target.target_node_id, target.target_path.as_ref());
        add(
            &target.source_node_id.clone().unwrap_or_default(),
            target.source_path.as_ref(),
        );
    }
    paths
}

async fn whole_layer_options_for_plan(
    state: &GatewayState,
    machines: &[Machine],
    group: &str,
) -> Option<WholeLayerOptions> {
    let source = machines.iter().find_map(|machine| {
        machine
            .held
            .iter()
            .find(|shard| shard.start_layer == 0 && shard.total_layers == Some(shard.end_layer + 1))
            .map(|shard| (machine, shard.path.clone(), shard.total_layers.unwrap_or(0)))
    })?;
    let (machine, path, total_layers) = source;
    let result: Result<ModelLayerOptions, String> = if machine.node_id == state.network.node_id {
        crate::webui::model_layer_options_local(state, &path).await
    } else {
        let Some(base) = machine.gateway_url.clone() else {
            return Some(WholeLayerOptions {
                source_node_id: machine.node_id.clone(),
                total_layers,
                cuts: Vec::new(),
                granularity: None,
                rule: "não foi possível confirmar as regras reais deste GGUF".to_string(),
                reason: Some(format!("{} não anuncia gateway", machine.label())),
            });
        };
        let Ok(client) = state.peer_http.for_peer(&machine.node_id) else {
            return Some(WholeLayerOptions {
                source_node_id: machine.node_id.clone(),
                total_layers,
                cuts: Vec::new(),
                granularity: None,
                rule: "não foi possível confirmar as regras reais deste GGUF".to_string(),
                reason: Some(format!("não foi possível falar com {}", machine.label())),
            });
        };
        let key = state.auth.read().await.connection_key.clone();
        let response = client
            .post(format!("{base}/api/ui/model-layer-options"))
            .bearer_auth(key)
            .json(&crate::webui::ModelLayerOptionsRequest { path })
            .timeout(PEER_TIMEOUT * 4)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "não foi possível ler as regras de fatiamento em {}: {error}",
                    machine.label()
                )
            });
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Some(WholeLayerOptions {
                    source_node_id: machine.node_id.clone(),
                    total_layers,
                    cuts: Vec::new(),
                    granularity: None,
                    rule: "não foi possível confirmar as regras reais deste GGUF".to_string(),
                    reason: Some(error),
                })
            }
        };
        let status = response.status();
        let body = match response.json::<serde_json::Value>().await {
            Ok(body) => body,
            Err(error) => {
                return Some(WholeLayerOptions {
                    source_node_id: machine.node_id.clone(),
                    total_layers,
                    cuts: Vec::new(),
                    granularity: None,
                    rule: "não foi possível confirmar as regras reais deste GGUF".to_string(),
                    reason: Some(format!(
                        "resposta inválida das regras de fatiamento: {error}"
                    )),
                })
            }
        };
        if !status.is_success() {
            Err(body
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("o peer recusou a leitura das regras de fatiamento")
                .to_string())
        } else {
            serde_json::from_value::<ModelLayerOptions>(body)
                .map_err(|error| format!("resposta inválida das regras de fatiamento: {error}"))
        }
    };
    match result {
        Ok(options) => Some(WholeLayerOptions {
            source_node_id: machine.node_id.clone(),
            total_layers: options.total_layers,
            cuts: options.cuts,
            granularity: options.granularity,
            rule: options.rule,
            reason: None,
        }),
        Err(error) => Some(WholeLayerOptions {
            source_node_id: machine.node_id.clone(),
            total_layers,
            cuts: Vec::new(),
            granularity: None,
            rule: "não foi possível confirmar as regras reais deste GGUF".to_string(),
            reason: Some(format!("{}: {}", group, error)),
        }),
    }
}

fn add_virtual_whole_slices(
    machines: &mut [Machine],
    group: &str,
    selection: &WholeSliceSelection,
    options: &WholeLayerOptions,
) -> Result<(), String> {
    if let Some(reason) = &options.reason {
        return Err(format!(
            "não é possível escolher camadas de {}: {}",
            group, reason
        ));
    }
    let cut = options
        .cuts
        .iter()
        .find(|cut| cut.end_layer == selection.end_layer)
        .ok_or_else(|| {
            format!(
                "o corte na camada L{} não respeita as regras reais do GGUF",
                selection.end_layer
            )
        })?;
    if selection.source_node_id == selection.target_node_id {
        return Err(
            "a máquina de origem e a máquina de destino precisam ser diferentes".to_string(),
        );
    }
    let source = machines
        .iter()
        .find(|machine| machine.node_id == selection.source_node_id)
        .ok_or_else(|| "a máquina que propõe o modelo não está no cluster".to_string())?;
    let whole = source
        .held
        .iter()
        .find(|shard| {
            shard.start_layer == 0
                && shard.end_layer + 1 == options.total_layers
                && !shard.virtual_slice
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} não tem o modelo inteiro selecionado para fatiar",
                source.label()
            )
        })?;
    if !machines
        .iter()
        .any(|machine| machine.node_id == selection.target_node_id)
    {
        return Err("a máquina de destino não está no cluster".to_string());
    }

    let mut ranges = vec![(0, cut.end_layer, cut.size_bytes)];
    if cut.end_layer + 1 < options.total_layers {
        ranges.push((
            cut.end_layer + 1,
            options.total_layers - 1,
            cut.remainder_bytes,
        ));
    }
    let source = machines
        .iter_mut()
        .find(|machine| machine.node_id == selection.source_node_id)
        .expect("origem validada");
    for (start_layer, end_layer, size_bytes) in ranges {
        if source.holds_ready(start_layer, end_layer).is_some()
            || source
                .holds(start_layer, end_layer)
                .is_some_and(|shard| shard.virtual_slice)
        {
            continue;
        }
        source.held.push(HeldShard {
            start_layer,
            end_layer,
            total_layers: Some(options.total_layers),
            name: format!("{group} L{start_layer}-{end_layer}"),
            path: whole.path.clone(),
            size_bytes,
            sha256: None,
            pom_id: None,
            virtual_slice: true,
            tp_rank: None,
            tp_size: None,
            is_moe: whole.is_moe,
            model_capabilities: whole.model_capabilities.clone(),
        });
    }
    Ok(())
}

/// Cut virtual ranges on the peer that owns the whole GGUF. The actual
/// fatiador remains in `webui`, so deploy only coordinates the local operation
/// and receives the content-addressed paths that transfer already understands.
async fn materialize_slices_on_peer(
    state: &GatewayState,
    source_node_id: &str,
    request: &SliceRangesRequest,
) -> Result<crate::webui::SliceRangesResponse, String> {
    if source_node_id == state.network.node_id {
        return crate::webui::materialize_slice_ranges(state, request).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == source_node_id)
        .ok_or_else(|| format!("a máquina de origem {source_node_id} não está no cluster"))?;
    let base = peer
        .gateway_url()
        .ok_or_else(|| format!("{} não anuncia porta de gateway", peer.hostname))?;
    let client = state
        .peer_http
        .for_peer(source_node_id)
        .map_err(|error| format!("sem cliente para {}: {error}", peer.hostname))?;
    let key = state.auth.read().await.connection_key.clone();
    let response = client
        .post(format!("{base}/api/ui/slice-ranges"))
        .bearer_auth(key)
        .timeout(Duration::from_secs(3600))
        .json(request)
        .send()
        .await
        .map_err(|error| {
            format!(
                "não foi possível fatiar o modelo em {}: {error}",
                peer.hostname
            )
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.map_err(|error| {
        format!(
            "{} respondeu uma materialização inválida: {error}",
            peer.hostname
        )
    })?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("o peer recusou o fatiamento local")
            .to_string());
    }
    serde_json::from_value(body).map_err(|error| {
        format!(
            "resposta inválida do fatiamento em {}: {error}",
            peer.hostname
        )
    })
}

async fn model_identity_on_peer(
    state: &GatewayState,
    node_id: &str,
    path: &str,
    hash: bool,
) -> Result<crate::webui::ModelIdentityResponse, String> {
    if node_id == state.network.node_id {
        return crate::webui::model_identity_local(state, path, hash).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| format!("peer {node_id} não está no cluster"))?;
    let base = peer
        .gateway_url()
        .ok_or_else(|| format!("{} não anuncia porta de gateway", peer.hostname))?;
    let client = state
        .peer_http
        .for_peer(node_id)
        .map_err(|e| format!("sem cliente para {node_id}: {e}"))?;
    let key = state.auth.read().await.connection_key.clone();
    let response = client
        .post(format!("{base}/api/ui/model-identity"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .json(&crate::webui::ModelIdentityRequest {
            path: path.to_string(),
            hash,
        })
        .send()
        .await
        .map_err(|e| format!("não foi possível consultar a identidade em {node_id}: {e}"))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("resposta de identidade inválida de {node_id}: {e}"))?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("peer respondeu HTTP {status}")));
    }
    serde_json::from_value(body)
        .map_err(|e| format!("resposta de identidade inválida de {node_id}: {e}"))
}

async fn resolve_requested_model_identity(
    state: &GatewayState,
    plan: &Plan,
    machines: &[Machine],
) -> Result<RequestedModelIdentity, String> {
    let mut pom_ids = Vec::new();
    for machine in machines {
        for shard in &machine.held {
            if let Some(pom_id) = shard.pom_id.as_deref() {
                if !pom_ids.iter().any(|known| known == pom_id) {
                    pom_ids.push(pom_id.to_string());
                }
            }
        }
    }

    let paths = planned_model_paths(plan);
    if paths.is_empty() {
        return Err(format!(
            "identidade indisponível: o plano de {} não informa o arquivo selecionado",
            plan.group
        ));
    }

    let mut paths_by_peer = HashMap::new();
    let needs_hash = if pom_ids.is_empty() {
        match plan.kind {
            DeployKind::Whole => true,
            DeployKind::TensorParallel if plan.tp_targets.is_empty() => true,
            DeployKind::Shards | DeployKind::TensorParallel => {
                return Err(format!(
                    "identidade indisponível: o plano de {} usa fatias sem pom_id; não é seguro provar o mesmo modelo por hash de uma fatia",
                    plan.group
                ));
            }
        }
    } else {
        false
    };
    let mut proof = if pom_ids.len() == 1 {
        Some(crate::deployment::DeploymentIdentity::PomId {
            pom_id: pom_ids.remove(0),
        })
    } else if pom_ids.len() > 1 {
        return Err(format!(
            "identidade indisponível: o plano de {} mistura mais de um pom_id",
            plan.group
        ));
    } else {
        None
    };

    let mut first_hash_error = None;
    for (index, (node_id, path)) in paths.iter().enumerate() {
        let ask_hash = needs_hash && index == 0;
        let identity = match model_identity_on_peer(state, node_id, path, ask_hash).await {
            Ok(identity) => identity,
            Err(error) if ask_hash => {
                first_hash_error = Some(error);
                model_identity_on_peer(state, node_id, path, false)
                    .await
                    .map_err(|fallback| {
                        format!(
                            "identidade indisponível para {}: {} (reserva de caminho também falhou: {})",
                            plan.group,
                            first_hash_error.as_deref().unwrap_or("hash não calculado"),
                            fallback
                        )
                    })?
            }
            Err(error) => {
                return Err(format!(
                    "identidade indisponível para {} em {}: {}",
                    plan.group, node_id, error
                ));
            }
        };
        if let Some(sha256) = identity.sha256 {
            proof = Some(crate::deployment::DeploymentIdentity::Sha256 { sha256 });
        }
        paths_by_peer.insert(node_id.clone(), identity.canonical_path);
    }

    if proof.is_none() && needs_hash {
        info!(
            model = %plan.group,
            error = ?first_hash_error,
            "usando caminho canônico como prova de reserva no peer de origem"
        );
    }

    Ok(RequestedModelIdentity {
        proof,
        paths_by_peer,
    })
}

async fn peer_node_snapshot(
    state: &GatewayState,
    peer: &PeerInfo,
) -> Result<serde_json::Value, String> {
    if peer.node_id == state.network.node_id {
        let config = state.runtime.config().await;
        let active_deployment = config
            .as_ref()
            .map(|config| config.deployment_name.clone())
            .unwrap_or_default();
        return Ok(json!({
            "config": config,
            "status": state.runtime.status().await,
            "active_model": state.active_model().await,
            "active_deployment": active_deployment,
        }));
    }
    let base = peer
        .gateway_url()
        .ok_or_else(|| format!("{} não anuncia porta de gateway", peer.hostname))?;
    let client = state
        .peer_http
        .for_peer(&peer.node_id)
        .map_err(|e| format!("sem cliente para {}: {e}", peer.hostname))?;
    let response = client
        .get(format!("{base}/api/ui/node"))
        .timeout(PEER_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{} não respondeu: {e}", peer.hostname))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("{} respondeu estado inválido: {e}", peer.hostname))?;
    if !status.is_success() {
        return Err(format!("{} respondeu HTTP {status}", peer.hostname));
    }
    Ok(body)
}

async fn canonical_current_path(
    state: &GatewayState,
    peer: &PeerInfo,
    snapshot: &serde_json::Value,
) -> Result<Option<String>, String> {
    let Some(path) = snapshot
        .pointer("/config/model_path")
        .and_then(|value| value.as_str())
        .filter(|path| !path.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(
        model_identity_on_peer(state, &peer.node_id, path, false)
            .await?
            .canonical_path,
    ))
}

async fn wait_for_peer_idle_with_timeout(
    state: &GatewayState,
    peer: &PeerInfo,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match peer_node_snapshot(state, peer).await {
            Ok(snapshot) if node_is_idle(&snapshot) => return Ok(()),
            Ok(snapshot) => node_state_detail(&snapshot),
            Err(error) => error,
        };
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "{} não ficou ocioso dentro do limite de {}s; ficou ocupado: {}",
                if peer.hostname.is_empty() {
                    &peer.node_id
                } else {
                    &peer.hostname
                },
                timeout.as_secs(),
                last
            ));
        }
        tokio::time::sleep(DEPLOY_IDLE_POLL.min(deadline - now)).await;
    }
}

async fn prepare_existing_model(
    state: &GatewayState,
    group: &str,
    desired: &RequestedModelIdentity,
    job_id: &str,
) -> Result<(), String> {
    let peers = state.all_peers().await;
    let mut to_release = Vec::new();

    for peer in peers.into_iter().filter(|peer| peer.total_layers > 0) {
        if peer.model.is_empty() {
            return Err(format!(
                "não foi possível provar a identidade antes de implantar {}: {} está ocupado, mas não anuncia qual modelo está servindo",
                group, peer.hostname
            ));
        }
        if peer.model != group {
            continue;
        }

        let record = if peer.deploy_id.is_empty() {
            None
        } else {
            find_record(state, &peer.deploy_id).await
        };
        let known_identity = record.as_ref().and_then(|record| {
            record.model.identity.clone().or_else(|| {
                record
                    .model
                    .pom_id
                    .clone()
                    .map(|pom_id| crate::deployment::DeploymentIdentity::PomId { pom_id })
            })
        });
        if desired.proof.is_some() && desired.proof == known_identity {
            to_release.push(peer);
            continue;
        }
        if desired.proof.is_some() && known_identity.is_some() && desired.proof != known_identity {
            continue;
        }

        let snapshot = peer_node_snapshot(state, &peer).await.map_err(|error| {
            format!(
                "não foi possível provar a identidade do modelo em {}: {}",
                peer.hostname, error
            )
        })?;
        let current_path = canonical_current_path(state, &peer, &snapshot)
            .await
            .map_err(|error| {
                format!(
                    "não foi possível provar a identidade do modelo em {}: {}",
                    peer.hostname, error
                )
            })?;
        let current_model = record
            .as_ref()
            .map(|record| record.model.clone())
            .unwrap_or_else(|| crate::deployment::DeploymentModel {
                label: peer.model.clone(),
                total_layers: peer.total_layers,
                pom_id: None,
                identity: None,
            });
        if deployment_identity_matches(
            desired,
            &current_model,
            &peer.node_id,
            current_path.as_deref(),
        ) {
            to_release.push(peer);
        } else {
            return Err(format!(
                "não foi possível provar que o modelo ativo em {} é o mesmo que {}: o registro não tem hash/pom_id e o caminho canônico não coincide",
                peer.hostname, group
            ));
        }
    }

    if to_release.is_empty() {
        return Ok(());
    }

    set_job(
        job_id,
        "running",
        "desimplantando",
        &format!(
            "liberando {} máquina(s) que já servem este modelo",
            to_release.len()
        ),
    )
    .await;
    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let releases = futures::future::join_all(to_release.iter().map(|peer| {
        let key = key.clone();
        let self_id = self_id.clone();
        async move { (peer, release_one(state, peer, &key, &self_id).await) }
    }))
    .await;
    let failures: Vec<String> = releases
        .iter()
        .filter_map(|(peer, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{}: {}", peer.hostname, error))
        })
        .collect();
    if !failures.is_empty() {
        return Err(format!(
            "a nova implantação foi recusada porque a desimplantação falhou: {}",
            failures.join("; ")
        ));
    }

    set_job(
        job_id,
        "running",
        "aguardando recursos ociosos",
        "esperando as máquinas liberarem memória e encerrarem a inferência",
    )
    .await;
    let waits = futures::future::join_all(to_release.iter().map(|peer| async move {
        (
            peer,
            wait_for_peer_idle_with_timeout(state, peer, DEPLOY_IDLE_TIMEOUT).await,
        )
    }))
    .await;
    let failures: Vec<String> = waits
        .iter()
        .filter_map(|(peer, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{}: {}", peer.hostname, error))
        })
        .collect();
    if !failures.is_empty() {
        return Err(format!(
            "a nova implantação foi recusada porque as máquinas não ficaram ociosas: {}",
            failures.join("; ")
        ));
    }
    Ok(())
}

/// Copy what has to be copied, then bind every peer to its range.
async fn run(
    state: GatewayState,
    plan: Plan,
    params: RuntimeParams,
    identity: RequestedModelIdentity,
    deploy_id: &str,
    job_id: &str,
) -> Result<(), String> {
    prepare_existing_model(&state, &plan.group, &identity, job_id).await?;
    let key = state.auth.read().await.connection_key.clone();

    // One unit of movement: a slice that has to reach a target. A merged step
    // moves one file per part; a plain step moves its one slice.
    struct Pull {
        target_node_id: String,
        target_host: String,
        source_node_id: String,
        source_path: String,
        /// Hash da parte, quando a origem a tem endereçada por conteúdo.
        source_sha256: Option<String>,
        source_needs_split: bool,
        name: String,
        start_layer: u32,
        end_layer: u32,
    }
    let mut pulls: Vec<Pull> = Vec::new();
    if !plan.tp_targets.is_empty() {
        for target in &plan.tp_targets {
            if target.ready {
                continue;
            }
            let (Some(source_node), Some(source_path)) =
                (&target.source_node_id, &target.source_path)
            else {
                return Err(format!(
                    "o rank {}of{} não tem de onde vir",
                    target.rank + 1,
                    target.size
                ));
            };
            pulls.push(Pull {
                target_node_id: target.target_node_id.clone(),
                target_host: target.target_host.clone(),
                source_node_id: source_node.clone(),
                source_path: source_path.clone(),
                source_sha256: target.source_sha256.clone(),
                source_needs_split: false,
                name: target.name.clone(),
                start_layer: target.start_layer,
                end_layer: target.end_layer,
            });
        }
    } else {
        for step in &plan.steps {
            if step.merged {
                for part in &step.parts {
                    if part.ready {
                        continue;
                    }
                    let (Some(source_node), Some(source_path)) =
                        (&part.source_node_id, &part.source_path)
                    else {
                        return Err(format!(
                            "a fatia L{}-{} não tem de onde vir",
                            part.start_layer, part.end_layer
                        ));
                    };
                    pulls.push(Pull {
                        target_node_id: step.target_node_id.clone(),
                        target_host: step.target_host.clone(),
                        source_node_id: source_node.clone(),
                        source_path: source_path.clone(),
                        source_sha256: part.source_sha256.clone(),
                        source_needs_split: part.source_needs_split,
                        name: part.name.clone(),
                        start_layer: part.start_layer,
                        end_layer: part.end_layer,
                    });
                }
            } else if !step.ready {
                let (Some(source_node), Some(source_path)) =
                    (&step.source_node_id, &step.source_path)
                else {
                    return Err(format!(
                        "a fatia L{}-{} não tem de onde vir",
                        step.start_layer, step.end_layer
                    ));
                };
                pulls.push(Pull {
                    target_node_id: step.target_node_id.clone(),
                    target_host: step.target_host.clone(),
                    source_node_id: source_node.clone(),
                    source_path: source_path.clone(),
                    source_sha256: step.source_sha256.clone(),
                    source_needs_split: step.source_needs_split,
                    name: step.name.clone(),
                    start_layer: step.start_layer,
                    end_layer: step.end_layer,
                });
            }
        }
    }

    // Virtual ranges are backed by the proposer's whole GGUF. Materialize all
    // of them on that peer before any destination asks the transfer service to
    // pull a path. Grouping keeps one source read/manifest operation per
    // source model even when the selected pipeline has several ranges.
    let mut materializations: HashMap<(String, String, u32, String), Vec<SliceRange>> =
        HashMap::new();
    for pull in &pulls {
        if pull.source_needs_split {
            let ranges = materializations
                .entry((
                    pull.source_node_id.clone(),
                    pull.source_path.clone(),
                    plan.total_layers.ok_or_else(|| {
                        format!("as fatias de {} não dizem o total de camadas", plan.group)
                    })?,
                    plan.group.clone(),
                ))
                .or_default();
            if !ranges.iter().any(|range| {
                range.start_layer == pull.start_layer && range.end_layer == pull.end_layer
            }) {
                ranges.push(SliceRange {
                    start_layer: pull.start_layer,
                    end_layer: pull.end_layer,
                });
            }
        }
    }
    for ((source_node_id, source_path, total_layers, name), ranges) in materializations {
        set_job(
            job_id,
            "running",
            "fatiando modelo inteiro",
            &format!(
                "materializando {} faixa(s) na máquina que propôs o modelo",
                ranges.len()
            ),
        )
        .await;
        let response = materialize_slices_on_peer(
            &state,
            &source_node_id,
            &SliceRangesRequest {
                model_path: source_path.clone(),
                name: name.clone(),
                total_layers,
                ranges,
            },
        )
        .await?;
        for materialized in response.slices {
            let Some(pull) = pulls.iter_mut().find(|pull| {
                pull.source_needs_split
                    && pull.source_node_id == source_node_id
                    && pull.source_path == source_path
                    && pull.start_layer == materialized.start_layer
                    && pull.end_layer == materialized.end_layer
            }) else {
                return Err(format!(
                    "o fatiador devolveu uma faixa L{}-{} que não estava no plano",
                    materialized.start_layer, materialized.end_layer
                ));
            };
            pull.source_path = materialized.path;
            pull.source_sha256 = Some(materialized.sha256);
            pull.source_needs_split = false;
        }
    }
    if pulls.iter().any(|pull| pull.source_needs_split) {
        return Err("o fatiamento local não devolveu todas as faixas pedidas".to_string());
    }

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !pulls.is_empty() {
        set_job(
            job_id,
            "running",
            "copiando fatias",
            &format!(
                "{} fatia(s), {:.1} GB no total",
                pulls.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        // (node_id, gateway, job id). O node_id entrou porque a sondagem do job
        // fala com o peer, e com o cluster em TLS ela precisa fixar o
        // certificado dele - uma URL sozinha não tem identidade a verificar.
        let mut watching: Vec<(String, String, String)> = Vec::new();
        for pull in &pulls {
            if pull.target_node_id == pull.source_node_id {
                continue;
            }
            let gateway = gateway_of(&state, &pull.target_node_id)
                .await
                .ok_or_else(|| format!("{} não anuncia porta de gateway", pull.target_host))?;

            let cliente = state
                .peer_http
                .for_peer(&pull.target_node_id)
                .map_err(|e| format!("{}: sem cliente para o peer: {e}", pull.target_host))?;
            let body: serde_json::Value = cliente
                .post(format!("{gateway}/api/transfer/pull"))
                .bearer_auth(&key)
                // Por hash sempre que a origem souber o dela. É o que faz a
                // fatia cair em `<shard_dir>/<modelo>/<sha>.gguf` com o `.pom`
                // ao lado, em vez de solta na raiz parecendo um modelo
                // inteiro. O pedido por caminho fica para a fatia sem hash -
                // um arquivo que ninguém endereçou por conteúdo ainda.
                .json(&json!({
                    "node_id": pull.source_node_id,
                    "sha256": pull.source_sha256,
                    "model": plan.group,
                    "remote_path": pull.source_path,
                    "name": pull.name,
                }))
                .send()
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", pull.target_host))?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} puxando L{}-{} de {}",
                        pull.target_host, pull.start_layer, pull.end_layer, pull.source_node_id
                    );
                    watching.push((pull.target_node_id.clone(), gateway, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} recusou a cópia: {}",
                        pull.target_host,
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("motivo não informado")
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
    let machines = survey(&state, &plan.group, plan.kind, None, None)
        .await
        .machines;
    let mut targets = launch_targets(
        &plan.steps,
        &machines,
        plan.kind,
        plan.tp.as_ref(),
        &plan.tp_targets,
    )?;
    if params.no_embeddings {
        // O planner já validou atomicamente a topologia. Marcar cada alvo
        // explicitamente evita que um interruptor global seja reinterpretado
        // de forma diferente por um peer.
        for target in &mut targets {
            target.no_embeddings = Some(true);
        }
    }

    // The pipeline is the layer order, so the entry point is whoever holds
    // layer 0 — the same machine the screen marks "processa o prompt".
    let primary = plan
        .steps
        .iter()
        .find(|s| s.primary)
        .map(|s| s.target_node_id.clone());

    // `plan` refuses an unknown size before anything is copied, so this is a
    // fact by the time we get here rather than a default worth inventing.
    let total_layers = plan
        .total_layers
        .ok_or_else(|| format!("as fatias de {} não dizem o total de camadas", plan.group))?;

    // O registro da implantação nasce aqui, com os alvos já resolvidos: é o
    // último ponto em que se sabe, de uma vez só, quem recebeu qual faixa e com
    // que configuração. Depois do `launch` essa intenção se dissolvia em N
    // `NodeConfig` espalhados, e ninguém no cluster sabia mais dizer que
    // aqueles nós eram *a mesma* implantação.
    let registro = build_record(
        deploy_id,
        &plan,
        &targets,
        &machines,
        total_layers,
        &params,
        &identity,
        &state.network.node_id,
    );

    let response = launch(
        ConnectionAuth,
        State(state.clone()),
        Json(LaunchRequest {
            deploy_id: deploy_id.to_string(),
            base_name: plan.group.clone(),
            // `group` is the selected value from the deployment model picker.
            // Its name is the cluster identity, not a heuristic profile name.
            model_label: Some(plan.group.clone()),
            deployment_name: plan.deployment_name.clone(),
            total_layers,
            targets,
            primary_node_id: primary,
            params,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(refusal(response).await);
    }

    // Só depois do launch dar certo: um registro de implantação que não subiu
    // seria uma implantação fantasma na listagem, e a tela de desimplantar
    // ofereceria algo que não está no ar.
    publish_record(&state, &registro).await;
    Ok(())
}

/// Monta o registro a partir do que o plano e os alvos já dizem.
///
/// Nada aqui é recalculado: as faixas, a posição no pipeline e os ajustes por
/// máquina são exatamente os que foram para o `launch`. Guardar o **resultado**
/// é o que faz "com que configuração aquela máquina subiu?" ser uma leitura em
/// vez de uma reconstrução.
fn build_record(
    deploy_id: &str,
    plan: &Plan,
    targets: &[LaunchTarget],
    machines: &[Machine],
    total_layers: u32,
    params: &RuntimeParams,
    identity: &RequestedModelIdentity,
    created_by: &str,
) -> crate::deployment::Deployment {
    let kind = match plan.kind {
        DeployKind::Shards => crate::deployment::DeploymentKind::Shards,
        DeployKind::Whole => crate::deployment::DeploymentKind::Whole,
        DeployKind::TensorParallel => crate::deployment::DeploymentKind::TensorParallel,
    };

    let total_alvos = targets.len() as u32;
    let nodes = targets
        .iter()
        .enumerate()
        .map(|(i, t)| crate::deployment::DeploymentNode {
            node_id: t.node_id.clone(),
            hostname: machines
                .iter()
                .find(|m| m.node_id == t.node_id)
                .map(|m| m.label())
                .unwrap_or_default(),
            start_layer: t.start_layer,
            end_layer: t.end_layer,
            peer_index: t.peer_index.unwrap_or(i as u32),
            peer_count: t.peer_count.unwrap_or(total_alvos),
            primary: plan
                .steps
                .iter()
                .any(|s| s.primary && s.target_node_id == t.node_id),
            part_sha256: t.part_sha256.clone(),
            tp_rank: t.tp.as_ref().map(|tp| tp.rank),
            // A mesma função que o `launch` usa para decidir o que cada máquina
            // recebe, para o registro não poder divergir do que subiu.
            config: crate::webui::params_for(params, t),
        })
        .collect();

    crate::deployment::Deployment::new(
        deploy_id.to_string(),
        kind,
        crate::deployment::DeploymentModel {
            label: plan.group.clone(),
            total_layers,
            pom_id: match identity.proof.as_ref() {
                Some(crate::deployment::DeploymentIdentity::PomId { pom_id }) => {
                    Some(pom_id.clone())
                }
                _ => None,
            },
            identity: identity.proof.clone(),
        },
        created_by,
        nodes,
    )
}

/// Grava o registro neste nó e manda uma cópia para cada participante.
///
/// Cada nó guarda o próprio exemplar: não há dono único que, ao cair, leve a
/// implantação junto, e um nó que reinicia lê no disco de que implantação ele
/// fazia parte. Uma cópia que não chega não derruba a implantação - ela já está
/// no ar; o que se perde é aquele nó saber se explicar depois de um reinício.
async fn publish_record(state: &GatewayState, registro: &crate::deployment::Deployment) {
    if let Err(e) = crate::deployment::save(registro) {
        warn!(
            "não deu para anotar a implantação {}: {e:#}",
            registro.deploy_id
        );
    }

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;

    for no in &registro.nodes {
        if no.node_id == self_id {
            continue;
        }
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == no.node_id)
            .and_then(|p| p.gateway_url())
        else {
            warn!("{} não anuncia gateway; sem cópia do registro", no.node_id);
            continue;
        };
        let Ok(cliente) = state.peer_http.for_peer(&no.node_id) else {
            continue;
        };
        let envio = cliente
            .post(format!("{base}/api/ui/deployments"))
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .json(registro)
            .send()
            .await;
        if let Err(e) = envio {
            warn!("{} não aceitou o registro da implantação: {e}", no.node_id);
        }
    }
}

// ── Mexer numa implantação que já está no ar ─────────────────────────────────

/// Acha o registro de uma implantação, aqui ou em quem tiver.
///
/// Nem todo nó guarda todo registro - só os participantes guardam -, então o
/// coordenador de um `redeploy` pode perfeitamente não ter a cópia.
async fn find_record(
    state: &GatewayState,
    deploy_id: &str,
) -> Option<crate::deployment::Deployment> {
    if let Ok(caminho) = crate::deployment::path_in(&crate::deployment::dir(), deploy_id) {
        if let Ok(d) = crate::deployment::load(&caminho) {
            return Some(d);
        }
    }

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let buscas = state.all_peers().await.into_iter().filter_map(|peer| {
        if peer.node_id == self_id {
            return None;
        }
        let http = state.peer_http.for_peer(&peer.node_id).ok()?;
        let base = peer.gateway_url()?;
        let key = key.clone();
        let alvo = deploy_id.to_string();
        Some(async move {
            let body: serde_json::Value = http
                .get(format!("{base}/api/ui/deployments?local=true"))
                .bearer_auth(&key)
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            let registros: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            registros.into_iter().find(|d| d.deploy_id == alvo)
        })
    });

    futures::future::join_all(buscas)
        .await
        .into_iter()
        .flatten()
        .next()
}

/// Torna esta a implantação que atende quem pede só o nome do modelo.
///
/// Mexe no `activated_at` e reespalha. Como a escolha da padrão é **derivada**
/// (a de ativação mais recente), não há eleição nem quórum: basta o carimbo
/// novo chegar, e quem receber converge sozinho. Ver `deployment::active`.
async fn activate_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("deploy_id inválido: {id}") }));
    }
    let Some(mut registro) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("implantação {id} desconhecida") }));
    };

    registro.activated_at = chrono::Utc::now().to_rfc3339();
    publish_record(&state, &registro).await;

    Json(json!({
        "activated": true,
        "deploy_id": registro.deploy_id,
        "model": registro.model.label,
    }))
    .into_response()
}

/// Ajuste por máquina, para os dois botões que são de verdade por máquina.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PerNodeParams {
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RedeployRequest {
    pub params: RuntimeParams,
    /// Ajustes por máquina, por `node_id`. Ausente = todo mundo com os mesmos.
    #[serde(default)]
    pub per_node: HashMap<String, PerNodeParams>,
}

/// O que aconteceu com **uma** máquina numa reconfiguração.
#[derive(Debug, Serialize)]
pub struct RedeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub reconfigured: bool,
    pub error: Option<String>,
}

/// Reimplanta mexendo só nos parâmetros.
///
/// A mesma implantação, o mesmo `deploy_id`, as mesmas máquinas e as mesmas
/// faixas - só os parâmetros mudam. Por isso **não** passa pelo `deploy`: não
/// há o que planejar nem o que copiar, e re-planejar cunharia uma implantação
/// nova, quebrando todo cliente preso a `modelo@deploy_id`.
///
/// Cada nó recebe um `/api/ui/reconfigure`, que relê o próprio `NodeConfig` e
/// troca só os parâmetros (ver `NodeConfig::with_params`). O registro é
/// reescrito **depois** de os nós aceitarem: um registro que descreve uma
/// configuração que ninguém está rodando é pior que um registro velho.
async fn redeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Json(req): Json<RedeployRequest>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("deploy_id inválido: {id}") }));
    }
    let Some(registro) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("implantação {id} desconhecida") }));
    };

    let job_id = uuid::Uuid::new_v4().to_string();
    let titulo = format!(
        "Reconfigurar {} em {} {}",
        registro.model.label,
        registro.nodes.len(),
        if registro.nodes.len() == 1 {
            "nó"
        } else {
            "nós"
        }
    );
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "redeploy", titulo, "recarregando"),
    );

    let id_job = job_id.clone();
    tokio::spawn(async move {
        match run_redeploy(state, registro, req, &id_job).await {
            Ok(falhas) if falhas == 0 => {
                set_job(&id_job, "done", "pronto", "as máquinas recarregaram").await
            }
            Ok(falhas) => {
                set_job(
                    &id_job,
                    "failed",
                    "parcial",
                    &format!("{falhas} máquina(s) não recarregaram"),
                )
                .await
            }
            Err(e) => {
                warn!("reconfiguração falhou: {e}");
                set_job(&id_job, "failed", "falhou", &e).await
            }
        }
    });

    Json(json!({ "job_id": job_id, "deploy_id": id })).into_response()
}

async fn run_redeploy(
    state: GatewayState,
    mut registro: crate::deployment::Deployment,
    req: RedeployRequest,
    job_id: &str,
) -> Result<usize, String> {
    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    let total = registro.nodes.len().max(1);

    let mut resultados: Vec<RedeployOutcome> = Vec::new();
    let mut aceitos: HashMap<String, RuntimeParams> = HashMap::new();

    // Uma máquina de cada vez, de propósito: recarregar o modelo é o pico de
    // memória do nó, e mandar o cluster inteiro fazer isso junto é o caminho
    // mais curto para várias quedas por falta de VRAM ao mesmo tempo.
    for (i, no) in registro.nodes.iter().enumerate() {
        set_job(
            job_id,
            "running",
            "recarregando",
            &format!("{} ({}/{})", no.hostname, i + 1, total),
        )
        .await;
        set_job_progress(job_id, 100.0 * i as f64 / total as f64).await;

        // Os ajustes por máquina entram por cima dos parâmetros comuns; sem
        // ajuste, a máquina fica com os dela, que é o que a tela mostrou.
        let mut params = req.params.clone();
        if let Some(ajuste) = req.per_node.get(&no.node_id) {
            if let Some(g) = ajuste.n_gpu_layers {
                params.n_gpu_layers = g;
            }
            if let Some(k) = ajuste.offload_kqv {
                params.offload_kqv = k;
            }
        }

        let erro = reconfigure_one(&state, &peers, &no.node_id, &self_id, &key, &params).await;
        match erro {
            None => {
                aceitos.insert(no.node_id.clone(), params);
                resultados.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: true,
                    error: None,
                });
            }
            Some(e) => resultados.push(RedeployOutcome {
                node_id: no.node_id.clone(),
                hostname: no.hostname.clone(),
                reconfigured: false,
                error: Some(e),
            }),
        }
    }

    // Só o que a máquina de fato aceitou entra no registro. Uma que recusou
    // continua descrita com a configuração antiga, que é a que ela está
    // rodando - o registro tem que dizer o que **é**, não o que se pediu.
    for no in registro.nodes.iter_mut() {
        if let Some(p) = aceitos.remove(&no.node_id) {
            no.config = p;
        }
    }
    publish_record(&state, &registro).await;

    let falhas = resultados.iter().filter(|r| !r.reconfigured).count();
    set_job_progress(job_id, 100.0).await;
    {
        let mut map = jobs().write().await;
        if let Some(j) = map.get_mut(job_id) {
            j.outputs = resultados
                .iter()
                .map(|r| match &r.error {
                    Some(e) => format!("{}: {e}", r.hostname),
                    None => format!("{}: recarregada", r.hostname),
                })
                .collect();
        }
    }
    Ok(falhas)
}

/// Manda **uma** máquina recarregar com outros parâmetros.
async fn reconfigure_one(
    state: &GatewayState,
    peers: &[PeerInfo],
    node_id: &str,
    self_id: &str,
    key: &str,
    params: &RuntimeParams,
) -> Option<String> {
    let corpo = json!({ "params": params });

    if node_id == self_id {
        let resposta = crate::webui::reconfigure_local(state.clone(), params.clone()).await;
        return resposta.err();
    }

    let base = peers
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())?;
    let cliente = state.peer_http.for_peer(node_id).ok()?;
    match cliente
        .post(format!("{base}/api/ui/reconfigure"))
        .bearer_auth(key)
        // Recarregar pesos é minutos, não segundos: o `PEER_TIMEOUT` de uma
        // chamada de controle mataria a conexão no meio de um carregamento que
        // vai terminar bem.
        .timeout(Duration::from_secs(1800))
        .json(&corpo)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => None,
        Ok(r) => {
            let status = r.status();
            let detalhe = r.text().await.unwrap_or_default();
            Some(format!("a máquina recusou: HTTP {status} {detalhe}"))
        }
        Err(e) => Some(format!("máquina inacessível: {e}")),
    }
}

/// A visão do cluster e o que a tela de implantações precisa saber.
#[cfg(test)]
mod deployments_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentKind, DeploymentModel, DeploymentNode};
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::get as axum_get;

    fn no(id: &str, inicio: u32, fim: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: inicio,
            end_layer: fim,
            peer_index: 0,
            peer_count: 1,
            primary: true,
            part_sha256: None,
            tp_rank: None,
            config: RuntimeParams::default(),
        }
    }

    fn registro(id: &str, rotulo: &str, ativado: &str) -> Deployment {
        let mut d = Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: rotulo.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            vec![no("n1", 0, 31)],
        );
        d.activated_at = ativado.into();
        d
    }

    /// Um peer que responde `/api/ui/deployments?local=true` com os registros dados.
    async fn peer_with_records(registros: Vec<Deployment>) -> u16 {
        let corpo = json!({ "node_id": "outro", "deployments": registros });
        let app = Router::new().route(
            "/api/ui/deployments",
            axum_get(move || {
                let corpo = corpo.clone();
                async move { Json(corpo) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// O registro tem uma cópia por nó participante, então a listagem junta e
    /// **deduplica** - senão uma implantação de três nós apareceria três vezes.
    /// Empate desfeito pela ativação mais recente: um nó que estava fora do ar
    /// durante um `activate` tem a cópia velha, e ela não pode ganhar.
    #[tokio::test]
    async fn copies_of_one_deployment_collapse_to_the_newest() {
        let velha = registro("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let nova = registro("d-aaaaaaaa", "qwen3:8b", "2026-08-19T00:00:00Z");

        let porta = peer_with_records(vec![nova.clone()]).await;
        let mut peer = crate::gateway::testkit::peer("outro", "outro", porta);
        peer.model = "qwen3:8b".into();
        peer.deploy_id = "d-aaaaaaaa".into();
        peer.total_layers = 32;

        let state = state_with("eu", vec![peer]).await;
        let _ = velha; // a cópia velha vive no disco de quem não respondeu

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;

        let linhas = body["deployments"].as_array().unwrap();
        assert_eq!(linhas.len(), 1, "uma implantação, uma linha");
        assert_eq!(linhas[0]["deployment"]["deploy_id"], "d-aaaaaaaa");
        assert_eq!(
            linhas[0]["deployment"]["activated_at"], "2026-08-19T00:00:00Z",
            "a cópia mais recentemente ativada é a que vale"
        );
        assert_eq!(linhas[0]["serving"], true, "há peer vivo anunciando o id");
        assert_eq!(linhas[0]["is_default"], true, "é a única do seu modelo");
    }

    /// "No ar" e "padrão" são coisas diferentes, e a tela não pode confundir:
    /// duas implantações do mesmo modelo podem estar **as duas** servindo, mas
    /// só uma atende quem pede o nome pelado.
    #[tokio::test]
    async fn serving_and_default_are_not_the_same_thing() {
        let antiga = registro("d-aaaaaaaa", "qwen3:8b", "2026-01-01T00:00:00Z");
        let recente = registro("d-bbbbbbbb", "qwen3:8b", "2026-08-19T00:00:00Z");

        let porta = peer_with_records(vec![antiga, recente]).await;
        let mut p1 = crate::gateway::testkit::peer("outro", "outro", porta);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        p1.total_layers = 32;
        // Só a `d-aaaaaaaa` tem peer vivo; a `d-bbbbbbbb` está registrada mas
        // ninguém a está servindo.
        let state = state_with("eu", vec![p1]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: false }),
        )
        .await;
        let body = body_of(resp).await;
        let linhas = body["deployments"].as_array().unwrap();
        assert_eq!(linhas.len(), 2);

        let por_id = |id: &str| -> serde_json::Value {
            linhas
                .iter()
                .find(|l| l["deployment"]["deploy_id"] == id)
                .cloned()
                .unwrap()
        };

        let a = por_id("d-aaaaaaaa");
        assert_eq!(a["serving"], true, "tem peer anunciando");
        assert_eq!(a["is_default"], false, "mas não é a mais recente");

        let b = por_id("d-bbbbbbbb");
        assert_eq!(b["serving"], false, "registrada, mas ninguém no ar");
        assert_eq!(b["is_default"], true, "é a de ativação mais recente");
    }

    /// `?local=true` responde do disco e **não** varre o cluster - é o que
    /// impede a varredura de se chamar em círculo, um nó perguntando ao outro
    /// que pergunta de volta.
    #[tokio::test]
    async fn the_local_answer_does_not_fan_out() {
        let porta = peer_with_records(vec![registro(
            "d-cccccccc",
            "qwen3:8b",
            "2026-08-19T00:00:00Z",
        )])
        .await;
        let peer = crate::gateway::testkit::peer("outro", "outro", porta);
        let state = state_with("eu", vec![peer]).await;

        let resp = list_deployments(
            ConnectionAuth,
            State(state),
            Query(ListDeploymentsQuery { local: true }),
        )
        .await;
        let body = body_of(resp).await;

        // O registro do peer não aparece: esta resposta é só do disco local.
        assert!(body["deployments"].as_array().unwrap().is_empty());
        assert!(body.get("node_id").is_some());
    }

    #[tokio::test]
    async fn a_bogus_deploy_id_is_refused_before_anything_happens() {
        let state = state_with("eu", vec![]).await;
        let resp = activate_deployment(
            ConnectionAuth,
            State(state.clone()),
            Path("../../etc/passwd".to_string()),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

        let resp = redeploy_endpoint(
            ConnectionAuth,
            State(state),
            Path("nao-e-id".to_string()),
            Json(RedeployRequest {
                params: RuntimeParams::default(),
                per_node: HashMap::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}

// ── Somar ou tirar uma maquina de um modelo ja no ar ─────────────────────────
//
// Os dois caminhos tocam **uma** maquina. `deploy_endpoint` reimplanta o
// conjunto inteiro, e usar aquele caminho aqui pararia a inferencia de quem ja
// esta servindo - exatamente o que somar uma maquina existe para evitar.

#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    pub group: String,
    /// A implantação que ganha a máquina. Vazio enquanto a tela não manda:
    /// aí o nó entra sem registro, servindo a faixa, e a implantação segue
    /// sendo identificada pelo rótulo como antes.
    #[serde(default)]
    pub deploy_id: String,
    /// Which catalog the range is looked up in. A whole model already in
    /// service takes a second machine the same way a slice does — the added
    /// node becomes a parallel route over the same layers.
    #[serde(default)]
    pub kind: DeployKind,
    pub target_node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub params: RuntimeParams,
}

#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    pub group: String,
    pub target_node_id: String,
}

/// Faixas que este grupo tem no ar agora, uma por peer que o esta servindo.
async fn live_ranges(state: &GatewayState, group: &str) -> Vec<(String, u32, u32)> {
    state
        .all_peers()
        .await
        .into_iter()
        .filter(|p| p.model == group && p.total_layers > 0)
        .map(|p| (p.node_id, p.start_layer, p.end_layer))
        .collect()
}

async fn add_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeRequest>,
) -> Response {
    if req.start_layer > req.end_layer {
        return bad(json!({ "error": "faixa de camadas invertida" }));
    }

    // Re-varre em vez de confiar na tela: a previa pode ter um minuto, e uma
    // maquina que saiu nesse meio tempo receberia uma fatia.
    let machines = survey(&state, &req.group, req.kind, None, None)
        .await
        .machines;
    let Some(target) = machines.iter().find(|m| m.node_id == req.target_node_id) else {
        return bad(json!({ "error": "a maquina escolhida nao esta no cluster" }));
    };
    let Some(shard) = target.holds(req.start_layer, req.end_layer) else {
        return bad(json!({
            "error": format!(
                "{} nao tem {}; mande para la antes",
                target.label(),
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    };
    let source_model_path = shard.path.clone();
    let shard_sha256 = shard.sha256.clone();

    let outras: Vec<(u32, u32)> = live_ranges(&state, &req.group)
        .await
        .into_iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (i, f))
        .collect();
    let sobrepostas = overlapping_layers(&outras, (req.start_layer, req.end_layer));

    let candidatos = candidates(&machines);
    let Some(total_layers) = model_size(&candidatos).value() else {
        return bad(json!({
            "error": format!("as fatias de {} nao dizem o total de camadas", req.group)
        }));
    };
    if req.end_layer >= total_layers {
        return bad(json!({
            "error": format!(
                "{} passa do fim de um modelo de {total_layers} camadas",
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    }

    // `peer_index`/`peer_count` alimentam duas decisoes so: se este no exporta
    // o estado residual e se ele pula os embeddings. As duas perguntam "sou o
    // ultimo?", e a resposta vem da ordem das camadas no cluster - nao da
    // posicao numa lista de alvos que este caminho, por existir para tocar uma
    // maquina so, nunca vai montar.
    let (peer_index, peer_count) = split_position(&outras, (req.start_layer, req.end_layer));
    let target_node_id = req.target_node_id.clone();
    let response = crate::webui::launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            deploy_id: req.deploy_id.clone(),
            base_name: req.group.clone(),
            model_label: Some(req.group.clone()),
            deployment_name: req.group.clone(),
            total_layers,
            // Um alvo so: o `launch` para a inferencia dos alvos que recebe, e
            // esta lista tem exatamente a maquina que esta entrando.
            targets: vec![LaunchTarget {
                node_id: target_node_id.clone(),
                source_model_path,
                start_layer: req.start_layer,
                end_layer: req.end_layer,
                skip_split: true,
                part_sha256: shard_sha256,
                merge: None,
                n_gpu_layers: None,
                offload_kqv: None,
                no_embeddings: None,
                // Um alvo por requisição, mas a posição é a do pipeline: sem
                // isto o nó somado se diz o último da divisão.
                peer_index: Some(peer_index),
                peer_count: Some(peer_count),
                tp: None,
            }],
            // Quem e a entrada do pipeline nao muda por isto: so quem fica com
            // a camada 0 recebe o prompt, e o `launch` so marca `primary` no
            // alvo que ele recebe.
            primary_node_id: if req.start_layer == 0 {
                Some(target_node_id)
            } else {
                None
            },
            params: req.params,
        }),
    )
    .await;

    if !response.status().is_success() {
        let reason = refusal(response).await;
        return bad(json!({ "error": reason }));
    }

    Json(json!({
        "applied": true,
        "overlapping_layers": sobrepostas,
        "peer_index": peer_index,
        "peer_count": peer_count,
    }))
    .into_response()
}

async fn remove_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Response {
    let vivas = live_ranges(&state, &req.group).await;
    let Some((_, inicio, fim)) = vivas
        .iter()
        .find(|(id, _, _)| id == &req.target_node_id)
        .cloned()
    else {
        return bad(json!({ "error": "essa maquina nao esta servindo este modelo" }));
    };
    let restantes: Vec<(u32, u32)> = vivas
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let descobertas = uncovered_after_removal(&restantes, (inicio, fim));

    // Quem libera a memoria e o dono dela, como no botao "Liberar fatia": este
    // no so encaminha o pedido.
    let key = state.auth.read().await.connection_key.clone();
    if req.target_node_id == state.network.node_id {
        state.runtime.stop().await;
    } else {
        let Some(base) = gateway_of(&state, &req.target_node_id).await else {
            return bad(json!({ "error": "peer sem porta de gateway anunciada" }));
        };
        let cliente = match state.peer_http.for_peer(&req.target_node_id) {
            Ok(c) => c,
            Err(e) => return bad(json!({ "error": format!("sem cliente para o peer: {e}") })),
        };
        match cliente
            .post(format!("{base}/api/ui/unload"))
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return bad(json!({ "error": format!("o peer recusou: HTTP {}", r.status()) }))
            }
            Err(e) => return bad(json!({ "error": format!("peer inacessivel: {e}") })),
        }
    }

    Json(json!({
        "removed": true,
        "uncovered_layers": descobertas,
    }))
    .into_response()
}

// ── Tirando a pipeline inteira do ar ─────────────────────────────────────────
//
// O contrário de "Implantar na rede". Remover uma máquina do modelo já existia
// (`/api/ui/deploy/node/remove`), mas desfazer a implantação inteira era
// clicar máquina por máquina, na ordem certa, torcendo para não esquecer
// nenhuma — e uma esquecida é RAM e VRAM presas numa fatia que não serve mais
// para nada, porque o pipeline só planeja com a cobertura fechada.
//
// **O que isto NÃO faz: mexer no disco.** Nenhuma fatia é apagada. Desimplantar
// é devolver a memória; os arquivos continuam onde estão e a mesma implantação
// volta com um clique em vez de com 17 minutos de transferência por máquina.
// Apagar fatias é o que a tela de Modelos faz, com o seu próprio pedido de
// confirmação, e é uma decisão diferente desta.

/// Quem pode tirar a pipeline inteira do ar.
///
/// **Primeira versão: qualquer nó da rede pode.** É deliberado, e é onde a
/// restrição "somente o NÓ PRINCIPAL desimplanta" entra quando chegar a hora:
/// aqui dentro, em nenhum outro lugar. `undeploy_endpoint` já chama esta função
/// antes de tocar em qualquer máquina, então fechar o portão é trocar o corpo
/// desta função por
///
/// ```ignore
/// if role != Role::Principal {
///     return Err("somente o nó principal desimplanta a pipeline".into());
/// }
/// ```
///
/// e atualizar `any_node_may_undeploy_for_now`, o teste que hoje documenta a
/// porta aberta — de propósito, para que fechá-la seja uma edição consciente e
/// não um efeito colateral.
///
/// Note que o portão é do **papel deste nó**, o que recebeu o clique, e não do
/// papel de quem foi chamado: as chamadas para os peers usam `/api/ui/unload`,
/// que é a mesma rota do botão de descarregar uma máquina só e continua valendo
/// para qualquer nó.
fn undeploy_authority(_role: &crate::config::Role) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct UndeployRequest {
    /// Modelo cuja pipeline sai do ar. Vazio significa "a que estiver no ar",
    /// que é o caso do painel: ele mostra uma pipeline de cada vez.
    #[serde(default)]
    pub group: String,
    /// A implantação exata que sai do ar.
    ///
    /// É a saída para o beco em que `resolve_group` batia: com duas
    /// implantações no ar - inclusive **do mesmo modelo**, que agora é
    /// possível - o rótulo não distingue, e o id sim.
    #[serde(default)]
    pub deploy_id: String,
}

/// O que aconteceu com **uma** máquina. Uma linha por máquina, sempre — inclusive
/// para a que não respondeu.
#[derive(Debug, Serialize)]
pub struct UndeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    /// `true` só quando a máquina confirmou. Um peer calado é `false` com o
    /// motivo em `error`; contá-lo como sucesso seria dizer que a memória foi
    /// liberada sem ninguém ter dito isso.
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Descarrega o modelo de todas as máquinas que servem `group`.
///
/// Responde 200 com o relatório mesmo quando alguma máquina falha: metade
/// desimplantada é o estado real do cluster, e escondê-lo atrás de um erro
/// deixaria o operador sem saber em quais máquinas ainda há memória presa.
/// Só pedido inválido (nenhuma pipeline no ar, ou ambiguidade sobre qual) é 400.
async fn undeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<UndeployRequest>,
) -> Response {
    {
        let role = state.auth.read().await.role.clone();
        if let Err(e) = undeploy_authority(&role) {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    }

    let peers = state.all_peers().await;
    let alvo = match resolve_target(&req.deploy_id, &req.group, &peers) {
        Ok(a) => a,
        Err(e) => return bad(json!({ "error": e })),
    };

    // Re-varre em vez de confiar na tela, como o `deploy`: uma máquina que saiu
    // no meio tempo não pode virar uma linha de falha inventada.
    let serving: Vec<PeerInfo> = peers
        .into_iter()
        .filter(|p| p.total_layers > 0 && alvo.matches(p))
        .collect();
    if serving.is_empty() {
        return bad(json!({
            "error": format!("nenhuma máquina está servindo {}", alvo.describe())
        }));
    }

    let group = alvo.label();
    info!(
        "desimplantando {} de {} máquina(s)",
        alvo.describe(),
        serving.len()
    );

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();

    // Em paralelo: com onze peers e o prazo de um controle por peer, em série a
    // tela ficaria quase um minuto sem resposta.
    let mut results: Vec<UndeployOutcome> =
        futures::future::join_all(serving.into_iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                let error = release_one(&state, &peer, &key, &self_id).await.err();
                UndeployOutcome {
                    node_id: peer.node_id.clone(),
                    hostname: if peer.hostname.is_empty() {
                        peer.node_id[..12.min(peer.node_id.len())].to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    released: error.is_none(),
                    error,
                }
            }
        }))
        .await;
    results.sort_by_key(|r| r.start_layer);

    let released = results.iter().filter(|r| r.released).count();
    let failed = results.len() - released;
    if failed > 0 {
        warn!("{failed} máquina(s) não confirmaram a desimplantação de {group}");
    }

    // O registro só é esquecido depois: um registro apagado com máquinas ainda
    // servindo deixaria memória presa sem nada que a nomeasse. Neste nó basta
    // apagar o arquivo; os outros participantes apagam o seu ao serem
    // descarregados, e um que não respondeu guarda o dele - que é o certo, já
    // que ele ainda está no ar.
    if let UndeployTarget::Deployment { deploy_id, .. } = &alvo {
        if let Err(e) = crate::deployment::remove_in(&crate::deployment::dir(), deploy_id) {
            warn!("não deu para esquecer a implantação {deploy_id}: {e:#}");
        }
    }

    Json(json!({
        "group": group,
        "deploy_id": match &alvo {
            UndeployTarget::Deployment { deploy_id, .. } => Some(deploy_id.clone()),
            UndeployTarget::Label(_) => None,
        },
        "released": released,
        "failed": failed,
        "results": results,
    }))
    .into_response()
}

/// Qual pipeline o pedido quer derrubar.
///
/// Vazio e uma só no ar é a intenção óbvia. Vazio com duas é ambiguidade que
/// esta função se recusa a resolver por conta própria: derrubar a errada custa
/// uma reimplantação inteira.
fn resolve_group(requested: &str, peers: &[PeerInfo]) -> Result<String, String> {
    if !requested.is_empty() {
        return Ok(requested.to_string());
    }
    let mut models: Vec<String> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    models.sort();
    models.dedup();
    match models.len() {
        0 => Err("nenhuma pipeline no ar para desimplantar".to_string()),
        1 => Ok(models.remove(0)),
        _ => Err(format!(
            "há mais de uma pipeline no ar ({}); diga qual desimplantar",
            models.join(", ")
        )),
    }
}

/// O que uma desimplantação vai derrubar: uma implantação, ou tudo que serve um
/// rótulo.
enum UndeployTarget {
    /// Escolha exata. É o que distingue duas implantações do mesmo modelo.
    Deployment { deploy_id: String, label: String },
    /// Todo mundo que serve este rótulo, que é o comportamento de sempre e o
    /// único possível com peers que não anunciam implantação.
    Label(String),
}

impl UndeployTarget {
    fn matches(&self, p: &PeerInfo) -> bool {
        match self {
            UndeployTarget::Deployment { deploy_id, .. } => &p.deploy_id == deploy_id,
            UndeployTarget::Label(l) => &p.model == l,
        }
    }
    fn label(&self) -> String {
        match self {
            UndeployTarget::Deployment { label, .. } => label.clone(),
            UndeployTarget::Label(l) => l.clone(),
        }
    }
    fn describe(&self) -> String {
        match self {
            UndeployTarget::Deployment { deploy_id, label } if label.is_empty() => {
                format!("a implantação {deploy_id}")
            }
            UndeployTarget::Deployment { deploy_id, label } => {
                format!("{label} (implantação {deploy_id})")
            }
            UndeployTarget::Label(l) => l.clone(),
        }
    }
}

/// Qual implantação o pedido quer derrubar.
///
/// Com `deploy_id`, não há o que deduzir - é aquela, e o rótulo serve só para a
/// mensagem. Sem ele, cai na regra antiga sobre rótulos, que continua sendo a
/// certa num cluster onde cada modelo está no ar uma vez só.
fn resolve_target(
    deploy_id: &str,
    group: &str,
    peers: &[PeerInfo],
) -> Result<UndeployTarget, String> {
    if !deploy_id.is_empty() {
        if !crate::deployment::is_deploy_id(deploy_id) {
            return Err(format!("deploy_id inválido: {deploy_id}"));
        }
        let label = peers
            .iter()
            .find(|p| p.deploy_id == deploy_id)
            .map(|p| p.model.clone())
            .unwrap_or_else(|| group.to_string());
        return Ok(UndeployTarget::Deployment {
            deploy_id: deploy_id.to_string(),
            label,
        });
    }
    resolve_group(group, peers).map(UndeployTarget::Label)
}

/// Libera a memória de **uma** máquina, pela mesma porta do botão que já existe.
///
/// Quem mata o processo de inferência é sempre o dono da memória: este nó só
/// encaminha, exatamente como `remove_node_endpoint`.
async fn release_one(
    state: &GatewayState,
    peer: &PeerInfo,
    key: &str,
    self_id: &str,
) -> Result<(), String> {
    if peer.node_id == self_id {
        state.runtime.stop().await;
        return Ok(());
    }
    let base = peer
        .gateway_url()
        .ok_or_else(|| "peer sem porta de gateway anunciada".to_string())?;
    let cliente = state
        .peer_http
        .for_peer(&peer.node_id)
        .map_err(|e| format!("sem cliente para a máquina: {e}"))?;
    match cliente
        .post(format!("{base}/api/ui/unload"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("a máquina recusou: HTTP {}", r.status())),
        Err(e) => Err(format!("máquina inacessível: {e}")),
    }
}

fn bad(body: serde_json::Value) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// The reason a refused launch gives, not just its status code.
///
/// `launch` answers `{"error": "..."}` and the operator only ever saw
/// "HTTP 400", which is the same dead end this whole screen exists to avoid.
async fn refusal(response: Response) -> String {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        });
    match body {
        Some(reason) => format!("o launch recusou: {reason}"),
        None => format!("o launch recusou: HTTP {status}"),
    }
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
    watching: &[(String, String, String)],
    job_id: &str,
) -> Result<(), String> {
    loop {
        let mut done = 0usize;
        let mut fraction_sum = 0.0;

        for (node_id, gateway, id) in watching {
            let cliente = state
                .peer_http
                .for_peer(node_id)
                .map_err(|e| format!("sem cliente para {gateway}: {e}"))?;
            let body: serde_json::Value = cliente
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
                        job.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("sem detalhe")
                    ))
                }
                _ => {
                    let total = job
                        .get("total_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let got = job
                        .get("received_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
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

/// A rota de plano lendo o inventário de verdade de peers de verdade.
///
/// Os testes de `plan` montam `Machine` à mão, o que prova a decisão mas não
/// prova de onde ela lê. Aqui a resposta vem por HTTP, do mesmo
/// `/api/ui/models` que cada máquina serve, e é por isto que "o modelo inteiro
/// aparece" e "a fatia de mesmo nome não entra no lugar dele" são fatos e não
/// suposições.
#[cfg(test)]
mod whole_route_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::get as axum_get;

    /// Uma máquina servindo o catálogo dado, com memória e disco de sobra.
    async fn catalog_machine(catalog: serde_json::Value) -> u16 {
        let app = Router::new()
            .route(
                "/api/ui/models",
                axum_get(move || {
                    let catalog = catalog.clone();
                    async move { Json(catalog) }
                }),
            )
            .route(
                "/api/ui/node",
                axum_get(|| async {
                    Json(json!({
                        "system": {
                            "ram": { "available_mb": 64_000.0 },
                            "disk": { "free_mb": 900_000.0 },
                        }
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// O catálogo de uma máquina que tem o GGUF inteiro **e** fatias de outro
    /// modelo, que é o estado normal de uma máquina do cluster.
    fn catalog() -> serde_json::Value {
        json!({
            "hostname": "ksia",
            "models": [
                {
                    "id": "1",
                    "name": "qwen3.5-9b",
                    "source": "lmstudio",
                    "source_label": "LM Studio",
                    "path": "/lm/qwen3.5-9b.gguf",
                    "size_bytes": 5_368_709_120u64,
                    "architecture": "qwen3",
                    "total_layers": 32,
                    "quant": "Q4_K_M",
                    "shard_range": serde_json::Value::Null,
                    "shard_group": serde_json::Value::Null,
                    "capabilities": {
                        "thinking": true,
                        "mtp": false,
                    },
                },
                {
                    "id": "2",
                    "name": "qwen_0_15.gguf",
                    "source": "folder",
                    "source_label": "Pasta",
                    "path": "/models/qwen/qwen_0_15.gguf",
                    "size_bytes": 2_000_000_000u64,
                    "architecture": "qwen3",
                    "total_layers": 32,
                    "quant": serde_json::Value::Null,
                    "shard_range": [0, 15],
                    "shard_group": "qwen",
                }
            ],
            "shard_sets": [{
                "group": "qwen",
                "total_layers": 32,
                "total_layers_inferred": false,
                "complete": false,
                "shards": [{
                    "name": "qwen_0_15.gguf",
                    "path": "/models/qwen/qwen_0_15.gguf",
                    "size_bytes": 2_000_000_000u64,
                    "total_layers": 32,
                    "shard_range": [0, 15],
                }],
            }],
            "scanned": [],
            "metadata_complete": true,
        })
    }

    /// Um estado cujo único inventário é a máquina de mentira.
    ///
    /// `all_peers` sempre inclui esta máquina, e o `gateway_port` do testkit é
    /// 8080 - o mesmo em que um nó de verdade escuta na máquina do captain. Sem
    /// apontá-lo para uma porta morta, a varredura sai da suíte e lê o catálogo
    /// do cluster real: o teste passa ou falha conforme o que estiver no disco
    /// de quem o roda.
    async fn state_seeing_only(port: u16) -> GatewayState {
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let mut state = state_with(
            "eu",
            vec![crate::gateway::testkit::peer("ksia", "ksia", port)],
        )
        .await;
        state.gateway_port = dead;
        state
    }

    async fn plan_body(state: GatewayState, req: serde_json::Value) -> serde_json::Value {
        body_of(
            plan_endpoint(
                ConnectionAuth,
                State(state),
                Json(serde_json::from_value(req).expect("DeployRequest")),
            )
            .await,
        )
        .await
    }

    async fn plan_for(kind: &str, group: &str) -> serde_json::Value {
        let port = catalog_machine(catalog()).await;
        let state = state_seeing_only(port).await;
        plan_body(state, json!({ "group": group, "kind": kind })).await
    }

    /// O caminho inteiro: a rota pergunta à máquina, acha o GGUF inteiro e
    /// devolve um plano de um passo cobrindo o modelo todo.
    #[tokio::test]
    async fn the_route_plans_a_whole_model_from_the_machines_own_catalog() {
        let body = plan_for("whole", "qwen3.5-9b").await;

        assert_eq!(body["kind"], "whole", "o plano diz que pergunta respondeu");
        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
        assert_eq!(body["complete"], true);
        assert_eq!(body["total_layers"], 32);

        let steps = body["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["start_layer"], 0);
        assert_eq!(steps[0]["end_layer"], 31);
        assert_eq!(steps[0]["target_node_id"], "ksia");
        assert_eq!(steps[0]["ready"], true);
        assert_eq!(steps[0]["primary"], true);

        // A fatia da mesma máquina não entrou no inventário desta pergunta.
        let available = body["available"].as_array().unwrap();
        assert_eq!(available.len(), 1, "só o modelo inteiro: {available:?}");
        assert_eq!(available[0]["name"], "qwen3.5-9b");
    }

    #[tokio::test]
    async fn the_plan_ignores_unrelated_catalog_enrichment() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31 }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn the_plan_names_metadata_missing_from_the_selected_model() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["models"][0]["total_layers"] = serde_json::Value::Null;
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31 }]
            }),
        )
        .await;

        let blocked = body["blocked"].as_str().unwrap_or_default();
        assert!(blocked.contains("seleção de qwen3.5-9b"), "{blocked}");
        assert!(blocked.contains("metadados do GGUF"), "{blocked}");
    }

    #[tokio::test]
    async fn the_plan_does_not_wait_for_an_old_peer_without_catalog_state() {
        let mut legacy = catalog();
        legacy.as_object_mut().unwrap().remove("metadata_complete");
        let port = catalog_machine(legacy).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31 }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn a_complete_selected_shard_set_is_ready_during_other_enrichment() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["shard_sets"][0]["complete"] = json!(true);
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen",
                "kind": "shards",
                "selection": [{ "start_layer": 0, "end_layer": 15 }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn an_unresolved_selected_shard_names_the_missing_range() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["shard_sets"][0]["complete"] = json!(true);
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen",
                "kind": "shards",
                "selection": [
                    { "start_layer": 0, "end_layer": 15 },
                    { "start_layer": 16, "end_layer": 31 }
                ]
            }),
        )
        .await;

        let blocked = body["blocked"].as_str().unwrap_or_default();
        assert!(blocked.contains("L16-31"), "{blocked}");
        assert!(blocked.contains("metadados do GGUF"), "{blocked}");
    }

    /// A pergunta padrão continua sendo a das fatias, e ela não enxerga o
    /// modelo inteiro - as duas listas saem da mesma varredura, e trocá-las
    /// faria um `plan` antigo mudar de resposta.
    #[tokio::test]
    async fn the_slice_question_is_untouched_and_still_the_default() {
        let body = plan_for("shards", "qwen").await;
        let available = body["available"].as_array().unwrap();
        assert_eq!(available.len(), 1, "{available:?}");
        assert_eq!(available[0]["name"], "qwen_0_15.gguf");
        assert_eq!(available[0]["end_layer"], 15);
        assert_eq!(body["kind"], "shards");

        // Sem `kind` nenhum, o mesmo resultado: `shards` é o padrão.
        let port = catalog_machine(catalog()).await;
        let sem_kind = plan_body(state_seeing_only(port).await, json!({ "group": "qwen" })).await;
        assert_eq!(sem_kind["kind"], "shards");
        assert_eq!(sem_kind["available"].as_array().unwrap().len(), 1);
    }

    /// Um modelo que nenhuma máquina tem é recusa nomeando o modelo, não uma
    /// lista vazia que se lê como "está tudo certo, mas não dá".
    #[tokio::test]
    async fn a_model_nobody_has_is_refused_by_name() {
        let body = plan_for("whole", "llama3-70b").await;
        let motivo = body["blocked"].as_str().expect("recusa");
        assert!(motivo.contains("llama3-70b"), "{motivo}");
        assert!(!motivo.contains("fatias de"), "{motivo}");
    }
}

/// Tirar a pipeline inteira do ar.
#[cfg(test)]
mod undeploy_tests {
    use super::*;
    use crate::config::Role;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Um peer servindo `qwen35` na faixa dada.
    fn peer(node_id: &str, host: &str, gateway_port: u16, range: (u32, u32)) -> PeerInfo {
        let mut p = crate::gateway::testkit::peer(node_id, host, gateway_port);
        p.model = "qwen35".into();
        p.start_layer = range.0;
        p.end_layer = range.1;
        p.total_layers = 32;
        p
    }

    /// Uma máquina que responde `/api/ui/unload` com o status pedido, contando
    /// quantas vezes foi chamada.
    async fn fake_machine(status: axum::http::StatusCode) -> (u16, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/ui/unload",
            axum_post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (status, Json(json!({ "unloaded": true })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, hits)
    }

    /// Duas implantações do mesmo modelo: derrubar uma tem que deixar a outra
    /// servindo.
    ///
    /// Antes do `deploy_id` isto era inexprimível nos dois sentidos - não dava
    /// para ter as duas no ar, e `resolve_group` só sabia falar de rótulos, de
    /// modo que um "desimplantar qwen35" teria levado as duas junto.
    #[tokio::test]
    async fn undeploying_one_deployment_leaves_the_other_serving() {
        let (porta_a, chamadas_a) = fake_machine(axum::http::StatusCode::OK).await;
        let (porta_b, chamadas_b) = fake_machine(axum::http::StatusCode::OK).await;

        let mut a = peer("no-a", "maquina-a", porta_a, (0, 31));
        let mut b = peer("no-b", "maquina-b", porta_b, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();
        b.deploy_id = "d-bbbbbbbb".into();

        let state = state_with("eu", vec![a, b]).await;
        state.auth.write().await.role = Role::Principal;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: "d-aaaaaaaa".into(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_of(resp).await;

        assert_eq!(body["released"], 1, "só a implantação escolhida sai do ar");
        assert_eq!(
            body["results"].as_array().unwrap().len(),
            1,
            "a outra implantação nem aparece no relatório"
        );
        assert_eq!(chamadas_a.load(Ordering::SeqCst), 1);
        assert_eq!(
            chamadas_b.load(Ordering::SeqCst),
            0,
            "a máquina da outra implantação não pode ser tocada"
        );
    }

    /// Um id que ninguém serve não pode virar "derruba o rótulo inteiro".
    #[tokio::test]
    async fn an_unknown_deployment_id_releases_nothing() {
        let (porta, chamadas) = fake_machine(axum::http::StatusCode::OK).await;
        let mut a = peer("no-a", "maquina-a", porta, (0, 31));
        a.deploy_id = "d-aaaaaaaa".into();

        let state = state_with("eu", vec![a]).await;
        state.auth.write().await.role = Role::Principal;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: "d-cccccccc".into(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(chamadas.load(Ordering::SeqCst), 0);
    }

    /// O relatório tem uma linha por máquina, e uma máquina que não confirmou
    /// **não** conta como liberada. Fingir sucesso aqui deixaria memória presa
    /// numa caixa que o operador acha que esvaziou.
    #[tokio::test]
    async fn every_machine_is_reported_and_a_silent_one_is_not_a_success() {
        let (ok_port, ok_hits) = fake_machine(axum::http::StatusCode::OK).await;
        let (bad_port, bad_hits) =
            fake_machine(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = state_with(
            "eu",
            vec![
                peer("no-a", "ksia", ok_port, (0, 15)),
                peer("no-b", "macbook", bad_port, (16, 31)),
            ],
        )
        .await;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: String::new(),
                group: "qwen35".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_of(resp).await;

        assert_eq!(body["group"], "qwen35");
        assert_eq!(body["released"], 1);
        assert_eq!(body["failed"], 1);

        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "uma linha por máquina, sempre");
        // Ordenado pela camada inicial: o relatório se lê na ordem do pipeline.
        assert_eq!(results[0]["hostname"], "ksia");
        assert_eq!(results[0]["released"], true);
        assert!(results[0].get("error").is_none());

        assert_eq!(results[1]["hostname"], "macbook");
        assert_eq!(results[1]["released"], false);
        assert!(
            results[1]["error"].as_str().unwrap().contains("500"),
            "o motivo tem que ser o que a máquina disse: {}",
            results[1]["error"]
        );

        // As duas foram mesmo chamadas: nenhuma foi pulada por causa da outra.
        assert_eq!(ok_hits.load(Ordering::SeqCst), 1);
        assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    }

    /// Uma máquina fora do ar é falha com motivo, não uma linha que some.
    #[tokio::test]
    async fn an_unreachable_machine_still_gets_a_line() {
        let (ok_port, _) = fake_machine(axum::http::StatusCode::OK).await;
        // Porta que ninguém atende: reservada e imediatamente devolvida.
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let state = state_with(
            "eu",
            vec![
                peer("no-a", "ksia", ok_port, (0, 15)),
                peer("no-b", "sumida", dead, (16, 31)),
            ],
        )
        .await;

        let body = body_of(
            undeploy_endpoint(
                ConnectionAuth,
                State(state),
                Json(UndeployRequest {
                    deploy_id: String::new(),
                    group: "qwen35".into(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(body["failed"], 1);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results[1]["hostname"], "sumida");
        assert_eq!(results[1]["released"], false);
        assert!(results[1]["error"].is_string());
    }

    /// Sem pipeline no ar não há o que desimplantar, e dizer isso é melhor que
    /// devolver um relatório vazio que se lê como sucesso.
    #[tokio::test]
    async fn undeploying_nothing_is_a_refusal_not_an_empty_report() {
        let state = state_with("eu", vec![]).await;
        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                deploy_id: String::new(),
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// Qual pipeline derrubar: óbvio com uma, recusa com duas. Derrubar a
    /// errada custa uma reimplantação inteira, então o palpite não é nosso.
    #[test]
    fn the_group_is_only_guessed_when_there_is_no_doubt() {
        let uma = vec![peer("a", "h", 1, (0, 15)), peer("b", "h", 2, (16, 31))];
        assert_eq!(resolve_group("", &uma).unwrap(), "qwen35");
        assert_eq!(resolve_group("outro", &uma).unwrap(), "outro");

        let mut duas = uma.clone();
        duas[1].model = "llama3".into();
        let erro = resolve_group("", &duas).unwrap_err();
        assert!(erro.contains("llama3") && erro.contains("qwen35"), "{erro}");

        assert!(resolve_group("", &[]).is_err());
    }

    /// **Documenta a porta aberta de propósito.** Nesta primeira versão qualquer
    /// nó desimplanta; quando a restrição "somente o nó principal" entrar em
    /// `undeploy_authority`, é este teste que tem que mudar junto - e é isso que
    /// torna o fechamento uma decisão, e não um efeito colateral.
    #[test]
    fn any_node_may_undeploy_for_now() {
        assert!(undeploy_authority(&Role::Node).is_ok());
        assert!(undeploy_authority(&Role::Principal).is_ok());
    }
}

#[cfg(test)]
mod redeploy_preflight_tests {
    use super::*;
    use crate::deployment::{Deployment, DeploymentIdentity, DeploymentKind, DeploymentNode};
    use crate::gateway::testkit::state_with;
    use axum::routing::{get as axum_get, post as axum_post};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    async fn peer_with_deployment(
        identity: Option<DeploymentIdentity>,
        unload_status: axum::http::StatusCode,
        identity_path: &'static str,
    ) -> (u16, Arc<AtomicU32>) {
        let released = Arc::new(AtomicBool::new(false));
        let unload_hits = Arc::new(AtomicU32::new(0));
        let record = Deployment::new(
            "d-aaaaaaaa".into(),
            DeploymentKind::Whole,
            crate::deployment::DeploymentModel {
                label: "qwen".into(),
                total_layers: 32,
                pom_id: None,
                identity,
            },
            "coordinator",
            vec![DeploymentNode {
                node_id: "peer-a".into(),
                hostname: "ksia".into(),
                start_layer: 0,
                end_layer: 31,
                peer_index: 0,
                peer_count: 1,
                primary: true,
                part_sha256: None,
                tp_rank: None,
                config: RuntimeParams::default(),
            }],
        );
        let deployments = json!({ "deployments": [record] });
        let released_for_unload = released.clone();
        let unload_hits_for_route = unload_hits.clone();
        let released_for_node = released.clone();
        let app = Router::new()
            .route(
                "/api/ui/deployments",
                axum_get(move || {
                    let deployments = deployments.clone();
                    async move { Json(deployments) }
                }),
            )
            .route(
                "/api/ui/unload",
                axum_post(move || {
                    let released = released_for_unload.clone();
                    let hits = unload_hits_for_route.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        if unload_status.is_success() {
                            released.store(true, Ordering::SeqCst);
                        }
                        (
                            unload_status,
                            Json(json!({ "unloaded": unload_status.is_success() })),
                        )
                    }
                }),
            )
            .route(
                "/api/ui/node",
                axum_get(move || {
                    let released = released_for_node.clone();
                    async move {
                        if released.load(Ordering::SeqCst) {
                            Json(json!({
                                "config": null,
                                "status": {"state": "disabled"},
                                "active_model": "",
                                "active_deployment": ""
                            }))
                        } else {
                            Json(json!({
                                "config": {"model_path": "/models/active.gguf"},
                                "status": {"state": "ready"},
                                "active_model": "qwen",
                                "active_deployment": "d-aaaaaaaa"
                            }))
                        }
                    }
                }),
            )
            .route(
                "/api/ui/model-identity",
                axum_post(move || async move {
                    Json(json!({
                        "canonical_path": identity_path,
                        "sha256": serde_json::Value::Null
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, unload_hits)
    }

    fn serving_peer(port: u16) -> PeerInfo {
        let mut peer = crate::gateway::testkit::peer("peer-a", "ksia", port);
        peer.model = "qwen".into();
        peer.deploy_id = "d-aaaaaaaa".into();
        peer.start_layer = 0;
        peer.end_layer = 31;
        peer.total_layers = 32;
        peer
    }

    fn desired(hash: char) -> RequestedModelIdentity {
        RequestedModelIdentity {
            proof: Some(DeploymentIdentity::Sha256 {
                sha256: hash.to_string().repeat(64),
            }),
            paths_by_peer: HashMap::new(),
        }
    }

    async fn install_job(state: &GatewayState, id: &str) {
        jobs()
            .write()
            .await
            .insert(id.into(), new_job(id, "deploy", "teste", "preparando"));
        let _ = state;
    }

    #[tokio::test]
    async fn same_proven_model_is_released_and_waited_before_continue() {
        let (port, hits) = peer_with_deployment(
            Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            axum::http::StatusCode::OK,
            "/models/active.gguf",
        )
        .await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-same").await;

        prepare_existing_model(&state, "qwen", &desired('a'), "preflight-same")
            .await
            .expect("a implantação igual deve ser liberada");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn identity_failure_is_visible_and_does_not_unload() {
        let (port, hits) =
            peer_with_deployment(None, axum::http::StatusCode::OK, "/models/different.gguf").await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-unknown").await;
        let error = prepare_existing_model(
            &state,
            "qwen",
            &RequestedModelIdentity {
                proof: Some(DeploymentIdentity::Sha256 {
                    sha256: "a".repeat(64),
                }),
                paths_by_peer: [("peer-a".into(), "/models/qwen.gguf".into())]
                    .into_iter()
                    .collect(),
            },
            "preflight-unknown",
        )
        .await
        .expect_err("sem prova, não pode desimplantar");
        assert!(error.contains("não foi possível provar"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_unload_is_visible_and_blocks_the_new_deploy() {
        let (port, hits) = peer_with_deployment(
            Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "/models/active.gguf",
        )
        .await;
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        install_job(&state, "preflight-failed").await;
        let error = prepare_existing_model(&state, "qwen", &desired('a'), "preflight-failed")
            .await
            .expect_err("falha de unload bloqueia o deploy");
        assert!(error.contains("desimplantação falhou"), "{error}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idle_timeout_names_the_peer_and_the_occupied_state() {
        let app = Router::new().route(
            "/api/ui/node",
            axum_get(|| async {
                Json(json!({
                    "config": {"model_path": "/models/active.gguf"},
                    "status": {"state": "ready"},
                    "active_model": "qwen",
                    "active_deployment": "d-aaaaaaaa"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let state = state_with("coordinator", vec![serving_peer(port)]).await;
        let error =
            wait_for_peer_idle_with_timeout(&state, &serving_peer(port), Duration::from_millis(10))
                .await
                .expect_err("peer ocupado deve estourar o limite");
        assert!(
            error.contains("ksia") && error.contains("ocupado"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::deployment::{DeploymentIdentity, DeploymentModel};

    #[test]
    fn model_identity_requires_proof_and_allows_same_peer_path_fallback() {
        let desired = super::RequestedModelIdentity {
            proof: Some(DeploymentIdentity::Sha256 {
                sha256: "a".repeat(64),
            }),
            paths_by_peer: [("peer-a".into(), "/models/qwen.gguf".into())]
                .into_iter()
                .collect(),
        };
        let same = DeploymentModel {
            label: "qwen".into(),
            total_layers: 32,
            pom_id: None,
            identity: desired.proof.clone(),
        };
        assert!(super::deployment_identity_matches(
            &desired,
            &same,
            "peer-a",
            Some("/other/path.gguf")
        ));

        let unknown = DeploymentModel {
            identity: None,
            ..same.clone()
        };
        assert!(super::deployment_identity_matches(
            &desired,
            &unknown,
            "peer-a",
            Some("/models/qwen.gguf")
        ));
        assert!(!super::deployment_identity_matches(
            &desired,
            &unknown,
            "peer-b",
            Some("/models/qwen.gguf")
        ));
        assert!(!super::deployment_identity_matches(
            &desired,
            &DeploymentModel {
                identity: Some(DeploymentIdentity::Sha256 {
                    sha256: "b".repeat(64),
                }),
                ..same
            },
            "peer-a",
            None
        ));
    }

    #[test]
    fn node_idle_requires_disabled_status_and_no_configuration() {
        assert!(super::node_is_idle(&serde_json::json!({
            "config": null,
            "status": {"state": "disabled"},
            "active_deployment": "",
            "active_model": ""
        })));
        assert!(!super::node_is_idle(&serde_json::json!({
            "config": null,
            "status": {"state": "starting"},
            "active_deployment": "",
            "active_model": ""
        })));
        assert!(!super::node_is_idle(&serde_json::json!({
            "config": {"model_path": "/models/qwen.gguf"},
            "status": {"state": "disabled"},
            "active_deployment": "",
            "active_model": ""
        })));
    }

    #[test]
    fn deployment_request_defaults_to_simple_and_normalizes_name() {
        let req: super::DeployRequest = serde_json::from_value(serde_json::json!({
            "group": "qwen35"
        }))
        .expect("request válida");

        assert_eq!(req.strategy, super::DeployStrategy::Simple);
        assert_eq!(
            super::normalize_deployment_name(req.deployment_name.as_deref(), &req.group)
                .expect("nome padrão"),
            "qwen35"
        );
    }

    #[test]
    fn deployment_name_rejects_blank_and_overlong_values() {
        assert_eq!(
            super::normalize_deployment_name(Some("  "), "qwen35").expect("fallback"),
            "qwen35"
        );
        assert!(super::normalize_deployment_name(Some(&"x".repeat(129)), "qwen35").is_err());
    }

    /// O tipo tensor-parallel nomeia o grupo, não uma fatia nem um modelo: a
    /// tela oferece o formato, e a recusa tem que falar a língua do formato.
    #[test]
    fn tensor_parallel_kind_names_the_group() {
        use super::DeployKind;
        assert_eq!(
            DeployKind::TensorParallel.unit("qwen", 0, 31),
            "o grupo tensor-parallel qwen"
        );
        assert_eq!(
            DeployKind::TensorParallel.indefinite(),
            "um grupo tensor-parallel"
        );
        assert!(!DeployKind::TensorParallel.whole(), "TP não é whole");
    }

    /// Um no somado no meio do pipeline NAO e o ultimo da divisao.
    ///
    /// `peer_index`/`peer_count` respondem a uma pergunta so - "sou o ultimo?" -
    /// e dela saem duas decisoes do llama.cpp: exportar o estado residual e
    /// pular os embeddings. O caminho incremental manda um alvo por vez, e
    /// deixar o `launch` deduzir a posicao da lista faria todo no somado se
    /// declarar o ultimo. O sintoma e o peer do meio responder
    /// "get_embeddings_ith: invalid embeddings id 0, reason: no embeddings",
    /// porque o motor subiu sem captura e a inferencia so descobre no primeiro
    /// token.
    #[test]
    fn a_node_added_in_the_middle_is_not_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (32, 39)], (12, 31));
        assert_eq!(count, 3, "tres faixas depois de somar esta");
        assert_eq!(index, 1, "12-31 fica no meio, entre 0-11 e 32-39");
        assert!(index + 1 < count, "nao pode se dizer o ultimo da divisao");
    }

    #[test]
    fn a_node_added_at_the_tail_is_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (32, 39));
        assert_eq!((index, count), (2, 3));
    }

    /// Substituir quem ja serve aquela faixa nao inventa um peer a mais.
    #[test]
    fn replacing_the_same_range_keeps_the_count() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (12, 31));
        assert_eq!((index, count), (1, 2));
    }

    /// Faixa sobreposta no cluster deixa de ser erro: e assim que se soma uma
    /// rota paralela. A invariante "uma camada, um no" continua valendo, mas
    /// dentro de um plano, e quem a garante e `plan_pipeline`, que varre camada
    /// a camada escolhendo um peer por posicao.
    #[test]
    fn overlapping_ranges_are_reported_but_not_refused() {
        let sobrepostas = super::overlapping_layers(&[(0, 19), (20, 39)], (0, 19));
        assert_eq!(sobrepostas, (0..=19).collect::<Vec<u32>>());
    }

    #[test]
    fn a_range_that_fills_a_gap_overlaps_nothing() {
        assert!(super::overlapping_layers(&[(0, 19)], (20, 39)).is_empty());
    }

    #[test]
    fn a_partial_overlap_reports_only_the_shared_layers() {
        assert_eq!(
            super::overlapping_layers(&[(0, 19)], (15, 25)),
            vec![15, 16, 17, 18, 19]
        );
    }

    /// Tirar uma replica nao descobre camada nenhuma: a rede so perde uma rota
    /// paralela. Avisar de buraco aqui seria mentira.
    #[test]
    fn removing_a_replica_uncovers_nothing() {
        assert!(super::uncovered_after_removal(&[(0, 19), (20, 39)], (0, 19)).is_empty());
    }

    /// Tirar a ultima daquela faixa descobre as camadas dela, e o pipeline para
    /// de planejar ate alguem assumir.
    #[test]
    fn removing_the_last_holder_uncovers_its_layers() {
        assert_eq!(
            super::uncovered_after_removal(&[(20, 39)], (0, 19)),
            (0..=19).collect::<Vec<u32>>(),
        );
    }

    #[test]
    fn a_partially_replicated_range_uncovers_only_the_rest() {
        assert_eq!(
            super::uncovered_after_removal(&[(0, 9), (20, 39)], (0, 19)),
            (10..=19).collect::<Vec<u32>>()
        );
    }

    use super::*;

    const GIB: u64 = 1_073_741_824;

    struct Spec {
        id: &'static str,
        mem_free_mb: u64,
        disk_free_mb: u64,
        held: Vec<(u32, u32)>,
        /// What each of this machine's slices declares as the model's size.
        /// `None` is a slice cut before `pom.original_block_count` existed.
        declares: Option<u32>,
    }

    fn m(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            id,
            mem_free_mb,
            disk_free_mb: 500_000,
            held: held.to_vec(),
            declares: Some(32),
        }
    }

    /// A machine whose slices carry no layer total, like the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf`.
    fn nameless_machine(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            declares: None,
            ..m(id, mem_free_mb, held)
        }
    }

    fn build(specs: Vec<Spec>) -> Vec<Machine> {
        specs
            .into_iter()
            .map(|s| Machine {
                node_id: s.id.to_string(),
                hostname: s.id.to_string(),
                gateway_url: None,
                inference_url: None,
                inference_candidates: Vec::new(),
                addr_candidates: Vec::new(),
                libp2p_peer_id: None,
                supports_tunnel: false,
                inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
                mem_free_mb: s.mem_free_mb,
                gpu_free_mb: s.mem_free_mb,
                ram_free_mb: s.mem_free_mb,
                disk_free_mb: s.disk_free_mb,
                rtt_ms: None,
                load: 0,
                held: s
                    .held
                    .iter()
                    .map(|(a, b)| HeldShard {
                        start_layer: *a,
                        end_layer: *b,
                        total_layers: s.declares,
                        name: format!("m_{a}_{b}.gguf"),
                        path: format!("/models/m_{a}_{b}.gguf"),
                        sha256: None,
                        pom_id: None,
                        virtual_slice: false,
                        tp_rank: None,
                        tp_size: None,
                        is_moe: false,
                        model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                            thinking: Some(true),
                            mtp: Some(false),
                        }),
                        size_bytes: GIB,
                    })
                    .collect(),
            })
            .collect()
    }

    fn pick(start: u32, end: u32, target: &str) -> SelectedSlice {
        SelectedSlice {
            start_layer: start,
            end_layer: end,
            target_node_id: target.to_string(),
            n_gpu_layers: None,
            offload_kqv: None,
        }
    }

    #[test]
    fn model_mtp_comes_from_the_final_selected_file_and_runtime_is_only_a_warning() {
        let mut machines = build(vec![
            m("first", 900_000, &[(0, 15)]),
            m("last", 900_000, &[(16, 31)]),
        ]);
        machines[0].held[0].model_capabilities.as_mut().unwrap().mtp = Some(false);
        machines[1].held[0].model_capabilities.as_mut().unwrap().mtp = Some(true);

        let p = plan(
            "model",
            &machines,
            Some(&[pick(0, 15, "first"), pick(16, 31, "last")]),
        );

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Supported
        );
        assert!(p
            .capabilities
            .mtp
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("runtime")));
    }

    #[test]
    fn runtime_only_speculation_is_visible_with_a_runtime_limitation_warning() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].inference_capabilities = Some(Vec::new());
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_ne!(
            p.capabilities.speculative_decoding.state,
            crate::capabilities::CapabilityState::Unsupported
        );
        assert!(p
            .capabilities
            .speculative_decoding
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("runtime")));
    }

    #[test]
    fn model_mtp_absence_is_the_only_case_that_hides_mtp() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].held[0].model_capabilities.as_mut().unwrap().mtp = Some(false);
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Unsupported
        );
        assert!(p.capabilities.mtp.warning.is_none());
    }

    #[test]
    fn model_mtp_unknown_stays_visible_and_actionable() {
        let mut machines = build(vec![m("only", 900_000, &[(0, 31)])]);
        machines[0].held[0].model_capabilities = None;
        let p = plan("model", &machines, Some(&[pick(0, 31, "only")]));

        assert_eq!(
            p.capabilities.mtp.state,
            crate::capabilities::CapabilityState::Unknown
        );
        assert!(p.capabilities.mtp.warning.is_some());
    }

    #[test]
    fn whole_model_cut_creates_virtual_ranges_for_local_materialization() {
        let mut machines = build(vec![
            m("source", 900_000, &[(0, 31)]),
            m("target", 900_000, &[]),
        ]);
        let options = WholeLayerOptions {
            source_node_id: "source".to_string(),
            total_layers: 32,
            cuts: vec![LayerCut {
                end_layer: 3,
                layers: 4,
                size_bytes: GIB,
                remainder_bytes: GIB,
            }],
            granularity: Some(4),
            rule: "fronteiras reais".to_string(),
            reason: None,
        };
        super::add_virtual_whole_slices(
            &mut machines,
            "model",
            &WholeSliceSelection {
                source_node_id: "source".to_string(),
                target_node_id: "target".to_string(),
                end_layer: 3,
            },
            &options,
        )
        .expect("corte válido");

        assert!(machines[0]
            .holds(0, 3)
            .is_some_and(|shard| shard.virtual_slice));
        assert!(machines[0]
            .holds(4, 31)
            .is_some_and(|shard| shard.virtual_slice));
        let selection = [pick(0, 3, "target"), pick(4, 31, "source")];
        let plan = super::plan(
            "model",
            &machines,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::Whole,
            None,
        );
        assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.steps.iter().all(|step| !step.ready));
        assert!(plan.steps[0].source_needs_split);
        assert!(plan.steps[1].source_needs_split);
    }

    /// The knobs are their own axis; every test below is about the placement,
    /// so they all run on the defaults unless they say otherwise.
    fn plan(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(
            group,
            machines,
            selection,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
        )
    }

    /// The merge axis, flipped on; everything else still defaults.
    fn merged(group: &str, machines: &[Machine], selection: &[SelectedSlice]) -> Plan {
        super::plan(
            group,
            machines,
            Some(selection),
            &RuntimeParams::default(),
            true,
            DeployKind::Shards,
            None,
        )
    }

    /// The whole-model axis: the same planner, told to read one un-sliced GGUF.
    fn whole(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(
            group,
            machines,
            selection,
            &RuntimeParams::default(),
            false,
            DeployKind::Whole,
            None,
        )
    }

    // ── The suggestion ──────────────────────────────────────────────────────

    /// The happy shape: each machine already has one of the slices, so the
    /// deploy is pure launch and copies nothing.
    #[test]
    fn slices_already_in_place_need_no_transfer() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-15 e 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0);
        assert!(p.steps.iter().all(|s| s.ready));
        assert!(p.auto);
    }

    #[test]
    fn complete_strategy_prefers_existing_finer_cuts() {
        let ms = build(vec![
            m("whole", 900_000, &[(0, 31)]),
            m("left", 500_000, &[(0, 15)]),
            m("right", 500_000, &[(16, 31)]),
        ]);
        let p = super::plan_with_options(
            "m",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Complete,
            "chat-prod",
            &[],
        );
        assert_eq!(p.strategy, DeployStrategy::Complete);
        assert_eq!(p.deployment_name, "chat-prod");
        assert_eq!(
            p.steps
                .iter()
                .map(|step| (step.start_layer, step.end_layer))
                .collect::<Vec<_>>(),
            vec![(0, 15), (16, 31)]
        );
    }

    /// An unused eligible peer is preferred for the second copy when it has
    /// capacity, so the automatic plan can spread work without refusing when
    /// a machine already holds every slice.
    #[test]
    fn automatic_planning_can_copy_to_an_idle_peer() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        assert_eq!(p.steps.len(), 2);
        let copied: Vec<&Step> = p.steps.iter().filter(|s| !s.ready).collect();
        assert_eq!(copied.len(), 1, "só uma fatia precisa viajar");
        assert_eq!(copied[0].source_node_id.as_deref(), Some("a"));
        assert_eq!(copied[0].target_node_id, "b");
        assert_eq!(p.transfer_bytes, GIB);
    }

    #[test]
    fn auto_plan_with_one_machine_can_cover_all_slices() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2);
        assert!(p.steps.iter().all(|step| step.target_node_id == "a"));
    }

    /// Layer 0 runs every prompt, so it goes to the machine with the most room
    /// — the one place the assignment is not arbitrary.
    #[test]
    fn layer_zero_goes_to_the_roomiest_machine() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let first = p.steps.iter().find(|s| s.start_layer == 0).unwrap();
        assert_eq!(first.target_node_id, "grande");
        assert!(first.primary, "quem tem a camada 0 recebe o prompt");
        assert_eq!(first.position, 1);
    }

    /// Fewer machines than ranges is valid when the selected coverage is
    /// complete. The planner spreads to an idle peer, then reuses a peer.
    #[test]
    fn more_slices_than_machines_can_share_a_machine() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 9), (10, 19), (20, 31)]),
            m("b", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 3);
    }

    /// A gap in the set is reported as incomplete rather than deployed into a
    /// pipeline that can never plan.
    #[test]
    fn a_gap_in_the_set_is_reported() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(20, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(!p.complete, "faltam as camadas 16-19");
        assert_eq!(p.missing, vec![[16, 19]]);
        assert!(p.warnings.iter().any(|w| w.contains("L16-19")));
    }

    /// Two cuts of the same model live side by side. The suggestion has to pick
    /// one coherent chain instead of stacking 0-4, 0-10, 5-31 and 11-31 on top
    /// of each other.
    #[test]
    fn overlapping_slicings_are_not_stacked_by_the_planner() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 4), (5, 31)]),
            m("b", 100_000, &[(0, 10), (11, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.overlap.is_empty(), "{:?}", p.overlap);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2, "duas fatias, não quatro");
        // Longest first from layer 0: 0-10 beats 0-4.
        assert_eq!((p.steps[0].start_layer, p.steps[0].end_layer), (0, 10));
        assert_eq!((p.steps[1].start_layer, p.steps[1].end_layer), (11, 31));
        assert_eq!(p.available.len(), 4, "as quatro continuam à disposição");
        assert_eq!(p.available.iter().filter(|c| c.selected).count(), 2);
    }

    #[test]
    fn a_set_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.unwrap().contains("nenhuma máquina"));
    }

    // ── The order the steps come back in ────────────────────────────────────
    //
    // `position` says "1-based place in the pipeline", the pipeline order *is*
    // the layer order, and the screen compares the plan it got back against the
    // rows it has on screen - which it keeps in layer order. Every one of those
    // three depends on the same promise, so it is asserted here rather than
    // left to fall out of the grouping loop.

    /// Steps come back in layer order when a machine takes two ranges.
    #[test]
    fn steps_stay_in_layer_order_when_a_machine_takes_two_ranges() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (16, 23)]),
            m("b", 100_000, &[(8, 15)]),
            m("c", 100_000, &[(24, 31)]),
        ]);
        let sel = [
            pick(0, 7, "a"),
            pick(8, 15, "b"),
            pick(16, 23, "a"),
            pick(24, 31, "c"),
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);
        let positions: Vec<u32> = p.steps.iter().map(|s| s.position).collect();
        assert_eq!(positions, vec![1, 2, 3, 4], "posição é o lugar no pipeline");
    }

    /// The auto planner reuses an eligible machine after all peers have been
    /// considered and still answers in layer order.
    #[test]
    fn auto_ranges_can_reuse_a_machine_without_empty_targets() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15), (16, 23), (24, 31)]),
            m("b", 100_000, &[(0, 7), (8, 15), (16, 23), (24, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert!(p.steps.iter().all(|step| !step.target_node_id.is_empty()));
    }

    /// Same thing from the operator's side: two ranges deliberately left
    /// without a machine must not read as one machine holding both.
    #[test]
    fn two_hand_picked_ranges_without_a_machine_keep_their_own_rows() {
        let ms = build(vec![m(
            "a",
            100_000,
            &[(0, 7), (8, 15), (16, 23), (24, 31)],
        )]);
        let sel = [
            pick(0, 7, "a"),
            pick(8, 15, ""),
            pick(16, 23, ""),
            pick(24, 31, ""),
        ];
        let p = plan("m", &ms, Some(&sel));
        let order: Vec<(u32, u32)> = p
            .steps
            .iter()
            .map(|s| (s.start_layer, s.end_layer))
            .collect();
        assert_eq!(order, vec![(0, 7), (8, 15), (16, 23), (24, 31)]);
        let blocked = p.blocked.expect("faixa sem máquina é recusa");
        assert!(blocked.contains("sem máquina"), "{blocked}");
    }

    // ── The operator's own choice ───────────────────────────────────────────

    /// Reassigning by hand is honoured, not silently re-planned: the roomy
    /// machine would have taken layer 0, and the operator says otherwise.
    #[test]
    fn an_explicit_assignment_is_honoured() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "pequena"), pick(16, 31, "grande")]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.auto);
        assert_eq!(p.steps[0].target_node_id, "pequena");
        assert_eq!(p.steps[1].target_node_id, "grande");
    }

    #[test]
    fn complete_mode_can_transfer_a_range_to_a_peer_with_capacity() {
        let ms = build(vec![
            m("holder", 100_000, &[(0, 15), (16, 31)]),
            m("capacity", 900_000, &[]),
        ]);
        let p = super::plan_with_options(
            "m",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Complete,
            "m",
            &[],
        );

        assert!(
            p.blocked.is_none(),
            "capacidade suficiente não deve bloquear: {:?}",
            p.blocked
        );
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[0].target_node_id, "holder");
        assert_eq!(p.steps[1].target_node_id, "capacity");
        assert_eq!(p.transfer_bytes, GIB);
    }

    /// Mixing two slicings is exactly the thing that has to be refused: layers
    /// 5-10 would run twice and the answer would be quietly wrong.
    #[test]
    fn overlapping_selection_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 10), (5, 31)]),
            m("b", 100_000, &[(0, 10), (5, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 10, "a"), pick(5, 31, "b")]));
        assert_eq!(p.overlap, vec![[5, 10]]);
        let blocked = p.blocked.expect("sobreposição tem que barrar");
        assert!(blocked.contains("L5-10"), "{blocked}");
        assert!(!p.complete);
    }

    /// A mesma faixa em máquinas diferentes é uma réplica deliberada da rota,
    /// não uma sobreposição de cortes incompatíveis. O pipeline escolhe uma
    /// das cópias quando chega à camada.
    #[test]
    fn an_exact_range_on_two_machines_is_a_parallel_route() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(0, 15)]),
            m("c", 100_000, &[(16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "a"), pick(0, 15, "b"), pick(16, 31, "c")]),
        );

        assert!(
            p.blocked.is_none(),
            "réplica exata não deve bloquear: {:?}",
            p.blocked
        );
        assert!(p.complete);
        assert_eq!(p.overlap, Vec::<[u32; 2]>::new());
        assert_eq!(p.steps.len(), 3);
        assert_eq!(
            p.steps
                .iter()
                .take(2)
                .map(|step| step.target_node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
        );
        let targets =
            launch_targets(&p.steps, &ms, DeployKind::Shards, None, &[]).expect("targets");
        assert_eq!(
            targets
                .iter()
                .map(|target| (
                    target.node_id.as_str(),
                    target.peer_index,
                    target.peer_count
                ))
                .collect::<Vec<_>>(),
            vec![
                ("a", Some(0), Some(2)),
                ("b", Some(0), Some(2)),
                ("c", Some(1), Some(2))
            ],
        );
    }

    /// A hole in the selected coverage refuses the plan before any launch.
    #[test]
    fn a_selection_with_a_hole_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (20, 31)]),
            m("b", 100_000, &[(0, 15), (20, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(20, 31, "b")]));
        let blocked = p.blocked.expect("um gap não pode ser implantado");
        assert!(blocked.contains("L16-19"), "{blocked}");
        assert!(!p.complete);
        assert_eq!(p.missing, vec![[16, 19]]);
    }

    /// Choosing part of the model leaves a global gap and is refused before
    /// launch, while still reporting the exact missing range.
    #[test]
    fn a_partial_selection_reports_what_is_left_out() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a")]));
        let blocked = p.blocked.expect("a cobertura incompleta é inválida");
        assert!(blocked.contains("L16-31"), "{blocked}");
        assert_eq!(p.missing, vec![[16, 31]]);
        assert_eq!(p.steps.len(), 1);
    }

    #[test]
    fn non_contiguous_ranges_on_one_machine_are_valid() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 3), (32, 35)]),
            m(
                "b",
                100_000,
                &[
                    (4, 7),
                    (8, 11),
                    (12, 15),
                    (16, 19),
                    (20, 23),
                    (24, 27),
                    (28, 31),
                ],
            ),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[
                pick(0, 3, "a"),
                pick(4, 7, "b"),
                pick(8, 11, "b"),
                pick(12, 15, "b"),
                pick(16, 19, "b"),
                pick(20, 23, "b"),
                pick(24, 27, "b"),
                pick(28, 31, "b"),
                pick(32, 35, "a"),
            ]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(
            p.steps
                .iter()
                .map(|step| {
                    (
                        step.start_layer,
                        step.end_layer,
                        step.target_node_id.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (0, 3, "a"),
                (4, 7, "b"),
                (8, 11, "b"),
                (12, 15, "b"),
                (16, 19, "b"),
                (20, 23, "b"),
                (24, 27, "b"),
                (28, 31, "b"),
                (32, 35, "a"),
            ]
        );
    }

    #[test]
    fn all_ranges_on_one_machine_are_valid() {
        let ms = build(vec![m(
            "a",
            100_000,
            &[
                (0, 3),
                (4, 7),
                (8, 11),
                (12, 15),
                (16, 19),
                (20, 23),
                (24, 27),
                (28, 31),
                (32, 35),
            ],
        )]);
        let p = plan(
            "m",
            &ms,
            Some(&[
                pick(0, 3, "a"),
                pick(4, 7, "a"),
                pick(8, 11, "a"),
                pick(12, 15, "a"),
                pick(16, 19, "a"),
                pick(20, 23, "a"),
                pick(24, 27, "a"),
                pick(28, 31, "a"),
                pick(32, 35, "a"),
            ]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 9);
    }

    // ── Merged units ─────────────────────────────────────────────────────────

    /// The whole point of the toggle: adjacent slices on the same machine
    /// become one file, so the machine serves the union. Everything is already
    /// there, so nothing travels and the plan is complete with one step fewer.
    #[test]
    fn merge_joins_adjacent_slices_on_one_machine() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "a"), pick(8, 15, "a"), pick(16, 31, "b")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-7 + 8-15 + 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0, "a já tem as duas fatias");
        assert_eq!(p.steps.len(), 2);

        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert_eq!((joined.start_layer, joined.end_layer), (0, 15));
        assert!(joined.ready);
        assert_eq!(joined.parts.len(), 2, "duas fatias entram no arquivo");
        assert!(joined.parts.iter().all(|part| part.ready));
        // Rótulo de tela, não nome de arquivo: o arquivo junto só ganha nome
        // depois de existir, e o nome dele é o hash.
        assert_eq!(joined.name, "m L0-15", "rótulo do passo de junção");

        let solo = p.steps.iter().find(|s| !s.merged).unwrap();
        assert_eq!((solo.start_layer, solo.end_layer), (16, 31));
        // The merged machine's table row names the union.
        let a = p.machines.iter().find(|m| m.node_id == "a").unwrap();
        assert_eq!(a.taking, Some([0, 15]));
    }

    /// Without the toggle, adjacent ranges remain independent pipeline steps.
    /// The machine is still allowed to serve both; only the file join is off.
    #[test]
    fn without_merge_adjacent_ranges_remain_independent_steps() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 7, "a"), pick(8, 15, "a"), pick(16, 31, "b")]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 3);
        assert!(p.steps.iter().all(|step| !step.merged));
    }

    /// `splitter::merge` only joins neighbours. Separated ranges remain
    /// independent steps even when the merge toggle is on.
    #[test]
    fn merge_only_joins_adjacent_ranges() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (9, 15)]),
            m("b", 100_000, &[(8, 8)]),
            m("c", 100_000, &[(16, 31)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[
                pick(0, 7, "a"),
                pick(8, 8, "b"),
                pick(9, 15, "a"),
                pick(16, 31, "c"),
            ],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 4);
        assert!(p.steps.iter().all(|step| !step.merged));
    }

    /// A merged unit may need copies: each part that is missing has its own
    /// source, and the transfer byte count is the union of the missing ones.
    #[test]
    fn merged_parts_keep_their_sources() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7)]),
            m("b", 100_000, &[(8, 15)]),
            m("d", 100_000, &[(0, 7), (8, 15)]),
            m("e", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "c"), pick(8, 15, "c"), pick(16, 31, "e")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert!(!joined.ready);
        assert_eq!(p.transfer_bytes, 2 * GIB, "as duas fatias viajam");
        let first = &joined.parts[0];
        assert_eq!(first.source_node_id.as_deref(), Some("a"));
        let second = &joined.parts[1];
        assert_eq!(second.source_node_id.as_deref(), Some("b"));
        // A machine holding every part is offered as ready for the merged unit.
        let option = joined
            .options
            .iter()
            .find(|o| o.node_id == "d" && o.has)
            .expect("d tem as duas fatias, logo está pronta para juntar");
        assert!(option.ok);
    }

    /// A merged unit whose target holds no part and no machine holds a part is
    /// a dead end, reported with the offending range named.
    #[test]
    fn a_merged_unit_with_an_unobtainable_part_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 7)])]);
        let p = merged("m", &ms, &[pick(0, 7, "a"), pick(8, 15, "a")]);
        let blocked = p.blocked.expect("L8-15 não existe em máquina nenhuma");
        assert!(blocked.contains("L8-15"), "{blocked}");
    }

    #[test]
    fn a_slice_with_no_machine_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "")]));
        assert!(p.blocked.unwrap().contains("sem máquina"));
    }

    #[test]
    fn a_machine_that_left_the_cluster_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "a"), pick(16, 31, "fantasma")]),
        );
        assert!(p.blocked.unwrap().contains("não está no cluster"));
    }

    /// A range nobody in the cluster has cannot be deployed however the
    /// machines are shuffled.
    #[test]
    fn a_selected_range_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "a")]));
        assert!(p.blocked.unwrap().contains("não existe em máquina nenhuma"));
    }

    /// Disk is a hard fact — `transfer.rs` would refuse the copy anyway, so it
    /// is better refused before anything moves.
    #[test]
    fn a_target_without_disk_room_for_the_copy_is_refused() {
        let mut ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 100_000, &[]),
        ]);
        ms[1].disk_free_mb = 100; // 100 MB for a 1 GiB slice
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        let blocked = p.blocked.expect("não cabe no disco");
        assert!(blocked.contains("disco"), "{blocked}");
    }

    /// Memory is an estimate, so it warns and marks the row instead of
    /// refusing: the reading is honest on Linux and unified on macOS, and a
    /// wrong stop would be worse than a loud line.
    #[test]
    fn a_target_short_on_memory_warns_and_still_deploys() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200, &[(16, 31)]), // 200 MB livres para 1 GiB
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let tight = p.steps.iter().find(|s| s.target_node_id == "b").unwrap();
        assert!(tight.note.as_deref().unwrap().contains("memória"));
        assert!(p.warnings.iter().any(|w| w.contains("memória")));
    }

    /// Every machine is offered for every range, including the one already busy
    /// elsewhere — that is the machine a swap needs.
    #[test]
    fn every_machine_is_offered_as_a_target() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        let first = &p.steps[0];
        assert_eq!(first.options.len(), 3);
        let a = first.options.iter().find(|o| o.node_id == "a").unwrap();
        assert!(a.has && a.ok);
        let c = first.options.iter().find(|o| o.node_id == "c").unwrap();
        assert!(!c.has && c.ok, "c não tem a fatia mas pode receber");
    }

    #[test]
    fn route_options_only_bridge_consecutive_selected_stages() {
        let mut ms = build(vec![
            m("p0", 100_000, &[(0, 1)]),
            m("p1", 100_000, &[(2, 4)]),
            m("p2", 100_000, &[(5, 6)]),
            m("p3", 100_000, &[(7, 10)]),
        ]);
        for machine in &mut ms {
            machine.inference_url = Some(format!("http://{}:8181", machine.node_id));
        }

        let selection = [
            pick(0, 1, "p0"),
            pick(2, 4, "p1"),
            pick(5, 6, "p2"),
            pick(7, 10, "p3"),
        ];
        let p = plan("qwen", &ms, Some(&selection));
        let pairs: Vec<_> = p
            .route_options
            .iter()
            .map(|route| (route.from_node_id.as_str(), route.to_node_id.as_str()))
            .collect();

        assert_eq!(pairs, vec![("p0", "p1"), ("p1", "p2"), ("p2", "p3")]);
    }

    #[test]
    fn route_options_use_the_directional_rtt_of_each_handoff() {
        let mut ms = build(vec![
            m("p0", 100_000, &[(0, 1)]),
            m("p1", 100_000, &[(2, 4)]),
            m("p2", 100_000, &[(5, 6)]),
            m("p3", 100_000, &[(7, 10)]),
        ]);
        for machine in &mut ms {
            machine.inference_url = Some(format!("http://{}:8181", machine.node_id));
        }
        let selection = [
            pick(0, 1, "p0"),
            pick(2, 4, "p1"),
            pick(5, 6, "p2"),
            pick(7, 10, "p3"),
        ];
        let mut rtts = PairwiseRtt::default();
        rtts.values.insert(("p0".into(), "p1".into()), 11.5);
        rtts.values.insert(("p1".into(), "p2".into()), 22.5);
        rtts.values.insert(("p2".into(), "p3".into()), 33.5);

        let p = plan_with_rtt_options(
            "qwen",
            &ms,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::Shards,
            None,
            DeployStrategy::Simple,
            "qwen",
            &[],
            &rtts,
            None,
        );
        let values: Vec<_> = p.route_options.iter().map(|route| route.rtt_ms).collect();
        assert_eq!(values, vec![Some(11.5), Some(22.5), Some(33.5)]);
    }

    /// The machine list carries what the screen needs to explain a refusal.
    #[test]
    fn the_plan_carries_the_machines_and_what_they_take() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert_eq!(p.machines.len(), 2);
        let a = p.machines.iter().find(|v| v.node_id == "a").unwrap();
        assert_eq!(a.holds, vec![[0, 15]]);
        assert_eq!(a.taking, Some([0, 15]));
    }

    // ── Execution settings ──────────────────────────────────────────────────

    /// The screen fills its form from the plan, so the plan has to carry the
    /// settings back — including the two derived numbers that decide whether a
    /// harness's prompt fits.
    #[test]
    fn the_plan_echoes_the_settings_and_the_prompt_budget() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let params = RuntimeParams {
            n_ctx: 32768,
            n_batch: 3072,
            max_sessions: 2,
            max_new_tokens: 256,
            prefill_chunk_tokens: 4096,
            ..RuntimeParams::default()
        };
        let p = super::plan("m", &ms, None, &params, false, DeployKind::Shards, None);

        assert_eq!(p.params.n_ctx, 32768);
        // Cada sessão recebe 32768; 256 ficam reservados para a resposta.
        assert_eq!(p.prompt_tokens, 32512);
        // O pedaço nunca passa de n_batch, por mais alto que seja o pedido.
        assert_eq!(p.chunk_tokens, 3072);
    }

    #[test]
    fn tensor_parallel_plans_disable_llama_fit() {
        let params = RuntimeParams::default();
        assert!(params.fit_memory);
        assert!(!effective_params_for_kind(DeployKind::TensorParallel, &params).fit_memory);
        assert!(effective_params_for_kind(DeployKind::Shards, &params).fit_memory);
    }

    #[test]
    fn inference_mode_round_trips_for_whole_shards_and_tensor_parallel() {
        for kind in [
            DeployKind::Whole,
            DeployKind::Shards,
            DeployKind::TensorParallel,
        ] {
            let request: DeployRequest = serde_json::from_value(serde_json::json!({
                "group": "model",
                "kind": kind,
                "params": { "inference_mode": "moe" }
            }))
            .unwrap();
            assert_eq!(request.params.inference_mode, InferenceMode::Moe);
            assert_eq!(
                effective_params_for_kind(kind, &request.params).inference_mode,
                InferenceMode::Moe
            );
        }
    }

    #[test]
    fn moe_capability_is_reported_for_whole_shards_and_tensor_parallel() {
        let mut params = RuntimeParams::default();
        params.inference_mode = InferenceMode::Moe;

        let mut whole = whole_machine("whole", 900_000, true);
        whole.held[0].is_moe = true;
        let p_whole = super::plan(
            "qwen-moe",
            std::slice::from_ref(&whole),
            None,
            &params,
            false,
            DeployKind::Whole,
            None,
        );
        let p_shards = super::plan(
            "qwen-moe",
            std::slice::from_ref(&whole),
            None,
            &params,
            false,
            DeployKind::Shards,
            None,
        );
        for (kind, p) in [(DeployKind::Whole, p_whole), (DeployKind::Shards, p_shards)] {
            assert!(p.is_moe);
            assert!(
                p.inference_mode_supported,
                "{kind:?}: {:?}",
                p.inference_mode_reason
            );
            assert!(p.inference_mode_reason.is_none());
        }

        let mut primary = tp_machine("primary", 0);
        let mut worker = tp_machine("worker", 1);
        primary.held[0].is_moe = true;
        worker.held[0].is_moe = true;
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "primary".into(),
            worker_node_ids: vec!["worker".into()],
        };
        let p = super::plan(
            "qwen-moe",
            &[primary, worker],
            None,
            &params,
            false,
            DeployKind::TensorParallel,
            Some(tp),
        );
        assert!(p.is_moe);
        assert!(!p.inference_mode_supported);
        assert_eq!(
            p.inference_mode_reason.as_deref(),
            Some("Tensor Parallelism não suporta MoE nesta versão.")
        );
    }

    /// A cluster of unlike machines is the whole point of the per-peer offload:
    /// the number the operator typed for one peer must reach that peer and no
    /// other.
    #[test]
    fn per_machine_gpu_offload_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                n_gpu_layers: Some(0),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].n_gpu_layers, None, "herda o valor do cluster");
        assert_eq!(p.steps[1].n_gpu_layers, Some(0));
    }

    /// Onde o cache KV mora é decidido pela VRAM da máquina, e as máquinas do
    /// cluster não se parecem: a escolha feita para uma não pode aparecer na
    /// outra, nem apagar a que não foi escolhida.
    #[test]
    fn per_machine_kv_cache_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                offload_kqv: Some(false),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].offload_kqv, None, "herda a escolha do cluster");
        assert_eq!(p.steps[1].offload_kqv, Some(false));
        // Um eixo não mexe no outro: nenhuma das duas pediu offload.
        assert_eq!(p.steps[1].n_gpu_layers, None);
    }

    // ── Reading a machine's free memory ─────────────────────────────────────

    #[test]
    fn nvidia_memory_comes_from_vram_and_everything_else_from_ram() {
        let nvidia = json!({
            "gpu_kind": "nvidia",
            "ram": { "available_mb": 3000.0 },
            "gpus": [{ "vram_total_mb": 16000.0, "vram_used_mb": 7600.0 }],
        });
        assert_eq!(free_memory_mb(Some(&nvidia)), 8400);

        let unified = json!({
            "gpu_kind": "unified",
            "ram": { "available_mb": 12000.0 },
            "gpus": [],
        });
        assert_eq!(free_memory_mb(Some(&unified)), 12000);

        // A peer that did not answer says nothing, and nothing is inferred.
        assert_eq!(free_memory_mb(None), 0);
    }

    // ── How many layers the model has ───────────────────────────────────────
    //
    // Reproduced on the captain's own cluster: both machines answer
    // `"total_layers": null` for the `qwen` set (slices cut before
    // `split_model.py` wrote `pom.original_block_count`). That `null`
    // became a `0`, the plan called itself complete, and every peer's apply
    // died on `runtime::NodeConfig::validate` with "total_layers não pode ser
    // zero" — after `launch` had already stopped inference on all of them.

    /// The exact shape of the captain's `qwen` set: no metadata anywhere, but
    /// the two ranges plainly reach layer 32 between them.
    #[test]
    fn slices_without_metadata_take_the_total_from_their_coverage() {
        let ms = build(vec![
            nameless_machine("a", 100_000, &[(0, 15)]),
            nameless_machine("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("qwen", &ms, None);

        assert_eq!(
            p.total_layers,
            Some(32),
            "0-15 e 16-31 alcançam a camada 32"
        );
        assert!(p.total_layers_inferred, "veio da cobertura, não do arquivo");
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert!(
            p.warnings.iter().any(|w| w.contains("deduzido")),
            "a tela tem de dizer que o total foi deduzido: {:?}",
            p.warnings
        );
    }

    /// The plan is what `launch` is handed, so a zero here is a peer that stops
    /// serving for nothing. There is no path that produces one.
    #[test]
    fn a_plan_never_carries_a_total_of_zero() {
        for machines in [
            build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]),
            build(vec![nameless_machine("a", 100_000, &[(0, 31)])]),
            build(vec![m("a", 100_000, &[(0, 15)])]),
        ] {
            let p = plan("m", &machines, None);
            assert_ne!(p.total_layers, Some(0));
            assert!(p.total_layers.is_some() || p.blocked.is_some());
        }
    }

    /// Nothing declares a total and nothing starts at layer 0: the honest
    /// answer is a refusal that names the files, not a guess and not a zero.
    #[test]
    fn a_set_that_cannot_say_its_size_is_refused_by_name() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(16, 31)])]);
        let p = plan("qwen", &ms, None);

        assert_eq!(p.total_layers, None);
        let reason = p.blocked.expect("sem total e sem camada 0 é recusa");
        assert!(
            reason.contains("m_16_31.gguf"),
            "a recusa tem de dizer qual fatia está sem o dado: {reason}"
        );
        assert!(
            reason.contains("Fatiar"),
            "e o que fazer a respeito: {reason}"
        );
    }

    /// Metadata is a fact and coverage is a deduction, so the fact wins — and
    /// the layers the set does not reach are reported as missing instead of
    /// disappearing with the total.
    #[test]
    fn a_declared_total_beats_the_coverage_and_exposes_the_gap() {
        let mut ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        ms[0].held[0].total_layers = Some(40);
        let p = plan("m", &ms, None);

        assert_eq!(p.total_layers, Some(40));
        assert!(!p.total_layers_inferred);
        assert_eq!(p.missing, vec![[16, 39]]);
        assert!(!p.complete);
    }

    /// Half a nameless set is a half deploy, and before the fix it was the
    /// silent one: with the total at zero the tail check was skipped, the plan
    /// called itself complete and the screen armed the button.
    #[test]
    fn half_of_a_nameless_set_is_reported_as_a_gap() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("qwen", &ms, Some(&[pick(0, 15, "a")]));

        assert_eq!(p.total_layers, Some(32));
        assert_eq!(p.missing, vec![[16, 31]], "L16-31 ficou de fora");
        assert!(!p.complete, "meia rede não é uma rede completa");
    }

    /// The deduction walks the reach from layer 0, so a model sliced twice with
    /// different cuts still resolves to one size, and a set that starts later
    /// resolves to none.
    #[test]
    fn the_deduction_walks_the_reach_from_layer_zero() {
        let size = |held: &[(u32, u32)]| {
            let ms = build(vec![nameless_machine("a", 100_000, held)]);
            model_size(&candidates(&ms))
        };
        assert_eq!(size(&[(0, 15), (16, 31)]), ModelSize::Inferred(32));
        // Two different cuts of the same model, side by side.
        assert_eq!(
            size(&[(0, 4), (5, 31), (0, 15), (16, 31)]),
            ModelSize::Inferred(32)
        );
        // A hole stops the reach where the hole is.
        assert_eq!(size(&[(0, 15), (20, 31)]), ModelSize::Inferred(16));
        assert_eq!(size(&[(4, 31)]), ModelSize::Unknown);
        assert_eq!(size(&[]), ModelSize::Unknown);
    }

    /// A peer running an older build answers `"total_layers": 0` where this one
    /// answers `null`. Both mean "this file does not say".
    #[test]
    fn a_shard_that_reports_zero_layers_is_a_shard_with_no_total() {
        let shard = parse_shard(
            &json!({
                "shard_range": [0, 15],
                "total_layers": 0,
                "name": "qwen_0_15.gguf",
                "path": "/models/qwen/qwen_0_15.gguf",
                "size_bytes": 2569314208u64,
            }),
            None,
        )
        .expect("a fatia é válida, só não diz o total");
        assert_eq!(shard.total_layers, None);

        let declared = parse_shard(
            &json!({
                "shard_range": [0, 5],
                "total_layers": 40,
                "name": "phi_0_5.gguf",
                "path": "/models/phi/phi_0_5.gguf",
                "size_bytes": 1u64,
            }),
            None,
        )
        .unwrap();
        assert_eq!(declared.total_layers, Some(40));
    }

    // ── Um modelo inteiro ───────────────────────────────────────────────────
    //
    // O caso degenerado: uma máquina, todas as camadas. O que estes testes
    // guardam não é uma segunda implementação - é que **não** existe uma. Um
    // modelo inteiro entra no mesmo `HeldShard`, sai no mesmo `Step` e é
    // lançado pelo mesmo `launch`; o que muda é de onde o inventário é lido e
    // qual substantivo a recusa usa.

    /// Uma máquina com (ou sem) o GGUF inteiro de 32 camadas.
    fn whole_machine(id: &'static str, mem_free_mb: u64, has: bool) -> Machine {
        Machine {
            node_id: id.to_string(),
            hostname: id.to_string(),
            gateway_url: Some(format!("http://{id}:8080")),
            inference_url: Some(format!("http://{id}:8181")),
            inference_candidates: vec![format!("http://{id}:8181")],
            addr_candidates: Vec::new(),
            libp2p_peer_id: None,
            supports_tunnel: false,
            inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
            mem_free_mb,
            gpu_free_mb: mem_free_mb,
            ram_free_mb: mem_free_mb,
            disk_free_mb: 500_000,
            rtt_ms: Some(0.0),
            load: 0,
            held: if has {
                vec![HeldShard {
                    sha256: None,
                    pom_id: None,
                    virtual_slice: false,
                    tp_rank: None,
                    tp_size: None,
                    is_moe: false,
                    model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                        thinking: Some(true),
                        mtp: Some(false),
                    }),
                    start_layer: 0,
                    end_layer: 31,
                    total_layers: Some(32),
                    name: "qwen3.5-9b".into(),
                    path: format!("/{id}/qwen3.5-9b.gguf"),
                    size_bytes: 5 * GIB,
                }]
            } else {
                Vec::new()
            },
        }
    }

    fn tp_machine(id: &'static str, rank: u32) -> Machine {
        let mut machine = whole_machine(id, 900_000, false);
        machine.held.push(HeldShard {
            start_layer: 0,
            end_layer: 31,
            total_layers: Some(32),
            name: format!("qwen · {}of2", rank + 1),
            path: format!("/{id}/rank-{rank}.gguf"),
            size_bytes: 2 * GIB,
            sha256: Some(format!("{:064x}", rank + 1)),
            pom_id: None,
            virtual_slice: false,
            tp_rank: Some(rank),
            tp_size: Some(2),
            is_moe: false,
            model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                thinking: Some(true),
                mtp: Some(false),
            }),
        });
        machine
    }

    /// O plano de um modelo inteiro é uma máquina cobrindo tudo - e é
    /// `complete`, sem buraco e sem sobreposição, porque a única faixa que
    /// existe é o modelo todo.
    #[test]
    fn a_whole_model_is_one_machine_covering_every_layer() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let p = whole("qwen3.5-9b", &ms, None);

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "uma faixa 0-31 cobre um modelo de 32 camadas");
        assert!(p.missing.is_empty() && p.overlap.is_empty());
        assert_eq!(p.total_layers, Some(32));
        assert!(
            !p.total_layers_inferred,
            "o GGUF inteiro declara o total; nada foi deduzido"
        );
        assert_eq!(p.kind, DeployKind::Whole);

        assert_eq!(p.steps.len(), 1);
        let s = &p.steps[0];
        assert_eq!((s.start_layer, s.end_layer), (0, 31));
        assert_eq!(s.position, 1);
        assert!(s.primary, "quem tem a camada 0 recebe o prompt");
        assert!(s.ready, "o modelo já está na máquina");
        assert!(!s.merged && s.parts.is_empty());
        assert_eq!(s.target_node_id, "a");
        assert_eq!(p.transfer_bytes, 0);
    }

    /// O operador escolhe a máquina, e a escolha é validada como qualquer
    /// outra: a faixa é a do modelo inteiro.
    #[test]
    fn the_operator_picks_the_machine_for_a_whole_model() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "b")]));

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.auto, "é a escolha do operador, não a sugestão do nó");
        assert_eq!(p.steps.len(), 1);
        assert_eq!(p.steps[0].target_node_id, "b");
        assert!(p.steps[0].ready);
    }

    /// Duas cópias do modelo inteiro são duas rotas válidas para a mesma
    /// faixa. O erro de modelo escolhido duas vezes só vale quando o mesmo
    /// destino aparece duas vezes na seleção.
    #[test]
    fn a_whole_model_on_two_machines_is_a_parallel_route() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole(
            "qwen3.5-9b",
            &ms,
            Some(&[pick(0, 31, "a"), pick(0, 31, "b")]),
        );

        assert!(
            p.blocked.is_none(),
            "réplica exata não deve bloquear: {:?}",
            p.blocked
        );
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[0].target_node_id, "a");
        assert_eq!(p.steps[1].target_node_id, "b");
    }

    /// Uma máquina que não tem o arquivo recebe uma cópia, exatamente como uma
    /// fatia - e o plano nomeia quem envia.
    #[test]
    fn a_machine_without_the_model_is_planned_as_a_copy() {
        let ms = vec![
            whole_machine("tem", 900_000, true),
            whole_machine("nao-tem", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "nao-tem")]));

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let s = &p.steps[0];
        assert!(!s.ready);
        assert_eq!(s.source_node_id.as_deref(), Some("tem"));
        assert_eq!(s.source_path.as_deref(), Some("/tem/qwen3.5-9b.gguf"));
        assert_eq!(p.transfer_bytes, 5 * GIB);
        // O aviso existe porque `transfer::safe_model_path` prende o sentido
        // "puxar" à pasta de fatias de quem envia, e um modelo inteiro mora
        // quase sempre fora dela.
        assert!(
            s.note
                .as_deref()
                .unwrap_or_default()
                .contains("Transferências"),
            "a cópia de um modelo inteiro tem que avisar de onde ela pode sair: {:?}",
            s.note
        );
    }

    /// A recusa fala do que está na tela. "a fatia L0-31" nomeia uma coisa que
    /// o operador que escolheu um modelo inteiro não vê em lugar nenhum.
    #[test]
    fn a_whole_model_refusal_names_the_model_and_not_a_slice() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let sem_maquina = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "")]));
        let motivo = sem_maquina.blocked.expect("sem máquina é recusa");
        assert!(
            motivo.contains("o modelo qwen3.5-9b") && !motivo.contains("fatia"),
            "{motivo}"
        );

        let nada = whole("qwen3.5-9b", &ms, Some(&[]));
        let motivo = nada.blocked.expect("nada escolhido é recusa");
        assert_eq!(motivo, "nenhum modelo selecionado");

        let ninguem = whole("nao-existe", &[whole_machine("a", 900_000, false)], None);
        let motivo = ninguem.blocked.expect("ninguém tem o modelo");
        assert!(motivo.contains("modelo inteiro nao-existe") || motivo.contains("nao-existe"));
        assert!(!motivo.contains("fatias de"), "{motivo}");
    }

    /// O menu de destinos diz "já tem o modelo", não "já tem a fatia".
    #[test]
    fn the_target_menu_speaks_of_a_model() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        let opcoes = &p.steps[0].options;
        let a = opcoes.iter().find(|o| o.node_id == "a").unwrap();
        assert!(a.has);
        assert_eq!(a.note.as_deref(), Some("já tem o modelo"));
        let b = opcoes.iter().find(|o| o.node_id == "b").unwrap();
        assert!(!b.has);
        assert!(b.note.as_deref().unwrap().contains("receberia"));
    }

    /// A camada 0 é a que processa o prompt inteiro, e com um modelo inteiro
    /// ela é a máquina escolhida - que a sugestão manda para a mais folgada.
    #[test]
    fn the_suggestion_puts_a_whole_model_on_the_roomiest_machine() {
        let ms = vec![
            whole_machine("pequena", 10_000, true),
            whole_machine("grande", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        assert_eq!(p.steps.len(), 1, "um modelo inteiro é um passo, sempre");
        assert_eq!(p.steps[0].target_node_id, "grande");
    }

    /// Um modelo cujo cabeçalho GGUF não deu o número de camadas não vira uma
    /// implantação com `total_layers` zero: ele simplesmente não entra no
    /// inventário. `NodeConfig::validate` recusaria o zero no destino, depois
    /// de `launch` já ter parado a inferência de lá.
    #[test]
    fn a_model_with_no_layer_count_never_becomes_a_candidate() {
        let sem_camadas = json!({
            "name": "quebrado",
            "path": "/m/quebrado.gguf",
            "size_bytes": 1u64,
            "total_layers": serde_json::Value::Null,
            "shard_range": serde_json::Value::Null,
        });
        assert!(parse_whole(&sem_camadas, "quebrado").is_none());

        let zero = json!({
            "name": "quebrado",
            "path": "/m/quebrado.gguf",
            "size_bytes": 1u64,
            "total_layers": 0,
            "shard_range": serde_json::Value::Null,
        });
        assert!(parse_whole(&zero, "quebrado").is_none());
    }

    /// Uma fatia não é um modelo inteiro, mesmo com o nome pedido - as duas
    /// listas saem da mesma varredura e `shard_range` é o que as separa.
    #[test]
    fn a_slice_is_never_read_as_a_whole_model() {
        let fatia = json!({
            "name": "qwen",
            "path": "/m/qwen_0_15.gguf",
            "size_bytes": 1u64,
            "total_layers": 32,
            "shard_range": [0, 15],
        });
        assert!(parse_whole(&fatia, "qwen").is_none());
    }

    /// A segunda metade da implantação: o que a máquina é mandada carregar.
    ///
    /// É aqui que "modelo inteiro" tem que sair como um alvo comum - faixa
    /// cheia, `skip_split`, sem `merge` e sem posição forçada -, porque é
    /// exatamente essa forma que faz o `apply` no destino se reconhecer como
    /// `single_peer` e subir sem extração de estado oculto. Um `skip_split`
    /// falso aqui mandaria a máquina fatiar um arquivo que já é o modelo todo.
    #[test]
    fn a_whole_model_is_launched_as_one_plain_target() {
        let ms = vec![whole_machine("a", 900_000, true)];
        let p = whole("qwen3.5-9b", &ms, None);
        let alvos = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[]).expect("um alvo");

        assert_eq!(alvos.len(), 1);
        let alvo = &alvos[0];
        assert_eq!(alvo.node_id, "a");
        // O caminho é o da máquina de destino, lido da varredura - nunca um
        // palpite sobre o sistema de arquivos alheio.
        assert_eq!(alvo.source_model_path, "/a/qwen3.5-9b.gguf");
        assert_eq!((alvo.start_layer, alvo.end_layer), (0, 31));
        assert!(alvo.skip_split, "o arquivo já é o modelo todo");
        assert!(alvo.merge.is_none());
        assert!(
            alvo.peer_index.is_none() && alvo.peer_count.is_none(),
            "a implantação inteira descreve o pipeline, então a posição sai da lista"
        );
    }

    /// Um grupo de tensor parallelism vira um alvo por rank: o primary (rank 0)
    /// e cada worker, todos apontando o mesmo rank0_url para a coletiva.
    #[test]
    fn a_tensor_parallel_group_launches_one_target_per_rank() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let p = whole("qwen3.5-9b", &ms, None);
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let alvos = launch_targets(&p.steps, &ms, DeployKind::TensorParallel, Some(&tp), &[])
            .expect("dois alvos");

        assert_eq!(alvos.len(), 2);

        let primary = &alvos[0];
        assert_eq!(primary.node_id, "a");
        assert_eq!(primary.tp.as_ref().unwrap().role, "primary");
        assert_eq!(primary.tp.as_ref().unwrap().rank, 0);
        // A URL da coletiva é derivada da porta de inferência anunciada pelo
        // primary - o operador só escolhe a máquina, não digita a URL.
        assert_eq!(
            primary.tp.as_ref().unwrap().rank0_url.as_deref(),
            Some("http://a:8181")
        );

        let worker = &alvos[1];
        assert_eq!(worker.node_id, "b");
        assert_eq!(worker.tp.as_ref().unwrap().role, "worker");
        assert_eq!(worker.tp.as_ref().unwrap().rank, 1);
        assert_eq!(
            worker.tp.as_ref().unwrap().rank0_url.as_deref(),
            Some("http://a:8181")
        );
    }

    /// A TP worker can also appear in the explicit selection sent by the UI.
    /// It is a rank in the collective, not a second pipeline stage, so the
    /// launch must still contain exactly one primary and one worker target.
    #[test]
    fn a_tensor_parallel_worker_is_not_launched_as_a_pipeline_stage() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let selection = [pick(0, 31, "a"), pick(0, 31, "b")];
        let p = super::plan(
            "qwen3.5-9b",
            &ms,
            Some(&selection),
            &RuntimeParams::default(),
            false,
            DeployKind::TensorParallel,
            Some(tp.clone()),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        let targets = launch_targets(&p.steps, &ms, DeployKind::TensorParallel, Some(&tp), &[])
            .expect("um alvo por rank");
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(targets[0].tp.as_ref().unwrap().role, "primary");
        assert_eq!(targets[1].tp.as_ref().unwrap().role, "worker");
    }

    #[test]
    fn pre_sliced_tensor_parallel_uses_one_existing_file_per_rank() {
        let ms = vec![tp_machine("a", 0), tp_machine("b", 1)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let p = super::plan(
            "qwen",
            &ms,
            None,
            &RuntimeParams::default(),
            false,
            DeployKind::TensorParallel,
            Some(tp.clone()),
        );

        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.tp_targets.len(), 2);
        assert!(p.tp_targets.iter().all(|target| target.ready));
        assert_eq!(p.tp_targets[0].name, "qwen · 1of2");

        let targets = launch_targets(
            &p.steps,
            &ms,
            DeployKind::TensorParallel,
            Some(&tp),
            &p.tp_targets,
        )
        .expect("um alvo por rank pré-fatiado");
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|target| target.skip_split));
        assert_eq!(
            targets[0].part_sha256.as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000001")
        );
        assert_eq!(
            targets[1].part_sha256.as_deref(),
            Some("0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(targets[0].source_model_path, "/a/rank-0.gguf");
        assert_eq!(targets[1].source_model_path, "/b/rank-1.gguf");
    }

    #[test]
    fn selected_tp_metadata_gate_names_missing_ranks() {
        let machines = vec![tp_machine("a", 0)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        let reason = selected_metadata_pending_reason_for(
            DeployKind::TensorParallel,
            "qwen",
            &[(0, 31)],
            Some(&tp),
            &machines,
        )
        .expect("rank ausente deve manter a seleção bloqueada");
        assert!(reason.contains("ranks 2"), "{reason}");
    }

    #[test]
    fn selected_tp_metadata_gate_opens_when_all_ranks_are_ready() {
        let machines = vec![tp_machine("a", 0), tp_machine("b", 1)];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(selected_metadata_pending_reason_for(
            DeployKind::TensorParallel,
            "qwen",
            &[(0, 31)],
            Some(&tp),
            &machines,
        )
        .is_none());
    }

    /// As validações estáticas do grupo TP recusam o plano antes de qualquer
    /// checagem remota: tamanho, presença no cluster, modelo inteiro e contagem
    /// de workers.
    #[test]
    fn tp_group_checks_rejects_bad_groups() {
        let ms = vec![
            whole_machine("a", 900_000, true),
            whole_machine("b", 900_000, true),
        ];

        let ok = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&ok, &ms, "qwen3.5-9b").is_none());

        let small = TpLaunch {
            size: 1,
            primary_node_id: "a".into(),
            worker_node_ids: vec![],
        };
        assert!(tp_group_checks(&small, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("2 ranks"));

        let ghost = TpLaunch {
            size: 2,
            primary_node_id: "zzz".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&ghost, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("não está no cluster"));

        let count = TpLaunch {
            size: 3,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        assert!(tp_group_checks(&count, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("precisa de 2 workers"));

        let self_worker = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["a".into()],
        };
        assert!(tp_group_checks(&self_worker, &ms, "qwen3.5-9b")
            .unwrap()
            .contains("próprio primary"));
    }

    /// A recusa do peer é repetida ao operador, em vez de virar palpite sobre a
    /// versão do binário dele. O caso que motivou: `/api/ui/tp/check` é
    /// autenticada, a chamada ia sem a chave, e o `{"error":...}` que voltava
    /// era anunciado como "binário antigo" - com as duas máquinas no mesmo
    /// commit.
    #[test]
    fn a_refusal_is_quoted_not_guessed_as_an_old_binary() {
        assert_eq!(tp_check_verdict(&json!({ "ok": true }), "ksia"), None);

        assert_eq!(
            tp_check_verdict(
                &json!({ "ok": false, "reason": "cabeças não dividem" }),
                "ksia"
            ),
            Some("cabeças não dividem".to_string())
        );

        let recusa =
            tp_check_verdict(&json!({ "error": "missing Authorization header" }), "ksia").unwrap();
        assert!(
            recusa.contains("missing Authorization header"),
            "a razão do peer tem que aparecer: {recusa}"
        );
        assert!(
            !recusa.contains("binário antigo"),
            "peer que recusou não é peer desatualizado: {recusa}"
        );

        // Só um corpo que não explica nada continua valendo como binário velho.
        assert!(tp_check_verdict(&json!({ "outra": 1 }), "ksia")
            .unwrap()
            .contains("binário antigo"));
    }

    /// A checagem tem que chegar autenticada no primary. Sem isto ela é 401 em
    /// qualquer máquina, e nenhuma implantação TP passa.
    #[tokio::test]
    async fn the_shardability_check_carries_the_operator_key() {
        use crate::gateway::testkit::state_with;
        use axum::routing::post as axum_post;
        use std::sync::{Arc, Mutex};

        let visto: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let capturado = visto.clone();
        let app = Router::new().route(
            "/api/ui/tp/check",
            axum_post(move |headers: axum::http::HeaderMap| {
                let capturado = capturado.clone();
                async move {
                    *capturado.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    Json(json!({ "ok": true }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let state = state_with("eu", vec![]).await;
        let machines = vec![Machine {
            node_id: "primary".into(),
            hostname: "ksia".into(),
            gateway_url: Some(format!("http://127.0.0.1:{port}")),
            inference_url: Some(format!("http://127.0.0.1:{port}")),
            inference_candidates: vec![format!("http://127.0.0.1:{port}")],
            addr_candidates: Vec::new(),
            libp2p_peer_id: None,
            supports_tunnel: false,
            inference_capabilities: Some(vec!["speculate/v1-lookup".to_string()]),
            mem_free_mb: 900_000,
            gpu_free_mb: 900_000,
            ram_free_mb: 900_000,
            disk_free_mb: 900_000,
            rtt_ms: Some(0.0),
            load: 0,
            held: vec![HeldShard {
                sha256: None,
                pom_id: None,
                virtual_slice: false,
                tp_rank: None,
                tp_size: None,
                is_moe: false,
                model_capabilities: Some(crate::gguf::ModelCapabilityEvidence {
                    thinking: Some(true),
                    mtp: Some(false),
                }),
                start_layer: 0,
                end_layer: 31,
                total_layers: Some(32),
                name: "qwen3.5-9b.gguf".into(),
                path: "/modelos/qwen3.5-9b.gguf".into(),
                size_bytes: 1,
            }],
        }];
        let tp = TpLaunch {
            size: 2,
            primary_node_id: "primary".into(),
            worker_node_ids: vec!["w".into()],
        };

        assert_eq!(tp_shardability_check(&state, &machines, &tp).await, None);
        let header = visto.lock().unwrap().clone();
        assert_eq!(
            header,
            Some("Bearer chave".to_string()),
            "a checagem chegou sem a chave de operador: {header:?}"
        );
    }

    /// Os membros saem em ordem de rank, e quem não anuncia porta de inferência
    /// é recusado **aqui**.
    ///
    /// O rank 0 dispara o decode de cada membro por essa URL; um membro sem ela
    /// travaria o grupo na primeira barreira, que é o pior lugar para descobrir
    /// o problema.
    #[test]
    fn tp_members_are_rank_ordered_and_a_memberless_url_is_refused() {
        let mut a = whole_machine("a", 900_000, true);
        a.inference_url = Some("https://a:8181".into());
        let mut b = whole_machine("b", 900_000, true);
        b.inference_url = Some("https://b:8181".into());
        let mut mudo = whole_machine("c", 900_000, true);
        mudo.inference_url = None;

        let tp = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["b".into()],
        };
        // A ordem no cluster não é a ordem dos ranks: o teste embaralha de
        // propósito, porque quem manda é `worker_node_ids`.
        let membros = tp_members(&tp, &[b, a]).unwrap();
        assert_eq!(membros.len(), 2);
        assert_eq!(membros[0].node_id, "a", "rank 0 é o primary");
        assert_eq!(membros[1].url, "https://b:8181");

        let com_mudo = TpLaunch {
            size: 2,
            primary_node_id: "a".into(),
            worker_node_ids: vec!["c".into()],
        };
        let mut a2 = whole_machine("a", 900_000, true);
        a2.inference_url = Some("https://a:8181".into());
        let erro = tp_members(&com_mudo, &[a2, mudo]).unwrap_err();
        assert!(erro.contains("URL de inferência"), "{erro}");
    }

    /// Uma máquina que não achou o arquivo depois da cópia é erro nomeando o
    /// modelo, não uma implantação que sobe apontando para lugar nenhum.
    #[test]
    fn a_missing_file_after_the_copy_is_an_error_that_names_the_model() {
        let ms = vec![
            whole_machine("tem", 900_000, true),
            whole_machine("nao-tem", 900_000, false),
        ];
        let p = whole("qwen3.5-9b", &ms, Some(&[pick(0, 31, "nao-tem")]));
        let erro = launch_targets(&p.steps, &ms, DeployKind::Whole, None, &[])
            .expect_err("o destino não tem o arquivo");
        assert!(
            erro.contains("nao-tem") && erro.contains("o modelo"),
            "{erro}"
        );
    }

    /// O caminho das fatias sai igualzinho pela mesma função - a extração não
    /// mudou o que o `launch` recebe.
    #[test]
    fn the_slice_path_still_launches_the_way_it_did() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let alvos =
            launch_targets(&p.steps, &ms, DeployKind::Shards, None, &[]).expect("dois alvos");
        assert_eq!(alvos.len(), 2);
        assert_eq!(alvos[0].source_model_path, "/models/m_0_15.gguf");
        assert_eq!((alvos[0].start_layer, alvos[0].end_layer), (0, 15));
        assert_eq!((alvos[1].start_layer, alvos[1].end_layer), (16, 31));
        assert!(alvos.iter().all(|a| a.skip_split && a.merge.is_none()));
    }

    /// O modelo inteiro vira a faixa 0..total-1, que é o que faz todo o resto
    /// do módulo funcionar sem saber que ele é inteiro.
    #[test]
    fn a_whole_model_reads_as_the_range_it_covers() {
        let inteiro = parse_whole(
            &json!({
                "name": "qwen3.5-9b",
                "path": "/m/qwen3.5-9b.gguf",
                "size_bytes": 5_368_709_120u64,
                "total_layers": 32,
                "shard_range": serde_json::Value::Null,
            }),
            "qwen3.5-9b",
        )
        .expect("um GGUF inteiro com 32 camadas");
        assert_eq!((inteiro.start_layer, inteiro.end_layer), (0, 31));
        assert_eq!(inteiro.total_layers, Some(32));
        assert_eq!(inteiro.name, "qwen3.5-9b");

        // Outro modelo com outro nome não entra no lugar deste.
        assert!(parse_whole(
            &json!({
                "name": "llama3",
                "path": "/m/llama3.gguf",
                "size_bytes": 1u64,
                "total_layers": 32,
                "shard_range": serde_json::Value::Null,
            }),
            "qwen3.5-9b"
        )
        .is_none());
    }
}
