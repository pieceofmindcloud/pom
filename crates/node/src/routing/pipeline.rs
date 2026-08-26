/// Pipeline planner for distributed layer-pipeline inference.
///
/// Given a set of peers (each covering a contiguous layer range), this module
/// selects the optimal ordered sequence of peers that covers layers 0..total_layers,
/// tracks per-peer request load, and exposes the plan to the gateway.
use crate::network::PeerInfo;
use crate::wire::{FrameEncoding, WireDtype};
use routing_contract::{ClusterSnapshot, PeerCapacity, RoutingRequest, ROUTING_API_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tracing::{debug, info};

// ── Load tracking ─────────────────────────────────────────────────────────────

/// Tracks in-flight inference requests per peer node.
/// RAII guards via `acquire()` ensure counts are always decremented.
#[derive(Clone, Default)]
pub struct LoadTracker(Arc<RwLock<HashMap<String, Arc<AtomicU32>>>>);

impl LoadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current in-flight count for a node (0 if unseen).
    pub async fn load(&self, node_id: &str) -> u32 {
        self.0
            .read()
            .await
            .get(node_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Increment the counter and return a guard that decrements on drop.
    pub async fn acquire(&self, node_id: &str) -> LoadGuard {
        let counter = {
            let mut map = self.0.write().await;
            map.entry(node_id.to_string())
                .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                .clone()
        };
        counter.fetch_add(1, Ordering::Relaxed);
        LoadGuard { counter }
    }
}

/// Decrements the peer load counter when dropped (RAII).
pub struct LoadGuard {
    counter: Arc<AtomicU32>,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

// ── Pipeline types ────────────────────────────────────────────────────────────

/// A single peer step within the inference pipeline.
///
/// Serializable because it travels on the wire as `next_hops` when relay
/// mode is active (`CAP_RELAY_CHAIN`): a relaying hop pops itself off the
/// front of the list it received and forwards the rest, unchanged, to the
/// next hop's URL — see `inference/relay.rs` and `inference/infer_server.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub node_id: String,
    /// Full HTTP base URL, e.g. "http://192.168.1.5:8181"
    pub inference_url: String,
    /// Todos os enderecos por onde este hop pode ser alcancado.
    ///
    /// `inference_url` continua preenchido e e o que um no antigo le. Quem
    /// disca prefere esta lista, porque a escolha tem que ser de quem mediu: o
    /// plano pode ter sido montado por uma maquina noutra rede.
    #[serde(default)]
    pub inference_candidates: Vec<String>,
    pub start_layer: u32,
    pub end_layer: u32,
    /// A sub-faixa que **este passo executa**, que nem sempre é a faixa que ele
    /// carregou.
    ///
    /// `start_layer`/`end_layer` dizem o que o peer tem em disco; estes dizem o
    /// que o remetente está mandando ele rodar agora. Os dois separados porque
    /// um peer com L0-59 pode ser entrada de um ramo (executa L0-59) e cauda de
    /// outro (executaria L40-59) - a faixa carregada é a mesma, o trabalho não.
    ///
    /// Quem manda decide, e manda sempre: sem isto o destinatário rodava a
    /// fatia inteira que carregou, que é como uma sobreposição virava camada
    /// executada duas vezes sem erro nenhum.
    pub exec_start: u32,
    pub exec_end: u32,
    /// Capabilities of the canonical inference frame and relay.
    pub inference_frame_capabilities: Vec<String>,
    /// This hop's libp2p `PeerId` (base58), when known — lets a caller reach
    /// it over the swarm as a NAT-traversal fallback when direct HTTP fails.
    #[serde(default)]
    pub libp2p_peer_id: Option<String>,
    /// Endereços UDP anunciados pelo hop para reconstruir o túnel libp2p.
    /// O plano leva estes endereços porque o processo que executa o hop pode
    /// não ter recebido o mesmo anúncio na própria tabela de descoberta.
    #[serde(default)]
    pub libp2p_addr_candidates: Vec<String>,
}

/// Advertised when a node can continue a relay itself — see the `next_hops`
/// field threaded through `inference/wire.rs` meta — instead of requiring
/// the gateway to call every hop directly. All-or-nothing per plan: if any
/// step lacks it, the whole request falls back to the hub relay unchanged.
pub const CAP_RELAY_CHAIN: &str = "relay/v1-chain";

/// Advertised when a node, relaying a chain, answers the *originator* directly
/// (the `reply_to` in the frame meta) instead of holding its own HTTP response
/// open until the result bubbles back up the call stack — see
/// [`crate::direct`]. Strictly stronger than [`CAP_RELAY_CHAIN`]: a node that
/// advertises this one also advertises that. Same all-or-nothing rule; a plan
/// with one older peer falls back to the bubbling ring unchanged.
pub const CAP_RELAY_DIRECT: &str = "relay/v2-direct";

/// Advertised when a node can draft and verify a batch of candidate tokens
/// in one decode (prompt-lookup speculative decoding) instead of exactly
/// one token per call. Same all-or-nothing-per-plan fallback rule.
pub const CAP_SPECULATE_LOOKUP: &str = "speculate/v1-lookup";

/// Anunciada quando o motor daquele nó conseguiu anexar a cabeça de rascunho
/// `nextn` do próprio modelo - predição multi-token (MTP). Distinta de
/// [`CAP_SPECULATE_LOOKUP`], que é a busca por repetição em
/// `infer_server::find_draft` e não usa peso nenhum: ver `crate::mtp`.
///
/// Não é all-or-nothing como as outras. O bloco `nextn` mora na última camada
/// do modelo, então num pipeline fatiado só a **cauda** o tem, e quem monta o
/// lote é a **cabeça**. Por isso a condição é "a cabeça consegue rascunhar",
/// que hoje é o mesmo que dizer que uma máquina serve o modelo inteiro; os
/// demais hops só precisam saber decodificar um lote (`CAP_SPECULATE_LOOKUP`),
/// que é exatamente o que `supports_mtp` exige deles.
pub const CAP_SPECULATE_MTP: &str = "speculate/v1-mtp";

/// Anunciada por todo binário novo, incondicionalmente, para marcar que ele
/// fala o protocolo do MTP fatiado: o `speculate` com `draft` pré-fabricado, o
/// campo `tokens` (que dá à cauda a composição do lote para o `observe`) e o
/// campo `mtp_k` (a largura de lote pedida pelo gateway).
///
/// É **all-or-nothing** de novo, e o motivo é fechamento de buraco: um peer
/// velho só tem `CAP_SPECULATE_MTP`/`CAP_SPECULATE_LOOKUP` e, sem esta cap,
/// `supports_mtp` deixaria um anel misto passar — a cabeça velha receberia um
/// `speculate {source: Mtp, draft}` que ela não entende, e a cauda nova
/// produziria um `next_draft` que ela ignora. Exigir a cap de **todos** os
/// passos exclui o cenário de uma vez, em vez de conviver com ele.
pub const CAP_SPECULATE_MTP_SLICED: &str = "speculate/v1-mtp-sliced";

/// Advertised when a node's libp2p swarm has actually come up
/// (`Network::libp2p_tunnel_ready`) — unlike the other capabilities above,
/// this is checked **per-hop**, not all-or-nothing across a plan: a mix of
/// directly-reachable and NAT'd hops in the same pipeline is normal. The
/// relay prefers this transport per hop and falls back to the direct client
/// when it is unavailable or fails at the transport layer.
pub const CAP_LIBP2P_TUNNEL: &str = "transport/libp2p-tunnel-v1";

/// Advertised only while the local inference server has the validated
/// multi-item scheduler active. It is the first and only inference frame
/// contract; peers are never downgraded to an older wire format.
pub const CAP_FRAME_BATCH: &str = "inference/frame-v1-batch";

/// Advertised when a node can decode and encode the optional FP8 E4M3FN
/// representation of hidden state. This is all-or-nothing across a plan:
/// sending f8 through one peer that only knows f16 would corrupt the answer.
pub const CAP_FRAME_DTYPE_F8: &str = "inference/frame-v1-dtype-f8";

/// A handoff chosen by the planner between two consecutive pipeline steps.
///
/// The layer boundaries let observers attach the edge to the exact advertised
/// ranges, even when a node advertises overlapping model slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub from_end_layer: u32,
    pub to_start_layer: u32,
}

impl PipelineStep {
    pub fn frame_encoding(&self) -> FrameEncoding {
        FrameEncoding::F16
    }

    /// Whether this step's advertised `inference_frame_capabilities` list
    /// contains a given capability string.
    pub fn supports(&self, capability: &str) -> bool {
        self.inference_frame_capabilities
            .iter()
            .any(|cap| cap == capability)
    }
}

/// Ordered pipeline plan for a single inference request.
#[derive(Debug, Clone)]
pub struct PipelinePlan {
    /// Steps in order from first layer to last.
    pub steps: Vec<PipelineStep>,
    /// Handoffs selected by the same planner invocation as `steps`.
    pub edges: Vec<PipelineEdge>,
}

/// A rota escolhida para uma sessão. A lista é a afinidade por estágio, não
/// uma lista de peers alternativos: depois que a sessão entra no KV de uma
/// réplica, as rodadas seguintes precisam voltar para os mesmos node IDs.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteAffinity {
    pub deployment_id: String,
    pub session_id: String,
    pub hop_replicas: Vec<String>,
}

#[allow(dead_code)]
impl RouteAffinity {
    pub fn from_plan(
        deployment_id: impl Into<String>,
        session_id: impl Into<String>,
        plan: &PipelinePlan,
    ) -> Self {
        Self {
            deployment_id: deployment_id.into(),
            session_id: session_id.into(),
            hop_replicas: plan.steps.iter().map(|step| step.node_id.clone()).collect(),
        }
    }

    pub fn matches_plan(&self, plan: &PipelinePlan) -> bool {
        self.hop_replicas.len() == plan.steps.len()
            && self
                .hop_replicas
                .iter()
                .zip(&plan.steps)
                .all(|(node_id, step)| node_id == &step.node_id)
    }
}

/// Translates transport-owned peer records into the stable routing-provider
/// input. Production routing remains on the existing planner until equivalence
/// coverage permits switching implementations - por isso ainda não tem chamador
/// fora dos testes, e o `allow` é a marca disso, não descuido.
#[allow(dead_code)]
pub(crate) fn routing_request_from_peers(total_layers: u32, peers: &[PeerInfo]) -> RoutingRequest {
    RoutingRequest {
        api_version: ROUTING_API_VERSION,
        cluster: ClusterSnapshot {
            total_layers,
            peers: peers
                .iter()
                .map(|peer| PeerCapacity {
                    peer_id: peer.node_id.clone(),
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    supports_inference: peer.inference_port.is_some(),
                })
                .collect(),
        },
    }
}

impl PipelinePlan {
    /// Validate the wire precision before the first request is sent.
    ///
    /// f16 is the original contract and remains valid for peers that do not
    /// advertise the optional f8 extension. A selected f8 plan must have the
    /// explicit capability on every hop, otherwise it fails visibly instead
    /// of allowing a mixed-version peer to interpret bytes incorrectly.
    pub fn validate_wire_dtype(&self, dtype: WireDtype) -> anyhow::Result<()> {
        if dtype != WireDtype::F8 {
            return Ok(());
        }
        for step in &self.steps {
            if !step.supports(CAP_FRAME_DTYPE_F8) {
                return Err(anyhow::anyhow!(
                    "o formato f8 do estado oculto não foi negociado: o peer {} não anuncia {}",
                    step.node_id,
                    CAP_FRAME_DTYPE_F8
                ));
            }
        }
        Ok(())
    }

    /// Human-readable hop chain: "abc123→def456→…"
    pub fn hop_chain(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.node_id[..s.node_id.len().min(8)].to_string())
            .collect::<Vec<_>>()
            .join("→")
    }

    /// True only when every step in the plan advertises `CAP_RELAY_CHAIN` —
    /// a plan with one old peer in the middle falls back to the hub relay
    /// entirely rather than mixing relay and hub calls mid-chain.
    pub fn supports_relay_chain(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.supports(CAP_RELAY_CHAIN))
    }

    /// True when every step can answer the originator directly and the chain
    /// is long enough for that to save anything.
    ///
    /// A single-hop plan is excluded deliberately: gateway→hop→gateway is
    /// already the minimum, and routing its answer out of band would only add
    /// a callback to the same two legs.
    pub fn supports_direct_reply(&self) -> bool {
        self.steps.len() >= 2 && self.steps.iter().all(|s| s.supports(CAP_RELAY_DIRECT))
    }

    /// Same all-or-nothing rule as `supports_relay_chain`, for speculative
    /// decoding: every hop must be able to decode a >1-token batch and, on
    /// the tail, verify it.
    pub fn supports_speculation(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.supports(CAP_SPECULATE_LOOKUP))
    }

    /// Este plano pode rascunhar pelo bloco `nextn` do modelo?
    ///
    /// O bloco mora na última camada, portanto na **cauda** do pipeline - é
    /// ela quem anuncia `CAP_SPECULATE_MTP`. A cabeça (que monta o lote)
    /// recebe o rascunho pronto do gateway, que por sua vez o colheu da
    /// resposta direta da cauda na rodada anterior (`docs/subsystems/
    /// inferencia.md` §5.2). **Todos** precisam de `CAP_SPECULATE_LOOKUP`
    /// porque o lote maior que um token passa por eles, e **todos** precisam
    /// de `CAP_SPECULATE_MTP_SLICED` porque o protocolo novo (rascunho
    /// pré-fabricado + `tokens` + `mtp_k` na wire) só existe em binários
    /// novos — um peer velho no anel receberia um `speculate` que não entende.
    pub fn supports_mtp(&self) -> bool {
        !self.steps.is_empty()
            && self.steps.iter().all(|s| s.supports(CAP_SPECULATE_LOOKUP))
            && self
                .steps
                .iter()
                .all(|s| s.supports(CAP_SPECULATE_MTP_SLICED))
            && self
                .steps
                .last()
                .is_some_and(|s| s.supports(CAP_SPECULATE_MTP))
    }
}

// ── Planner ───────────────────────────────────────────────────────────────────

/// Select the optimal inference pipeline for a given model.
///
/// Scoring: `layers_count * 100 - current_load * 10`
/// — prefers peers that cover more layers and are less busy.
///
/// `deploy_id` escolhe **qual** implantação atende, quando o mesmo modelo está
/// no ar mais de uma vez. `None` quer dizer "a que estiver lá": casa pelo
/// rótulo, que é o comportamento de sempre e o único possível com um peer de
/// versão anterior, que não anuncia implantação nenhuma.
///
/// Returns `None` if there is any gap in layer coverage (0..total_layers).
pub async fn plan_pipeline(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    tracker: &LoadTracker,
) -> Option<PipelinePlan> {
    plan_pipeline_with_rtt(
        peers,
        model_name,
        deploy_id,
        total_layers,
        tracker,
        &HashMap::new(),
    )
    .await
}

/// Same planner, with RTTs measured by the gateway that is about to originate
/// the request. Exact range replicas are the same logical stage, so the lowest
/// measured RTT wins before load is considered. A caller without measurements
/// can keep using `plan_pipeline` and retains the stable legacy ordering.
pub async fn plan_pipeline_with_rtt(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    tracker: &LoadTracker,
    rtts: &HashMap<String, f64>,
) -> Option<PipelinePlan> {
    // Only peers that expose an inference server AND match the model name
    let servindo: Vec<&PeerInfo> = peers
        .iter()
        .filter(|p| {
            p.inference_port.is_some()
                && p.is_alive()
                && !p.is_tp_worker()
                && match deploy_id {
                    Some(id) => p.deploy_id == id,
                    None => p.model == model_name,
                }
        })
        .collect();

    if servindo.is_empty() {
        debug!(
            "Pipeline plan failed: No alive peers for model '{}'{} with inference_port",
            model_name,
            deploy_id
                .map(|d| format!(" (implantação {d})"))
                .unwrap_or_default()
        );
        return None;
    }

    // Um pipeline não pode misturar implantações. Sem isto, duas implantações
    // do mesmo modelo - que é exatamente o que o `deploy_id` passou a permitir -
    // virariam um conjunto só de peers, e o passo a passo por camada escolheria
    // ora de uma ora de outra: metade do prompt numa configuração, metade na
    // outra, com o operador vendo um pipeline que não implantou.
    //
    // Quando quem chamou não escolheu, escolhe-se aqui, e de forma determinística:
    // duas máquinas do cluster têm que chegar à mesma implantação. A ordem por
    // id é o critério estável até o registro dizer qual é a ativa
    // (`deployment::active`).
    let candidates: Vec<&PeerInfo> = match deploy_id {
        Some(_) => servindo,
        None => {
            let mut ids: Vec<&str> = servindo
                .iter()
                .map(|p| p.deploy_id.as_str())
                .filter(|d| !d.is_empty())
                .collect();
            ids.sort_unstable();
            ids.dedup();
            match ids.first().copied() {
                // Nenhum peer anuncia implantação: cluster de versão anterior,
                // ou nós subidos pela linha de comando. Casa por rótulo.
                None => servindo,
                Some(escolhida) => {
                    if ids.len() > 1 {
                        debug!(
                            "modelo '{model_name}' está no ar em {} implantações; usando {escolhida}",
                            ids.len()
                        );
                    }
                    servindo
                        .into_iter()
                        // Um nó sem implantação declarada continua servindo o
                        // rótulo: ele entra junto em vez de abrir buraco.
                        .filter(|p| p.deploy_id == escolhida || p.deploy_id.is_empty())
                        .collect()
                }
            }
        }
    };

    let mut steps: Vec<PipelineStep> = Vec::new();
    let mut pos: u32 = 0; // next layer we need covered

    while pos < total_layers {
        // Only a peer whose loaded range starts at `pos` can be the next hop.
        // The inference engine enters at the first block of its loaded slice;
        // choosing a peer that merely covers `pos` would emit an execution
        // subrange beginning in the middle of that slice and fail after the
        // request had already crossed the network.
        let reachable: Vec<&PeerInfo> = candidates
            .iter()
            .copied()
            .filter(|p| p.start_layer == pos && p.end_layer >= pos)
            .collect();

        if reachable.is_empty() {
            info!(
                "Pipeline plan failed: Gap at layer {} for model '{}'",
                pos, model_name
            );
            return None; // gap — no peer covers layer `pos`
        }

        // Pick the best-scoring candidate
        let mut best: Option<(&PeerInfo, i64)> = None;
        for peer in reachable {
            let load = tracker.load(&peer.node_id).await as i64;
            let layers = (peer.end_layer - pos + 1) as i64;
            // Layer coverage remains the dominant factor. For peers covering
            // the same range, lower RTT wins, followed by lower in-flight
            // load. Unknown RTT is a stable last resort.
            let rtt_penalty = rtts
                .get(&peer.node_id)
                .map(|rtt| (*rtt * 100.0).max(0.0) as i64)
                .unwrap_or(100_000);
            let score = layers * 100_000 - rtt_penalty.min(99_999) - load * 10;
            if best
                .map(|(current, s)| score > s || (score == s && peer.node_id < current.node_id))
                .unwrap_or(true)
            {
                best = Some((peer, score));
            }
        }

        let chosen = best.unwrap().0;

        // `PeerInfo::inference_url` is the one place that knows how to turn an
        // address into a URL. Splitting `chosen.addr` on ':' here instead cut
        // "[2804:d55:...]:42777" down to "[2804", and the hop died inside
        // reqwest with nothing but "builder error" to show for it.
        let Some(inference_url) = chosen.inference_url() else {
            return None;
        };

        steps.push(PipelineStep {
            node_id: chosen.node_id.clone(),
            inference_url,
            inference_candidates: chosen.inference_candidates(),
            start_layer: chosen.start_layer,
            end_layer: chosen.end_layer,
            // O que falta cobrir a partir de `pos`, não a fatia inteira. Quando
            // os dois coincidem - o caso comum - nada muda; quando não, é aqui
            // que fica registrado que o peer teria de entrar no meio da faixa,
            // em vez de o destinatário descobrir isso tarde demais.
            exec_start: pos,
            exec_end: chosen.end_layer,
            inference_frame_capabilities: chosen.inference_frame_capabilities.clone(),
            libp2p_peer_id: chosen.libp2p_peer_id.clone(),
            libp2p_addr_candidates: if chosen.addr_candidates.is_empty() {
                vec![chosen.addr.clone()]
            } else {
                chosen.addr_candidates.clone()
            },
        });

        pos = chosen.end_layer + 1;
    }

    let edges = steps
        .windows(2)
        .filter(|pair| pair[1].exec_start == pair[0].exec_end.saturating_add(1))
        .map(|pair| PipelineEdge {
            from_node_id: pair[0].node_id.clone(),
            to_node_id: pair[1].node_id.clone(),
            from_end_layer: pair[0].exec_end,
            to_start_layer: pair[1].exec_start,
        })
        .collect();

    Some(PipelinePlan { steps, edges })
}

/// Endereco por onde falar com este hop, decidido por quem vai discar.
///
/// Tres niveis, nesta ordem:
///
/// 1. a rota medida, se existir e ainda for um candidato anunciado;
/// 2. o `inference_url`, que sai de `peer.addr` - de onde os pacotes daquele
///    peer chegaram, e portanto o unico endereco *comprovado* antes da
///    primeira sonda;
/// 3. o primeiro candidato anunciado, quando nem isso ha.
///
/// A ordem dos dois ultimos ja foi a inversa, e custou requisicoes: como
/// `merge_observed` acrescenta o endereco observado ao **fim** da lista, o
/// primeiro candidato costuma ser a LAN do peer, e discar nela manda a
/// requisicao para um `192.168.x.y` de outro site. Palpite nao passa na frente
/// de fato conhecido; a medicao e que promove um candidato melhor, depois.
///
/// `fixadas` e a memoria da sessao: uma vez escolhido, o endereco daquele no
/// nao muda ate a sessao acabar.
pub fn resolve_step_url(
    step: &PipelineStep,
    tabela: &crate::route::RouteTable,
    fixadas: &mut std::collections::HashMap<String, String>,
) -> String {
    if let Some(ja) = fixadas.get(&step.node_id) {
        return ja.clone();
    }
    let escolhido = tabela
        .resolve(&step.node_id)
        .filter(|url| step.inference_candidates.iter().any(|c| c == url))
        .unwrap_or_else(|| {
            if step.inference_url.is_empty() {
                step.inference_candidates
                    .first()
                    .cloned()
                    .unwrap_or_default()
            } else {
                step.inference_url.clone()
            }
        });
    fixadas.insert(step.node_id.clone(), escolhido.clone());
    escolhido
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;
    use crate::network::PeerInfo;

    fn peer(addr: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "n".repeat(64),
            pubkey: "p".into(),
            addr: addr.into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: start,
            end_layer: end,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            gateway_port: Some(8090),
            role: String::new(),
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            tp: None,
        }
    }

    fn peer_tp(addr: &str, start: u32, end: u32, role: &str, rank: u32) -> PeerInfo {
        let mut p = peer(addr, start, end);
        p.tp = Some(crate::network::TpIdentity {
            role: role.into(),
            group_id: "g".into(),
            size: 2,
            rank,
            rank0_url: None,
            rank0_node_id: None,
            members: Vec::new(),
        });
        p
    }

    /// Um worker de tensor parallelism não é hop do pipeline: quem entra na
    /// cadeia é o primary do grupo, mesmo cobrindo a mesma faixa.
    #[tokio::test]
    async fn a_tp_worker_is_not_a_pipeline_hop() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let worker = peer_tp("192.168.1.10:42777", 0, 31, "worker", 1);
        let mut primary = peer_tp("192.168.1.11:42777", 0, 31, "primary", 0);
        primary.node_id = "primary".into();
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&[worker, primary], "qwen35", None, 32, &tracker)
            .await
            .expect("plano");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].node_id, "primary");
    }

    /// Duas implantações do **mesmo modelo**, no mesmo cluster.
    ///
    /// É o caso que não existia: antes do `deploy_id` os quatro peers eram um
    /// conjunto só (todos com `model == "qwen35"`), e o pipeline saía com
    /// metade das camadas de uma implantação e metade da outra - sem que nada
    /// nem ninguém pudesse notar.
    #[tokio::test]
    async fn two_deployments_of_one_model_never_mix() {
        // As faixas se cruzam de propósito: a caminhada gulosa por camada
        // prefere quem cobre mais camadas, então sem agrupamento ela pegaria
        // `a1` (0-19, a maior em 0) e depois `b2` (16-31, a maior em 20) -
        // um pipeline com um pé em cada implantação.
        let mut a1 = peer("10.0.0.1:42777", 0, 19);
        let mut a2 = peer("10.0.0.2:42777", 20, 31);
        let mut b1 = peer("10.0.0.3:42777", 0, 15);
        let mut b2 = peer("10.0.0.4:42777", 16, 31);
        // O `peer` de teste dá o mesmo `node_id` a todo mundo; aqui eles
        // precisam ser distinguíveis, que é o ponto do teste.
        for (p, id, dep) in [
            (&mut a1, "a1", "d-aaaaaaaa"),
            (&mut a2, "a2", "d-aaaaaaaa"),
            (&mut b1, "b1", "d-bbbbbbbb"),
            (&mut b2, "b2", "d-bbbbbbbb"),
        ] {
            p.node_id = id.to_string();
            p.deploy_id = dep.to_string();
        }
        let todos = vec![a1, a2, b1, b2];
        let tracker = LoadTracker::new();

        // Pedindo uma implantação, só as máquinas dela entram.
        let plano = plan_pipeline(&todos, "qwen35", Some("d-aaaaaaaa"), 32, &tracker)
            .await
            .expect("a implantação A cobre o modelo inteiro");
        assert_eq!(plano.steps.len(), 2);
        assert!(
            plano
                .steps
                .iter()
                .all(|s| s.inference_url.contains("10.0.0.1")
                    || s.inference_url.contains("10.0.0.2")),
            "nenhum hop pode vir da implantação B: {:?}",
            plano
                .steps
                .iter()
                .map(|s| &s.inference_url)
                .collect::<Vec<_>>()
        );

        let plano_b = plan_pipeline(&todos, "qwen35", Some("d-bbbbbbbb"), 32, &tracker)
            .await
            .unwrap();
        assert!(plano_b
            .steps
            .iter()
            .all(|s| s.inference_url.contains("10.0.0.3") || s.inference_url.contains("10.0.0.4")));

        // Sem escolher, ainda assim não se misturam: escolhe-se uma, inteira.
        let qualquer = plan_pipeline(&todos, "qwen35", None, 32, &tracker)
            .await
            .unwrap();
        let ids: std::collections::BTreeSet<&str> = qualquer
            .steps
            .iter()
            .map(|s| {
                todos
                    .iter()
                    .find(|p| p.node_id == s.node_id)
                    .map(|p| p.deploy_id.as_str())
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(ids.len(), 1, "um pipeline, uma implantação: {ids:?}");
    }

    /// Um id que ninguém serve não pode cair de volta no rótulo e devolver a
    /// outra implantação calado - seria atender numa configuração que o cliente
    /// explicitamente não pediu.
    #[tokio::test]
    async fn an_unknown_deployment_id_serves_nobody() {
        let mut p1 = peer("10.0.0.1:42777", 0, 31);
        p1.deploy_id = "d-aaaaaaaa".into();
        let plano =
            plan_pipeline(&[p1], "qwen35", Some("d-cccccccc"), 32, &LoadTracker::new()).await;
        assert!(plano.is_none());
    }

    /// Cluster em atualização: um peer já anuncia implantação, o outro ainda
    /// não. O que não anuncia continua servindo o rótulo, senão a atualização
    /// de um nó por vez abriria um buraco de camadas.
    #[tokio::test]
    async fn a_peer_without_a_deployment_still_serves_its_label() {
        let mut novo = peer("10.0.0.1:42777", 0, 15);
        novo.deploy_id = "d-aaaaaaaa".into();
        let antigo = peer("10.0.0.2:42777", 16, 31); // deploy_id vazio

        let plano = plan_pipeline(&[novo, antigo], "qwen35", None, 32, &LoadTracker::new())
            .await
            .expect("os dois juntos cobrem o modelo");
        assert_eq!(plano.steps.len(), 2);
    }

    #[tokio::test]
    async fn planner_follows_an_exact_subrange_entry_instead_of_entering_mid_slice() {
        let mut head = peer("10.0.0.1:42777", 0, 3);
        let mut overlapping = peer("10.0.0.2:42777", 2, 15);
        let mut exact = peer("10.0.0.3:42777", 4, 15);
        head.node_id = "head".into();
        overlapping.node_id = "overlapping".into();
        exact.node_id = "exact".into();

        let plan = plan_pipeline(
            &[head, overlapping, exact],
            "qwen35",
            None,
            16,
            &LoadTracker::new(),
        )
        .await
        .expect("a cobertura contígua existe");

        assert_eq!(
            plan.steps
                .iter()
                .map(|step| step.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["head", "exact"]
        );
        assert_eq!((plan.steps[1].exec_start, plan.steps[1].exec_end), (4, 15));
    }

    #[tokio::test]
    async fn equivalent_entry_branches_choose_the_lowest_gateway_rtt() {
        let mut slow = peer("10.0.0.1:42777", 0, 7);
        let mut fast = peer("10.0.0.2:42777", 0, 7);
        let mut tail = peer("10.0.0.3:42777", 8, 15);
        slow.node_id = "slow-entry".into();
        fast.node_id = "fast-entry".into();
        tail.node_id = "tail".into();

        let mut rtts = HashMap::new();
        rtts.insert("slow-entry".into(), 25.0);
        rtts.insert("fast-entry".into(), 3.0);

        let plan = plan_pipeline_with_rtt(
            &[slow, fast, tail],
            "qwen35",
            None,
            16,
            &LoadTracker::new(),
            &rtts,
        )
        .await
        .expect("as duas entradas cobrem o mesmo ramo");

        assert_eq!(plan.steps[0].node_id, "fast-entry");
    }

    #[test]
    fn peers_without_a_compression_capability_keep_the_v1_f16_wire_format() {
        let step = PipelineStep {
            node_id: "old".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            inference_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            exec_start: 0,
            exec_end: 1,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        };
        assert_eq!(step.frame_encoding(), crate::wire::FrameEncoding::F16);
    }

    #[test]
    fn f8_is_refused_when_any_pipeline_step_lacks_the_capability() {
        let mut missing = step(&[CAP_FRAME_DTYPE_F8]);
        missing.node_id = "old-peer".into();
        let error = plan_of(vec![step(&[CAP_FRAME_DTYPE_F8]), missing])
            .validate_wire_dtype(crate::wire::WireDtype::F8)
            .unwrap_err();
        assert!(error.contains("old-peer"), "erro inesperado: {error}");
        assert!(error.contains("f8"), "erro inesperado: {error}");
    }

    #[test]
    fn f8_is_accepted_when_every_pipeline_step_advertises_it() {
        let plan = plan_of(vec![
            step(&[CAP_FRAME_DTYPE_F8]),
            step(&[CAP_FRAME_DTYPE_F8]),
        ]);
        assert!(plan.validate_wire_dtype(crate::wire::WireDtype::F8).is_ok());
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::F16)
            .is_ok());
    }

    #[test]
    fn route_affinity_keeps_the_selected_replica_for_the_whole_session() {
        let plan = PipelinePlan {
            steps: vec![
                PipelineStep {
                    node_id: "replica-a".into(),
                    inference_url: "http://127.0.0.1:8181".into(),
                    inference_candidates: Vec::new(),
                    start_layer: 0,
                    end_layer: 7,
                    exec_start: 0,
                    exec_end: 7,
                    inference_frame_capabilities: Vec::new(),
                    libp2p_peer_id: None,
                    libp2p_addr_candidates: Vec::new(),
                },
                PipelineStep {
                    node_id: "replica-b".into(),
                    inference_url: "http://127.0.0.1:8182".into(),
                    inference_candidates: Vec::new(),
                    start_layer: 8,
                    end_layer: 15,
                    exec_start: 8,
                    exec_end: 15,
                    inference_frame_capabilities: Vec::new(),
                    libp2p_peer_id: None,
                    libp2p_addr_candidates: Vec::new(),
                },
            ],
            edges: Vec::new(),
        };
        let affinity = RouteAffinity::from_plan("deploy-1", "session-1", &plan);
        assert_eq!(affinity.hop_replicas, vec!["replica-a", "replica-b"]);
        assert!(affinity.matches_plan(&plan));

        let mut changed = plan.clone();
        changed.steps[0].node_id = "replica-c".into();
        assert!(!affinity.matches_plan(&changed));
    }

    fn step_com(candidatos: Vec<&str>, url: &str) -> PipelineStep {
        PipelineStep {
            node_id: "n1".into(),
            inference_url: url.into(),
            inference_candidates: candidatos.into_iter().map(String::from).collect(),
            start_layer: 0,
            end_layer: 9,
            exec_start: 0,
            exec_end: 9,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        }
    }

    /// Um step sem candidatos e um no rodando binario anterior. Cair no
    /// `inference_url` e o que impede um cluster de versoes misturadas de
    /// partir ao meio.
    #[test]
    fn a_step_without_candidates_falls_back_to_the_resolved_url() {
        let tabela = crate::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        let step = step_com(vec![], "http://192.168.1.9:8181");
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://192.168.1.9:8181",
        );
    }

    /// A escolha da sessao nao muda no meio: o cache KV do outro lado e por
    /// sessao, e trocar de endereco na metade e pior que a rota lenta.
    #[test]
    fn the_route_is_pinned_for_the_whole_session() {
        let tabela = crate::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        fixadas.insert("n1".to_string(), "http://escolhido:8181".to_string());
        let step = step_com(vec!["http://outro:8181"], "http://legado:8181");
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://escolhido:8181",
        );
    }

    /// Sem medicao, vale o endereco por onde o peer ja falou conosco.
    ///
    /// `merge_observed` acrescenta o endereco de origem do pacote **ao fim** da
    /// lista, entao o primeiro candidato e o que o peer anunciou de si - em
    /// geral a LAN dele. Discar nele antes de medir manda a requisicao para um
    /// `192.168.x.y` de outro site, que e inalcancavel daqui: em 2026-08-14 o
    /// log mostrou tres tentativas em `http://192.168.0.37:8181/v1/prefill`
    /// para um peer que so responde pelo IPv6 global.
    ///
    /// O `inference_url` nao e um palpite: ele sai de `peer.addr`, que e de
    /// onde os pacotes daquele peer chegaram. E o unico endereco comprovado que
    /// temos antes da primeira sonda.
    #[test]
    fn without_a_measurement_the_observed_address_wins_over_an_announced_one() {
        let tabela = crate::route::RouteTable::new();
        let mut fixadas = std::collections::HashMap::new();
        let step = step_com(
            vec!["http://192.168.0.37:8181", "http://[2804:f30::1]:8181"],
            "http://[2804:f30::1]:8181",
        );
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://[2804:f30::1]:8181",
        );
    }

    /// Medido vence: e o ponto da fase de rota dinamica.
    #[test]
    fn a_measured_route_beats_the_observed_address() {
        let tabela = crate::route::RouteTable::new();
        tabela.remember_for_test("n1", "http://rapido:8181", 3.0);
        let mut fixadas = std::collections::HashMap::new();
        let step = step_com(
            vec!["http://rapido:8181", "http://lento:8181"],
            "http://lento:8181",
        );
        assert_eq!(
            resolve_step_url(&step, &tabela, &mut fixadas),
            "http://rapido:8181"
        );
    }

    fn step(caps: &[&str]) -> PipelineStep {
        PipelineStep {
            node_id: "n".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            inference_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            exec_start: 0,
            exec_end: 1,
            inference_frame_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        }
    }

    fn plan_of(steps: Vec<PipelineStep>) -> PipelinePlan {
        PipelinePlan {
            steps,
            edges: Vec::new(),
        }
    }

    /// A exigência do MTP cai na **cauda**, que tem o bloco `nextn`. Todos
    /// precisam saber decodificar um lote, porque o rascunho viaja para a cabeça.
    #[test]
    fn mtp_needs_the_tail_to_have_nextn_and_everyone_to_batch() {
        let com_mtp = &[
            CAP_SPECULATE_LOOKUP,
            CAP_SPECULATE_MTP,
            CAP_SPECULATE_MTP_SLICED,
        ][..];
        let so_lote = &[CAP_SPECULATE_LOOKUP, CAP_SPECULATE_MTP_SLICED][..];
        let antigo = &[CAP_SPECULATE_LOOKUP][..];

        assert!(
            plan_of(vec![step(com_mtp)]).supports_mtp(),
            "uma máquina só"
        );
        assert!(
            plan_of(vec![step(so_lote), step(com_mtp)]).supports_mtp(),
            "a cauda tem o bloco — é ela quem rascunha, o gateway leva o rascunho à cabeça"
        );
        assert!(
            !plan_of(vec![step(com_mtp), step(so_lote)]).supports_mtp(),
            "a cauda sem o bloco não pode rascunhar"
        );
        assert!(
            !plan_of(vec![step(com_mtp), step(&[])]).supports_mtp(),
            "um hop que não decodifica lote quebra a rodada inteira"
        );
        assert!(
            !plan_of(vec![step(so_lote), step(antigo)]).supports_mtp(),
            "um peer velho no anel não entende o speculate novo — a cap nova é exigida em todos"
        );
        assert!(!plan_of(vec![]).supports_mtp());
    }

    #[test]
    fn relay_chain_requires_every_step_to_advertise_it() {
        assert!(
            plan_of(vec![step(&[CAP_RELAY_CHAIN]), step(&[CAP_RELAY_CHAIN])])
                .supports_relay_chain()
        );
        assert!(!plan_of(vec![step(&[CAP_RELAY_CHAIN]), step(&[])]).supports_relay_chain());
        assert!(!plan_of(vec![]).supports_relay_chain());
    }

    #[test]
    fn direct_reply_requires_every_step_and_more_than_one_hop() {
        assert!(
            plan_of(vec![step(&[CAP_RELAY_DIRECT]), step(&[CAP_RELAY_DIRECT])])
                .supports_direct_reply()
        );
        // Um passo antigo no meio derruba a cadeia inteira para o anel.
        assert!(
            !plan_of(vec![step(&[CAP_RELAY_DIRECT]), step(&[CAP_RELAY_CHAIN])])
                .supports_direct_reply()
        );
        // Um hop só já é o mínimo; desviar a resposta não economiza perna.
        assert!(!plan_of(vec![step(&[CAP_RELAY_DIRECT])]).supports_direct_reply());
        assert!(!plan_of(vec![]).supports_direct_reply());
    }

    #[test]
    fn speculation_requires_every_step_to_advertise_it() {
        assert!(plan_of(vec![
            step(&[CAP_SPECULATE_LOOKUP]),
            step(&[CAP_SPECULATE_LOOKUP])
        ])
        .supports_speculation());
        assert!(!plan_of(vec![step(&[CAP_SPECULATE_LOOKUP]), step(&[])]).supports_speculation());
        assert!(!plan_of(vec![]).supports_speculation());
    }

    #[test]
    fn routing_request_from_peers_preserves_peer_ranges() {
        let peers = vec![
            peer("192.168.1.10:42777", 0, 15),
            peer("192.168.1.11:42777", 16, 31),
        ];

        let request = routing_request_from_peers(32, &peers);

        assert_eq!(request.api_version, routing_contract::ROUTING_API_VERSION);
        assert_eq!(request.cluster.total_layers, 32);
        assert_eq!(request.cluster.peers[0].peer_id, peers[0].node_id);
        assert_eq!(request.cluster.peers[0].start_layer, 0);
        assert_eq!(request.cluster.peers[0].end_layer, 15);
        assert!(request.cluster.peers[0].supports_inference);
        assert_eq!(request.cluster.peers[1].start_layer, 16);
        assert_eq!(request.cluster.peers[1].end_layer, 31);
    }

    /// O passo do plano tem que virar URL de verdade: com IPv6 o endereco era
    /// cortado no primeiro ':' e o hop morria com "builder error" no reqwest.
    #[tokio::test]
    async fn an_ipv6_peer_becomes_a_valid_step_url() {
        // O esquema da URL sai de um estado global do processo
        // (`peer_client::tls_ativo`), e os testes rodam em paralelo. Sem esta
        // trava, um teste que liga a política do cluster faz este aqui ver
        // `https://` onde espera `http://`.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let peers = vec![peer(
            "[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777",
            0,
            31,
        )];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", None, 32, &tracker)
            .await
            .expect("plano");
        assert_eq!(
            plan.steps[0].inference_url,
            "http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8181"
        );
        let url = format!("{}/v1/prefill", plan.steps[0].inference_url);
        assert!(url.parse::<reqwest::Url>().is_ok(), "URL invalida: {url}");
    }

    #[tokio::test]
    async fn an_ipv4_peer_is_unchanged() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let peers = vec![peer("192.168.100.138:42777", 0, 31)];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", None, 32, &tracker)
            .await
            .expect("plano");
        assert_eq!(plan.steps[0].inference_url, "http://192.168.100.138:8181");
    }

    #[tokio::test]
    async fn an_exact_replica_prefers_the_lower_rtt() {
        let mut slow = peer("192.168.1.10:42777", 0, 31);
        slow.node_id = "slow".into();
        let mut fast = peer("192.168.1.11:42777", 0, 31);
        fast.node_id = "fast".into();
        let rtts = HashMap::from([(String::from("slow"), 80.0), (String::from("fast"), 12.0)]);

        let plan = plan_pipeline_with_rtt(
            &[slow, fast],
            "qwen35",
            None,
            32,
            &LoadTracker::new(),
            &rtts,
        )
        .await
        .expect("plano");

        assert_eq!(plan.steps[0].node_id, "fast");
    }

    #[tokio::test]
    async fn plan_exposes_handoffs_for_the_selected_steps() {
        let mut first = peer("192.168.1.10:42777", 0, 15);
        first.node_id = "first".into();
        let mut second = peer("192.168.1.11:42777", 16, 31);
        second.node_id = "second".into();

        let plan = plan_pipeline(&[first, second], "qwen35", None, 32, &LoadTracker::new())
            .await
            .expect("plano");

        assert_eq!(
            plan.edges,
            vec![PipelineEdge {
                from_node_id: "first".into(),
                to_node_id: "second".into(),
                from_end_layer: 15,
                to_start_layer: 16,
            }]
        );
    }
}
