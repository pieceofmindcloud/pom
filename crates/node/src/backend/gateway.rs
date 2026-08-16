use crate::auth::AuthState;
use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo};
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline, LoadTracker, PipelinePlan};
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
use tokio::sync::mpsc;
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

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GatewayState {
    pub network: Arc<Network>,
    pub ollama: Arc<OllamaClient>,
    pub default_model: String,
    /// Current assignment of the local node.
    pub local_assignment: Arc<tokio::sync::RwLock<Option<LayerAssignment>>>,
    /// Tracks in-flight inference requests per peer (for pipeline load balancing).
    pub loads: Arc<LoadTracker>,
    /// Shared HTTP client with long timeout for pipeline calls.
    pub http: Arc<reqwest::Client>,
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
    http: &reqwest::Client,
    peers: Vec<PeerInfo>,
    timeout: Duration,
) -> Vec<PeerInfo> {
    futures::future::join_all(peers.into_iter().map(|peer| {
        let http = http.clone();
        async move {
            let Some(base) = peer.inference_url() else {
                return None;
            };
            let url = format!("{base}/health");
            match tokio::time::timeout(timeout, http.get(url).send()).await {
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
            // pelo operador vence; na falta dele, o endereço público que
            // algum peer já observou por `identify` (ver
            // `Network::effective_host`) — sem isto, o próprio cartão deste
            // nó no painel mostrava o IP da LAN mesmo depois de
            // `self_addr_candidates`/`Announce` já saberem o certo.
            let self_host = self_dial_host(
                self.network.effective_host(),
                crate::pkarr_node::detect_local_ip(),
            );
            peers.push(PeerInfo {
                node_id: self.network.node_id.clone(),
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
                        "frame/v1-f16".into(),
                        "frame/v2-zstd-f16".into(),
                        "relay/v1-chain".into(),
                        "relay/v2-direct".into(),
                        "speculate/v1-lookup".into(),
                    ];
                    if self.network.mtp_ready() {
                        caps.push("speculate/v1-mtp".into());
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
/// The KV cache is split evenly across `max_sessions` slots, so the window one
/// request sees is `n_ctx / max_sessions` — this divisor used to be hardcoded
/// to 8, which silently cut the configured context to an eighth and produced
/// "contexto insuficiente" no matter how high `--n-ctx` was set.
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
pub fn prompt_budget_of(n_ctx: u32, max_sessions: u32, max_new_tokens: u32) -> u32 {
    let per_session_ctx = n_ctx / max_sessions.max(1);
    // Room has to be left for the tokens we are about to generate.
    per_session_ctx.saturating_sub(max_new_tokens)
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
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "pipeline:<hop-chain>" | "local" | "peer:<node_id>" | "error"
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
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

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
    /// Detailed per-request observations. It never affects planning.
    pub trace: Option<crate::prefill::RequestTrace>,
}

/// Run one generation, emitting each token as it is decoded.
///
/// This is the single generation path in the gateway: `/api/generate` collects
/// the events into one response, the OpenAI endpoints forward them as SSE. The
/// per-token loop and the peer protocol are unchanged — one round trip through
/// every peer yields exactly one token.
pub async fn generate_stream(
    state: GatewayState,
    model: String,
    prompt: String,
    max_tokens: u32,
    tx: mpsc::Sender<GenEvent>,
) {
    // Size the prompt up before any peer sees it — see `prompt_token_budget`.
    if let Some(cfg) = state.runtime.config().await {
        let budget = prompt_budget(&cfg);
        let estimated = estimate_tokens(&prompt);
        if estimated > budget {
            let _ = tx
                .send(GenEvent::Failed(format!(
                    "Prompt grande demais: ~{estimated} tokens estimados contra um limite de \
                     {budget}. O limite é n_ctx/{} ({}) menos max_new_tokens ({}). O prompt vai \
                     em pedaços de {} tokens, então n_batch ({}) não limita o prompt. Aumente \
                     n_ctx em Configurar → ajustes avançados, reduza max_new_tokens, ou reduza \
                     as requisições simultâneas.",
                    cfg.max_sessions.max(1),
                    cfg.n_ctx / cfg.max_sessions.max(1),
                    cfg.max_new_tokens,
                    prompt_chunk_tokens(&cfg),
                    cfg.n_batch,
                )))
                .await;
            return;
        }
    }

    let peers = ready_for_pipeline(
        &state.http,
        state.all_peers().await,
        PIPELINE_HEALTH_TIMEOUT,
    )
    .await;

    // Total layers for THIS model, as reported by a peer that serves it.
    let total_layers = peers
        .iter()
        .find(|p| p.model == model)
        .map(|p| p.total_layers)
        .unwrap_or(32);

    // Each request gets a unique session_id passed to every inference server
    // call, so parallel requests keep isolated KV cache state.
    let session_id = Uuid::new_v4().to_string();

    let Some(mut plan) = plan_pipeline(&peers, &model, total_layers, &state.loads).await else {
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
            speculative_k_of(
                cfg.as_ref().map(|c| c.speculative_k).unwrap_or(4),
                n_batch,
            ),
            cfg.as_ref().map(|c| c.speculative_lookup_window).unwrap_or(3),
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
    let (speculative_enabled, speculative_k, draft_source) =
        if mtp_enabled && plan.supports_mtp() {
            (true, mtp_k, crate::prefill::DraftSource::Mtp)
        } else {
            (
                lookup_enabled,
                lookup_k,
                crate::prefill::DraftSource::Lookup,
            )
        };

    // Whatever ends this generation — EOS, the token limit, the client hanging
    // up, or a peer erroring — every peer has to get its KV slot back. Only the
    // sampling peer used to free anything, and only on EOS, so the first peer
    // leaked a slot per request and died after eight of them.
    async {
        // ── Prefill: the prompt goes in chunk by chunk, and the last peer hands
        // back the first token once the whole prompt is in.
        let first = match crate::prefill::run(
            &plan,
            &model,
            &session_id,
            &prompt,
            chunk_tokens,
            dtype,
            &state.http,
            &state.network.routes,
            direct.as_ref(),
            state.tunnel_tx.as_ref(),
        )
        .await
        {
            Ok(o) => {
                info!(
                    "📥 prompt de {} tokens em {} pedaço(s) · {:.1}s até a primeira palavra",
                    o.prompt_tokens,
                    o.chunks,
                    gen_start.elapsed().as_secs_f64(),
                );
                o
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
        if !first.text.is_empty() && tx.send(GenEvent::Token(first.text)).await.is_err() {
            info!("cliente desconectou logo após o prompt");
            return;
        }
        if first.done {
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
                    trace: Some(trace),
                }))
                .await;
            return;
        }

        if plan.supports_speculation() && speculative_enabled {
            // Vira `false` na primeira rodada em que a cabeça recusar o
            // rascunho. Pedir de novo só repetiria a recusa, e o pedido custa
            // caro por um motivo que não se vê: `step_speculative` nunca
            // aprendeu a resposta direta, então insistir mantém a cadeia no
            // anel borbulhado (2N travessias por token) quando o caminho
            // direto (N+1) está disponível — ver `crate::direct`.
            let mut especulando = true;
            while tokens_generated < max_tokens {
                let res = if especulando {
                    crate::prefill::step_speculative(
                        &plan,
                        &model,
                        &session_id,
                        next_token,
                        speculative_k,
                        speculative_m,
                        draft_source,
                        dtype,
                        &state.http,
                        &state.network.routes,
                        state.tunnel_tx.as_ref(),
                    )
                    .await
                } else {
                    crate::prefill::step(
                        &plan,
                        &model,
                        &session_id,
                        next_token,
                        dtype,
                        &state.http,
                        &state.network.routes,
                        direct.as_ref(),
                        state.tunnel_tx.as_ref(),
                    )
                    .await
                    .map(crate::prefill::SpeculativeStep::of_one_token)
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
                if especulando && !step.speculated {
                    // A cabeça recusou rascunhar — resposta legítima, e a única
                    // pista que o gateway tem de que este modelo não especula.
                    info!(
                        "nenhum hop rascunhou: seguindo sem especulação \
                         (o cache KV do modelo não desloca — ver kv_can_shift em /health)"
                    );
                    especulando = false;
                }
                trace.merge(&step.trace);

                // A round can confirm more tokens than remain in the budget —
                // stream only what fits, and note it so `step.done` (which
                // may refer to a position we never even looked at) isn't
                // trusted past the cut.
                let remaining = (max_tokens - tokens_generated) as usize;
                let take = step.accepted.len().min(remaining);
                let truncated_by_budget = take < step.accepted.len();

                for (token_id, text) in step.accepted.iter().take(take) {
                    next_token = *token_id;
                    tokens_generated += 1;
                    if !text.is_empty()
                        && tx.send(GenEvent::Token(text.clone())).await.is_err()
                    {
                        info!("cliente desconectou após {tokens_generated} tokens");
                        return;
                    }
                }

                let finished =
                    (step.done && !truncated_by_budget) || tokens_generated >= max_tokens;
                if finished {
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
                    rollback_sessions(
                        &plan,
                        &session_id,
                        step.accept_count,
                        step.batch_width,
                        &state.http,
                    )
                    .await;
                }
            }
            return;
        }

        for i in 0..max_tokens {
            let res =
                crate::prefill::step(
                    &plan,
                    &model,
                    &session_id,
                    next_token,
                    dtype,
                    &state.http,
                    &state.network.routes,
                    direct.as_ref(),
                    state.tunnel_tx.as_ref(),
                )
                    .await;

            match res {
                Ok((token_id, text, done, step_trace)) => {
                    trace.merge(&step_trace);
                    next_token = token_id;
                    tokens_generated += 1;
                    if !text.is_empty() && tx.send(GenEvent::Token(text)).await.is_err() {
                        // Client hung up; stop burning GPU time on a dead request.
                        info!("cliente desconectou após {tokens_generated} tokens");
                        return;
                    }

                    if done || i == max_tokens - 1 {
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
                                trace: Some(trace),
                            }))
                            .await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("Erro no passo {i} do pipeline: {e}");
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
                                trace: {
                                    trace.output_tokens = tokens_generated;
                                    Some(trace)
                                },
                            }))
                            .await;
                    } else {
                        let _ = tx
                            .send(GenEvent::Failed(format!(
                                "Pipeline falhou no passo {i}: {e}"
                            )))
                            .await;
                    }
                    return;
                }
            }
        }
    }
    .await;

    release_sessions(&plan, &session_id, &state.http).await;
}

/// Hand every peer in the plan its KV slot back. Best effort: a peer that is
/// already gone has nothing to release, and failing here must not turn a good
/// answer into an error.
async fn release_sessions(plan: &PipelinePlan, session_id: &str, http: &reqwest::Client) {
    for step in &plan.steps {
        let url = format!("{}/v1/session/release", step.inference_url);
        match http
            .post(&url)
            .json(&serde_json::json!({ "session_id": session_id }))
            .timeout(Duration::from_secs(10))
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
async fn rollback_sessions(
    plan: &PipelinePlan,
    session_id: &str,
    accept_count: u32,
    batch_width: u32,
    http: &reqwest::Client,
) {
    for step in &plan.steps {
        let url = format!("{}/v1/session/rollback", step.inference_url);
        match http
            .post(&url)
            .json(&serde_json::json!({
                "session_id": session_id,
                "accept_count": accept_count,
                "batch_width": batch_width,
            }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!(
                "rollback de sessão em {} respondeu {}",
                step.node_id,
                r.status()
            ),
            Err(e) => warn!("rollback de sessão em {} falhou: {e}", step.node_id),
        }
    }
}

/// Pessimistic token count for a prompt, used only by the size guard.
fn estimate_tokens(prompt: &str) -> u32 {
    prompt.chars().count().div_ceil(CHARS_PER_TOKEN) as u32
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
/// putting a caller argument through every early return in `generate_stream` —
/// which is the hot inference path and not worth destabilising for a log line.
#[allow(clippy::too_many_arguments)]
pub fn spawn_logged_generation(
    state: GatewayState,
    route: &'static str,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
    tx: mpsc::Sender<GenEvent>,
) {
    let entry_node_id = state.network.node_id.clone();
    let prompt_tokens = estimate_tokens(&prompt);
    let logged_model = model.clone();

    let (inner_tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream(state, model, prompt, max_tokens, inner_tx));

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
pub async fn generate_blocking(
    state: GatewayState,
    route: &'static str,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
) -> std::result::Result<(String, GenSummary), String> {
    let (tx, mut rx) = mpsc::channel(64);
    spawn_logged_generation(state, route, caller, model, prompt, max_tokens, tx);

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
    let admin = admin::admin_routes().with_state(state.clone());
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
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
        // no risk of compressing already-zstd-compressed payloads here.
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
    // cliente por padrão; o `axum::serve` deixa `tcp_nodelay` em `None`, então
    // o lado servidor ficava com Nagle ligado em todas as respostas.
    axum::serve(listener, app).tcp_nodelay(true).await?;
    Ok(())
}

// ── Route handlers ────────────────────────────────────────────────────────────

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

    info!(
        "📨 /api/generate model={} prompt_len={}",
        model,
        req.prompt.len()
    );

    // `/api/generate` has never been behind auth, so there is no credential to
    // attribute the request to — the log says exactly that instead of guessing.
    match generate_blocking(
        state,
        "/api/generate",
        crate::reqlog::Caller::anonymous(),
        model,
        req.prompt,
        DEFAULT_MAX_TOKENS,
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

    /// The self-entry `.addr` used to consult only the operator's own pin
    /// (`advertised_host`), never a peer-observed public address — so a NAT'd
    /// node's own card in the dashboard kept showing its private LAN IP even
    /// after `Announce`/`self_addr_candidates` already knew the right one.
    #[tokio::test]
    async fn the_self_entry_address_picks_up_an_observed_public_address() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.network.set_observed_addr("203.0.113.5:9999");

        let peers = state.all_peers().await;
        let me = peers.iter().find(|p| p.node_id == "eu").expect("self entry");

        assert!(
            me.addr.starts_with("203.0.113.5:"),
            "self entry addr should use the observed public address, got {}",
            me.addr
        );
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
            model_path: String::new(),
            model_label: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx,
            offload_kqv: true,
            n_batch,
            max_sessions,
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
    fn concurrency_divides_the_window() {
        let c = cfg(262_144, 262_144, 1024, 8);
        assert_eq!(prompt_budget(&c), 262_144 / 8 - 1024);
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
            &reqwest::Client::new(),
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
        }
    }

    /// Este nó, com `peers` já na tabela.
    pub(crate) async fn state_with(node_id: &str, peers: Vec<PeerInfo>) -> GatewayState {
        let net = Arc::new(
            Network::new(node_id.into(), "pk".into(), 0, vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera"),
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
            http: Arc::new(reqwest::Client::new()),
            runtime: NodeRuntime::new(net, assignment),
            gateway_port: 8080,
            auth: Arc::new(RwLock::new(Config {
                role: Role::Node,
                node_name: None,
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
            })),
            transfers: Default::default(),
            reply_inbox: crate::direct::ReplyInbox::new(),
            tunnel_tx: None,
        }
    }

    /// Uma configuração de nó com a decodificação especulativa ligada — o
    /// interruptor do operador que o gateway lê antes de cada geração.
    pub(crate) fn spec_config() -> crate::runtime::NodeConfig {
        crate::runtime::NodeConfig {
            model_path: String::new(),
            model_label: "lab".into(),
            start_layer: 0,
            end_layer: 1,
            total_layers: 2,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_port: 8181,
            n_gpu_layers: 0,
            n_ctx: 8192,
            offload_kqv: true,
            n_batch: 512,
            max_sessions: 1,
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
