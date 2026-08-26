use crate::auth::AuthState;
use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo};
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
const DEFAULT_MAX_TOKENS: u32 = 512;
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
                deploy_id: String::new(),
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
            });
        }
        peers
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

#[derive(Debug, Deserialize)]
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

fn resolve_request_max_tokens(
    requested: Option<u32>,
    configured_limit: u32,
) -> Result<u32, String> {
    let max_tokens = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens == 0 {
        return Err("max_tokens deve ser maior que zero".to_string());
    }
    if configured_limit == 0 {
        return Err("max_new_tokens configurado deve ser maior que zero".to_string());
    }
    if max_tokens > configured_limit {
        return Err(format!(
            "max_tokens solicitado ({max_tokens}) excede max_new_tokens configurado ({configured_limit})"
        ));
    }
    Ok(max_tokens)
}

fn remaining_generation_steps(max_tokens: u32, tokens_generated: u32) -> u32 {
    max_tokens.saturating_sub(tokens_generated)
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "pipeline:<hop-chain>" | "local" | "peer:<node_id>" | "error"
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub stop_sequence: Option<String>,
    pub trace: Option<crate::prefill::RequestTrace>,
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
    pub(crate) max_tokens: u32,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
    /// "stop" when a server-side stop sequence ended the generation.
    pub finish_reason: Option<String>,
    /// The exact stop sequence that was matched, when one ended the stream.
    pub stop_sequence: Option<String>,
    /// Detailed per-request observations. It never affects planning.
    pub trace: Option<crate::prefill::RequestTrace>,
}

/// OpenAI's `stop` accepts either one string or a list. Keeping this parser in
/// the gateway lets all HTTP adapters share the same matcher and keeps the
/// internal inference protocol unchanged.
#[derive(Debug, Clone, Deserialize)]
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

async fn send_stopped_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    gen_start: Instant,
    tokens_generated: u32,
    mut trace: crate::prefill::RequestTrace,
    stop_sequence: String,
) {
    let elapsed = gen_start.elapsed();
    trace.output_tokens = tokens_generated;
    trace.complete = true;
    let _ = tx
        .send(GenEvent::Done(GenSummary {
            source: format!("pipeline:{}", plan.hop_chain()),
            tokens_generated,
            tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
            duration_ms: elapsed.as_millis() as u64,
            partial: false,
            finish_reason: Some("stop".to_string()),
            stop_sequence: Some(stop_sequence),
            trace: Some(trace),
        }))
        .await;
}

async fn send_length_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    gen_start: Instant,
    tokens_generated: u32,
    mut trace: crate::prefill::RequestTrace,
) {
    let elapsed = gen_start.elapsed();
    trace.output_tokens = tokens_generated;
    trace.complete = true;
    let _ = tx
        .send(GenEvent::Done(GenSummary {
            source: format!("pipeline:{}", plan.hop_chain()),
            tokens_generated,
            tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
            duration_ms: elapsed.as_millis() as u64,
            partial: false,
            finish_reason: Some("length".to_string()),
            stop_sequence: None,
            trace: Some(trace),
        }))
        .await;
}

/// Run one generation, emitting each token as it is decoded.
///
/// This is the single generation path in the gateway: `/api/generate` collects
/// the events into one response, the OpenAI endpoints forward them as SSE. The
/// per-token loop and the peer protocol are unchanged — one round trip through
/// every peer yields exactly one token.
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

async fn generate_stream_inner(
    input: GenerationInput,
    tx: mpsc::Sender<GenEvent>,
    cancellation: crate::prefill::Cancellation,
) {
    let GenerationInput {
        state,
        model,
        prompt,
        chat_template,
        max_tokens,
        stops,
        cache_key,
    } = input;
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
        "🔀 Pipeline planejado: {} hops [{}] session={} max_tokens={} resposta={}",
        plan.steps.len(),
        plan.hop_chain(),
        &session_id[..8],
        max_tokens,
        if direct.is_some() && plan.supports_direct_reply() {
            "direta"
        } else if plan.supports_relay_chain() {
            "anel"
        } else {
            "hub"
        },
    );

    let gen_start = Instant::now();
    let mut tokens_generated: u32 = 0;
    let max_tokens = max_tokens.max(1);
    let mut retained_prefix_tokens: Option<u32> = None;
    // Quantos tokens do prompt a cabeça de fato não reprocessou. Não é a
    // escolha do cache, é o que o pipeline executou - ver
    // `PrefillOutcome::cached_tokens`. Sai no `GenSummary` e daí para
    // `usage.prompt_tokens_details.cached_tokens`.
    let mut honored_cached_tokens: u32 = 0;

    // Chunk size, wire precision, and the speculative-decoding knobs all come
    // from the node's configuration, so they're operator knobs rather than
    // constants baked into the protocol.
    let (chunk_tokens, dtype, lookup_enabled, lookup_k, speculative_m, mtp_enabled, mtp_k) = {
        let cfg = state.runtime.config().await;
        let n_batch = cfg.as_ref().map(|c| c.n_batch).unwrap_or(512);
        (
            cfg.as_ref()
                .map(|c| c.prefill_chunk_tokens)
                .unwrap_or(crate::prefill::DEFAULT_CHUNK_TOKENS),
            crate::wire::WireDtype::F16,
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
        let prefill_attempt = |reuse: bool| {
            crate::prefill::run(
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
            )
        };
        let mut prefill_result = prefill_attempt(reuse_prefix).await;
        if reuse_prefix {
            if let Err(e) = &prefill_result {
                if crate::relay::is_frame_size_error(e) {
                    warn!(
                        "prefill com prefixo falhou por limite de frame ({e}); \
                         não é recuperável repetindo o mesmo chunk"
                    );
                } else if !crate::prefill::is_cancelled(e) {
                    warn!("prefill com prefixo reaproveitado falhou ({e}); repetindo do zero");
                    prefill_result = prefill_attempt(false).await;
                }
            }
        }
        let first = match prefill_result {
            Ok(o) => {
                info!(
                    "📥 prompt de {} tokens em {} pedaço(s), {} do cache · {:.1}s até a primeira palavra",
                    o.prompt_tokens,
                    o.chunks,
                    o.cached_tokens,
                    gen_start.elapsed().as_secs_f64(),
                );
                retained_prefix_tokens = Some(o.retained_tokens);
                honored_cached_tokens = o.cached_tokens;
                o
            }
            Err(e) if crate::prefill::is_cancelled(&e) => {
                info!("cliente desconectou durante o prefill; cancelando pipeline");
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(GenEvent::Failed(format!("Prefill falhou: {e}")))
                    .await;
                return;
            }
        };

        let mut next_token = first.token_id;
        let mut trace = first.trace;
        tokens_generated += 1;
        match send_text_with_stop(&tx, &mut stop_matcher, &first.text).await {
            StopSend::Disconnected => {
                info!("cliente desconectou logo após o prompt");
                return;
            }
            StopSend::Stopped(stop_sequence) => {
                send_stopped_summary(
                    &tx,
                    &plan,
                    gen_start,
                    tokens_generated,
                    trace,
                    stop_sequence,
                )
                .await;
                return;
            }
            StopSend::Continue => {}
        }
        if first.done {
            if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                return;
            }
            let elapsed = gen_start.elapsed();
            trace.output_tokens = tokens_generated;
            trace.complete = true;
            let _ = tx
                .send(GenEvent::Done(GenSummary {
                    source: format!("pipeline:{}", plan.hop_chain()),
                    tokens_generated,
                    tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                    duration_ms: elapsed.as_millis() as u64,
                    partial: false,
                    finish_reason: None,
                    stop_sequence: None,
                    trace: Some(trace),
                }))
                .await;
            return;
        }

        if remaining_generation_steps(max_tokens, tokens_generated) == 0 {
            if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                return;
            }
            send_length_summary(&tx, &plan, gen_start, tokens_generated, trace).await;
            return;
        }

        if plan.supports_speculation() && speculative_enabled {
            // Vira `false` na primeira rodada em que a cabeça recusar o
            // rascunho. Pedir de novo só repetiria a recusa, e o pedido custa
            // caro por um motivo que não se vê: `step_speculative` nunca
            // aprendeu a resposta direta, então insistir mantém a cadeia no
            // anel borbulhado (2N travessias por token) quando o caminho
            // direto (N+1) está disponível — ver `crate::direct`.
            // O MTP fatiado é a outra metade da mesma rodada: a cauda (dona
            // do bloco `nextn`) rascunha na resposta direta e o gateway
            // injeta o rascunho no `speculate` da rodada seguinte. Só com 2+
            // hops — com um é o modelo inteiro, que já usa `step_speculative`.
            let sliced_mtp = mtp_enabled && plan.supports_mtp() && plan.steps.len() >= 2;
            let mut especulando = true;
            // Rascunho que a cauda devolveu na rodada anterior, esperando
            // para ser injetado na próxima. `draft_sent` é lido antes do
            // `take`: distingue a rodada 1 (nenhum rascunho viajou ainda, e a
            // recusa da cabeça é esperada) das seguintes (o rascunho foi
            // mandado — ignorado aí é motivo para parar).
            let mut stored_draft: Option<Vec<i32>> = None;
            while remaining_generation_steps(max_tokens, tokens_generated) > 0 {
                if cancellation.is_cancelled() {
                    return;
                }
                let draft_sent = especulando && sliced_mtp && stored_draft.is_some();
                let res = if especulando {
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
                        let elapsed = gen_start.elapsed();
                        if tokens_generated > 0 {
                            info!("devolvendo {tokens_generated} tokens parciais");
                            let _ = tx
                                .send(GenEvent::Done(GenSummary {
                                    source: format!("pipeline:{} (partial)", plan.hop_chain()),
                                    tokens_generated,
                                    tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                                    duration_ms: elapsed.as_millis() as u64,
                                    partial: true,
                                    finish_reason: None,
                                    stop_sequence: None,
                                    trace: {
                                        trace.output_tokens = tokens_generated;
                                        Some(trace)
                                    },
                                }))
                                .await;
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
                        // Dois motivos para desistir, e só dois. (a) O rascunho
                        // foi mandado e a cabeça o ignorou — só acontece hoje
                        // se ela não é da versão nova (`should_draft` aceita
                        // todo `draft` pronto incondicionalmente), e pedir de
                        // novo só repetiria a recusa. (b) A rodada 1 não
                        // mandou rascunho (ainda não havia um pronto) e a
                        // cauda não devolveu nenhum — este pipeline fatiado
                        // não rascunha por aqui.
                        if (draft_sent && !step.speculated)
                            || (!draft_sent && step.next_draft.is_none())
                        {
                            info!(
                                "pipeline fatiado sem rascunho MTP: seguindo \
                                 sem especulação"
                            );
                            especulando = false;
                        } else {
                            stored_draft = step.next_draft;
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
                let remaining = remaining_generation_steps(max_tokens, tokens_generated) as usize;
                let take = step.accepted.len().min(remaining);
                let truncated_by_budget = take < step.accepted.len();

                for (token_id, text) in step.accepted.iter().take(take) {
                    next_token = *token_id;
                    tokens_generated += 1;
                    match send_text_with_stop(&tx, &mut stop_matcher, text).await {
                        StopSend::Disconnected => {
                            info!("cliente desconectou após {tokens_generated} tokens");
                            return;
                        }
                        StopSend::Stopped(stop_sequence) => {
                            send_stopped_summary(
                                &tx,
                                &plan,
                                gen_start,
                                tokens_generated,
                                trace,
                                stop_sequence,
                            )
                            .await;
                            return;
                        }
                        StopSend::Continue => {}
                    }
                }

                let finished =
                    (step.done && !truncated_by_budget) || tokens_generated >= max_tokens;
                if finished {
                    if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                        return;
                    }
                    let elapsed = gen_start.elapsed();
                    trace.output_tokens = tokens_generated;
                    trace.complete = true;
                    info!(
                        "✅ Pipeline completo ({}): {} tokens em {:.1}s ({:.1} tok/s)",
                        if especulando {
                            "especulativo"
                        } else {
                            "sem especulação"
                        },
                        tokens_generated,
                        elapsed.as_secs_f64(),
                        rate(tokens_generated, elapsed.as_secs_f64())
                    );
                    let _ = tx
                        .send(GenEvent::Done(GenSummary {
                            source: format!("pipeline:{}", plan.hop_chain()),
                            tokens_generated,
                            tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                            duration_ms: elapsed.as_millis() as u64,
                            partial: false,
                            finish_reason: None,
                            stop_sequence: None,
                            trace: Some(trace),
                        }))
                        .await;
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
                        let elapsed = gen_start.elapsed();
                        if tokens_generated > 0 {
                            let _ = tx
                                .send(GenEvent::Done(GenSummary {
                                    source: format!("pipeline:{} (partial)", plan.hop_chain()),
                                    tokens_generated,
                                    tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                                    duration_ms: elapsed.as_millis() as u64,
                                    partial: true,
                                    finish_reason: None,
                                    stop_sequence: None,
                                    trace: {
                                        trace.output_tokens = tokens_generated;
                                        Some(trace)
                                    },
                                }))
                                .await;
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

        while remaining_generation_steps(max_tokens, tokens_generated) > 0 {
            let res = crate::prefill::step(
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
            .await;

            match res {
                Ok(step) => {
                    trace.merge(&step.trace);
                    // O formato comum de `step` é um único token confirmado.
                    let (token_id, text) = &step.accepted[0];
                    next_token = *token_id;
                    tokens_generated += 1;
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
                                gen_start,
                                tokens_generated,
                                trace,
                                stop_sequence,
                            )
                            .await;
                            return;
                        }
                        StopSend::Continue => {}
                    }
                    let done = step.done;

                    if done || remaining_generation_steps(max_tokens, tokens_generated) == 0 {
                        if !flush_stop_matcher(&tx, &mut stop_matcher).await {
                            return;
                        }
                        let elapsed = gen_start.elapsed();
                        trace.output_tokens = tokens_generated;
                        trace.complete = true;
                        info!(
                            "✅ Pipeline completo: {} tokens em {:.1}s ({:.1} tok/s)",
                            tokens_generated,
                            elapsed.as_secs_f64(),
                            rate(tokens_generated, elapsed.as_secs_f64())
                        );
                        let _ = tx
                            .send(GenEvent::Done(GenSummary {
                                source: format!("pipeline:{}", plan.hop_chain()),
                                tokens_generated,
                                tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                                duration_ms: elapsed.as_millis() as u64,
                                partial: false,
                                finish_reason: (!done).then(|| "length".to_string()),
                                stop_sequence: None,
                                trace: Some(trace),
                            }))
                            .await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("Erro no passo {tokens_generated} do pipeline: {e}");
                    let elapsed = gen_start.elapsed();
                    if tokens_generated > 0 {
                        // Some output is better than none — the original behaviour.
                        info!("devolvendo {tokens_generated} tokens parciais");
                        let _ = tx
                            .send(GenEvent::Done(GenSummary {
                                source: format!("pipeline:{} (partial)", plan.hop_chain()),
                                tokens_generated,
                                tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                                duration_ms: elapsed.as_millis() as u64,
                                partial: true,
                                finish_reason: None,
                                stop_sequence: None,
                                trace: {
                                    trace.output_tokens = tokens_generated;
                                    Some(trace)
                                },
                            }))
                            .await;
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

    let (inner_tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream_with_stops(input, inner_tx));

    tokio::spawn(async move {
        let started = Instant::now();
        let mut generated: u32 = 0;
        // `None` means the client hung up before the generation ended, which is
        // its own outcome and worth seeing in the log.
        let mut outcome: Option<Result<GenSummary, String>> = None;

        while let Some(event) = inner_rx.recv().await {
            let terminal = match &event {
                GenEvent::Token(_) => {
                    generated += 1;
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
                if s.partial { "partial" } else { "ok" },
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

        crate::reqlog::record(crate::reqlog::Entry {
            id: Uuid::new_v4().simple().to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            route: route.to_string(),
            model: logged_model,
            token_label: caller.token_label,
            token_hint: caller.token_hint,
            token_node_id: caller.token_node_id,
            entry_node_id,
            hops,
            source,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens,
            duration_ms,
            status,
            error,
        })
        .await;
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
                    stop_sequence: None,
                    trace: None,
                }),
            )
                .into_response();
        }
    };

    let model = match req.model.as_deref() {
        Some(m) => m.to_string(),
        None => state.active_model().await,
    };
    let configured_limit = state
        .runtime
        .config()
        .await
        .map(|cfg| cfg.max_new_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let max_tokens = match resolve_request_max_tokens(req.max_tokens, configured_limit) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: error,
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    stop_sequence: None,
                    trace: None,
                }),
            )
                .into_response();
        }
    };
    let stops = req.stop.map(StopInput::into_vec).unwrap_or_default();

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
            max_tokens,
            stops,
            // `/api/generate` recebe um prompt, não uma conversa: o prefixo é
            // o próprio texto.
            cache_key: None,
        },
    )
    .await
    {
        Ok((text, summary)) => (
            StatusCode::OK,
            Json(GenerateResponse {
                response: text,
                source: summary.source,
                tokens_generated: summary.tokens_generated,
                tokens_per_sec: summary.tokens_per_sec,
                duration_ms: summary.duration_ms,
                stop_sequence: summary.stop_sequence,
                trace: summary.trace,
            }),
        )
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
                stop_sequence: None,
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

    #[test]
    fn api_generate_max_tokens_defaults_accepts_and_rejects_by_configured_limit() {
        let absent: GenerateRequest = serde_json::from_str(r#"{"prompt":"oi"}"#).unwrap();
        let requested: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":64}"#).unwrap();
        let excessive: GenerateRequest =
            serde_json::from_str(r#"{"prompt":"oi","max_tokens":513}"#).unwrap();

        assert_eq!(resolve_request_max_tokens(absent.max_tokens, 512), Ok(512));
        assert_eq!(
            resolve_request_max_tokens(requested.max_tokens, 512),
            Ok(64)
        );
        assert_eq!(
            resolve_request_max_tokens(excessive.max_tokens, 512),
            Err("max_tokens solicitado (513) excede max_new_tokens configurado (512)".into())
        );
    }

    #[test]
    fn api_generate_max_tokens_rejects_zero_instead_of_silently_clamping() {
        assert_eq!(
            resolve_request_max_tokens(Some(0), 512),
            Err("max_tokens deve ser maior que zero".into())
        );
    }

    #[test]
    fn output_budget_counts_the_first_token_returned_by_prefill() {
        assert_eq!(remaining_generation_steps(64, 1), 63);
        assert_eq!(remaining_generation_steps(1, 1), 0);
        assert_eq!(remaining_generation_steps(64, 64), 0);
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
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::runtime::NodeConfig;

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
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            n_batch,
            max_sessions,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            max_new_tokens: max_new,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
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
            gateway_port: 8080,
            auth: Arc::new(RwLock::new(Config {
                gpus: None,
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
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            n_batch: 512,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: true,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
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
