use crate::auth::AuthState;
use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo, RuntimeCapacity};
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline_with_rtt, LoadTracker, PipelinePlan};
use crate::runtime::NodeRuntime;
use crate::{admin, session};
use anyhow::Result;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

/// Tokens `/api/generate` will produce before stopping on its own.
pub const DEFAULT_MAX_TOKENS: u32 = 512;
/// A peer that cannot answer its inference health endpoint is not eligible for
/// a new pipeline. UDP discovery is deliberately lightweight and may still
/// hear an isolated or old node, so it is not sufficient proof of a usable hop.
const PIPELINE_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Characters per token, used only to size up a prompt before sending it.
/// Deliberately pessimistic — the real tokenizer lives in the first peer's
/// llama.cpp context, and under-estimating here would let through the very
/// request this guard exists to stop.
const CHARS_PER_TOKEN: usize = 3;

/// Prazo do `/v1/session/rewind`. Curto de propósito: o evento final do stream
/// espera por ele (ver `generate_stream_inner`), e do outro lado é só apagar
/// uma sequência de KV.
const REWIND_TIMEOUT: Duration = Duration::from_secs(3);

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct ClusterSnapshot {
    pub(crate) fingerprint: String,
    pub(crate) body: serde_json::Value,
    pub(crate) at: Instant,
    pub(crate) version: u64,
    pub(crate) etag: String,
}

#[derive(Default)]
pub(crate) struct ClusterSnapshotCache {
    snapshot: Option<ClusterSnapshot>,
    next_version: u64,
}

impl ClusterSnapshotCache {
    pub(crate) fn get(
        &self,
        fingerprint: &str,
        now: Instant,
        ttl: Duration,
    ) -> Option<ClusterSnapshot> {
        let snapshot = self.snapshot.as_ref()?;
        let age = now.checked_duration_since(snapshot.at)?;
        (snapshot.fingerprint == fingerprint && age < ttl).then(|| snapshot.clone())
    }

    pub(crate) fn store(
        &mut self,
        fingerprint: impl Into<String>,
        body: serde_json::Value,
        etag: impl Into<String>,
        at: Instant,
    ) -> ClusterSnapshot {
        let fingerprint = fingerprint.into();
        let version = self
            .snapshot
            .as_ref()
            .filter(|previous| previous.body == body)
            .map(|previous| previous.version)
            .unwrap_or_else(|| {
                self.next_version = self.next_version.saturating_add(1);
                self.next_version
            });
        let snapshot = ClusterSnapshot {
            fingerprint,
            body,
            at,
            version,
            etag: etag.into(),
        };
        self.snapshot = Some(snapshot.clone());
        snapshot
    }
}

#[derive(Clone)]
pub struct GatewayState {
    pub network: Arc<Network>,
    pub ollama: Arc<OllamaClient>,
    pub default_model: String,
    /// Current assignment of the local node.
    pub local_assignment: Arc<tokio::sync::RwLock<Option<LayerAssignment>>>,
    /// Tracks in-flight inference requests per peer (for pipeline load balancing).
    pub loads: Arc<LoadTracker>,
    /// Prefixos de conversas que continuam vivos entre turnos. A entrada só
    /// guarda uma sessão KV já existente, nunca pesos ou estado oculto.
    pub prefix_cache: Arc<crate::prefix_cache::PrefixCache>,
    /// Clientes HTTP de saída, **um por peer**, com o certificado do destino
    /// fixado pelo node_id (`peer_client::PeerClients`).
    ///
    /// Aqui havia também um `http` compartilhado e sem pinning. Ele saiu de
    /// propósito: com o cluster em TLS ele não tem como verificar o certificado
    /// de ninguém, e todo ponto de chamada que o pegasse por hábito falhava
    /// calado - foi assim que a tela de implantar parou de enxergar as fatias
    /// dos peers. Para destino que não é peer (loopback, Ollama) use
    /// `peer_http.plain()`, que diz o que está fazendo.
    pub peer_http: Arc<crate::peer_client::PeerClients>,
    /// Mutable node configuration driven by the web UI.
    pub runtime: Arc<NodeRuntime>,
    /// Cycle policy used before a deployment exists. A loaded deployment's
    /// `NodeConfig` takes precedence, so this is also the CLI fallback for a
    /// gateway-only process.
    pub cycle_config: crate::cycle::CycleConfig,
    /// Durable audit trail for requests entering this gateway.
    pub audit: Arc<crate::reqlog::AuditLog>,
    /// HTTP port this node serves the API and UI on.
    pub gateway_port: u16,
    /// Setup-level configuration (role, connection key, bootstrap nodes).
    pub auth: AuthState,
    /// In-flight slice transfers and the offers waiting for an answer.
    pub transfers: crate::transfer::Transfers,
    /// Frames this node dispatched into a pipeline and is still waiting on.
    ///
    /// Only used by the direct-reply relay (`relay/v2-direct`), where the tail
    /// answers this process instead of the answer bubbling back through every
    /// hop — see [`crate::direct`].
    pub reply_inbox: crate::direct::ReplyInbox,
    /// Dispatches a hop-to-hop call over the libp2p swarm instead of plain
    /// HTTP, as a NAT-tunnel fallback — see `relay::send_frame_or_tunnel`.
    /// `None` whenever this node's own libp2p discovery never came up
    /// (disabled or failed to start); every prefill/decode call site treats
    /// that exactly like "the fallback wasn't available this time."
    pub tunnel_tx: Option<crate::infer_tunnel::TunnelOutboundSender>,
    /// Certificado deste nó e a política de texto claro, quando o binário
    /// conseguiu derivá-los da identidade. `None` só nos servidores de teste,
    /// que sobem em `127.0.0.1:0` e não têm par de chaves a apresentar.
    pub tls_server: Option<crate::serve::TlsServer>,
    /// Retrato curto do cluster, isolado por gateway e por instância de teste.
    pub(crate) cluster_cache: Arc<Mutex<ClusterSnapshotCache>>,
    /// Provider de modelos remotos e seus jobs de download.
    pub(crate) model_service: Arc<crate::model_provider_backend::ModelService>,
}

#[cfg(test)]
mod cluster_cache_tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn a_known_state_change_bypasses_the_ttl_and_advances_the_version() {
        let mut cache = ClusterSnapshotCache::default();
        let at = Instant::now();
        let first = cache.store("state-a", json!({ "peers": [] }), "etag-a", at);

        assert!(cache
            .get(
                "state-a",
                at + Duration::from_millis(50),
                Duration::from_secs(2)
            )
            .is_some());
        assert!(cache
            .get(
                "state-b",
                at + Duration::from_millis(50),
                Duration::from_secs(2)
            )
            .is_none());

        let second = cache.store("state-b", json!({ "peers": [{ "id": "b" }] }), "etag-b", at);
        assert!(second.version > first.version);
    }

    #[test]
    fn an_unchanged_snapshot_expires_after_the_short_ttl() {
        let mut cache = ClusterSnapshotCache::default();
        let at = Instant::now();
        cache.store("state-a", json!({ "peers": [] }), "etag-a", at);

        assert!(cache
            .get(
                "state-a",
                at + Duration::from_secs(2),
                Duration::from_secs(2)
            )
            .is_none());
    }
}

impl axum::extract::FromRef<GatewayState> for AuthState {
    fn from_ref(state: &GatewayState) -> Self {
        state.auth.clone()
    }
}

/// The connection key gives the operator authority over which remote identities
/// this node trusts. A blocked peer may keep broadcasting UDP announcements,
/// but it must not enter the dashboard, deployment survey, or inference plan.
fn peers_visible_to_cluster(peers: Vec<PeerInfo>, blocked: &[String]) -> Vec<PeerInfo> {
    peers
        .into_iter()
        .filter(|peer| !blocked.contains(&peer.node_id))
        .collect()
}

/// Keep only peers whose actual inference endpoint can answer now. This is a
/// per-request admission check: a stale UDP announcer must not poison the
/// layer count or leave a request waiting for a hop that no longer serves HTTP.
async fn ready_for_pipeline(
    http: &crate::peer_client::PeerClients,
    peers: Vec<PeerInfo>,
    timeout: Duration,
) -> Vec<PeerInfo> {
    futures::future::join_all(peers.into_iter().map(|peer| {
        // Um cliente por peer: a sonda tem que falar a mesma língua que a
        // inferência vai falar com aquele nó, senão ela mede - ou recusa - um
        // caminho que a requisição de verdade não percorre.
        let cliente = http.for_peer(&peer.node_id);
        async move {
            let Some(base) = peer.inference_url() else {
                return None;
            };
            let Ok(cliente) = cliente else {
                return None;
            };
            let url = format!("{base}/health");
            match tokio::time::timeout(timeout, cliente.get(url).send()).await {
                Ok(Ok(response)) if response.status().is_success() => Some(peer),
                _ => None,
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

impl GatewayState {
    /// Model label the pipeline planner matches on right now.
    pub async fn active_model(&self) -> String {
        self.runtime.model_label(&self.default_model).await
    }

    /// Where this node writes slices, keeps received files, and the only
    /// directory a peer may pull from. Read from the live configuration, so a
    /// change on the screen takes effect without a restart.
    pub async fn shard_dir(&self) -> std::path::PathBuf {
        self.auth.read().await.shard_dir_path()
    }

    /// Directories of whole models this node scans.
    pub async fn model_dirs(&self) -> Vec<std::path::PathBuf> {
        self.auth.read().await.model_dir_paths()
    }

    /// Largest prompt this node's inference server can take, in tokens.
    ///
    /// llama.cpp aborts the *whole process* on
    /// `GGML_ASSERT(n_tokens_all <= cparams.n_batch)`, so an oversized prompt
    /// does not fail one request — it kills the peer and every request in
    /// flight on it. Coding harnesses send multi-thousand-token system prompts,
    /// which makes that trivially reachable from the public API. Refusing early
    /// keeps the node up; the caller gets a message naming the flags to raise.
    pub async fn prompt_token_budget(&self) -> Option<u32> {
        Some(prompt_budget(&self.runtime.config().await?))
    }

    /// Endereços por onde um peer alcança **este** nó, na ordem de preferência.
    ///
    /// Mesmo insumo que o nó já anuncia de si (`self_dial_host` primeiro, os
    /// candidatos enumerados depois): é o que a cauda de um pipeline usa para
    /// devolver o resultado direto, e errar aqui é a diferença entre uma
    /// resposta e um prazo estourado.
    pub fn self_reply_hosts(&self) -> Vec<String> {
        let mut hosts = vec![self_dial_host(
            self.network.effective_host(),
            crate::pkarr_node::detect_local_ip(),
        )];
        hosts.extend(self.network.self_addr_candidates());
        hosts
    }

    /// Alive peers plus this node, so the local assignment participates in
    /// planning exactly like a remote one.
    pub async fn all_peers(&self) -> Vec<PeerInfo> {
        let blocked = self.auth.read().await.blocked_nodes.clone();
        let mut peers = peers_visible_to_cluster(self.network.alive_peers().await, &blocked);
        // This machine is always part of the cluster, model or no model. Hiding
        // it when there is no assignment made the whole machine list disappear
        // on a node configured through the web UI.
        {
            let local = self.local_assignment.read().await.clone();
            // Não é `127.0.0.1`: esta entrada viaja como está dentro de
            // `next_hops` para o repasse em anel, e um hop de outra máquina
            // discando "127.0.0.1" alcança a si mesmo, não a nós. O modo hub
            // discando a própria máquina pelo IP real em vez do loopback
            // continua funcionando (o servidor de inferência liga em
            // `0.0.0.0`), então não há troca a fazer aqui.
            //
            // E não é o IP da LAN quando existe `advertise_addr`: entre sites,
            // um `192.168.x.y` é tão inalcançável para o outro lado quanto o
            // loopback. O sintoma era mudo e assimétrico - com este nó de
            // cabeça o pipeline ia, porque o gateway falava consigo mesmo pela
            // LAN; com ele de sucessor, o hop remoto recebia um endereço
            // privado e a requisição morria sem chegar a lugar nenhum.
            // `effective_host` já vem com colchetes quando é IPv6. Pinado
            // pelo operador vence; sem pin, o endereço público atual da
            // interface vence o endereço antigo observado por `identify`.
            // O observado continua sendo fallback para NAT.
            let self_host = self_dial_host(
                self.network.effective_host(),
                crate::pkarr_node::detect_local_ip(),
            );
            peers.push(PeerInfo {
                deploy_id: local
                    .as_ref()
                    .map(|assignment| assignment.deploy_id.clone())
                    .unwrap_or_default(),
                node_id: self.network.node_id.clone(),
                tls: self.network.tls_enabled(),
                pubkey: self.network.pubkey.clone(),
                addr: format!("{}:{}", self_host, self.network.bind_addr.port()),
                addr_candidates: self.network.self_addr_candidates(),
                model: local.as_ref().map(|l| l.model.clone()).unwrap_or_default(),
                start_layer: local.as_ref().map(|l| l.start_layer).unwrap_or(0),
                end_layer: local.as_ref().map(|l| l.end_layer).unwrap_or(0),
                total_layers: local.as_ref().map(|l| l.total_layers).unwrap_or(0),
                memory_mb: local.as_ref().map(|l| l.memory_used_mb).unwrap_or(0.0),
                last_seen: chrono::Utc::now(),
                ollama_port: self.network.ollama_port,
                inference_port: self.network.inference_port,
                // Keep in sync with network.rs's `self_announce` and
                // infer_server.rs's health/announce builder, and with the
                // capability constants in routing/pipeline.rs.
                inference_frame_capabilities: {
                    let mut caps = vec![
                        crate::pipeline::CAP_FRAME_BATCH.into(),
                        crate::pipeline::CAP_FRAME_DTYPE_F8.into(),
                        crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into(),
                        "relay/v1-chain".into(),
                        "relay/v2-direct".into(),
                        "speculate/v1-lookup".into(),
                        "speculate/v1-mtp-sliced".into(),
                    ];
                    if self.network.mtp_ready() {
                        caps.push("speculate/v1-mtp".into());
                    }
                    if self.network.batching_ready() {
                        caps.push(crate::pipeline::CAP_FRAME_BATCH.into());
                    }
                    if self.network.libp2p_tunnel_ready() {
                        caps.push("transport/libp2p-tunnel-v1".into());
                    }
                    caps
                },
                gateway_port: Some(self.gateway_port),
                role: self.network.role.clone(),
                hostname: self.network.hostname.clone(),
                capabilities: local
                    .as_ref()
                    .map(|l| {
                        vec![routing_contract::ModelCapability {
                            model_id: l.model.clone(),
                            start_layer: l.start_layer,
                            end_layer: l.end_layer,
                            total_layers: Some(l.total_layers),
                            supports_inference: self.network.inference_port.is_some(),
                        }]
                    })
                    .unwrap_or_default(),
                connection_verified: false,
                disk_total_mb: 0,
                disk_free_mb: 0,
                api_keys_active: 0,
                libp2p_peer_id: self.network.libp2p_peer_id.clone(),
                tp: self.network.tp_identity(),
                runtime_capacity: local.and_then(|_| self.network.runtime_capacity()),
            });
        }
        peers
    }

    /// Capacidade deste processo. Depois que o `/health` local confirma o
    /// motor, usa o valor efetivo que o llama.cpp reportou; antes disso cai no
    /// alvo configurado para manter os testes e a validação local honestos.
    async fn local_runtime_capacity(&self) -> Option<RuntimeCapacity> {
        if let Some(capacity) = self.network.runtime_capacity() {
            return Some(capacity);
        }
        let runtime = self.runtime.config().await?;
        Some(RuntimeCapacity {
            n_ctx: runtime.n_ctx,
            max_new_tokens: runtime.max_new_tokens,
        })
    }

    /// Menor capacidade entre os peers vivos que podem atender o seletor.
    ///
    /// A geração atravessa todos os estágios do pipeline, então anunciar o
    /// maior peer seria uma mentira quando outro ainda está em 8192. Um peer
    /// sem anúncio de capacidade é um nó antigo ou ainda inicializando; nesse
    /// caso a configuração local continua sendo o fallback conservador só
    /// quando não há peer correspondente. Se há peer vivo sem anúncio, a
    /// requisição fica pendente: aceitar com a capacidade local poderia
    /// esconder um estágio antigo no pipeline.
    async fn runtime_capacity_for_model(
        &self,
        model: Option<&str>,
    ) -> (Option<RuntimeCapacity>, bool) {
        let Some(model) = model else {
            return (self.local_runtime_capacity().await, false);
        };
        let peers = self.all_peers().await;
        let (capacity, pending) = runtime_capacity_from_peers(&peers, model);
        if capacity.is_some() || pending {
            return (capacity, pending);
        }
        (self.local_runtime_capacity().await, false)
    }
}

/// Host que os outros nós devem discar para chegar a este.
///
/// A ordem importa e é a razão do defeito de 2026-08-13: o endereço anunciado
/// vem primeiro porque é o único que vale fora da LAN. Cair para o IP local é
/// correto num cluster de uma rede só, e o loopback é o último recurso, bom
/// apenas para um nó sozinho na própria máquina.
fn self_dial_host(advertised: Option<String>, local_ip: Option<String>) -> String {
    advertised
        .or(local_ip)
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// Reduz uma fotografia de peers para a capacidade do modelo selecionado.
/// Manter essa função sobre um slice permite que `/v1/models` use exatamente
/// o mesmo snapshot para descobrir IDs e publicar seus limites.
fn runtime_capacity_from_peers(peers: &[PeerInfo], model: &str) -> (Option<RuntimeCapacity>, bool) {
    let (label, deploy_id) = crate::deployment::split_selector(model);
    let matching: Vec<&PeerInfo> = peers
        .iter()
        .filter(|peer| {
            peer.is_alive()
                && peer.model == label
                && deploy_id.is_none_or(|id| peer.deploy_id == id)
        })
        .collect();
    let capacities: Vec<RuntimeCapacity> = matching
        .iter()
        .filter_map(|peer| peer.runtime_capacity)
        .collect();
    if matching.is_empty() {
        return (None, false);
    }
    if capacities.len() != matching.len() {
        return (None, true);
    }
    (
        capacities.into_iter().reduce(|a, b| RuntimeCapacity {
            n_ctx: a.n_ctx.min(b.n_ctx),
            max_new_tokens: a.max_new_tokens.min(b.max_new_tokens),
        }),
        false,
    )
}

/// Prompt tokens a peer running `cfg` can accept.
///
/// `n_ctx` is the context window for one request. `max_sessions` only controls
/// how many such windows the inference context must reserve at once.
///
/// `n_batch` deliberately does **not** appear here. It did while the prompt
/// crossed as a single `llama_batch`; since [`crate::prefill`] advances the
/// prompt in chunks, `n_batch` bounds one *chunk* — see
/// [`prompt_chunk_tokens`]. Leaving it in this formula capped every prompt at
/// 512 tokens no matter how large `--n-ctx` was, which is what a harness sending
/// a real conversation hits immediately.
pub fn prompt_budget(cfg: &crate::runtime::NodeConfig) -> u32 {
    prompt_budget_of(cfg.n_ctx, cfg.max_sessions, cfg.max_new_tokens)
}

/// The same figure from loose numbers, for a configuration that is still being
/// chosen and has no running node behind it yet — the deploy screen quotes it
/// while the operator is editing the knobs, and quoting it from anywhere but
/// here is how the two go out of step.
pub fn prompt_budget_of(n_ctx: u32, _max_sessions: u32, max_new_tokens: u32) -> u32 {
    // Room has to be left for the tokens we are about to generate.
    n_ctx.saturating_sub(max_new_tokens)
}

/// Tokens per prefill chunk — this is what `n_batch` actually constrains.
///
/// A chunk is decoded as one batch on the peer, so a configured chunk larger
/// than `n_batch` would abort llama.cpp exactly the way whole prompts used to.
pub fn prompt_chunk_tokens(cfg: &crate::runtime::NodeConfig) -> u32 {
    prompt_chunk_of(cfg.prefill_chunk_tokens, cfg.n_batch)
}

pub fn prompt_chunk_of(prefill_chunk_tokens: u32, n_batch: u32) -> u32 {
    prefill_chunk_tokens.max(1).min(n_batch.max(1))
}

/// Same clamp as `prompt_chunk_of`, for the same reason: a speculative batch
/// (`1 + drafted tokens`) is decoded as one batch on the head too, so it
/// can't exceed `n_batch` either. Trivially satisfied at the defaults (4 «
/// 512), but an operator's `speculative_k` shouldn't be trusted alone.
pub fn speculative_k_of(speculative_k: u32, n_batch: u32) -> u32 {
    speculative_k.max(1).min(n_batch.max(1))
}

// O protocolo antigo `/v1/infer` (tensor em base64, uma chamada por peer)
// saiu daqui: hoje o pipeline fala `/v1/prefill` e `/v1/step` com o
// enquadramento binario de `inference/wire.rs`, e o encadeamento dos hops
// mora em `inference/prefill.rs`.

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub model: Option<String>,
    pub prompt: String,
    /// Total completion-token budget. The first token is emitted by prefill
    /// but counts here, just as it does in the other HTTP APIs.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Option<StopInput>,
}

/// Os dois orçamentos de uma geração, que nunca são o mesmo número.
///
/// `leg` é o que **uma** geração de backend pode produzir - o `max_new_tokens`
/// da implantação, escolha do operador. `total` é o que a **requisição** pode
/// produzir. Esgotar a perna não encerra a resposta: o laço de continuação
/// abre a próxima e segue decodificando na mesma sessão e no mesmo fluxo SSE.
/// Confundir os dois é o defeito que este orçamento existe para impedir - era
/// exatamente ele que devolvia `finish_reason: "length"` ao cliente no meio de
/// uma chamada de ferramenta.
/// O `total` é **imposto e inviolável**: nada o ultrapassa, por motivo nenhum,
/// nem uma chamada de ferramenta aberta no token do corte. É a regra dos dois
/// provedores - `max_tokens` é teto por resposta, atingi-lo é parada terminal,
/// e a saída trunca no meio. O que existe no lugar do transbordo é o orçamento
/// de tarefa (`messages::TaskBudget`), que avisa o modelo em vez de esticar o
/// teto. Ver `docs/subsystems/inferencia.md` (9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    /// Teto da requisição inteira, somando todas as pernas.
    pub total: u32,
    /// Teto de uma perna.
    pub leg: u32,
}

impl OutputBudget {
    /// Um orçamento sem continuação: uma perna que já é a requisição inteira.
    /// É o que os testes e as bancadas usam quando o laço não é o assunto.
    pub fn single(total: u32) -> Self {
        Self { total, leg: total }
    }

    /// Números utilizáveis: nunca zero, e a perna nunca maior que o total.
    fn normalized(self) -> Self {
        let total = self.total.max(1);
        Self {
            total,
            leg: self.leg.clamp(1, total),
        }
    }
}

/// The output budget visible to a protocol adapter after applying the selected
/// deployment and the remaining context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OutputTokenLimit {
    #[serde(rename = "requested_max_tokens")]
    pub requested: u32,
    /// O que a requisição inteira vai poder gerar. É o total do orçamento.
    #[serde(rename = "effective_max_tokens")]
    pub effective: u32,
    /// `max_new_tokens` da implantação. **Não** limita mais a requisição, só
    /// uma perna do laço de continuação.
    #[serde(rename = "deployment_max_tokens")]
    pub deployment: Option<u32>,
    #[serde(rename = "context_available_tokens")]
    pub context_available: Option<u32>,
    /// Tokens que cabem numa perna: `min(deployment, effective)`.
    #[serde(rename = "leg_max_tokens")]
    pub leg: u32,
    /// Um modelo com peer vivo sem capacidade confirmada não pode usar o
    /// fallback local com segurança. O campo é interno porque a requisição é
    /// recusada antes de qualquer resposta `x_pom` ser gerada.
    #[serde(skip)]
    capacity_pending: bool,
}

impl OutputTokenLimit {
    pub fn was_clamped(self) -> bool {
        self.effective < self.requested
    }

    /// Os dois números que a geração precisa, sem o adaptador ter de escolher.
    pub fn budget(self) -> OutputBudget {
        OutputBudget {
            total: self.effective,
            leg: self.leg,
        }
    }

    /// Esta requisição pode precisar de mais de uma perna. Não é promessa de
    /// que vai precisar: o modelo pode parar antes.
    pub fn continuation_possible(self) -> bool {
        self.leg < self.effective
    }

    pub fn capacity_pending(self) -> bool {
        self.capacity_pending
    }

    pub fn rejection_reason(self) -> Option<String> {
        if self.capacity_pending {
            return Some(
                "a capacidade efetiva do modelo ainda não foi confirmada por todos os peers vivos"
                    .into(),
            );
        } else if self.effective > 0 && self.leg > 0 {
            return None;
        }

        if self.context_available == Some(0) {
            Some(
                "o prompt ocupa todo o contexto disponível; não há espaço para tokens de saída"
                    .into(),
            )
        } else if self.deployment == Some(0) {
            Some("max_new_tokens da implantação precisa ser maior que zero".into())
        } else {
            Some("não há tokens de saída disponíveis para esta requisição".into())
        }
    }
}

/// Resolve one request's output budget.
///
/// Dois números saem daqui, e é de propósito. O **total** da requisição é
/// `min(pedido, contexto que sobrou)`: o `max_new_tokens` da implantação
/// **não** entra no total, porque ele nunca foi limite do backend -
/// sempre foi um corte do gateway, e cortar aí calado é o defeito. O que ele
/// vira é o teto de **uma perna**, que o laço de continuação atravessa quantas
/// vezes o total permitir. Nenhum adaptador de protocolo duplica esta conta.
pub fn output_token_limit(
    requested: u32,
    deployment: Option<u32>,
    context_length: Option<u32>,
    prompt_tokens: u32,
) -> OutputTokenLimit {
    let requested = requested.max(1);
    let context_available = context_length.map(|context| context.saturating_sub(prompt_tokens));
    let effective = [Some(requested), context_available]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0);
    // A perna nunca passa do total: pedir 10 com implantação de 1000 continua
    // sendo uma perna de 10, e o laço nem abre.
    let leg = deployment.unwrap_or(effective).min(effective);

    OutputTokenLimit {
        requested,
        effective,
        deployment,
        context_available,
        leg,
        capacity_pending: false,
    }
}

impl GatewayState {
    /// Resolve an adapter request against the live deployment configuration
    /// and the remaining context reported by the active runtime.
    pub async fn request_output_limit(&self, requested: u32, prompt: &str) -> OutputTokenLimit {
        self.request_output_limit_for_model(None, requested, prompt)
            .await
    }

    /// Resolve the request against the capacity of every live peer serving the
    /// selected model. The deployment leg remains separate from the request's
    /// context-derived total.
    pub async fn request_output_limit_for_model(
        &self,
        model: Option<&str>,
        requested: u32,
        prompt: &str,
    ) -> OutputTokenLimit {
        let runtime = self.runtime.config().await;
        let (capacity, capacity_pending) = self.runtime_capacity_for_model(model).await;
        if capacity_pending {
            return OutputTokenLimit {
                requested: requested.max(1),
                effective: 0,
                deployment: None,
                context_available: None,
                leg: 0,
                capacity_pending: true,
            };
        }
        output_token_limit(
            requested,
            capacity
                .map(|current| current.max_new_tokens)
                .or_else(|| runtime.as_ref().map(|cfg| cfg.max_new_tokens)),
            capacity
                .map(|current| current.n_ctx)
                .or_else(|| runtime.as_ref().map(|cfg| cfg.n_ctx)),
            estimate_tokens(prompt),
        )
    }

    /// Metadados publicados por `/v1/models`, derivados da mesma capacidade
    /// que a validação de uma requisição usa.
    pub async fn advertised_output_limits(
        &self,
        model: Option<&str>,
    ) -> (Option<u32>, Option<u32>) {
        let peers = self.all_peers().await;
        self.advertised_output_limits_from_peers(&peers, model)
            .await
    }

    /// Variante que recebe a mesma fotografia usada para descobrir o
    /// catálogo. Assim um peer não pode desaparecer ou entrar em restart entre
    /// a decisão do ID e a publicação de `max_completion_tokens`.
    pub async fn advertised_output_limits_from_peers(
        &self,
        peers: &[PeerInfo],
        model: Option<&str>,
    ) -> (Option<u32>, Option<u32>) {
        let runtime = self.runtime.config().await;
        let (capacity, capacity_pending) = match model {
            Some(model) => runtime_capacity_from_peers(peers, model),
            None => (self.local_runtime_capacity().await, false),
        };
        if capacity_pending {
            return (None, None);
        }
        let context = capacity
            .map(|current| current.n_ctx)
            .or_else(|| runtime.as_ref().map(|cfg| cfg.n_ctx));
        (context, context)
    }
}

pub fn log_output_limit(route: &str, limit: OutputTokenLimit) {
    if limit.capacity_pending() {
        info!(
            route,
            "capacidade efetiva aguardando confirmação de todos os peers vivos"
        );
        return;
    }
    if limit.was_clamped() {
        info!(
            route,
            requested_max_tokens = limit.requested,
            effective_max_tokens = limit.effective,
            context_available_tokens = ?limit.context_available,
            "limite de saída reduzido pelo contexto"
        );
    }
    if limit.continuation_possible() {
        info!(
            route,
            effective_max_tokens = limit.effective,
            leg_max_tokens = limit.leg,
            deployment_max_tokens = ?limit.deployment,
            "saída maior que uma geração da implantação: o laço de continuação \
             costura as pernas no mesmo fluxo"
        );
    }
}

/// `/api/generate` valida apenas o orçamento pedido. A capacidade do runtime é
/// resolvida depois, a partir do contexto efetivo e da implantação ativa.
fn resolve_request_max_tokens(requested: Option<u32>) -> Result<u32, String> {
    let max_tokens = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens == 0 {
        return Err("max_tokens deve ser maior que zero".to_string());
    }
    Ok(max_tokens)
}

fn remaining_generation_steps(max_tokens: u32, tokens_generated: u32) -> u32 {
    max_tokens.saturating_sub(tokens_generated)
}

/// Decide whether a generation step has a terminal reason. The budget is
/// checked first because the backend may report EOS on the same step that
/// consumed the last permitted token; the public API must call that result
/// `length`, not a normal stop.
fn generation_finish_reason(
    max_tokens: u32,
    tokens_generated: u32,
    model_done: bool,
) -> Option<&'static str> {
    if tokens_generated >= max_tokens {
        Some("length")
    } else if model_done {
        Some("stop")
    } else {
        None
    }
}

/// Audit has a distinct state for a response cut by its effective output
/// budget. Transport failures remain `partial`, even when they happened to
/// produce the same amount of text.
fn audit_status_for_summary(summary: &GenSummary) -> &'static str {
    if summary.finish_reason.as_deref() == Some("length") {
        "incomplete"
    } else if summary.partial {
        "partial"
    } else {
        "ok"
    }
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "pipeline:<hop-chain>" | "local" | "peer:<node_id>" | "error"
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub timings: Option<GenerationTimings>,
    pub stop_sequence: Option<String>,
    /// Motivo terminal. `cycle` é uma interrupção explícita do detector de
    /// degenerescência, distinta de `stop` e do limite `length`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub partial: bool,
    pub trace: Option<crate::prefill::RequestTrace>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Serialize)]
pub struct PeersResponse {
    pub peers: Vec<PeerSummary>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct PeerSummary {
    pub node_id: String,
    pub addr: String,
    pub model: String,
    pub layers: String,
    pub ollama_url: Option<String>,
    pub inference_url: Option<String>,
    pub load: u32,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub ollama_available: bool,
    pub peer_count: usize,
    pub pipeline_peers: usize,
    pub default_model: String,
    /// Só fica presente quando o `/health` do servidor de inferência local
    /// confirmou o runtime desta geração.
    pub runtime_capacity: Option<RuntimeCapacity>,
}

// ── Streaming generation ─────────────────────────────────────────────────────

/// One event from a running generation.
#[derive(Debug, Clone)]
pub enum GenEvent {
    /// A newly decoded token's text.
    Token(String),
    /// Generation finished (possibly early — see `partial`).
    Done(GenSummary),
    /// Generation produced nothing usable.
    Failed(String),
}

/// Inputs shared by the blocking, streaming and logged generation adapters.
/// Keeping them together prevents each adapter from growing a subtly
/// different list of generation parameters.
pub(crate) struct GenerationInput {
    pub(crate) state: GatewayState,
    pub(crate) model: String,
    pub(crate) prompt: String,
    pub(crate) chat_template: bool,
    /// Os dois tetos desta requisição: o da perna e o do total. Ver
    /// [`OutputBudget`].
    pub(crate) budget: OutputBudget,
    pub(crate) stops: Vec<String>,
    /// Como o cache de prefixo reconhece que esta requisição continua a
    /// anterior. `None` usa o próprio prompt.
    ///
    /// O texto renderizado não serve como chave: `render_prompt` devolve uma
    /// mensagem de usuário solitária **crua** e rotula a partir da segunda, de
    /// modo que o prompt do turno 1 não é prefixo do prompt do turno 2 e a
    /// transição 1→2 de qualquer chat nunca acertava. A chave vem do array de
    /// mensagens, que só cresce: imune a template, a rótulo e a bloco de
    /// ferramentas. Ver `crate::openai::conversation_cache_key`.
    pub(crate) cache_key: Option<String>,
    /// The parsed HTTP request body, retained for audit without headers or
    /// credentials.
    pub(crate) request_details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub timings: GenerationTimings,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
    /// "stop" when a server-side stop sequence ended the generation.
    pub finish_reason: Option<String>,
    /// The exact stop sequence that was matched, when one ended the stream.
    pub stop_sequence: Option<String>,
    /// Quantas pernas do laço de continuação a resposta costurou. `1` é uma
    /// geração só - nenhuma costura. Maior que `1` quer dizer que a saída
    /// passou do `max_new_tokens` da implantação sem que ele fosse alterado, e
    /// é o que torna a costura inspecionável de fora.
    pub legs: u32,
    /// Detailed per-request observations. It never affects planning.
    pub trace: Option<crate::prefill::RequestTrace>,
}

/// Resumo de desempenho de uma requisição, agregado sobre o pipeline inteiro.
/// Cada taxa usa seu próprio relógio e conjunto de tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationTimings {
    pub ttft_ms: u64,
    pub prefill_ms: u64,
    pub prefill_tokens: u32,
    pub prefill_tokens_per_sec: Option<f64>,
    pub generation_ms: u64,
    pub generation_tokens: u32,
    pub generation_tokens_per_sec: Option<f64>,
    pub total_ms: u64,
    pub total_tokens_per_sec: f64,
}

/// OpenAI's `stop` accepts either one string or a list. Keeping this parser in
/// the gateway lets all HTTP adapters share the same matcher and keeps the
/// internal inference protocol unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopInput {
    One(String),
    Many(Vec<String>),
}

impl StopInput {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(stop) => vec![stop],
            Self::Many(stops) => stops,
        }
    }
}

/// Normalize request-provided stop sequences once at the gateway boundary.
/// Empty strings cannot delimit meaningful generated text, and duplicates do
/// not change matching semantics.
pub fn normalize_stop_sequences(stops: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for stop in stops {
        if stop.is_empty() || normalized.iter().any(|known| known == &stop) {
            continue;
        }
        normalized.push(stop);
    }
    normalized
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopEmission {
    pub text: String,
    pub matched: Option<String>,
}

/// Incremental stop matcher for already-valid UTF-8 output.
///
/// A suffix that could still become a stop sequence is retained between
/// token events. This is deliberately byte-indexed only after `char_indices`
/// has found a UTF-8 boundary, so a split multibyte character is never cut.
#[derive(Debug, Clone)]
pub struct StopMatcher {
    stops: Vec<String>,
    pending: String,
    matched: Option<String>,
}

impl StopMatcher {
    pub fn new(stops: impl IntoIterator<Item = String>) -> Self {
        Self {
            stops: normalize_stop_sequences(stops),
            pending: String::new(),
            matched: None,
        }
    }

    pub fn feed(&mut self, text: &str) -> StopEmission {
        if self.matched.is_some() {
            return StopEmission {
                text: String::new(),
                matched: self.matched.clone(),
            };
        }

        self.pending.push_str(text);
        if self.stops.is_empty() {
            return StopEmission {
                text: std::mem::take(&mut self.pending),
                matched: None,
            };
        }

        let mut match_at: Option<(usize, usize)> = None;
        let mut matched_stop = None;
        for (index, stop) in self.stops.iter().enumerate() {
            if let Some(found) = self.pending.find(stop) {
                if match_at.is_none_or(|current| (found, index) < current) {
                    match_at = Some((found, index));
                    matched_stop = Some(stop.clone());
                }
            }
        }

        if let Some((index, _)) = match_at {
            let emitted = self.pending[..index].to_string();
            self.pending.clear();
            self.matched = matched_stop.clone();
            return StopEmission {
                text: emitted,
                matched: matched_stop,
            };
        }

        let hold = self
            .pending
            .char_indices()
            .filter_map(|(index, _)| {
                let suffix = &self.pending[index..];
                self.stops
                    .iter()
                    .any(|stop| stop.starts_with(suffix))
                    .then_some(suffix.len())
            })
            .max()
            .unwrap_or(0);
        let emit_len = self.pending.len().saturating_sub(hold);
        let emitted = self.pending[..emit_len].to_string();
        self.pending.drain(..emit_len);
        StopEmission {
            text: emitted,
            matched: None,
        }
    }

    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopSend {
    Continue,
    Stopped(String),
    Disconnected,
}

async fn send_text_with_stop(
    tx: &mpsc::Sender<GenEvent>,
    matcher: &mut StopMatcher,
    text: &str,
) -> StopSend {
    let emission = matcher.feed(text);
    if !emission.text.is_empty() && tx.send(GenEvent::Token(emission.text)).await.is_err() {
        return StopSend::Disconnected;
    }
    match emission.matched {
        Some(stop) => StopSend::Stopped(stop),
        None => StopSend::Continue,
    }
}

async fn flush_stop_matcher(tx: &mpsc::Sender<GenEvent>, matcher: &mut StopMatcher) -> bool {
    let tail = matcher.finish();
    tail.is_empty() || tx.send(GenEvent::Token(tail)).await.is_ok()
}

fn rate_if_measurable(tokens: u32, milliseconds: u64) -> Option<f64> {
    (tokens > 0 && milliseconds > 0).then(|| tokens as f64 / (milliseconds as f64 / 1000.0))
}

/// Calcula taxas com relógios que não se misturam. `generation_ms` começa no
/// primeiro token e, portanto, não varia com o tamanho do prompt.
pub(crate) fn timing_metrics(
    total_ms: u64,
    ttft_ms: u64,
    prefill_ms: u64,
    prefill_tokens: u32,
    generation_tokens: u32,
) -> GenerationTimings {
    let generation_ms = total_ms.saturating_sub(ttft_ms);
    GenerationTimings {
        ttft_ms,
        prefill_ms,
        prefill_tokens,
        prefill_tokens_per_sec: rate_if_measurable(prefill_tokens, prefill_ms),
        generation_ms,
        generation_tokens,
        generation_tokens_per_sec: rate_if_measurable(generation_tokens, generation_ms),
        total_ms,
        total_tokens_per_sec: rate(generation_tokens, total_ms as f64 / 1000.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn summary_for(
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    mut trace: crate::prefill::RequestTrace,
    partial: bool,
    finish_reason: Option<String>,
    stop_sequence: Option<String>,
) -> GenSummary {
    let total_ms = request_started.elapsed().as_millis() as u64;
    let ttft_ms = first_token_at
        .saturating_duration_since(request_started)
        .as_millis() as u64;
    trace.output_tokens = tokens_generated;
    trace.complete = !partial;
    let timings = timing_metrics(
        total_ms,
        ttft_ms,
        trace.prefill_ms,
        trace.input_tokens.saturating_sub(trace.cached_tokens),
        tokens_generated,
    );
    GenSummary {
        source: format!(
            "pipeline:{}{}",
            plan.hop_chain(),
            if partial { " (partial)" } else { "" }
        ),
        tokens_generated,
        tokens_per_sec: timings.total_tokens_per_sec,
        duration_ms: timings.total_ms,
        timings,
        partial,
        finish_reason,
        stop_sequence,
        legs,
        trace: Some(trace),
    }
}

/// Contabiliza um token confirmado contra o orçamento de perna.
///
/// A perna é o teto de **uma** geração de backend (`max_new_tokens` da
/// implantação). Fechá-la não encerra a resposta: o laço de continuação abre a
/// seguinte e segue decodificando na mesma sessão de KV e no mesmo fluxo SSE,
/// então nada disso é observável do lado do cliente. Só o total encerra, e só
/// ele vira `finish_reason: "length"`.
fn note_leg_boundary(budget: OutputBudget, tokens_generated: u32, legs: &mut u32) {
    if tokens_generated.is_multiple_of(budget.leg) && tokens_generated < budget.total {
        *legs += 1;
        info!(
            legs = *legs,
            tokens_generated,
            leg_max_tokens = budget.leg,
            total_max_tokens = budget.total,
            "perna esgotada: continuando a mesma geração na perna seguinte"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_stopped_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
    stop_sequence: String,
) {
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        false,
        Some("stop".to_string()),
        Some(stop_sequence),
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

async fn send_length_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
) {
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        false,
        Some("length".to_string()),
        None,
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

async fn send_cycle_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
    cycle: crate::cycle::CycleMatch,
) {
    info!(
        tokens_generated,
        period_tokens = cycle.period,
        repetitions = cycle.repetitions,
        "ciclo de repetição detectado; encerrando a geração"
    );
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        true,
        Some("cycle".to_string()),
        None,
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

/// Run one generation, emitting each token as it is decoded.
///
/// This is the single generation path in the gateway: `/api/generate` collects
/// the events into one response, the OpenAI endpoints forward them as SSE.
/// The established path keeps one round trip through every peer per token;
/// `remote-stream` moves the decode loop to the remote head and reports
/// opportunistic batches through the callback.
pub async fn generate_stream_with_stops(input: GenerationInput, tx: mpsc::Sender<GenEvent>) {
    let cancellation = crate::prefill::Cancellation::new();
    let monitor_cancel = cancellation.clone();
    let monitor_tx = tx.clone();
    let monitor = tokio::spawn(async move {
        monitor_tx.closed().await;
        monitor_cancel.cancel();
    });

    generate_stream_inner(input, tx, cancellation).await;
    monitor.abort();
}

fn remote_stream_capable(
    plan: &PipelinePlan,
    local_node_id: &str,
    direct_reply_available: bool,
) -> bool {
    !plan.steps.is_empty()
        && plan.steps[0].node_id != local_node_id
        && plan.steps[0].supports(crate::pipeline::CAP_REMOTE_TOKEN_STREAM)
        && direct_reply_available
}

fn remote_stream_speculation(
    plan: &PipelinePlan,
    speculative_enabled: bool,
    mtp_enabled: bool,
    speculative_k: u32,
    speculative_m: u32,
    mtp_k: u32,
    draft_source: crate::prefill::DraftSource,
) -> Option<crate::infer_server::Speculate> {
    let sliced_mtp = mtp_enabled && plan.supports_mtp() && plan.steps.len() >= 2;
    if sliced_mtp {
        return Some(crate::infer_server::Speculate {
            k: mtp_k,
            m: speculative_m,
            source: crate::infer_server::DraftSource::Mtp,
            draft: None,
        });
    }
    (plan.steps.len() == 1 && plan.supports_speculation() && speculative_enabled).then(|| {
        crate::infer_server::Speculate {
            k: speculative_k,
            m: speculative_m,
            source: match draft_source {
                crate::prefill::DraftSource::Lookup => crate::infer_server::DraftSource::Lookup,
                crate::prefill::DraftSource::Mtp => crate::infer_server::DraftSource::Mtp,
            },
            draft: None,
        }
    })
}

fn sliced_mtp_disable_reason(plan: &PipelinePlan) -> &'static str {
    if !plan
        .steps
        .iter()
        .all(|step| step.supports(crate::pipeline::CAP_SPECULATE_MTP_SLICED))
        || !plan
            .steps
            .iter()
            .all(|step| step.supports(crate::pipeline::CAP_SPECULATE_LOOKUP))
    {
        crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
    } else if !plan
        .steps
        .last()
        .is_some_and(|step| step.supports(crate::pipeline::CAP_SPECULATE_MTP))
    {
        crate::infer_server::SPECULATION_REASON_MODEL_WITHOUT_MTP
    } else {
        crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
    }
}

fn sliced_mtp_round_continues(
    remote_stream_mode: bool,
    draft_sent: bool,
    step: &crate::prefill::SpeculativeStep,
) -> bool {
    if remote_stream_mode {
        step.speculation_active
    } else {
        !((draft_sent && !step.speculated) || (!draft_sent && step.next_draft.is_none()))
    }
}

async fn generate_stream_inner(
    input: GenerationInput,
    tx: mpsc::Sender<GenEvent>,
    cancellation: crate::prefill::Cancellation,
) {
    let request_started = Instant::now();
    let GenerationInput {
        state,
        model,
        prompt,
        chat_template,
        budget,
        stops,
        cache_key,
        request_details: _,
    } = input;
    let budget = budget.normalized();
    if cancellation.is_cancelled() {
        return;
    }
    // Size the prompt up before any peer sees it — see `prompt_token_budget`.
    if let Some(cfg) = state.runtime.config().await {
        if let Some(error) = prompt_budget_error(&cfg, &prompt) {
            let _ = tx.send(GenEvent::Failed(error)).await;
            return;
        }
    }

    let peers = ready_for_pipeline(
        &state.peer_http,
        state.all_peers().await,
        PIPELINE_HEALTH_TIMEOUT,
    )
    .await;

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
        .routes
        .snapshot()
        .into_iter()
        .map(|(node_id, route)| (node_id, route.rtt_ms))
        .collect::<std::collections::HashMap<_, _>>();
    let Some(mut plan) = plan_pipeline_with_rtt(
        &peers,
        model,
        deploy_id,
        total_layers,
        &state.loads,
        &route_rtt,
    )
    .await
    else {
        let _ = tx
            .send(GenEvent::Failed(format!(
                "Nenhum pipeline válido para o modelo '{model}' (verifique a cobertura de camadas 0..{total_layers})"
            )))
            .await;
        return;
    };

    // A rota e escolhida por quem vai discar, nao por quem planejou: o plano
    // pode ter vindo de uma maquina noutra rede, e so este no mediu os
    // candidatos. Resolver aqui, uma vez, fixa o endereco de cada hop para a
    // sessao inteira - o cache KV do outro lado e por sessao, e trocar de
    // endereco na metade e pior que a rota lenta.
    {
        let mut fixadas = std::collections::HashMap::new();
        for step in plan.steps.iter_mut() {
            step.inference_url =
                crate::pipeline::resolve_step_url(step, &state.network.routes, &mut fixadas);
        }
    }
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
    let cache_prompt = cache_key.unwrap_or_else(|| prompt.clone());
    let prefix_lease =
        state
            .prefix_cache
            .acquire(prefix_key, &cache_prompt, Uuid::new_v4().to_string());
    let session_id = prefix_lease.session_id().to_string();
    // A sessão não pode trocar de réplica depois que o primeiro prefill criou
    // o KV. O plano é fixado uma vez e esta afinidade é a referência explícita
    // usada para revisar qualquer transformação futura da rota.
    let route_affinity = crate::pipeline::RouteAffinity::from_plan(
        deploy_id.unwrap_or_default(),
        &session_id,
        &plan,
    );
    debug_assert!(route_affinity.matches_plan(&plan));
    let reuse_prefix = prefix_lease.is_hit();

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
            cfg.as_ref()
                .map(|c| c.prefill_chunk_tokens)
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
    let (tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    let pump = {
        let client_tx = client_tx.clone();
        tokio::spawn(async move {
            let mut terminal = None;
            while let Some(event) = inner_rx.recv().await {
                match event {
                    event @ (GenEvent::Done(_) | GenEvent::Failed(_)) => terminal = Some(event),
                    event => {
                        if client_tx.send(event).await.is_err() {
                            // Cliente foi embora. Fechar o lado de cá é o que
                            // faz o laço de geração perceber e cancelar.
                            break;
                        }
                    }
                }
            }
            terminal
        })
    };

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
                // 1ª recuperação: retomar do pedaço que falhou. Os peers refazem
                // o próprio KV a partir daquela posição; nada do prompt já
                // ingerido volta pela rede.
                if let Some(rp) = crate::prefill::resume_point(e) {
                    warn!(
                        "prefill falhou no pedaço {} ({e}); retomando da posição {}",
                        rp.from_chunk, rp.start_pos
                    );
                    prefill_result = prefill_attempt(reuse_prefix, Some(rp)).await;
                }
                // 2ª recuperação: refazer o prefill inteiro a frio, uma vez -
                // é o comportamento de antes destas funcionalidades. Cobre o
                // motor recorrente que recusa recuar o KV e a sessão que o peer
                // perdeu.
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
                dtype,
                &state.peer_http,
                &state.network.routes,
                direct.as_ref().expect("stream capability requires reply address"),
                state.tunnel_tx.as_ref(),
                speculate,
                &cancellation,
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
                if step.accept_count < step.batch_width {
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
                lease.commit(cache_prompt, retained_tokens);
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

/// Confirma em cada peer que a sessão ficou parada no fim do prompt, pronta
/// para o turno seguinte. Falha em um hop invalida a entrada inteira: manter só
/// parte da cadeia criaria um buraco silencioso.
///
/// Do lado do peer isto **não** recua nada - a geração correu numa sequência de
/// KV separada e some inteira (ver `infer_server::handle_rewind`). Por isso o
/// prazo aqui é curto: é escrituração, não inferência, e o evento final do
/// stream espera por ela.
async fn rewind_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    retained_tokens: u32,
    clients: &crate::peer_client::PeerClients,
) -> bool {
    let mut all_ok = true;
    for step in &plan.steps {
        let url = format!("{}/v1/session/rewind", step.inference_url);
        let Ok(http) = clients.for_peer(&step.node_id) else {
            all_ok = false;
            continue;
        };
        let request = http
            .post(&url)
            .json(&serde_json::json!({
                "session_id": session_id,
                "n_tokens": retained_tokens,
            }))
            .timeout(REWIND_TIMEOUT);
        match clients
            .authorize_inference_request(request)
            .await
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                match response.json::<serde_json::Value>().await {
                    Ok(body)
                        if body.get("rewound").and_then(|v| v.as_bool()) == Some(true)
                            && body.get("n_past").and_then(|v| v.as_u64())
                                == Some(retained_tokens as u64) => {}
                    Ok(body) => {
                        warn!("rewind recusado por {}: {}", step.node_id, body);
                        all_ok = false;
                    }
                    Err(error) => {
                        warn!("resposta ilegível no rewind de {}: {error}", step.node_id);
                        all_ok = false;
                    }
                }
            }
            Ok(response) => {
                warn!("rewind em {} respondeu {}", step.node_id, response.status());
                all_ok = false;
            }
            Err(error) => {
                warn!("rewind em {} falhou: {error}", step.node_id);
                all_ok = false;
            }
        }
    }
    all_ok
}

/// Hand every peer in the plan its KV slot back. Best effort: a peer that is
/// already gone has nothing to release, and failing here must not turn a good
/// answer into an error.
async fn release_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    clients: &crate::peer_client::PeerClients,
) {
    for step in &plan.steps {
        let url = format!("{}/v1/session/release", step.inference_url);
        let Ok(http) = clients.for_peer(&step.node_id) else {
            warn!("sem cliente para liberar sessão em {}", step.node_id);
            continue;
        };
        let request = http
            .post(&url)
            .json(&serde_json::json!({ "session_id": session_id }))
            .timeout(Duration::from_secs(10));
        match clients
            .authorize_inference_request(request)
            .await
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!(
                "release de sessão em {} respondeu {}",
                step.node_id,
                r.status()
            ),
            Err(e) => warn!("release de sessão em {} falhou: {e}", step.node_id),
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
async fn rollback_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    accept_count: u32,
    batch_width: u32,
    http: &crate::peer_client::PeerClients,
) -> bool {
    let mut all_ok = true;
    for step in &plan.steps {
        let url = format!("{}/v1/session/rollback", step.inference_url);
        let Ok(cliente) = http.for_peer(&step.node_id) else {
            warn!("rollback: sem cliente para {}", step.node_id);
            all_ok = false;
            continue;
        };
        let request = cliente
            .post(&url)
            .json(&serde_json::json!({
                "session_id": session_id,
                "accept_count": accept_count,
                "batch_width": batch_width,
            }))
            .timeout(Duration::from_secs(10));
        match http.authorize_inference_request(request).await.send().await {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(body) => {
                    if !body
                        .get("rolled_back")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        warn!(
                            "rollback de sessão em {} recusado: {}",
                            step.node_id,
                            body.get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("sem motivo informado")
                        );
                        all_ok = false;
                    }
                }
                Err(e) => {
                    warn!(
                        "rollback de sessão em {}: corpo ilegível: {e}",
                        step.node_id
                    );
                    all_ok = false;
                }
            },
            Ok(r) => {
                warn!(
                    "rollback de sessão em {} respondeu {}",
                    step.node_id,
                    r.status()
                );
                all_ok = false;
            }
            Err(e) => {
                warn!("rollback de sessão em {} falhou: {e}", step.node_id);
                all_ok = false;
            }
        }
    }
    all_ok
}

/// Pessimistic token count for a prompt, used only by the size guard.
fn estimate_tokens(prompt: &str) -> u32 {
    prompt.chars().count().div_ceil(CHARS_PER_TOKEN) as u32
}

pub fn prompt_budget_error(cfg: &crate::runtime::NodeConfig, prompt: &str) -> Option<String> {
    let budget = prompt_budget(cfg);
    let estimated = estimate_tokens(prompt);
    (estimated > budget).then(|| {
        format!(
            "Prompt grande demais: ~{estimated} tokens estimados contra um limite de \
             {budget}. Cada sessão recebe n_ctx ({}) menos max_new_tokens ({}). O prompt \
             vai em pedaços de {} tokens, então n_batch ({}) não limita o prompt. Aumente \
             n_ctx em Configurar → ajustes avançados, reduza max_new_tokens, ou reduza \
             as requisições simultâneas.",
            cfg.n_ctx,
            cfg.max_new_tokens,
            prompt_chunk_tokens(cfg),
            cfg.n_batch,
        )
    })
}

fn rate(tokens: u32, secs: f64) -> f64 {
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
    let entry_node_id = input.state.network.node_id.clone();
    let prompt_tokens = estimate_tokens(&input.prompt);
    let logged_model = input.model.clone();
    let logged_prompt = input.prompt.clone();
    let request_details = input.request_details.clone();
    let audit = input.state.audit.clone();
    let request_id = Uuid::new_v4().simple().to_string();
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

// ── Server startup ────────────────────────────────────────────────────────────

pub async fn start_gateway(state: GatewayState, port: u16) -> Result<()> {
    let warm = state.clone();
    let tls_server = state.tls_server.clone();
    let admin = admin::admin_routes().with_state(state.clone());
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .route(crate::tls_policy::TLS_POLICY_PATH, get(cluster_tls))
        .route("/handshake", post(session::handle_handshake))
        // Onde a cauda de um pipeline entrega o resultado. Sem autenticação de
        // chave por desenho: o hop que responde é um par de inferência, que
        // nunca teve a chave do operador. Quem autoriza é o ticket — um UUID v4
        // sorteado por quadro, que só a própria cadeia recebeu, e que deixa de
        // existir assim que o quadro fecha.
        .route(
            crate::direct::RELAY_RESULT_PATH,
            post(handle_relay_result).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)),
        )
        .route(
            crate::direct::REMOTE_STREAM_RESULT_PATH,
            post(handle_remote_stream_result)
                .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)),
        )
        .merge(crate::openai::routes())
        .merge(crate::messages::routes())
        .merge(crate::webui::routes())
        .merge(crate::model_provider_backend::routes())
        .merge(crate::deploy::routes())
        .merge(crate::transfer::routes())
        .merge(crate::devupdate::router())
        .merge(admin)
        // Genuinely last: this one ends in a `/*path` catch-all that hands
        // unknown paths to the client router, so anything registered after it
        // would never be reached.
        .merge(crate::webapp::routes())
        .with_state(state)
        // JSON responses and the static UI bundle compress well; the default
        // predicate already skips `text/event-stream`, so the SSE token
        // stream in openai::routes() is untouched — compressing it would
        // trade first-token latency for bytes on exactly the path that
        // cares most about the former. Peer-to-peer tensor frames never pass
        // through this router (see inference/infer_server.rs), so there is
        // no risk of compressing binary inference frames here.
        .layer(tower_http::compression::CompressionLayer::new());

    // Both startup paths (with a model and gateway-only) come through here, so
    // this is the one place that warms every node's catalog exactly once.
    crate::webui::warm_catalog(warm);

    let listener = tokio::net::TcpListener::bind(format!("[::]:{}", port)).await?;
    info!(
        "🌐 Interface e API em http://localhost:{port}  \
         — UI /  ·  OpenAI /v1/chat/completions  ·  Anthropic /v1/messages  ·  \
         POST /api/generate  ·  GET /peers /health"
    );
    // Nagle atrasa uma escrita pequena até o ACK da anterior. No SSE cada token
    // é uma escrita pequena, e a interação com o delayed-ACK do outro lado soma
    // dezenas de ms ao que o cliente sente. O `reqwest` já desliga isso no lado
    // cliente por padrão; o `axum::serve` deixava `tcp_nodelay` em `None`, então
    // o lado servidor ficava com Nagle ligado em todas as respostas. O
    // `tcp_nodelay` mudou de lugar, não sumiu: `serve` o aplica em cada conexão
    // aceita, junto com a decisão entre TLS e texto claro.
    crate::serve::serve(listener, app, tls_server).await?;
    Ok(())
}

// ── Route handlers ────────────────────────────────────────────────────────────

/// O que o principal responde à catraca dos outros nós.
///
/// Autenticada pela chave de conexão: é isso que impede um intruso de responder
/// pelo principal e rebaixar o cluster para texto claro. Ver `tls_policy`.
async fn cluster_tls(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "tls_required": state.auth.read().await.tls_required,
    }))
}

async fn handle_relay_result(State(state): State<GatewayState>, body: Bytes) -> impl IntoResponse {
    crate::direct::handle_relay_result(&state.reply_inbox, body)
}

async fn handle_remote_stream_result(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    crate::direct::handle_remote_stream_result(&state.reply_inbox, body).await
}

/// Accept raw bytes so callers don't need `Content-Type: application/json`.
async fn handle_generate(State(state): State<GatewayState>, body: Bytes) -> impl IntoResponse {
    let req: GenerateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: format!("Invalid JSON: {}", e),
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    timings: None,
                    stop_sequence: None,
                    finish_reason: None,
                    partial: false,
                    trace: None,
                }),
            )
                .into_response();
        }
    };
    let request_details = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));

    let model = match req.model.as_deref() {
        Some(m) => m.to_string(),
        None => state.active_model().await,
    };
    let requested_max_tokens = match resolve_request_max_tokens(req.max_tokens) {
        Ok(value) => value,
        Err(error) => {
            let error_for_audit = error.clone();
            let prompt_for_audit = req.prompt.clone();
            record_rejected(
                &state,
                "/api/generate",
                crate::reqlog::Caller::anonymous(),
                model.clone(),
                prompt_for_audit,
                request_details.clone(),
                error_for_audit,
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: error,
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    timings: None,
                    stop_sequence: None,
                    finish_reason: None,
                    partial: false,
                    trace: None,
                }),
            )
                .into_response();
        }
    };
    let stops = req.stop.map(StopInput::into_vec).unwrap_or_default();
    let output_limit = state
        .request_output_limit_for_model(Some(&model), requested_max_tokens, &req.prompt)
        .await;
    log_output_limit("/api/generate", output_limit);
    if let Some(error) = output_limit.rejection_reason() {
        record_rejected(
            &state,
            "/api/generate",
            crate::reqlog::Caller::anonymous(),
            model.clone(),
            req.prompt.clone(),
            request_details.clone(),
            error.clone(),
        );
        return (
            if output_limit.capacity_pending() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            },
            Json(GenerateResponse {
                response: error,
                source: "error".to_string(),
                tokens_generated: 0,
                tokens_per_sec: 0.0,
                duration_ms: 0,
                timings: None,
                stop_sequence: None,
                finish_reason: None,
                partial: false,
                trace: None,
            }),
        )
            .into_response();
    }

    info!(
        "📨 /api/generate model={} prompt_len={}",
        model,
        req.prompt.len()
    );

    // `/api/generate` has never been behind auth, so there is no credential to
    // attribute the request to — the log says exactly that instead of guessing.
    match generate_blocking_with_stops(
        "/api/generate",
        crate::reqlog::Caller::anonymous(),
        GenerationInput {
            state,
            model,
            prompt: req.prompt,
            chat_template: true,
            budget: output_limit.budget(),
            stops,
            request_details,
            // `/api/generate` recebe um prompt, não uma conversa: o prefixo é
            // o próprio texto.
            cache_key: None,
        },
    )
    .await
    {
        Ok((text, summary)) => {
            let cycle_interrupted = summary.finish_reason.as_deref() == Some("cycle");
            (
                StatusCode::OK,
                Json(GenerateResponse {
                    response: text,
                    source: summary.source,
                    tokens_generated: summary.tokens_generated,
                    tokens_per_sec: summary.tokens_per_sec,
                    duration_ms: summary.duration_ms,
                    timings: Some(summary.timings),
                    stop_sequence: summary.stop_sequence,
                    // Keep the established `/api/generate` shape for ordinary
                    // stops and transport-partial responses. These fields are
                    // the explicit public marker for the new cycle interruption.
                    finish_reason: cycle_interrupted.then(|| "cycle".to_string()),
                    partial: cycle_interrupted,
                    trace: summary.trace,
                }),
            )
        }
        .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(GenerateResponse {
                response: format!(
                    "Inference failed. Error: {e}. Start peers with --inference-port."
                ),
                source: "none".to_string(),
                tokens_generated: 0,
                tokens_per_sec: 0.0,
                duration_ms: 0,
                timings: None,
                stop_sequence: None,
                finish_reason: None,
                partial: false,
                trace: None,
            }),
        )
            .into_response(),
    }
}

async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let peers = state.all_peers().await;

    let mut summaries = Vec::new();
    for p in &peers {
        let load = state.loads.load(&p.node_id).await;
        summaries.push(PeerSummary {
            node_id: p.node_id[..p.node_id.len().min(12)].to_string(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            layers: p.layer_range(),
            ollama_url: p.ollama_url(),
            inference_url: p.inference_url(),
            load,
        });
    }
    let count = summaries.len();
    Json(PeersResponse {
        peers: summaries,
        count,
    })
}

async fn handle_health(State(state): State<GatewayState>) -> Json<HealthResponse> {
    let ollama_available = state.ollama.is_available().await;
    let peers = state.all_peers().await;

    let peer_count = peers.len();
    let pipeline_peers = peers.iter().filter(|p| p.inference_port.is_some()).count();
    Json(HealthResponse {
        status: "ok",
        ollama_available,
        peer_count,
        pipeline_peers,
        default_model: state.active_model().await,
        runtime_capacity: state.network.runtime_capacity(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn stop_matcher_holds_a_suffix_that_is_a_stop_prefix() {
        let mut matcher = StopMatcher::new(vec!["<END>".to_string()]);

        assert_eq!(
            matcher.feed("answer<").text,
            "answer",
            "the possible prefix must stay buffered"
        );
        assert_eq!(matcher.feed("EN").text, "");
        let matched = matcher.feed("D>ignored");
        assert_eq!(matched.text, "");
        assert_eq!(matched.matched.as_deref(), Some("<END>"));
        assert_eq!(matcher.feed("after").text, "");
    }

    #[test]
    fn stop_matcher_emits_text_before_a_stop_and_flushes_unmatched_suffix() {
        let mut matcher = StopMatcher::new(vec!["STOP".to_string()]);

        assert_eq!(matcher.feed("abcST").text, "abc");
        assert_eq!(matcher.finish(), "ST");

        let mut matcher = StopMatcher::new(vec!["STOP".to_string()]);
        assert_eq!(matcher.feed("abcST").text, "abc");
        assert_eq!(matcher.feed("ill").text, "STill");
        assert_eq!(matcher.finish(), "");
    }

    #[test]
    fn stop_matcher_preserves_utf8_boundaries_and_flushes_at_end() {
        let mut matcher = StopMatcher::new(vec!["fim".to_string()]);

        assert_eq!(matcher.feed("olá f").text, "olá ");
        assert_eq!(matcher.feed("oi").text, "foi");
        assert_eq!(matcher.finish(), "");

        let mut unicode = StopMatcher::new(vec!["終".to_string()]);
        assert_eq!(unicode.feed("pré").text, "pré");
        let emission = unicode.feed("終depois");
        assert_eq!(emission.text, "");
        assert_eq!(emission.matched.as_deref(), Some("終"));
    }

    #[test]
    fn stop_matcher_uses_the_earliest_match_and_normalizes_requests() {
        let mut matcher = StopMatcher::new(vec![
            "DONE".to_string(),
            "DONE".to_string(),
            "END".to_string(),
            String::new(),
        ]);

        let emission = matcher.feed("prefixENDsuffix");
        assert_eq!(emission.text, "prefix");
        assert_eq!(emission.matched.as_deref(), Some("END"));
    }

    #[test]
    fn openai_stop_input_accepts_one_string_or_a_list() {
        let one: StopInput = serde_json::from_str(r#""END""#).unwrap();
        let many: StopInput = serde_json::from_str(r#"["END","DONE"]"#).unwrap();

        assert_eq!(one.into_vec(), vec!["END"]);
        assert_eq!(many.into_vec(), vec!["END", "DONE"]);
    }

    /// O orçamento pedido pelo cliente é validado aqui; capacidade e contexto
    /// efetivos são resolvidos a partir do runtime ativo.
    #[test]
    fn api_generate_max_tokens_defaults_accepts_and_rejects_zero_only() {
        let absent: GenerateRequest = serde_json::from_str(r#"{"prompt":"oi"}"#).unwrap();
        let requested: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":64}"#).unwrap();
        let excessive: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":513}"#).unwrap();

        assert_eq!(resolve_request_max_tokens(absent.max_tokens), Ok(512));
        assert_eq!(resolve_request_max_tokens(requested.max_tokens), Ok(64));
        assert_eq!(resolve_request_max_tokens(excessive.max_tokens), Ok(513));
    }

    #[test]
    fn api_generate_max_tokens_rejects_zero_instead_of_silently_clamping() {
        assert_eq!(
            resolve_request_max_tokens(Some(0)),
            Err("max_tokens deve ser maior que zero".into())
        );
    }

    #[test]
    fn output_budget_counts_the_first_token_returned_by_prefill() {
        assert_eq!(remaining_generation_steps(64, 1), 63);
        assert_eq!(remaining_generation_steps(1, 1), 0);
        assert_eq!(remaining_generation_steps(64, 64), 0);
    }

    #[test]
    fn generation_finish_reason_prioritizes_the_effective_limit() {
        assert_eq!(generation_finish_reason(4000, 4000, true), Some("length"));
        assert_eq!(generation_finish_reason(4000, 3640, true), Some("stop"));
        assert_eq!(generation_finish_reason(4000, 3640, false), None);
    }

    /// Exercicio de ponta a ponta do caminho que um consumidor de
    /// `/api/generate` usa: o gateway transmite as fichas repetidas, para
    /// antes de pedir a próxima, libera a sessão e publica `cycle` em vez de
    /// amputar silenciosamente o orçamento.
    #[tokio::test]
    async fn api_generate_reports_and_stops_a_detected_cycle() {
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let script: std::sync::Arc<Vec<&'static str>> = std::sync::Arc::new(vec![
            "alpha ", "beta ", "alpha ", "beta ", "alpha ", "beta ",
        ]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/health",
                axum::routing::get(|| async {
                    axum::Json(json!({"status": "ok", "model_loaded": true}))
                }),
            )
            .route(
                "/v1/prefill",
                axum::routing::post(|| async {
                    let response = json!({
                        "done": false,
                        "more_chunks": false,
                        "n_prompt_tokens": 1,
                        "token_id": 1,
                        "text": "seed "
                    });
                    let bytes = crate::wire::encode_batch_single(
                        &response,
                        None,
                        crate::wire::WireDtype::F16,
                    )
                    .unwrap();
                    (
                        StatusCode::OK,
                        [("content-type", "application/octet-stream")],
                        bytes,
                    )
                        .into_response()
                }),
            )
            .route(
                "/v1/step",
                axum::routing::post({
                    let steps = steps.clone();
                    let script = script.clone();
                    move || {
                        let steps = steps.clone();
                        let script = script.clone();
                        async move {
                            let index = steps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let text = script.get(index).copied().unwrap_or("unexpected ");
                            let response = json!({
                                "done": false,
                                "token_id": index as u32 + 2,
                                "text": text
                            });
                            let bytes = crate::wire::encode_batch_single(
                                &response,
                                None,
                                crate::wire::WireDtype::F16,
                            )
                            .unwrap();
                            (
                                StatusCode::OK,
                                [("content-type", "application/octet-stream")],
                                bytes,
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/session/release",
                axum::routing::post(|| async { StatusCode::OK }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut peer = crate::gateway::testkit::peer("head", "head", 8080);
        peer.model = "lab".into();
        peer.start_layer = 0;
        peer.end_layer = 0;
        peer.total_layers = 1;
        peer.inference_port = Some(port);
        let state = crate::gateway::testkit::state_with("eu", vec![peer]).await;
        let mut config = crate::gateway::testkit::spec_config();
        config.total_layers = 1;
        config.end_layer = 0;
        config.peer_count = 1;
        config.speculative_decoding_enabled = false;
        config.cycle_detection_min_window_tokens = 2;
        config.cycle_detection_max_window_tokens = 2;
        config.cycle_detection_repetitions = 3;
        state.runtime.seed_config_for_test(config).await;

        let request = serde_json::to_vec(&GenerateRequest {
            model: Some("lab".into()),
            prompt: "Oi".into(),
            max_tokens: Some(32),
            stop: None,
        })
        .unwrap();
        let response = handle_generate(State(state), axum::body::Bytes::from(request))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["response"], "seed alpha beta alpha beta alpha beta ");
        assert_eq!(body["tokens_generated"], 7);
        assert_eq!(body["finish_reason"], "cycle");
        assert_eq!(body["partial"], true);
        assert_eq!(body["stop_sequence"], Value::Null);
        assert_eq!(steps.load(std::sync::atomic::Ordering::SeqCst), 6);
    }

    #[test]
    fn normal_api_generate_responses_keep_the_existing_shape() {
        let response = GenerateResponse {
            response: "ok".into(),
            source: "pipeline:head".into(),
            tokens_generated: 1,
            tokens_per_sec: 1.0,
            duration_ms: 1,
            timings: None,
            stop_sequence: None,
            finish_reason: None,
            partial: false,
            trace: None,
        };
        let value = serde_json::to_value(response).unwrap();
        assert!(!value.as_object().unwrap().contains_key("finish_reason"));
        assert!(!value.as_object().unwrap().contains_key("partial"));
    }

    #[test]
    fn audit_status_distinguishes_length_from_transport_partial() {
        let summary = |finish_reason: Option<&str>, partial: bool| GenSummary {
            source: "pipeline:node-a".into(),
            tokens_generated: 4000,
            tokens_per_sec: 1.0,
            duration_ms: 1,
            timings: timing_metrics(1, 1, 1, 1, 4000),
            partial,
            finish_reason: finish_reason.map(str::to_owned),
            stop_sequence: None,
            legs: 1,
            trace: None,
        };

        assert_eq!(
            audit_status_for_summary(&summary(Some("length"), false)),
            "incomplete"
        );
        assert_eq!(audit_status_for_summary(&summary(None, true)), "partial");
        assert_eq!(
            audit_status_for_summary(&summary(Some("stop"), false)),
            "ok"
        );
    }

    /// O endereco anunciado tem que ganhar do IP da LAN. Esta entrada viaja
    /// dentro de `next_hops`, e um hop noutro site que receba `192.168.x.y`
    /// disca um endereco que nao existe para ele - a requisicao morre sem
    /// chegar a lugar nenhum, e so quando este no NAO e a cabeca do pipeline.
    #[test]
    fn an_advertised_address_beats_the_lan_ip_for_the_self_entry() {
        assert_eq!(
            self_dial_host(
                Some("[2804:d55:5579:1000:89f3:3f94:7c07:e183]".into()),
                Some("192.168.100.138".into()),
            ),
            "[2804:d55:5579:1000:89f3:3f94:7c07:e183]",
        );
    }

    /// Sem endereco anunciado o IP da LAN continua valendo: num cluster de uma
    /// rede so ele e alcancavel, e o loopback nao seria.
    #[test]
    fn without_an_advertised_address_the_lan_ip_is_kept() {
        assert_eq!(
            self_dial_host(None, Some("192.168.100.138".into())),
            "192.168.100.138",
        );
        assert_eq!(self_dial_host(None, None), "127.0.0.1");
    }

    /// The self-entry `.addr` uses the same address precedence as Announce:
    /// explicit pin, current public local address, then peer observation.
    #[tokio::test]
    async fn the_self_entry_address_picks_up_an_observed_public_address() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.network.set_observed_addr("203.0.113.5:9999");

        let peers = state.all_peers().await;
        let me = peers
            .iter()
            .find(|p| p.node_id == "eu")
            .expect("self entry");

        let expected_host = state
            .network
            .effective_host()
            .expect("self entry has an effective host");
        assert!(
            me.addr.starts_with(&format!("{expected_host}:")),
            "self entry addr should use the effective host, got {}",
            me.addr
        );
    }

    /// Live regression for the captain's Gemma4 request. Set
    /// `POM_FTK_E2E_URL` to a gateway before running this ignored test; it is
    /// deliberately not part of the default suite because it consumes a real
    /// model and GPU time.
    #[tokio::test]
    #[ignore = "requires a live Gemma4 MoE gateway"]
    async fn gemma4_hello_world_request_uses_moe_and_stops_on_its_turn() {
        let base = std::env::var("POM_FTK_E2E_URL")
            .expect("POM_FTK_E2E_URL must point at the live gateway");
        let model = std::env::var("POM_FTK_E2E_MODEL")
            .unwrap_or_else(|_| "Gemma4-26b-NVFP4.gguf".to_string());
        let response = reqwest::Client::new()
            .post(format!("{base}/api/generate"))
            .json(&json!({
                "model": model,
                "prompt": "Escreva hello world 10 vezes.",
                "max_tokens": 64
            }))
            .send()
            .await
            .expect("gateway request");
        assert!(
            response.status().is_success(),
            "gateway returned {}",
            response.status()
        );
        let body: Value = response.json().await.expect("JSON generation response");

        assert!(body["tokens_generated"].as_u64().unwrap_or(513) <= 64);
        assert!(body["trace"]["input_tokens"].as_u64().unwrap_or(0) > 0);
        assert!(body["trace"]["complete"].as_bool().unwrap_or(false));
        for phase in ["prefill", "decode"] {
            let phase = &body["trace"]["hops"][0][phase];
            assert_eq!(phase["executed_engine"], "moe");
            assert_eq!(phase["engine_mismatch"], false);
        }
        let text = body["response"].as_str().unwrap_or_default();
        for marker in ["<|im_start|>", "<|im_end|>", "<|channel>", "<channel|>"] {
            assert!(
                !text.contains(marker),
                "raw template marker leaked: {marker}"
            );
        }
    }

    #[test]
    fn token_estimate_is_pessimistic() {
        // 3 chars per token errs high, which is what a crash guard needs.
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(3000)), 1000);
    }

    #[test]
    fn token_estimate_counts_characters_not_bytes() {
        // Accented Portuguese must not inflate the count through UTF-8 bytes.
        assert_eq!(estimate_tokens("ção"), 1);
    }

    #[test]
    fn rate_is_zero_for_an_instant_generation() {
        assert_eq!(rate(10, 0.0), 0.0);
        assert!((rate(10, 2.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn timing_metrics_separates_normal_prefill_from_generation_and_total() {
        let timings = timing_metrics(2_000, 1_200, 1_000, 500, 20);

        assert_eq!(timings.ttft_ms, 1_200);
        assert_eq!(timings.prefill_ms, 1_000);
        assert_eq!(timings.prefill_tokens, 500);
        assert_eq!(timings.generation_ms, 800);
        assert_eq!(timings.generation_tokens, 20);
        assert_eq!(timings.prefill_tokens_per_sec, Some(500.0));
        assert_eq!(timings.generation_tokens_per_sec, Some(25.0));
        assert_eq!(timings.total_ms, 2_000);
        assert_eq!(timings.total_tokens_per_sec, 10.0);
    }

    #[test]
    fn timing_metrics_does_not_divide_by_zero_after_full_prefix_reuse() {
        let timings = timing_metrics(1_000, 400, 0, 0, 12);

        assert_eq!(timings.prefill_tokens_per_sec, None);
        assert_eq!(timings.generation_ms, 600);
        assert_eq!(timings.generation_tokens_per_sec, Some(20.0));
    }

    #[test]
    fn timing_metrics_uses_the_aggregate_prefill_for_multiple_chunks() {
        let timings = timing_metrics(3_500, 2_000, 1_800, 900, 30);

        assert_eq!(timings.prefill_tokens, 900);
        assert_eq!(timings.prefill_ms, 1_800);
        assert_eq!(timings.prefill_tokens_per_sec, Some(500.0));
    }

    #[test]
    fn timing_metrics_keeps_a_single_output_token_unmeasurable_when_instant() {
        let timings = timing_metrics(900, 900, 700, 100, 1);

        assert_eq!(timings.generation_ms, 0);
        assert_eq!(timings.generation_tokens_per_sec, None);
    }

    #[test]
    fn generation_speed_does_not_change_with_prompt_size() {
        let short_prompt = timing_metrics(1_000, 700, 650, 20, 14);
        let long_prompt = timing_metrics(2_000, 1_700, 1_650, 2_000, 14);

        assert_eq!(short_prompt.generation_ms, long_prompt.generation_ms);
        assert_eq!(
            short_prompt.generation_tokens_per_sec,
            long_prompt.generation_tokens_per_sec
        );
        assert_ne!(short_prompt.prefill_tokens, long_prompt.prefill_tokens);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::runtime::NodeConfig;

    /// A reprodução do defeito, em forma de conta: a implantação **não** pode
    /// mais decidir quanto a requisição inteira gera. Era exatamente isto que
    /// devolvia 1000 tokens a quem pediu 4000, calado.
    #[test]
    fn the_deployment_ceiling_bounds_a_leg_and_no_longer_bounds_the_request() {
        let limit = output_token_limit(4_000, Some(1_000), Some(200_000), 18);
        assert_eq!(limit.requested, 4_000);
        assert_eq!(limit.deployment, Some(1_000));
        assert_eq!(limit.effective, 4_000, "o cliente recebe o que pediu");
        assert_eq!(limit.leg, 1_000, "uma geração de backend continua em 1000");
        assert!(!limit.was_clamped());
        assert!(limit.continuation_possible());
        assert_eq!(limit.effective.div_ceil(limit.leg), 4, "quatro pernas");
    }

    /// O contexto que sobrou continua apertando sem recusar - ele sai de uma
    /// **estimativa** do prompt, e recusar por estimativa rejeitaria pedido
    /// válido. O `x_pom` conta a verdade (`was_clamped`), que é o canal certo
    /// para um número aproximado.
    #[test]
    fn the_remaining_context_still_clamps_because_it_is_an_estimate() {
        let limit = output_token_limit(50_000, Some(32_768), Some(40_000), 39_000);
        assert_eq!(limit.context_available, Some(1_000));
        assert_eq!(limit.effective, 1_000);
        assert_eq!(limit.leg, 1_000, "a perna nunca passa do total");
        assert!(!limit.continuation_possible());
        assert!(limit.was_clamped());
    }

    #[test]
    fn requested_client_limit_is_effective_when_it_fits_under_every_ceiling() {
        let limit = output_token_limit(4_000, Some(32_768), Some(100_000), 2_000);
        assert_eq!(limit.requested, 4_000);
        assert_eq!(limit.effective, 4_000);
        assert_eq!(limit.leg, 4_000);
        assert!(!limit.continuation_possible());
        assert!(!limit.was_clamped());
    }

    #[test]
    fn output_limit_preserves_the_requested_value_when_no_deployment_is_loaded() {
        let limit = output_token_limit(8_192, None, None, 0);
        assert_eq!(limit.effective, 8_192);
        assert_eq!(limit.leg, 8_192);
        assert!(!limit.was_clamped());
    }

    #[test]
    fn output_limit_contract_has_no_removed_absolute_capacity_field() {
        let limit = output_token_limit(32_000, Some(32_000), Some(100_000), 0);
        let value = serde_json::to_value(limit).unwrap();
        let fields = value.as_object().expect("limites de saída serializados");
        let legacy_field = ["absolute", "max", "tokens"].join("_");

        assert_eq!(fields.len(), 5);
        assert!(!fields.contains_key(&legacy_field));
    }

    #[test]
    fn output_limit_reports_no_capacity_when_the_prompt_fills_the_context() {
        let limit = output_token_limit(32_768, Some(32_768), Some(10_000), 10_000);
        assert_eq!(limit.effective, 0);
        assert!(limit.was_clamped());
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("todo o contexto"));
    }

    #[test]
    fn a_deployment_that_cannot_generate_is_still_a_refusal_not_a_silent_cut() {
        let limit = output_token_limit(4_000, Some(0), Some(100_000), 10);
        assert_eq!(limit.leg, 0);
        assert!(limit.rejection_reason().unwrap().contains("max_new_tokens"));
    }

    #[test]
    fn an_unconfirmed_peer_capacity_is_not_replaced_by_local_fallback() {
        let limit = OutputTokenLimit {
            requested: 32_000,
            effective: 0,
            deployment: None,
            context_available: None,
            leg: 0,
            capacity_pending: true,
        };
        assert!(limit.capacity_pending());
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("todos os peers vivos"));
    }

    #[tokio::test]
    async fn a_live_peer_without_capacity_blocks_model_specific_requests() {
        let mut peer = crate::gateway::testkit::peer("n1", "n1", 8080);
        peer.model = "qwen".into();
        peer.runtime_capacity = None;
        let state = crate::gateway::testkit::state_with("self", vec![peer]).await;

        let limit = state
            .request_output_limit_for_model(Some("qwen"), 32_000, "oi")
            .await;

        assert!(limit.capacity_pending());
        assert_eq!(limit.effective, 0);
        assert!(limit
            .rejection_reason()
            .unwrap()
            .contains("confirmada por todos"));
    }

    /// O laço tem que ficar dentro do orçamento total mesmo quando a perna não
    /// divide o total, e a perna nunca pode ser zero (divisão por zero no
    /// `note_leg_boundary`).
    #[test]
    fn the_budget_normalizes_to_usable_numbers() {
        assert_eq!(
            OutputBudget { total: 0, leg: 0 }.normalized(),
            OutputBudget { total: 1, leg: 1 }
        );
        assert_eq!(
            OutputBudget {
                total: 10,
                leg: 1_000
            }
            .normalized(),
            OutputBudget { total: 10, leg: 10 }
        );
        assert_eq!(
            OutputBudget::single(7).normalized(),
            OutputBudget { total: 7, leg: 7 }
        );
    }

    /// A contagem de pernas é o que torna a costura inspecionável. Ela avança
    /// ao fechar a perna, e **não** avança na última - `legs` é quantas
    /// pernas a resposta usou, não quantas caberiam.
    #[test]
    fn legs_are_counted_as_the_loop_crosses_each_deployment_budget() {
        let budget = OutputBudget { total: 10, leg: 4 };
        let mut legs = 1;
        let mut observed = Vec::new();
        for tokens in 1..=10u32 {
            note_leg_boundary(budget, tokens, &mut legs);
            observed.push(legs);
        }
        assert_eq!(observed, vec![1, 1, 1, 2, 2, 2, 2, 3, 3, 3]);
        assert_eq!(legs, 3);
    }

    #[test]
    fn a_generation_that_fits_one_leg_reports_a_single_leg() {
        let budget = OutputBudget {
            total: 100,
            leg: 100,
        };
        let mut legs = 1;
        for tokens in 1..=100u32 {
            note_leg_boundary(budget, tokens, &mut legs);
        }
        assert_eq!(legs, 1, "acabar o total não abre perna nenhuma");
    }

    fn cfg(n_ctx: u32, n_batch: u32, max_new: u32, max_sessions: u32) -> NodeConfig {
        NodeConfig {
            deploy_id: String::new(),
            model_path: String::new(),
            model_label: String::new(),
            deployment_name: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            inference_mode: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch,
            max_sessions,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            prefill_max_inflight_chunks:
                crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32,
            max_new_tokens: max_new,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            cycle_detection_enabled: true,
            cycle_detection_min_window_tokens: crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS,
            cycle_detection_max_window_tokens: crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS,
            cycle_detection_repetitions: crate::cycle::DEFAULT_CYCLE_REPETITIONS,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            tp: None,
            speculative_k: 4,
            speculative_lookup_window: 3,
        }
    }

    /// The whole point of the fix: one session means the configured context is
    /// the window, instead of an eighth of it.
    #[test]
    fn a_single_session_gets_the_whole_context() {
        let c = cfg(262_144, 262_144, 1024, 1);
        assert_eq!(prompt_budget(&c), 262_144 - 1024);
    }

    #[test]
    fn each_session_keeps_the_configured_window() {
        let c = cfg(262_144, 262_144, 1024, 8);
        assert_eq!(prompt_budget(&c), 262_144 - 1024);
    }

    /// The bug a harness hits on its first real conversation: with the prompt
    /// travelling in chunks, a small `n_batch` must not cap the prompt. The Pi
    /// sent ~23k tokens into a 32k window and was refused against a limit of
    /// 512, which was `n_batch`.
    #[test]
    fn n_batch_no_longer_caps_the_prompt() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_budget(&c), 32_768 - 1024);
    }

    /// `n_batch` did not stop mattering — it moved to the chunk, which is what
    /// llama.cpp actually decodes in one go.
    #[test]
    fn n_batch_caps_the_chunk_instead() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_chunk_tokens(&c), 512);

        let wide = cfg(32_768, 4096, 1024, 1);
        assert_eq!(
            prompt_chunk_tokens(&wide),
            crate::prefill::DEFAULT_CHUNK_TOKENS
        );
    }

    #[test]
    fn zero_sessions_does_not_divide_by_zero() {
        let c = cfg(8192, 8192, 256, 0);
        assert_eq!(prompt_budget(&c), 8192 - 256);
    }
}

#[cfg(test)]
mod remote_stream_selection_tests {
    use super::*;

    fn mtp_capabilities() -> Vec<String> {
        vec![
            crate::pipeline::CAP_SPECULATE_LOOKUP.into(),
            crate::pipeline::CAP_SPECULATE_MTP_SLICED.into(),
            crate::pipeline::CAP_SPECULATE_MTP.into(),
        ]
    }

    fn plan(node_id: &str, capabilities: Vec<String>) -> PipelinePlan {
        PipelinePlan {
            steps: vec![crate::pipeline::PipelineStep {
                node_id: node_id.into(),
                inference_url: "http://127.0.0.1:8181".into(),
                inference_candidates: Vec::new(),
                start_layer: 0,
                end_layer: 31,
                exec_start: 0,
                exec_end: 31,
                inference_frame_capabilities: capabilities,
                libp2p_peer_id: None,
                libp2p_addr_candidates: Vec::new(),
            }],
            edges: Vec::new(),
        }
    }

    #[test]
    fn remote_stream_never_replaces_the_local_shortcut() {
        let plan = plan(
            "local",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        assert!(!remote_stream_capable(&plan, "local", true));
    }

    #[test]
    fn remote_stream_requires_the_new_capability_and_a_callback() {
        let without_cap = plan("remote", Vec::new());
        assert!(!remote_stream_capable(&without_cap, "local", true));
        let with_cap = plan(
            "remote",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        assert!(remote_stream_capable(&with_cap, "local", true));
        assert!(!remote_stream_capable(&with_cap, "local", false));
    }

    #[test]
    fn remote_stream_keeps_the_remaining_pipeline_inside_the_remote_head() {
        let mut multi = plan(
            "remote-head",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });
        assert!(remote_stream_capable(&multi, "local", true));
    }

    #[test]
    fn stream_de_dois_hops_inicia_mtp_na_cabeca_remota() {
        let mut multi = plan(
            "remote-head",
            [
                vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
                mtp_capabilities(),
            ]
            .concat(),
        );
        multi.steps[0]
            .inference_frame_capabilities
            .extend(mtp_capabilities());
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: mtp_capabilities(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });

        let speculate = remote_stream_speculation(
            &multi,
            true,
            true,
            4,
            3,
            4,
            crate::prefill::DraftSource::Lookup,
        )
        .expect("MTP fatiado deve entrar no stream remoto");

        assert_eq!(speculate.source, crate::infer_server::DraftSource::Mtp);
        assert_eq!(speculate.k, 4);
        assert!(speculate.draft.is_none());
    }

    #[test]
    fn stream_de_dois_hops_nao_desliga_mtp_apos_o_token_de_aquecimento() {
        let step = crate::prefill::SpeculativeStep {
            accepted: vec![(4, "A".into())],
            done: false,
            accept_count: 1,
            batch_width: 1,
            speculated: false,
            speculation_active: true,
            speculation_disabled_reason: None,
            next_draft: Some(vec![5, 6]),
            trace: crate::prefill::RequestTrace::new("qwen", &[("remote-head", 0, 31)]),
        };

        assert!(sliced_mtp_round_continues(true, false, &step));
    }

    #[test]
    fn peer_sem_capacidade_mtp_fatiado_degrada_com_razao() {
        let mut multi = plan(
            "remote-head",
            vec![crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into()],
        );
        multi.steps.push(crate::pipeline::PipelineStep {
            node_id: "remote-tail".into(),
            inference_url: "http://127.0.0.1:8182".into(),
            inference_candidates: Vec::new(),
            start_layer: 32,
            end_layer: 63,
            exec_start: 32,
            exec_end: 63,
            inference_frame_capabilities: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        });

        assert_eq!(
            sliced_mtp_disable_reason(&multi),
            crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
        );
        assert!(remote_stream_speculation(
            &multi,
            true,
            true,
            4,
            3,
            4,
            crate::prefill::DraftSource::Mtp,
        )
        .is_none());
    }
}

#[cfg(test)]
mod peer_visibility_tests {
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: id.into(),
            pubkey: String::new(),
            addr: "127.0.0.1:42777".into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(8090),
            role: String::new(),
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
            tp: None,
            runtime_capacity: None,
        }
    }

    #[test]
    fn a_blocked_peer_is_excluded_from_cluster_planning() {
        let visible =
            peers_visible_to_cluster(vec![peer("current"), peer("stale")], &["stale".to_string()]);

        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].node_id, "current");
    }

    #[tokio::test]
    async fn a_peer_whose_inference_health_times_out_is_excluded_from_the_plan() {
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_port = healthy_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
            axum::serve(healthy_listener, app).await.unwrap();
        });

        let stalled_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stalled_port = stalled_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _connection = stalled_listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut healthy = peer("healthy");
        healthy.inference_port = Some(healthy_port);
        let mut stalled = peer("stalled");
        stalled.inference_port = Some(stalled_port);

        let reachable = ready_for_pipeline(
            &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
            vec![healthy, stalled],
            Duration::from_millis(25),
        )
        .await;

        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].node_id, "healthy");
    }

    #[tokio::test]
    async fn local_peer_preserves_the_active_deployment_selector() {
        let state = crate::gateway::testkit::state_with("self", vec![]).await;
        *state.local_assignment.write().await = Some(LayerAssignment {
            deploy_id: "deployment-42".into(),
            model: "qwen".into(),
            total_layers: 32,
            start_layer: 0,
            end_layer: 15,
            layers_count: 16,
            memory_used_mb: 0.0,
            memory_allocated_mb: 0.0,
        });

        let peers = state.all_peers().await;
        let local = peers
            .iter()
            .find(|peer| peer.node_id == "self")
            .expect("o gateway sempre inclui o peer local");
        assert_eq!(local.deploy_id, "deployment-42");
    }
}

/// Um nó de verdade, montado à mão, para os testes que exercitam rotas.
///
/// Existe porque três módulos precisavam do mesmo andaime — `transfer`,
/// `deploy` e `webui` todos testam handlers que só falam a verdade com um
/// `GatewayState` inteiro atrás deles. Nada aqui fala UDP: a tabela de peers é
/// semeada à mão, então um teste nunca anuncia nada na LAN nem esbarra num
/// cluster de verdade.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use crate::config::{Config, Role};
    use tokio::sync::RwLock;

    /// Um peer alcançável no `gateway_port` dado, na loopback.
    pub(crate) fn peer(node_id: &str, hostname: &str, gateway_port: u16) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.into(),
            pubkey: "pk".into(),
            addr: format!("127.0.0.1:{gateway_port}"),
            addr_candidates: Vec::new(),
            model: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(gateway_port),
            role: String::new(),
            hostname: hostname.into(),
            capabilities: Vec::new(),
            connection_verified: true,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
            tp: None,
            runtime_capacity: Some(RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 256,
            }),
        }
    }

    /// Este nó, com `peers` já na tabela.
    pub(crate) async fn state_with(node_id: &str, peers: Vec<PeerInfo>) -> GatewayState {
        state_named(node_id, "", peers).await
    }

    /// O mesmo, com um **nome anunciado** — o que `node_name` do `pom.json`
    /// produz em `with_identity`, e o nome pelo qual o cluster inteiro chama
    /// esta máquina. Distinto do hostname cru do sistema operacional.
    pub(crate) async fn state_named(
        node_id: &str,
        hostname: &str,
        peers: Vec<PeerInfo>,
    ) -> GatewayState {
        let net = Arc::new(
            Network::new(node_id.into(), "pk".into(), 0, vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera")
                .with_identity(String::new(), hostname.into()),
        );
        {
            let mut table = net.peers.write().await;
            for p in peers {
                table.insert(p.node_id.clone(), p);
            }
        }
        let assignment = Arc::new(RwLock::new(None));
        GatewayState {
            network: net.clone(),
            ollama: Arc::new(OllamaClient::new("http://127.0.0.1:1")),
            default_model: String::new(),
            local_assignment: assignment.clone(),
            loads: Arc::new(LoadTracker::new()),
            prefix_cache: crate::prefix_cache::PrefixCache::default_arc(),
            peer_http: Arc::new(crate::peer_client::PeerClients::new(
                std::time::Duration::from_secs(600),
            )),
            runtime: NodeRuntime::new(net, assignment),
            cycle_config: crate::cycle::CycleConfig::default(),
            audit: crate::reqlog::AuditLog::new(std::path::PathBuf::from(":memory:")),
            gateway_port: 8080,
            auth: Arc::new(RwLock::new(Config {
                gpus: None,
                thermal: Default::default(),
                cache_type_k: crate::config::KvCacheType::F16,
                cache_type_v: crate::config::KvCacheType::F16,
                role: Role::Node,
                tls_required: false,
                node_name: None,
                huggingface_token: None,
                connection_key: "chave".into(),
                gateway_port: 8080,
                p2p_port: 42777,
                inference_port: None,
                bootstrap_nodes: vec![],
                advertise_addr: None,
                api_keys: vec![],
                blocked_nodes: vec![],
                model_dirs: None,
                shard_dir: None,
                shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
            })),
            transfers: Default::default(),
            reply_inbox: crate::direct::ReplyInbox::new(),
            tunnel_tx: None,
            tls_server: None,
            cluster_cache: Arc::new(Mutex::new(Default::default())),
            model_service: Arc::new(crate::model_provider_backend::ModelService::new(Arc::new(
                crate::huggingface::HuggingFaceProvider::new(),
            ))),
        }
    }

    /// Uma configuração de nó com a decodificação especulativa ligada — o
    /// interruptor do operador que o gateway lê antes de cada geração.
    pub(crate) fn spec_config() -> crate::runtime::NodeConfig {
        crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: String::new(),
            model_label: "lab".into(),
            deployment_name: "lab".into(),
            start_layer: 0,
            end_layer: 1,
            total_layers: 2,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_mode: Default::default(),
            inference_port: 8181,
            n_gpu_layers: 0,
            n_ctx: 8192,
            offload_kqv: true,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            prefill_max_inflight_chunks:
                crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            cycle_detection_enabled: true,
            cycle_detection_min_window_tokens: crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS,
            cycle_detection_max_window_tokens: crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS,
            cycle_detection_repetitions: crate::cycle::DEFAULT_CYCLE_REPETITIONS,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: true,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
            mtp_cpu_fallback_enabled: true,
            tp: None,
            speculative_k: 4,
            speculative_lookup_window: 3,
        }
    }

    /// Faz o estado saber um endereço de si mesmo que responde de verdade.
    ///
    /// A resposta direta só existe quando a cauda tem para onde entregar
    /// (`DirectReply::new`), e o endereço sai de `self_reply_hosts` + o
    /// `gateway_port`. Sem um servidor ali o modo direto de um teste esperaria
    /// `DIRECT_REPLY_DEADLINE` inteiro.
    pub(crate) async fn with_self_reachable_gateway(mut state: GatewayState) -> GatewayState {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let inbox = state.reply_inbox.clone();
        let router = axum::Router::new().route(
            crate::direct::RELAY_RESULT_PATH,
            axum::routing::post(move |body: Bytes| {
                let inbox = inbox.clone();
                async move { crate::direct::handle_relay_result(&inbox, body) }
            }),
        );
        let inbox = state.reply_inbox.clone();
        let router = router.route(
            crate::direct::REMOTE_STREAM_RESULT_PATH,
            axum::routing::post(move |body: Bytes| {
                let inbox = inbox.clone();
                async move { crate::direct::handle_remote_stream_result(&inbox, body).await }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });

        state.gateway_port = port;
        let net = Network::new("eu".into(), "pk".into(), 0, vec![], None, None, Some(port))
            .await
            .expect("socket UDP em porta efêmera");
        // Primeiro da lista de `self_reply_hosts`, então a cauda acerta na
        // primeira tentativa em vez de esperar o timeout de um IP de LAN.
        net.set_advertise_addr(Some("127.0.0.1:42777"));
        {
            let antigos = state.network.peers.read().await.clone();
            *net.peers.write().await = antigos;
        }
        state.network = Arc::new(net);
        state
    }

    /// O corpo JSON de uma resposta, para o teste poder afirmar sobre ele.
    pub(crate) async fn body_of(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("corpo legível");
        serde_json::from_slice(&bytes).expect("corpo JSON")
    }
}
