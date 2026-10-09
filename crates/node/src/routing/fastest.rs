//! Planejador por tempo de entrega: a pipeline escolhida é a que termina a
//! requisição mais cedo, não a de menor latência de rede.
//!
//! O custo de um caminho soma, para esta requisição:
//!
//! ```text
//! prefill: tokens_novos × ms/token de cada estágio × (1 + fila do estágio)
//! decode:  tokens_esperados × (ms/passo de cada estágio × (1 + fila))
//! rede:    (1 + tokens_esperados) × (RTT de cada aresta, inclusive
//!          quem pede → cabeça e cauda → quem pede)
//! ```
//!
//! - **Rede** vem das rotas fixadas (`route_plan`): quem pede mede as próprias
//!   arestas; as arestas entre dois outros peers vêm do `/health` do peer de
//!   origem (`route_rtt_ms`, `gateway_rtt_ms`).
//! - **Velocidade** vem do `/health` de cada peer (`perf`): teste ao servir e
//!   média do tráfego real.
//! - **Balanceamento**: a fila (`active_generations` do peer, ou a carga em voo
//!   deste gateway, o que for maior) multiplica o custo do estágio. Um peer
//!   rápido com fila vira mais lento que um médio ocioso, e a carga se espalha
//!   sem rodízio artificial.
//! - **Prefixo**: um caminho que já tem o prefixo deste prompt paga só os
//!   tokens novos (`estimate_chain` com `cached_tokens`), então a reutilização
//!   vence quando compensa e perde quando o peer do cache está saturado.
//!
//! Como os termos são somas por estágio e por aresta, o melhor caminho sai de
//! uma programação dinâmica sobre as faixas de camadas.

use std::collections::HashMap;

use serde::Serialize;

use crate::network::PeerInfo;
use crate::pipeline::{model_pipeline_candidates, plan_from_steps, step_for, PipelinePlan};

/// RTT assumido para uma aresta que ninguém mediu ainda.
const UNKNOWN_RTT_MS: f64 = 100.0;

/// Custo por camada quando nenhum peer do modelo tem velocidade medida. O
/// valor absoluto não importa - é igual para todos -, só precisa ser maior que
/// zero para a fila ainda pesar na escolha.
const FALLBACK_PREFILL_MS_PER_LAYER: f64 = 0.1;
const FALLBACK_DECODE_MS_PER_LAYER: f64 = 1.0;

/// O que o `/health` de um peer diz sobre custo e carga.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PeerPerf {
    pub prefill_ms_per_token: Option<f64>,
    pub decode_ms_per_step: Option<f64>,
    pub active_generations: u32,
    /// RTT da rota fixada deste peer até o servidor de inferência de outro.
    pub route_rtt_ms: HashMap<String, f64>,
    /// RTT da rota fixada deste peer até o gateway de outro.
    pub gateway_rtt_ms: HashMap<String, f64>,
    /// Transporte (`quic`, `http`, `tunnel`) das rotas fixadas deste peer.
    pub route_kinds: HashMap<String, String>,
    pub gateway_route_kinds: HashMap<String, String>,
    /// Peers que este só alcança por circuito num relé (`relayed_peers`): uma
    /// rota `tunnel` até eles passa por um servidor intermediário.
    pub relayed_peers: std::collections::HashSet<String>,
}

impl PeerPerf {
    pub fn from_health(body: &serde_json::Value) -> Self {
        let number =
            |value: &serde_json::Value| value.as_f64().filter(|v| v.is_finite() && *v > 0.0);
        let map = |value: Option<&serde_json::Value>| -> HashMap<String, f64> {
            value
                .and_then(serde_json::Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(id, rtt)| number(rtt).map(|rtt| (id.clone(), rtt)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let kinds = |value: Option<&serde_json::Value>| -> HashMap<String, String> {
            value
                .and_then(serde_json::Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(id, kind)| {
                            kind.as_str().map(|kind| (id.clone(), kind.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let perf = body.get("perf");
        Self {
            prefill_ms_per_token: perf
                .and_then(|p| p.get("prefill_ms_per_token"))
                .and_then(number),
            decode_ms_per_step: perf
                .and_then(|p| p.get("decode_ms_per_step"))
                .and_then(number),
            active_generations: body
                .get("active_generations")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32,
            route_rtt_ms: map(body.get("route_rtt_ms")),
            gateway_rtt_ms: map(body.get("gateway_rtt_ms")),
            route_kinds: kinds(body.get("route_kinds")),
            gateway_route_kinds: kinds(body.get("gateway_route_kinds")),
            relayed_peers: body
                .get("relayed_peers")
                .and_then(serde_json::Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Tudo que a estimativa precisa, já colhido pelo gateway.
pub struct CostInputs<'a> {
    /// `node_id` de quem origina a requisição.
    pub requester: &'a str,
    /// RTT de quem pede até o servidor de inferência de cada peer.
    pub requester_rtt_ms: &'a HashMap<String, f64>,
    pub perf: &'a HashMap<String, PeerPerf>,
    /// Requisições em voo que este gateway já mandou para cada peer.
    pub local_load: &'a HashMap<String, u32>,
    pub prompt_tokens: f64,
    pub decode_tokens: f64,
    /// Modo de volta imposto (`POM_RETURN_MODE`, para teste); `None` deixa o
    /// planejador escolher.
    pub forced_return: Option<ReturnMode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageEstimate {
    pub node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub prefill_ms_per_token: f64,
    pub decode_ms_per_step: f64,
    /// `false` quando o custo é estimado pela média por camada dos outros.
    pub measured: bool,
    pub queue: u32,
    /// RTT da aresta que chega a este estágio.
    pub rtt_in_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEstimate {
    pub stages: Vec<StageEstimate>,
    /// RTT da cauda de volta a quem pediu; infinito quando não há volta.
    pub return_rtt_ms: f64,
    /// Por onde a resposta volta, escolhido junto com o caminho.
    pub return_mode: ReturnMode,
    pub cached_tokens: u32,
    pub prefill_ms: f64,
    /// Tempo de um token de decode com as filas atuais: estágios mais rede.
    pub decode_ms_per_token: f64,
    pub total_ms: f64,
}

struct Speeds {
    prefill_per_layer: f64,
    decode_per_layer: f64,
}

fn layers(peer: &PeerInfo) -> f64 {
    (peer.end_layer - peer.start_layer + 1) as f64
}

/// Custo médio por camada dos peers medidos, para estimar quem ainda não foi.
fn fallback_speeds(candidates: &[&PeerInfo], perf: &HashMap<String, PeerPerf>) -> Speeds {
    let mut prefill = (0.0, 0.0);
    let mut decode = (0.0, 0.0);
    for peer in candidates {
        let Some(p) = perf.get(&peer.node_id) else {
            continue;
        };
        if let Some(ms) = p.prefill_ms_per_token {
            prefill.0 += ms;
            prefill.1 += layers(peer);
        }
        if let Some(ms) = p.decode_ms_per_step {
            decode.0 += ms;
            decode.1 += layers(peer);
        }
    }
    Speeds {
        prefill_per_layer: if prefill.1 > 0.0 {
            prefill.0 / prefill.1
        } else {
            FALLBACK_PREFILL_MS_PER_LAYER
        },
        decode_per_layer: if decode.1 > 0.0 {
            decode.0 / decode.1
        } else {
            FALLBACK_DECODE_MS_PER_LAYER
        },
    }
}

fn stage(peer: &PeerInfo, inputs: &CostInputs, speeds: &Speeds, rtt_in_ms: f64) -> StageEstimate {
    let perf = inputs.perf.get(&peer.node_id);
    let prefill = perf.and_then(|p| p.prefill_ms_per_token);
    let decode = perf.and_then(|p| p.decode_ms_per_step);
    let queue = perf
        .map(|p| p.active_generations)
        .unwrap_or(0)
        .max(inputs.local_load.get(&peer.node_id).copied().unwrap_or(0));
    StageEstimate {
        node_id: peer.node_id.clone(),
        start_layer: peer.start_layer,
        end_layer: peer.end_layer,
        prefill_ms_per_token: prefill.unwrap_or(speeds.prefill_per_layer * layers(peer)),
        decode_ms_per_step: decode.unwrap_or(speeds.decode_per_layer * layers(peer)),
        measured: prefill.is_some() && decode.is_some(),
        queue,
        rtt_in_ms,
    }
}

/// RTT de `from` até o servidor de inferência de `to`.
fn edge_rtt(from: &str, to: &str, inputs: &CostInputs) -> f64 {
    measured_edge_rtt(from, to, inputs).unwrap_or(UNKNOWN_RTT_MS)
}

/// `edge_rtt` sem palpite: `None` quando ninguém mediu a aresta.
fn measured_edge_rtt(from: &str, to: &str, inputs: &CostInputs) -> Option<f64> {
    if from == to {
        return Some(0.0);
    }
    if from == inputs.requester {
        return inputs.requester_rtt_ms.get(to).copied();
    }
    inputs
        .perf
        .get(from)
        .and_then(|p| p.route_rtt_ms.get(to).copied())
        // Sem medição do peer de origem, a do gateway até o destino é o
        // melhor palpite disponível.
        .or_else(|| inputs.requester_rtt_ms.get(to).copied())
}

/// Por onde a resposta de cada token volta a quem pediu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReturnMode {
    /// A cauda entrega direto ao gateway (`reply_to`).
    Direct,
    /// A resposta volta pelas mesmas conexões da ida.
    Ring,
}

impl ReturnMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReturnMode::Direct => "direct",
            ReturnMode::Ring => "ring",
        }
    }

    /// Fração do RTT de cada aresta da ida que o modo paga por token: a ida
    /// é só o caminho de chegada na resposta direta, e ida e volta no anel.
    fn forward_weight(self) -> f64 {
        match self {
            ReturnMode::Direct => 0.5,
            ReturnMode::Ring => 1.0,
        }
    }
}

/// `POM_RETURN_MODE=direct|ring` impõe o modo de volta, para testar um caminho
/// que o planejador não escolheria - por exemplo, a entrega direta a um
/// gateway atrás de NAT, que o anel esconderia. Ausente ou `auto`, o
/// planejador decide. Com `direct`, uma volta que não existe vira falha de
/// inferência em vez de ser contornada.
pub fn forced_return_mode() -> Option<ReturnMode> {
    match crate::brand::env("RETURN_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "direct" | "direct" => Some(ReturnMode::Direct),
        "ring" | "ring" => Some(ReturnMode::Ring),
        _ => None,
    }
}

/// RTT medido da cauda de volta a quem pediu. `None` quando a cauda já mediu
/// rotas até outros gateways e não tem nenhuma até este: sem volta, a resposta
/// direta é inviável. Sem medição nenhuma da cauda, o palpite é o RTT deste
/// gateway até ela.
fn return_rtt(tail: &str, inputs: &CostInputs) -> Option<f64> {
    match measured_return_rtt(tail, inputs) {
        ReturnLeg::Measured(rtt) => Some(rtt),
        ReturnLeg::Missing => None,
        ReturnLeg::Unmeasured => Some(UNKNOWN_RTT_MS),
    }
}

enum ReturnLeg {
    Measured(f64),
    /// A cauda mediu rotas, mas nenhuma até este gateway.
    Missing,
    /// Ninguém mediu nada ainda.
    Unmeasured,
}

fn measured_return_rtt(tail: &str, inputs: &CostInputs) -> ReturnLeg {
    if tail == inputs.requester {
        return ReturnLeg::Measured(0.0);
    }
    let from_gateway = || match inputs.requester_rtt_ms.get(tail) {
        Some(rtt) => ReturnLeg::Measured(*rtt),
        None => ReturnLeg::Unmeasured,
    };
    match inputs.perf.get(tail) {
        Some(perf) => match perf.gateway_rtt_ms.get(inputs.requester) {
            Some(rtt) => ReturnLeg::Measured(*rtt),
            None if !perf.gateway_rtt_ms.is_empty() => ReturnLeg::Missing,
            None => from_gateway(),
        },
        None => from_gateway(),
    }
}

/// Custo de um estágio nesta requisição, com a fila aplicada.
fn stage_cost(stage: &StageEstimate, prompt_tokens: f64, decode_tokens: f64) -> f64 {
    let load = 1.0 + stage.queue as f64;
    (prompt_tokens * stage.prefill_ms_per_token + decode_tokens * stage.decode_ms_per_step) * load
}

fn edge_cost(rtt_ms: f64, decode_tokens: f64) -> f64 {
    (1.0 + decode_tokens) * rtt_ms
}

/// Rede de um token no caminho, pelo modo de volta: meio RTT por aresta da ida
/// mais meio RTT da cauda até o gateway na resposta direta; o RTT inteiro de
/// cada aresta no anel. `None` quando o modo não tem volta.
fn network_ms(
    stages: &[StageEstimate],
    return_rtt_ms: Option<f64>,
    mode: ReturnMode,
) -> Option<f64> {
    let forward: f64 = stages.iter().map(|s| s.rtt_in_ms).sum::<f64>() * mode.forward_weight();
    match mode {
        ReturnMode::Direct => return_rtt_ms.map(|back| forward + back / 2.0),
        ReturnMode::Ring => Some(forward),
    }
}

/// Estima um caminho já escolhido, pelo melhor modo de volta. `cached_tokens`
/// do prompt não passam pelo prefill (prefixo já retido nesse caminho).
pub fn estimate_chain(
    chain: &[&PeerInfo],
    inputs: &CostInputs,
    candidates: &[&PeerInfo],
    cached_tokens: u32,
) -> PlanEstimate {
    let speeds = fallback_speeds(candidates, inputs.perf);
    let prompt = (inputs.prompt_tokens - cached_tokens as f64).max(1.0);
    let mut stages = Vec::with_capacity(chain.len());
    let mut from = inputs.requester.to_string();
    for peer in chain {
        let rtt = edge_rtt(&from, &peer.node_id, inputs);
        stages.push(stage(peer, inputs, &speeds, rtt));
        from = peer.node_id.clone();
    }
    let return_rtt_ms = chain
        .last()
        .and_then(|tail| return_rtt(&tail.node_id, inputs));
    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    let (return_mode, network) = modes
        .into_iter()
        .filter_map(|mode| network_ms(&stages, return_rtt_ms, mode).map(|ms| (mode, ms)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((ReturnMode::Ring, 0.0));
    let prefill_ms = stages
        .iter()
        .map(|s| prompt * s.prefill_ms_per_token * (1.0 + s.queue as f64))
        .sum::<f64>()
        + network;
    let decode_ms_per_token = stages
        .iter()
        .map(|s| s.decode_ms_per_step * (1.0 + s.queue as f64))
        .sum::<f64>()
        + network;
    let total_ms = stages
        .iter()
        .map(|s| stage_cost(s, prompt, inputs.decode_tokens))
        .sum::<f64>()
        + edge_cost(network, inputs.decode_tokens);
    PlanEstimate {
        stages,
        return_rtt_ms: return_rtt_ms.unwrap_or(f64::INFINITY),
        return_mode,
        cached_tokens,
        prefill_ms,
        decode_ms_per_token,
        total_ms,
    }
}

/// Um caminho como o painel o mostra: as mesmas fontes e a mesma conta do
/// planejador, mas sem palpite - perna não medida fica `None` e não fecha
/// total.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChainView {
    /// RTT de ida e volta de cada perna: quem pede -> cabeça, cada passagem
    /// entre estágios e, se a cauda não for quem pede, cauda -> quem pede.
    pub legs_ms: Vec<Option<f64>>,
    /// Modo de volta que o planejador escolheria para este caminho.
    pub return_mode: Option<ReturnMode>,
    /// Rede paga por token no modo escolhido (não a soma das pernas: na
    /// resposta direta cada perna custa meio RTT; no anel a volta da cauda
    /// não existe).
    pub network_ms: Option<f64>,
    /// Compute de um passo de decode somado sobre os estágios, com a fila.
    pub compute_ms: Option<f64>,
    /// Um token pela rota: rede no modo escolhido mais compute.
    pub decode_ms_per_token: Option<f64>,
}

pub fn describe_chain(chain: &[&PeerInfo], inputs: &CostInputs) -> ChainView {
    let mut legs_ms = Vec::with_capacity(chain.len() + 1);
    let mut from = inputs.requester;
    for peer in chain {
        legs_ms.push(measured_edge_rtt(from, &peer.node_id, inputs));
        from = &peer.node_id;
    }
    let back = match chain.last() {
        Some(tail) if tail.node_id != inputs.requester => {
            let leg = measured_return_rtt(&tail.node_id, inputs);
            legs_ms.push(match leg {
                ReturnLeg::Measured(rtt) => Some(rtt),
                _ => None,
            });
            leg
        }
        _ => ReturnLeg::Measured(0.0),
    };
    let forward = legs_ms[..chain.len()]
        .iter()
        .copied()
        .collect::<Option<Vec<_>>>()
        .map(|legs| legs.into_iter().sum::<f64>());
    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    // Sem a volta medida, o planejador compararia com um palpite; o painel
    // não escolhe o modo por ele.
    let undecided = matches!(back, ReturnLeg::Unmeasured) && modes.contains(&ReturnMode::Direct);
    let chosen = forward.filter(|_| !undecided).and_then(|forward| {
        modes
            .into_iter()
            .filter_map(|mode| {
                let network = match (mode, &back) {
                    (ReturnMode::Ring, _) => forward * mode.forward_weight(),
                    (ReturnMode::Direct, ReturnLeg::Measured(back)) => {
                        forward * mode.forward_weight() + back / 2.0
                    }
                    (ReturnMode::Direct, _) => return None,
                };
                Some((mode, network))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    });
    let compute_ms = chain
        .iter()
        .map(|peer| {
            let perf = inputs.perf.get(&peer.node_id)?;
            let queue = perf
                .active_generations
                .max(inputs.local_load.get(&peer.node_id).copied().unwrap_or(0));
            perf.decode_ms_per_step.map(|ms| ms * (1.0 + queue as f64))
        })
        .collect::<Option<Vec<_>>>()
        .map(|steps| steps.into_iter().sum::<f64>());
    ChainView {
        legs_ms,
        return_mode: chosen.map(|(mode, _)| mode),
        network_ms: chosen.map(|(_, ms)| ms),
        compute_ms,
        decode_ms_per_token: chosen
            .zip(compute_ms)
            .map(|((_, network), compute)| network + compute),
    }
}

/// O caminho e o modo de volta de menor tempo de entrega, e a estimativa.
///
/// O modo de volta entra no planejamento, não depois dele: o mesmo caminho
/// custa diferente com resposta direta e com anel, e o melhor caminho de um
/// modo pode não ser o do outro. Por isso a programação dinâmica roda uma vez
/// por modo e vence o menor total.
///
/// Só entra peer cuja faixa começa exatamente onde a anterior termina (a mesma
/// regra do planejador guloso). `preferred_head`, quando escolhido na
/// interface, é a única cabeça aceita: escolha inválida falha fechada.
pub fn plan_fastest(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    inputs: &CostInputs,
    preferred_head: Option<&str>,
) -> Option<(PipelinePlan, PlanEstimate)> {
    let mut candidates = model_pipeline_candidates(peers, model_name, deploy_id);
    candidates.sort_by(|a, b| {
        a.start_layer
            .cmp(&b.start_layer)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    let speeds = fallback_speeds(&candidates, inputs.perf);

    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    let mut winner: Option<(f64, Vec<&PeerInfo>)> = None;
    for mode in modes {
        let weight = mode.forward_weight();
        // Melhor custo acumulado até o fim de cada candidato, e de onde veio.
        let mut best: HashMap<&str, (f64, Option<usize>)> = HashMap::new();
        for (index, peer) in candidates.iter().enumerate() {
            let own = |rtt: f64| {
                stage_cost(
                    &stage(peer, inputs, &speeds, rtt),
                    inputs.prompt_tokens,
                    inputs.decode_tokens,
                ) + edge_cost(rtt * weight, inputs.decode_tokens)
            };
            let entry = if peer.start_layer == 0 {
                if preferred_head.is_some_and(|head| head != peer.node_id) {
                    continue;
                }
                Some((own(edge_rtt(inputs.requester, &peer.node_id, inputs)), None))
            } else {
                candidates[..index]
                    .iter()
                    .enumerate()
                    .filter(|(_, prev)| prev.end_layer + 1 == peer.start_layer)
                    .filter_map(|(prev_index, prev)| {
                        best.get(prev.node_id.as_str()).map(|(cost, _)| {
                            (
                                cost + own(edge_rtt(&prev.node_id, &peer.node_id, inputs)),
                                Some(prev_index),
                            )
                        })
                    })
                    .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            };
            if let Some(entry) = entry {
                best.insert(peer.node_id.as_str(), entry);
            }
        }

        let tail = candidates
            .iter()
            .enumerate()
            .filter(|(_, peer)| peer.end_layer + 1 == total_layers)
            .filter_map(|(index, peer)| {
                let (cost, _) = best.get(peer.node_id.as_str())?;
                let back = match mode {
                    ReturnMode::Direct => edge_cost(
                        return_rtt(&peer.node_id, inputs)? / 2.0,
                        inputs.decode_tokens,
                    ),
                    ReturnMode::Ring => 0.0,
                };
                Some((index, cost + back))
            })
            .min_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| candidates[a.0].node_id.cmp(&candidates[b.0].node_id))
            });
        let Some((tail_index, total)) = tail else {
            continue;
        };
        let mut chain = Vec::new();
        let mut cursor = Some(tail_index);
        while let Some(index) = cursor {
            let peer = candidates[index];
            chain.push(peer);
            cursor = best.get(peer.node_id.as_str()).and_then(|(_, prev)| *prev);
        }
        chain.reverse();
        if winner.as_ref().is_none_or(|(cost, _)| total < *cost) {
            winner = Some((total, chain));
        }
    }

    let (_, chain) = winner?;
    let plan = plan_for_chain(&chain)?;
    // A estimativa refaz a escolha do modo para o caminho vencedor, com a
    // mesma conta; é dela que o gateway lê o modo.
    let estimate = estimate_chain(&chain, inputs, &candidates, 0);
    Some((plan, estimate))
}

/// Por que a resposta deve voltar pelo anel em vez de ser entregue direto ao
/// gateway, ou `None` quando a entrega direta serve.
///
/// Quem entrega direto é a cauda (resposta direta) ou a cabeça (remote-stream).
/// Cada um publica no `/health` a rota fixada que tem até cada gateway
/// (`gateway_route_kinds`). O anel devolve a resposta pela mesma conexão da ida,
/// então é preferível quando esse nó:
///
/// - não tem rota medida até este gateway, embora já tenha medido rotas até
///   outros (sem volta: um gateway atrás de NAT que só a ida alcança); ou
/// - só volta pelo túnel libp2p enquanto a ida até ele é toda direta (QUIC ou
///   HTTP) - o túnel costuma ser o caminho mais lento e frágil.
///
/// Sem dados do nó (ainda não sondado, plano ainda não medido) nada muda.
pub fn return_path_prefers_ring(
    chain: &[String],
    requester: &str,
    perf: &HashMap<String, PeerPerf>,
    requester_route_kinds: &HashMap<String, String>,
) -> Option<String> {
    let forward_kind = |index: usize| -> Option<&str> {
        if index == 0 {
            if chain[0] == requester {
                return Some("local");
            }
            return requester_route_kinds.get(&chain[0]).map(String::as_str);
        }
        perf.get(&chain[index - 1])
            .and_then(|p| p.route_kinds.get(&chain[index]))
            .map(String::as_str)
    };
    let deliverers = [chain.first(), chain.last()];
    for (position, deliverer) in deliverers.into_iter().enumerate() {
        let Some(deliverer) = deliverer else {
            continue;
        };
        if deliverer == requester {
            continue;
        }
        let Some(deliverer_perf) = perf.get(deliverer) else {
            continue;
        };
        let back = deliverer_perf
            .gateway_route_kinds
            .get(requester)
            .map(String::as_str);
        let short = deliverer.get(..12).unwrap_or(deliverer);
        match back {
            None if !deliverer_perf.gateway_route_kinds.is_empty() => {
                return Some(format!("{short} has no route back to this gateway"));
            }
            Some("tunnel") => {
                let upto = if position == 0 { 0 } else { chain.len() - 1 };
                let forward_direct = (0..=upto)
                    .all(|index| matches!(forward_kind(index), Some("quic" | "http" | "local")));
                if forward_direct {
                    return Some(format!(
                        "{short} only comes back through the libp2p tunnel and the outbound path is direct"
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// Os candidatos do modelo, na ordem em que o planejador os considera.
pub fn candidates<'a>(
    peers: &'a [PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
) -> Vec<&'a PeerInfo> {
    model_pipeline_candidates(peers, model_name, deploy_id)
}

/// Resolve uma cadeia de `node_id`s (por exemplo, a de um prefixo em cache)
/// contra os candidatos vivos. `None` se algum sumiu ou se a cadeia não cobre
/// `0..total_layers` em faixas contíguas.
pub fn resolve_chain<'a>(
    candidates: &[&'a PeerInfo],
    node_ids: &[String],
    total_layers: u32,
    preferred_head: Option<&str>,
) -> Option<Vec<&'a PeerInfo>> {
    let mut chain = Vec::with_capacity(node_ids.len());
    let mut pos = 0;
    for node_id in node_ids {
        let peer = *candidates.iter().find(|peer| peer.node_id == *node_id)?;
        if peer.start_layer != pos {
            return None;
        }
        pos = peer.end_layer + 1;
        chain.push(peer);
    }
    if pos != total_layers {
        return None;
    }
    if let Some(head) = preferred_head {
        if chain.first().map(|peer| peer.node_id.as_str()) != Some(head) {
            return None;
        }
    }
    Some(chain)
}

pub fn plan_for_chain(chain: &[&PeerInfo]) -> Option<PipelinePlan> {
    let steps = chain
        .iter()
        .map(|peer| step_for(peer, peer.start_layer))
        .collect::<Option<Vec<_>>>()?;
    Some(plan_from_steps(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(node_id: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.to_string(),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: "10.0.0.1:42777".into(),
            addr_candidates: Vec::new(),
            model: "m".into(),
            start_layer: start,
            end_layer: end,
            total_layers: 48,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: None,
            gateway_port: Some(8090),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    fn perf(prefill: f64, decode: f64, active: u32) -> PeerPerf {
        PeerPerf {
            prefill_ms_per_token: Some(prefill),
            decode_ms_per_step: Some(decode),
            active_generations: active,
            ..PeerPerf::default()
        }
    }

    fn chain_of(plan: &PipelinePlan) -> Vec<String> {
        plan.steps.iter().map(|step| step.node_id.clone()).collect()
    }

    /// Cabeça distante mas rápida vence a próxima e lenta quando termina antes.
    #[test]
    fn a_distant_fast_head_beats_a_near_slow_one() {
        let peers = vec![
            peer("near", 0, 27),
            peer("far", 0, 27),
            peer("tail", 28, 47),
        ];
        let rtt = HashMap::from([
            ("near".to_string(), 1.0),
            ("far".to_string(), 30.0),
            ("tail".to_string(), 20.0),
        ]);
        let perfs = HashMap::from([
            ("near".to_string(), perf(2.0, 80.0, 0)),
            ("far".to_string(), perf(0.5, 15.0, 0)),
            ("tail".to_string(), perf(1.0, 20.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 200.0,
            decode_tokens: 200.0,
            forced_return: None,
        };
        let (plan, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["far", "tail"]);
        assert!(estimate.total_ms > 0.0);
        // Pouca saída: a vantagem de rede da cabeça próxima volta a pesar.
        let short = CostInputs {
            decode_tokens: 0.0,
            prompt_tokens: 1.0,
            ..inputs
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &short, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["near", "tail"]);
    }

    /// A fila multiplica o custo: o peer mais rápido ocupado cede a vez ao
    /// médio ocioso, e a carga se espalha.
    #[test]
    fn a_busy_fast_peer_yields_to_an_idle_one() {
        let peers = vec![peer("fast", 0, 47), peer("slow", 0, 47)];
        let rtt = HashMap::from([("fast".to_string(), 5.0), ("slow".to_string(), 5.0)]);
        let idle = HashMap::from([
            ("fast".to_string(), perf(1.0, 20.0, 0)),
            ("slow".to_string(), perf(1.5, 30.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &idle,
            local_load: &load,
            prompt_tokens: 100.0,
            decode_tokens: 100.0,
            forced_return: None,
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["fast"]);

        let busy = HashMap::from([
            ("fast".to_string(), perf(1.0, 20.0, 2)),
            ("slow".to_string(), perf(1.5, 30.0, 0)),
        ]);
        let inputs = CostInputs {
            perf: &busy,
            ..inputs
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["slow"]);
    }

    /// As arestas entre peers entram na conta: o estágio do meio que fala
    /// rápido com a cabeça escolhida vence o que só é rápido para o gateway.
    #[test]
    fn inter_hop_latency_decides_between_equal_replicas() {
        let peers = vec![
            peer("head", 0, 23),
            peer("mid-a", 24, 47),
            peer("mid-b", 24, 47),
        ];
        let rtt = HashMap::from([
            ("head".to_string(), 5.0),
            ("mid-a".to_string(), 5.0),
            ("mid-b".to_string(), 40.0),
        ]);
        let mut head = perf(1.0, 10.0, 0);
        head.route_rtt_ms =
            HashMap::from([("mid-a".to_string(), 60.0), ("mid-b".to_string(), 2.0)]);
        let perfs = HashMap::from([
            ("head".to_string(), head),
            ("mid-a".to_string(), perf(1.0, 10.0, 0)),
            ("mid-b".to_string(), perf(1.0, 10.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 50.0,
            decode_tokens: 50.0,
            forced_return: None,
        };
        let (plan, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["head", "mid-b"]);
        assert_eq!(estimate.stages[1].rtt_in_ms, 2.0);
    }

    #[test]
    fn the_chosen_head_is_the_only_head_and_a_cached_chain_is_resolved() {
        let peers = vec![peer("a", 0, 47), peer("b", 0, 47)];
        let rtt = HashMap::new();
        let perfs = HashMap::from([
            ("a".to_string(), perf(1.0, 10.0, 0)),
            ("b".to_string(), perf(5.0, 50.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 1000.0,
            decode_tokens: 10.0,
            forced_return: None,
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, Some("b")).unwrap();
        assert_eq!(chain_of(&plan), vec!["b"]);
        assert!(plan_fastest(&peers, "m", None, 48, &inputs, Some("ghost")).is_none());

        // O prefixo em cache desconta o prefill: o peer lento com 990 tokens
        // já retidos passa a entregar antes.
        let all = candidates(&peers, "m", None);
        let chain = resolve_chain(&all, &["b".to_string()], 48, None).unwrap();
        let cached = estimate_chain(&chain, &inputs, &all, 990);
        let a_chain = resolve_chain(&all, &["a".to_string()], 48, None).unwrap();
        let cold = estimate_chain(&a_chain, &inputs, &all, 0);
        assert!(cached.total_ms < cold.total_ms);
        assert!(resolve_chain(&all, &["x".to_string()], 48, None).is_none());
    }

    fn perf_with_back(kind: Option<&str>, others: bool) -> PeerPerf {
        let mut perf = PeerPerf::default();
        if let Some(kind) = kind {
            perf.gateway_route_kinds
                .insert("gateway".into(), kind.into());
        }
        if others {
            perf.gateway_route_kinds
                .insert("other".into(), "http".into());
        }
        perf
    }

    #[test]
    fn the_answer_returns_by_the_ring_when_the_tail_cannot_reach_the_gateway() {
        let chain = vec!["head".to_string(), "tail".to_string()];
        let own = HashMap::from([("head".to_string(), "quic".to_string())]);
        let mut head = perf_with_back(Some("quic"), false);
        head.route_kinds.insert("tail".into(), "quic".into());

        // Sem rota de volta, tendo medido outras: anel.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(None, true)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_some());

        // Volta só pelo túnel com a ida toda em QUIC: anel.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(Some("tunnel"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_some());

        // Volta direta: entrega direta.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(Some("http"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());

        // A ida também é pelo túnel: o anel não ganha nada.
        let mut tunneled = head.clone();
        tunneled.route_kinds.insert("tail".into(), "tunnel".into());
        let perf = HashMap::from([
            ("head".to_string(), tunneled),
            ("tail".to_string(), perf_with_back(Some("tunnel"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());

        // Nó ainda sem medição nenhuma: nada muda.
        let perf = HashMap::from([
            ("head".to_string(), head),
            ("tail".to_string(), PeerPerf::default()),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());
    }

    /// O modo de volta sai do planejamento: o gateway é a cabeça (L0-15) e a
    /// cauda está noutra máquina. Ida em QUIC com 26 ms.
    #[test]
    fn the_return_mode_is_chosen_with_the_path() {
        let peers = vec![peer("gw", 0, 15), peer("tail", 16, 47)];
        let rtt = HashMap::from([("tail".to_string(), 26.0)]);
        let load = HashMap::new();
        let plan_with = |tail_back: HashMap<String, f64>| {
            let mut tail = perf(1.0, 10.0, 0);
            tail.gateway_rtt_ms = tail_back;
            let perfs = HashMap::from([
                ("gw".to_string(), perf(1.0, 10.0, 0)),
                ("tail".to_string(), tail),
            ]);
            let inputs = CostInputs {
                requester: "gw",
                requester_rtt_ms: &rtt,
                perf: &perfs,
                local_load: &load,
                prompt_tokens: 50.0,
                decode_tokens: 50.0,
                forced_return: None,
            };
            plan_fastest(&peers, "m", None, 48, &inputs, None)
                .unwrap()
                .1
                .return_mode
        };
        // Volta só pelo túnel, 48 ms: direta custaria 13 + 24 = 37 ms por
        // token, o anel 26 ms.
        assert_eq!(
            plan_with(HashMap::from([("gw".to_string(), 48.0)])),
            ReturnMode::Ring
        );
        // Cauda sem volta até este gateway (mediu só outro): anel.
        assert_eq!(
            plan_with(HashMap::from([("other".to_string(), 5.0)])),
            ReturnMode::Ring
        );
        // Volta rápida, 20 ms: direta custa 13 + 10 = 23 ms, abaixo do anel.
        assert_eq!(
            plan_with(HashMap::from([("gw".to_string(), 20.0)])),
            ReturnMode::Direct
        );
    }

    #[test]
    fn a_forced_return_mode_is_the_only_one_planned() {
        let peers = vec![peer("gw", 0, 15), peer("tail", 16, 47)];
        let rtt = HashMap::from([("tail".to_string(), 26.0)]);
        let load = HashMap::new();
        let mut tail = perf(1.0, 10.0, 0);
        tail.gateway_rtt_ms = HashMap::from([("gw".to_string(), 48.0)]);
        let perfs = HashMap::from([
            ("gw".to_string(), perf(1.0, 10.0, 0)),
            ("tail".to_string(), tail),
        ]);
        let inputs = CostInputs {
            requester: "gw",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 50.0,
            decode_tokens: 50.0,
            forced_return: Some(ReturnMode::Direct),
        };
        // Sozinho o planejador escolheria o anel; forçado, é a direta.
        let (_, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(estimate.return_mode, ReturnMode::Direct);
    }

    #[test]
    fn health_body_becomes_peer_perf() {
        let body = serde_json::json!({
            "active_generations": 2,
            "perf": {"prefill_ms_per_token": 0.8, "decode_ms_per_step": 24.5},
            "route_rtt_ms": {"peer-b": 26.0},
            "gateway_rtt_ms": {"peer-g": 3.5, "bad": -1},
            "relayed_peers": ["peer-g"],
        });
        let perf = PeerPerf::from_health(&body);
        assert_eq!(perf.active_generations, 2);
        assert_eq!(perf.decode_ms_per_step, Some(24.5));
        assert_eq!(perf.route_rtt_ms["peer-b"], 26.0);
        assert_eq!(perf.gateway_rtt_ms.len(), 1);
        assert!(perf.relayed_peers.contains("peer-g"));
    }
}
