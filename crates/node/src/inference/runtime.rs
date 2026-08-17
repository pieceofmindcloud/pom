//! Mutable operating state of this node.
//!
//! Everything that used to be fixed by the command line at startup — which GGUF
//! to serve, which layers, the sampling knobs — lives here so the web UI can
//! change it while the process keeps running. The runtime owns the in-process
//! Rust inference server and restarts it when the configuration changes.

use crate::model::{LayerAssignment, ModelProfile};
use crate::network::Network;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{watch, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

/// Give up restarting a crash-looping inference server after this many tries.
const MAX_RESTARTS: u32 = 10;
/// Largest context the product exposes. Larger allocations can exhaust a node
/// before llama.cpp can report a useful error, so keep the operator-facing
/// contract explicit.
pub const MAX_CONTEXT_TOKENS: u32 = 250_000;

// ── Configuration ────────────────────────────────────────────────────────────

/// What this node is currently serving. Serialisable so the UI can round-trip it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    /// Path of the GGUF this peer loads — a full model or one slice of one.
    pub model_path: String,
    /// Label shared by every peer of the same model. The pipeline planner only
    /// chains peers whose label matches exactly, so this must agree across the
    /// cluster (see `ModelProfile::guess_from_name`).
    pub model_label: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    /// How many peers the model is split across.
    pub peer_count: u32,
    /// 0-based position of this peer in that split.
    pub peer_index: u32,
    /// This peer is the entry point clients talk to.
    pub primary: bool,

    pub inference_port: u16,
    pub n_gpu_layers: u32,
    pub n_ctx: u32,
    /// Keep the llama.cpp KV cache on the GPU. Disable to trade latency for
    /// host RAM capacity on large-context hardware.
    #[serde(default = "default_offload_kqv")]
    pub offload_kqv: bool,
    pub n_batch: u32,
    /// Concurrent generations; the context is split between them.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// Prompt tokens pushed through the pipeline per round trip.
    #[serde(default = "default_prefill_chunk")]
    pub prefill_chunk_tokens: u32,
    pub max_new_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
    pub repeat_last_n: u32,
    pub no_thinking: bool,
    pub single_peer: bool,
    /// Disable embedding extraction in llama.cpp. Set on the last (sampling)
    /// peer of a multi-peer pipeline when `embeddings=True` breaks logits
    /// (observed with phi-4 where the model outputs EOS as first token).
    #[serde(default)]
    pub no_embeddings: bool,
    /// Operator kill-switch, independent of whether every peer in a plan
    /// negotiated `speculate/v1-lookup` — off by default so turning the
    /// feature on is opt-in even on an all-capable cluster.
    #[serde(default)]
    pub speculative_decoding_enabled: bool,
    /// Draft width: the confirmed seed token plus up to `speculative_k - 1`
    /// guessed continuation tokens, batched through one decode.
    #[serde(default = "default_speculative_k")]
    pub speculative_k: u32,
    /// How many trailing tokens of history define the pattern
    /// `find_draft` searches for a prior repeat of.
    #[serde(default = "default_speculative_lookup_window")]
    pub speculative_lookup_window: u32,
    /// Predição multi-token pelo bloco `nextn` do próprio modelo.
    ///
    /// **Interruptor separado de `speculative_decoding_enabled` de
    /// propósito**: são dois mecanismos diferentes, e o de cima
    /// (prompt-lookup) não usa peso nenhum do modelo. Ver `crate::mtp` para
    /// a diferença e `mtp_effective` para quando ele realmente entra em ação.
    #[serde(default)]
    pub mtp_enabled: bool,
    /// Quantos tokens a cabeça `nextn` rascunha por rodada.
    #[serde(default = "default_mtp_draft_tokens")]
    pub mtp_draft_tokens: u32,
    /// Confiança mínima para o rascunho continuar. Zero aceita qualquer
    /// palpite (o padrão do llama.cpp); acima disso o rascunho para no
    /// primeiro token duvidoso, em vez de alargar o lote de todos os hops
    /// para uma aposta que a verificação vai recusar.
    #[serde(default = "default_mtp_min_probability")]
    pub mtp_min_probability: f32,
}

fn default_max_sessions() -> u32 {
    1
}
fn default_prefill_chunk() -> u32 {
    crate::prefill::DEFAULT_CHUNK_TOKENS
}
fn default_offload_kqv() -> bool {
    true
}
fn default_speculative_k() -> u32 {
    4
}
fn default_speculative_lookup_window() -> u32 {
    3
}
/// O mesmo padrão do `llama.cpp` (`common_params_speculative_draft::n_max`).
fn default_mtp_draft_tokens() -> u32 {
    3
}
/// Zero, como o `p_min` do llama.cpp: sem corte por confiança até que uma
/// medição diga que cortar rende mais que rascunhar.
fn default_mtp_min_probability() -> f32 {
    0.0
}

impl NodeConfig {
    pub fn layers_count(&self) -> u32 {
        self.end_layer.saturating_sub(self.start_layer) + 1
    }

    /// Este nó precisa mesmo entregar o estado oculto ao sucessor?
    ///
    /// É a **única** condição que justifica instalar o
    /// `ggml_backend_sched_eval_callback` de captura em `llm.rs`: o callback
    /// muda como o scheduler do ggml agrupa os nós do grafo e custou 7,8-8,2 ms
    /// por token na medição de 2026-08-12, contra 0,14-0,67 ms no hop que não o
    /// instala — mesmo número de camadas, e o que não instala ainda carrega a
    /// LM head. O custo segue a posição, não a máquina.
    ///
    /// Errar para menos não corrompe em silêncio: `LlmEngine::hidden_state`
    /// recusa com "residual capture did not fire" em vez de devolver o
    /// pós-norm.
    pub fn needs_residual_export(&self) -> bool {
        if self.single_peer || self.no_embeddings {
            return false;
        }
        let serves_last_layer = self.end_layer + 1 >= self.total_layers;
        let last_in_split = self.peer_index + 1 >= self.peer_count;
        !serves_last_layer && !last_in_split
    }

    /// O MTP vale a pena **neste** nó?
    ///
    /// Dois casos, e eles não se confundem:
    ///
    /// * **Modelo inteiro:** uma máquina só serve todas as camadas, inclusive a
    ///   última com o bloco `nextn`. O rascunho e o lote saem da mesma cabeça.
    /// * **Cauda de um pipeline fatiado:** o bloco `nextn` mora aqui mas quem
    ///   monta o lote é a cabeça, duas máquinas diferentes. Aqui só se liga o
    ///   *motor* de rascunho — a produção do draft via `mtp_draft` e a
    ///   observação (`observe`) depois do `decode_hidden`. O gateway faz o resto.
    ///
    /// Que o modelo tenha ou não bloco `nextn` **não** se decide aqui: quem
    /// sabe isso é o GGUF carregado, e `MtpDrafter::attach` devolve `None`
    /// quando não tem.
    pub fn mtp_effective(&self) -> bool {
        if !self.mtp_enabled {
            return false;
        }
        let serves_last_layer = self.end_layer + 1 >= self.total_layers;
        if !serves_last_layer {
            return false;
        }
        // Modelo inteiro.
        if self.start_layer == 0 && self.peer_count <= 1 {
            return true;
        }
        // Cauda de um pipeline fatiado — só a última camada tem o bloco.
        if self.start_layer > 0 && self.peer_count > 1 {
            return true;
        }
        false
    }

    fn validate(&self) -> Result<()> {
        if self.model_path.trim().is_empty() {
            bail!("model_path é obrigatório");
        }
        if !PathBuf::from(&self.model_path).exists() {
            bail!("arquivo não encontrado: {}", self.model_path);
        }
        if self.start_layer > self.end_layer {
            bail!(
                "faixa de camadas inválida: {}-{}",
                self.start_layer,
                self.end_layer
            );
        }
        if self.total_layers == 0 {
            bail!("total_layers não pode ser zero");
        }
        if self.n_ctx == 0 || self.n_ctx > MAX_CONTEXT_TOKENS {
            bail!("n_ctx precisa estar entre 1 e {MAX_CONTEXT_TOKENS}");
        }
        if self.end_layer >= self.total_layers {
            bail!(
                "camada final {} fora do modelo de {} camadas",
                self.end_layer,
                self.total_layers
            );
        }
        if self.peer_count == 0 {
            bail!("peer_count precisa ser >= 1");
        }
        if self.peer_index >= self.peer_count {
            bail!(
                "peer_index {} fora de um split de {} peers",
                self.peer_index,
                self.peer_count
            );
        }
        Ok(())
    }

    /// The assignment this configuration implies, for gossip and the TUI.
    pub fn to_assignment(&self, profile: &ModelProfile) -> LayerAssignment {
        let count = self.layers_count();
        LayerAssignment {
            model: self.model_label.clone(),
            total_layers: self.total_layers,
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            layers_count: count,
            memory_used_mb: count as f64 * profile.memory_per_layer_mb + profile.overhead_mb,
            memory_allocated_mb: 0.0,
        }
    }
}

// ── Status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    /// No inference server configured on this node.
    Disabled,
    /// Process spawned, model still loading.
    Starting,
    /// `/health` answered and the model is loaded.
    Ready,
    /// Crashed past the restart budget.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub state: ServerState,
    pub detail: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub log_path: Option<String>,
    /// Layer count read back from the shard's GGUF metadata via `/health`.
    pub reported_total_layers: Option<u32>,
    pub changed_at: String,
}

impl RuntimeStatus {
    fn new(state: ServerState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            pid: None,
            restarts: 0,
            log_path: None,
            reported_total_layers: None,
            changed_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// ── Runtime ──────────────────────────────────────────────────────────────────

pub struct NodeRuntime {
    config: RwLock<Option<NodeConfig>>,
    /// Behind its own Arc so supervisor tasks, which outlive any `&self`
    /// borrow, can keep writing to it.
    status: Arc<RwLock<RuntimeStatus>>,
    assignment: Arc<RwLock<Option<LayerAssignment>>>,
    net: Arc<Network>,
    /// Bumped on every `apply`; the supervisor of an older generation exits.
    generation: watch::Sender<u64>,
    /// Certificado que o servidor de inferência apresenta, quando há um.
    /// `None` nos testes, que sobem em `127.0.0.1` e falam em claro.
    tls: Option<crate::serve::TlsServer>,
}

impl NodeRuntime {
    pub fn new(net: Arc<Network>, assignment: Arc<RwLock<Option<LayerAssignment>>>) -> Arc<Self> {
        Self::new_with_tls(net, assignment, None)
    }

    /// Como `new`, mas dizendo qual certificado o servidor de inferência
    /// apresenta. É o caminho que o `main.rs` usa; `new` fica para os testes.
    pub fn new_with_tls(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        tls: Option<crate::serve::TlsServer>,
    ) -> Arc<Self> {
        let (generation, _) = watch::channel(0u64);
        Arc::new(Self {
            config: RwLock::new(None),
            status: Arc::new(RwLock::new(RuntimeStatus::new(
                ServerState::Disabled,
                "nenhum modelo carregado",
            ))),
            assignment,
            net,
            generation,
            tls,
        })
    }

    pub async fn config(&self) -> Option<NodeConfig> {
        self.config.read().await.clone()
    }

    /// Põe uma configuração sem subir servidor de inferência nenhum.
    ///
    /// `apply` é o caminho de verdade e carrega um modelo; um teste que só
    /// precisa que o gateway leia `speculative_decoding_enabled` não tem GGUF
    /// para dar a ele.
    #[cfg(test)]
    pub(crate) async fn seed_config_for_test(&self, cfg: NodeConfig) {
        *self.config.write().await = Some(cfg);
    }

    pub async fn status(&self) -> RuntimeStatus {
        self.status.read().await.clone()
    }

    /// Label the pipeline planner matches on; falls back to the CLI default.
    pub async fn model_label(&self, fallback: &str) -> String {
        self.config
            .read()
            .await
            .as_ref()
            .map(|c| c.model_label.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Stop the current inference server and announce to peers that this
    /// node no longer serves any layers. Called before a cluster reconfiguration
    /// so no peer sends inference requests to a stale endpoint.
    pub async fn stop(&self) {
        info!("parando servidor de inferencia neste no");

        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        *self.config.write().await = None;
        *self.assignment.write().await = None;
        *self.status.write().await =
            RuntimeStatus::new(ServerState::Disabled, "parado para reconfiguracao");

        // Parar é decisão de quem operou. Esquecer a anotação é o que impede o
        // nó de voltar servindo no próximo arranque e desfazer essa decisão.
        crate::lastslice::clear();

        let _ = self.net.announce(None).await;
    }

    /// Switch this node to a new configuration: stop the current inference
    /// server, re-announce the new layer range, start the new server.
    pub async fn apply(&self, cfg: NodeConfig) -> Result<()> {
        cfg.validate()?;

        let profile = ModelProfile::guess_from_name(&cfg.model_path);
        let assignment = cfg.to_assignment(&profile);

        info!(
            "⚙️  Reconfigurando nó: {} camadas {}-{}/{} (peer {}/{})",
            cfg.model_label,
            cfg.start_layer,
            cfg.end_layer,
            cfg.total_layers,
            cfg.peer_index + 1,
            cfg.peer_count
        );

        // Bumping the generation stops the previous supervisor, which kills its
        // child on the way out.
        //
        // `send_replace`, not `send`: with no live receivers — the case on the
        // very first apply, before any supervisor exists — `send` reports an
        // error and leaves the stored value untouched, so the supervisor we are
        // about to spawn would read a stale generation and exit immediately.
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        *self.config.write().await = Some(cfg.clone());
        *self.assignment.write().await = Some(assignment.clone());
        *self.status.write().await =
            RuntimeStatus::new(ServerState::Starting, "iniciando servidor de inferência");

        // Let peers know immediately instead of waiting for the next gossip tick.
        if let Err(e) = self.net.announce(Some(&assignment)).await {
            warn!("announce após reconfiguração falhou: {e}");
        }

        // Anotada já aqui porque um processo que morre de segfault não executa
        // código nenhum, e sem anotação toda queda tirava o nó do cluster para
        // sempre - o Docker religava o container e ele voltava sem fatia,
        // abrindo um buraco de camadas que só aparecia na requisição seguinte,
        // como "Gap at layer N".
        //
        // Mas anotada como `Pending`, e não como servindo: neste ponto o modelo
        // não carregou uma única vez. Anotar aqui o que só o `/health` sabe era
        // o laço infinito de 2026-08-15 - uma fatia grande demais matava o
        // processo antes de carregar, o Docker religava, e o arranque
        // ressuscitava a mesma fatia para sempre. Quem promove a `Serving` é
        // `spawn_health_watch`.
        if let Err(e) = crate::lastslice::save(&cfg, crate::lastslice::SliceState::Pending) {
            warn!("não deu para anotar a fatia aplicada: {e}");
        }

        self.spawn_supervisor(cfg, generation);
        Ok(())
    }

    /// Registra que a fatia anotada não voltou porque nunca chegou a carregar.
    ///
    /// O arranque que recusa retomá-la tem que dizer isso: o painel de um nó
    /// vazio e calado é indistinguível do de uma máquina que nunca recebeu
    /// fatia nenhuma, e esta é a máquina cuja última tentativa derrubou o
    /// processo. É o que o operador precisa ler para saber que tem de dar
    /// menos camadas a ela.
    pub async fn note_unloadable_slice(&self, cfg: &NodeConfig) {
        let detail = format!(
            "a fatia {} {}-{} não chegou a carregar na tentativa anterior e não foi restaurada",
            cfg.model_label, cfg.start_layer, cfg.end_layer
        );
        warn!("⚠️  {detail}");
        *self.status.write().await = RuntimeStatus::new(ServerState::Disabled, detail);
    }

    /// Start (or restart) the Rust inference server for `generation`.
    fn spawn_supervisor(&self, cfg: NodeConfig, generation: u64) {
        let gen_rx = self.generation.subscribe();
        let status = StatusWriter {
            status: self.status_handle(),
        };
        let port = cfg.inference_port;
        let cfg_for_health = cfg.clone();
        // Só o motor sabe se o GGUF traz o bloco `nextn`; o anúncio lê daqui.
        // Ver `Network::mtp_ready`.
        let mtp_ready = self.net.mtp_ready_handle();
        // A porta de inferência só recebe peer: nada de redirecionar para
        // https quem chegar em claro, porque não há navegador aqui.
        let tls = self.tls.as_ref().map(|t| t.so_de_peers());

        tokio::spawn(async move {
            status.set_log(&log_file_path(port).to_string_lossy()).await;
            append_log(port, "supervisor iniciado");
            let mut restarts = 0u32;
            loop {
                if *gen_rx.borrow() != generation {
                    return; // superseded by a newer configuration
                }
                // A cada tentativa, não só antes do laço: quem segura a porta
                // pode ter aparecido depois, ou não ter morrido na primeira
                // investida. Limpar uma vez só fazia as outras nove tentativas
                // baterem na mesma porta ocupada.
                let porta_presa = free_port(port).await.err();
                if restarts > 0 {
                    warn!(
                        "reiniciando servidor de inferência (tentativa {}/{})",
                        restarts, MAX_RESTARTS
                    );
                    sleep(Duration::from_secs(5)).await;
                    if *gen_rx.borrow() != generation {
                        return;
                    }
                }

                // Porta presa não pode passar em silêncio: o servidor vai
                // morrer com "Address already in use" e, enquanto o orçamento
                // de tentativas não esgota, o operador via só "carregando" e a
                // TUI seguia anunciando saúde. O estado visível diz o motivo
                // desde a primeira tentativa (mesmo padrão de
                // `note_unloadable_slice`).
                match &porta_presa {
                    Some(motivo) => {
                        append_log(port, motivo);
                        status
                            .set(
                                ServerState::Failed,
                                format!("{motivo}; tentativa {}/{MAX_RESTARTS}", restarts + 1),
                                restarts,
                            )
                            .await;
                    }
                    None => {
                        append_log(
                            port,
                            &format!(
                                "carregando {} (tentativa {}/{MAX_RESTARTS})",
                                short_path(&cfg.model_path),
                                restarts + 1
                            ),
                        );
                        status
                            .set(
                                ServerState::Starting,
                                format!("carregando {}", short_path(&cfg.model_path)),
                                restarts,
                            )
                            .await;
                    }
                }

                match crate::infer_server::run(
                    cfg.clone(),
                    gen_rx.clone(),
                    generation,
                    mtp_ready.clone(),
                    tls.clone(),
                )
                .await
                {
                    Ok(()) => {
                        info!("servidor de inferência encerrado graciosamente");
                        append_log(port, "encerrado graciosamente");
                        mtp_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    Err(e) => {
                        warn!("servidor de inferência encerrou com erro: {e}");
                        mtp_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                        append_log(port, &format!("encerrou com erro: {e}"));
                        restarts += 1;
                        if restarts >= MAX_RESTARTS {
                            // Uma fatia que chegou a servir (e por isso está
                            // anotada como `Serving`) e depois quebrou
                            // MAX_RESTARTS vezes seguidas não pode voltar no
                            // próximo arranque: o orçamento de tentativas é de
                            // memória e não atravessa um reinício.
                            crate::lastslice::clear();
                            let detail = match &porta_presa {
                                Some(motivo) => {
                                    format!("desistiu depois de {MAX_RESTARTS} tentativas: {motivo}")
                                }
                                None => format!("encerrou {MAX_RESTARTS}x: {e}"),
                            };
                            append_log(port, &detail);
                            status.set(ServerState::Failed, detail, restarts).await;
                            return;
                        }
                    }
                }
            }
        });

        self.spawn_health_watch(cfg_for_health, generation);
    }

    /// Poll the inference server until it answers, then mark the node ready and
    /// correct `total_layers` from the shard's GGUF metadata.
    fn spawn_health_watch(&self, cfg: NodeConfig, generation: u64) {
        let port = cfg.inference_port;
        let status = StatusWriter {
            status: self.status_handle(),
        };
        let assignment = self.assignment.clone();
        let gen_rx = self.generation.subscribe();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        tokio::spawn(async move {
            let url = format!("http://127.0.0.1:{port}/health");
            loop {
                sleep(Duration::from_secs(3)).await;
                if *gen_rx.borrow() != generation {
                    return;
                }

                let Ok(resp) = http.get(&url).send().await else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(body) = resp.json::<serde_json::Value>().await else {
                    continue;
                };
                if !body
                    .get("model_loaded")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    continue;
                }

                let reported = body
                    .get("original_block_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                // Shards written by split_model.py carry the real layer count;
                // trust it over the name-based guess.
                if let Some(total) = reported {
                    let mut w = assignment.write().await;
                    if let Some(ref mut a) = *w {
                        if a.total_layers != total {
                            info!(
                                "total_layers corrigido: {} → {total} (metadata GGUF)",
                                a.total_layers
                            );
                            a.total_layers = total;
                        }
                    }
                }

                // O único ponto do código que prova que a fatia está no ar:
                // `/health` respondeu `model_loaded`. Só daqui em diante ela
                // pode ser retomada num arranque seguinte.
                if let Err(e) = crate::lastslice::save(&cfg, crate::lastslice::SliceState::Serving)
                {
                    warn!("não deu para anotar a fatia servida: {e}");
                }

                append_log(port, "modelo carregado (/health respondeu model_loaded)");
                status.set_ready(reported).await;
                return;
            }
        });
    }

    fn status_handle(&self) -> Arc<RwLock<RuntimeStatus>> {
        self.status.clone()
    }
}

/// Onde ficam os logs do servidor de inferência desta porta.
///
/// Um arquivo por porta, sob o project root, porque duas instâncias na mesma
/// máquina (`POM_HOME` diferente) não podem escrever no mesmo arquivo.
pub fn log_file_path(port: u16) -> PathBuf {
    crate::config::project_root().join(format!("infer_server_{port}.log"))
}

/// Anexa uma linha ao log do servidor de inferência.
///
/// A TUI limpa a tela a cada `--refresh`, então o stdout do processo é o pior
/// lugar possível para o histórico de arranque e queda do servidor: quando o
/// operador vai olhar, a linha já rolou. O arquivo é o que sobra, e é ele que
/// `RuntimeStatus::log_path` aponta.
fn append_log(port: u16, line: &str) {
    use std::io::Write;
    let path = log_file_path(port);
    let stamped = format!("{} {line}\n", chrono::Utc::now().to_rfc3339());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match opened {
        Ok(mut f) => {
            if let Err(e) = f.write_all(stamped.as_bytes()) {
                warn!("não deu para escrever em {}: {e}", path.display());
            }
        }
        Err(e) => warn!("não deu para abrir {}: {e}", path.display()),
    }
}

fn short_path(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Can the inference server still take this port?
///
/// It binds the dual-stack wildcard `::`, so that is what has to be probed: a
/// `0.0.0.0` probe succeeds while a `::` listener already owns the port on some
/// stacks, and the supervisor would then start a child doomed to `address
/// already in use`. Falls back to IPv4 on a host with IPv6 turned off.
fn port_is_free(port: u16) -> bool {
    use std::net::TcpListener;
    match TcpListener::bind(("::", port)) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => false,
        Err(_) => TcpListener::bind(("0.0.0.0", port)).is_ok(),
    }
}

/// Os PIDs que seguram `port`, lidos da saída do `netstat -ano` do Windows.
///
/// Separada da chamada de processo justamente para poder ser testada em
/// qualquer sistema: é a parte que erra calado, e um PID lido errado aqui
/// vira um `taskkill` em processo inocente.
///
/// Formato das linhas (o estado some nas de UDP, mas o PID é sempre o último
/// campo):
///
/// ```text
///   TCP    0.0.0.0:8181     0.0.0.0:0    LISTENING    1234
///   TCP    [::]:8181        [::]:0       LISTENING    1234
///   UDP    0.0.0.0:8181     *:*                       5678
/// ```
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat_pids(saida: &str, port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    for linha in saida.lines() {
        let campos: Vec<&str> = linha.split_whitespace().collect();
        if campos.len() < 4 {
            continue;
        }
        // O endereço local é o segundo campo. Comparar a porta como número, e
        // não o sufixo textual, evita casar 18181 com 8181.
        let local = campos[1];
        let Some((_, porta)) = local.rsplit_once(':') else {
            continue;
        };
        if porta.parse::<u16>() != Ok(port) {
            continue;
        }
        if let Ok(pid) = campos[campos.len() - 1].parse::<u32>() {
            // PID 0 é o processo ocioso do sistema: não é dono de nada que
            // possamos matar, e mandar um taskkill nele só gera erro.
            if pid != 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Quem está segurando `port`, perguntado do jeito de cada sistema.
async fn pids_segurando_porta(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    {
        let out = tokio::process::Command::new("netstat")
            .args(["-ano"])
            .output()
            .await;
        let Ok(out) = out else {
            warn!("nao foi possivel executar netstat para verificar a porta {port}");
            return Vec::new();
        };
        return parse_netstat_pids(&String::from_utf8_lossy(&out.stdout), port);
    }

    #[cfg(not(windows))]
    {
        let out = tokio::process::Command::new("lsof")
            .args(["-ti", &format!(":{port}")])
            .output()
            .await;
        let Ok(out) = out else {
            warn!("nao foi possivel executar lsof para verificar a porta {port}");
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect()
    }
}

/// Mata `pid` à força.
async fn matar_pid(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .await;

    #[cfg(not(windows))]
    let _ = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output()
        .await;
}

/// Kill any process holding `port` so the inference server can bind.
///
/// Unix: `lsof -ti :PORT` acha o PID e leva SIGKILL. Windows: `netstat -ano` e
/// `taskkill /F /PID`, que são os equivalentes que vêm com o sistema. Depois,
/// nos dois, a porta é sondada por até 5 segundos até ficar livre.
async fn free_port(port: u16) -> Result<(), String> {
    // Fast path: if we can bind, the port is already free.
    if port_is_free(port) {
        return Ok(());
    }

    info!("porta {port} em uso — procurando processo para matar");

    let pids = pids_segurando_porta(port).await;
    if pids.is_empty() {
        let motivo =
            format!("porta {port} ocupada e o processo que a segura não pôde ser identificado");
        warn!("{motivo}");
        return Err(motivo);
    }

    for pid in pids {
        // Skip our own process - never kill ourselves.
        if pid == std::process::id() {
            continue;
        }
        info!("matando processo {pid} que ocupa a porta {port}");
        matar_pid(pid).await;
    }

    // Poll for up to 5 seconds until the port is free.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if port_is_free(port) {
            info!("porta {port} liberada");
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            let motivo = format!("porta {port} continua ocupada 5s depois de matar o ocupante");
            warn!("{motivo}");
            return Err(motivo);
        }
        sleep(Duration::from_millis(200)).await;
    }
}

// ── Status plumbing ──────────────────────────────────────────────────────────
//
// Supervisor tasks outlive the borrow of `&self`, so they need an owned handle
// to the status cell. The runtime keeps that cell behind its own Arc.

struct StatusWriter {
    status: Arc<RwLock<RuntimeStatus>>,
}

impl StatusWriter {
    async fn set(&self, state: ServerState, detail: String, restarts: u32) {
        let mut s = self.status.write().await;
        s.state = state;
        s.detail = detail;
        s.restarts = restarts;
        s.changed_at = chrono::Utc::now().to_rfc3339();
    }

    async fn set_log(&self, path: &str) {
        self.status.write().await.log_path = Some(path.to_string());
    }

    async fn set_ready(&self, reported_total_layers: Option<u32>) {
        let mut s = self.status.write().await;
        s.state = ServerState::Ready;
        s.detail = "modelo carregado".to_string();
        s.reported_total_layers = reported_total_layers;
        s.changed_at = chrono::Utc::now().to_rfc3339();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Porta presa é falha visível, não espera silenciosa.
    ///
    /// Com o ocupante não identificável (ou não matável, como aqui: quem segura
    /// a porta é o próprio processo de teste, e o supervisor nunca se mata), o
    /// servidor morre com "Address already in use" a cada tentativa. Antes, o
    /// operador via só "carregando" até `MAX_RESTARTS` esgotar, com a TUI
    /// anunciando saúde o tempo todo; o peer só quebrava na hora da inferência.
    /// O motivo tem que estar no status desde a primeira tentativa - e o
    /// arquivo de log tem que existir, porque a TUI limpa a tela a cada
    /// `--refresh` e o stdout não sobrevive.
    #[tokio::test]
    async fn a_porta_presa_aparece_no_status_e_no_log() {
        let dir = std::env::temp_dir().join(format!("pom-porta-presa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("POM_HOME", &dir);

        // Alguém já está com a porta, e é este processo: `free_port` nunca se
        // mata, então a porta segue presa como no defeito real.
        // No curinga dual-stack, que é o que `port_is_free` sonda e o que o
        // servidor de inferência liga.
        let ocupante = match tokio::net::TcpListener::bind("[::]:0").await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap(),
        };
        let port = ocupante.local_addr().unwrap().port();

        let net = Arc::new(
            Network::new("no".into(), "pk".into(), 0, vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));
        let mut c = cfg();
        c.inference_port = port;
        rt.spawn_supervisor(c, 0);

        let mut status = rt.status().await;
        for _ in 0..80 {
            sleep(Duration::from_millis(250)).await;
            status = rt.status().await;
            if status.state == ServerState::Failed {
                break;
            }
        }

        let log = log_file_path(port);
        let conteudo = std::fs::read_to_string(&log).unwrap_or_default();
        std::env::remove_var("POM_HOME");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            status.state,
            ServerState::Failed,
            "porta presa não pode ficar em Starting: {}",
            status.detail
        );
        assert!(
            status.detail.contains(&port.to_string()),
            "o status tem que nomear a porta, e diz: {}",
            status.detail
        );
        assert_eq!(
            status.log_path.as_deref(),
            Some(log.to_string_lossy().as_ref()),
            "o status tem que apontar o arquivo de log"
        );
        assert!(
            conteudo.contains("supervisor iniciado"),
            "o arquivo de log tem que existir e ter as linhas do supervisor: {conteudo:?}"
        );
    }

    /// A TUI mostra o estado do servidor, não um rótulo fixo.
    #[test]
    fn o_rotulo_da_tui_muda_com_o_estado() {
        let mut s = RuntimeStatus::new(ServerState::Failed, "porta 8181 continua ocupada");
        assert!(crate::runtime_mem_source(&s).contains("porta 8181"));
        s.state = ServerState::Ready;
        assert_eq!(crate::runtime_mem_source(&s), "inference_server");
    }

    fn cfg() -> NodeConfig {
        NodeConfig {
            model_path: "/nonexistent/model.gguf".into(),
            model_label: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            max_sessions: 1,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            n_batch: 512,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: false,
            mtp_draft_tokens: 3,
            mtp_min_probability: 0.0,
        }
    }

    /// A outra metade da lei: quem promove a anotação a `Serving` é o
    /// `/health` respondendo `model_loaded`, e mais ninguém.
    ///
    /// Sem esta promoção o `apply` anotaria `Pending` para sempre e a garantia
    /// de `c7b70d8` morreria calada - um nó que estava servindo saudável e caiu
    /// voltaria vazio, abrindo um buraco de camadas no cluster. O teste sobe um
    /// `/health` de mentira porque não há GGUF que carregue nesta máquina; o
    /// que ele prova é o contrato entre o watch e o disco, não o llama.cpp.
    #[tokio::test]
    async fn o_health_e_quem_promove_a_anotacao_para_servindo() {
        let dir = std::env::temp_dir().join(format!("pom-health-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("pom.json");
        std::env::set_var("POM_CONFIG", &cfg_path);
        let anotacao = crate::lastslice::path();
        std::fs::remove_file(&anotacao).ok();

        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({ "model_loaded": true }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let net = Arc::new(
            Network::new("no".into(), "pk".into(), 0, vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));

        let mut c = cfg();
        c.inference_port = port;
        // Como o `apply` deixa: aplicada, ainda sem ter carregado.
        crate::lastslice::save(&c, crate::lastslice::SliceState::Pending).unwrap();
        rt.spawn_health_watch(c, 0);

        // O watch dorme 3 s antes da primeira sondagem.
        for _ in 0..40 {
            sleep(Duration::from_millis(250)).await;
            if rt.status().await.state == ServerState::Ready {
                break;
            }
        }

        assert_eq!(
            rt.status().await.state,
            ServerState::Ready,
            "o /health subiu"
        );
        assert!(
            matches!(
                crate::lastslice::take(),
                crate::lastslice::LastSlice::Serving(_)
            ),
            "uma fatia vista carregada tem que voltar no próximo arranque"
        );

        std::env::remove_var("POM_CONFIG");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// O arranque que recusa retomar uma fatia tem que dizer isso ao operador.
    ///
    /// Sem esta linha o nó sobe vazio e calado: o painel diria só "nenhum
    /// modelo carregado", indistinguível de uma máquina que nunca recebeu
    /// fatia nenhuma — e a máquina que acabou de derrubar o processo tentando
    /// carregar é justamente a que precisa ser explicada.
    #[tokio::test]
    async fn a_fatia_que_nao_carregou_aparece_no_status_com_a_razao() {
        let net = Arc::new(
            Network::new("no".into(), "pk".into(), 0, vec![], None, None, None)
                .await
                .expect("socket UDP em porta efêmera"),
        );
        let rt = NodeRuntime::new(net, Arc::new(RwLock::new(None)));

        rt.note_unloadable_slice(&cfg()).await;

        let s = rt.status().await;
        assert_eq!(s.state, ServerState::Disabled, "o nó não serve nada");
        assert!(
            s.detail.contains("qwen35") && s.detail.contains("0-15"),
            "o status tem que nomear a fatia recusada, e diz: {}",
            s.detail
        );
        assert!(
            s.detail.contains("carregar"),
            "e dizer por que ela não voltou, e diz: {}",
            s.detail
        );
    }

    /// O callback de captura do residual custa ~8 ms por token (medido em
    /// 2026-08-12: 7,8 e 8,2 ms no hop que o instala, contra 0,14 e 0,67 no que
    /// não instala). Quem serve a última camada amostra tokens e não tem para
    /// quem exportar — instalar ali é pagar 11% do orçamento por nada. A
    /// derivação tem que valer também para o nó subido por linha de comando,
    /// que não passa por `ApplyRequest` e sempre mandava `no_embeddings: false`.
    #[test]
    fn a_slice_that_serves_the_last_layer_never_exports_the_residual() {
        let mut c = cfg();
        assert!(c.needs_residual_export(), "0-15 de 32 tem sucessor");

        c.start_layer = 16;
        c.end_layer = 31;
        assert!(
            !c.needs_residual_export(),
            "16-31 de 32 amostra tokens, não exporta estado"
        );
    }

    #[test]
    fn single_peer_and_the_operator_kill_switch_still_win() {
        let mut c = cfg();
        c.single_peer = true;
        assert!(!c.needs_residual_export());

        let mut c = cfg();
        c.no_embeddings = true;
        assert!(!c.needs_residual_export());
    }

    /// O último da divisão pode não ser o último em camadas quando o plano vem
    /// da interface com `peer_index`/`peer_count`; os dois caminhos valem.
    #[test]
    fn the_last_peer_of_the_split_also_skips_it() {
        let mut c = cfg();
        c.peer_index = 1;
        c.peer_count = 2;
        c.end_layer = 15;
        assert!(!c.needs_residual_export());
    }

    /// O MTP só faz sentido em quem serve o modelo inteiro. O bloco `nextn`
    /// mora na última camada e quem monta o lote é a cabeça: numa fatia, as
    /// duas pontas são máquinas diferentes.
    #[test]
    fn mtp_only_where_one_machine_serves_the_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = true;
        c.start_layer = 0;
        c.end_layer = 31;
        c.total_layers = 32;
        c.peer_count = 1;
        c.peer_index = 0;
        assert!(c.mtp_effective(), "0-31 de 32, sozinho: é o modelo inteiro");
    }

    #[test]
    fn mtp_only_on_the_tail_of_a_split_and_on_a_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = true;
        c.total_layers = 32;
        c.peer_count = 2;

        c.start_layer = 0;
        c.end_layer = 15;
        c.peer_index = 0;
        assert!(!c.mtp_effective(), "a cabeça não tem o bloco nextn");

        c.start_layer = 16;
        c.end_layer = 31;
        c.peer_index = 1;
        assert!(
            c.mtp_effective(),
            "a cauda de um pipeline fatiado tem o bloco nextn — é aqui que o rascunho nasce"
        );
    }

    /// O interruptor desligado vence tudo: ligar o MTP muda o grafo de toda
    /// decodificação (`llama_set_embeddings_nextn` no contexto alvo), então
    /// ele não pode ser inferido de "o modelo tem o bloco".
    #[test]
    fn mtp_off_stays_off_on_a_whole_model() {
        let mut c = cfg();
        c.mtp_enabled = false;
        c.start_layer = 0;
        c.end_layer = 31;
        c.total_layers = 32;
        c.peer_count = 1;
        assert!(!c.mtp_effective());
    }

    #[test]
    fn rejects_missing_file() {
        assert!(cfg().validate().is_err());
    }

    #[test]
    fn rejects_inverted_range() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.start_layer = 20;
        c.end_layer = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_range_past_end_of_model() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.end_layer = 40;
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_a_250k_context_but_refuses_a_larger_configured_limit() {
        let mut c = cfg();
        c.model_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/inference/runtime.rs")
            .to_string_lossy()
            .into_owned();
        c.n_ctx = 250_000;
        assert!(c.validate().is_ok());

        c.n_ctx = 250_001;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_peer_index_outside_split() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.peer_index = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn layers_count_is_inclusive() {
        assert_eq!(cfg().layers_count(), 16);
    }

    /// A saída do `netstat -ano` é a única pista de quem segura a porta no
    /// Windows, e um PID lido errado ali vira um `taskkill` em processo
    /// inocente. O teste roda em qualquer sistema de propósito: o parser é
    /// código comum, só a chamada do processo é que muda por plataforma.
    #[test]
    fn netstat_da_os_pids_da_porta_pedida() {
        let saida = "\
Conexoes ativas

  Proto  Endereco local         Endereco externo       Estado           PID
  TCP    0.0.0.0:8181           0.0.0.0:0              LISTENING       1234
  TCP    [::]:8181              [::]:0                 LISTENING       1234
  TCP    127.0.0.1:52777        0.0.0.0:0              LISTENING       9999
  UDP    0.0.0.0:8181          *:*                                     5678
";
        // 1234 aparece duas vezes (IPv4 e IPv6) e volta uma vez só.
        assert_eq!(parse_netstat_pids(saida, 8181), vec![1234, 5678]);
        assert_eq!(parse_netstat_pids(saida, 52777), vec![9999]);
        assert!(parse_netstat_pids(saida, 8080).is_empty());
    }

    /// Comparar a porta como texto casaria 18181 com 8181 e mataria o nó
    /// vizinho da mesma máquina.
    #[test]
    fn netstat_nao_confunde_porta_com_sufixo() {
        let saida = "  TCP    0.0.0.0:18181    0.0.0.0:0    LISTENING    4242\n";
        assert!(parse_netstat_pids(saida, 8181).is_empty());
        assert_eq!(parse_netstat_pids(saida, 18181), vec![4242]);
    }

    /// PID 0 é o processo ocioso do sistema: mandar taskkill nele só gera erro.
    #[test]
    fn netstat_ignora_pid_zero() {
        let saida = "  TCP    0.0.0.0:8181    0.0.0.0:0    LISTENING    0\n";
        assert!(parse_netstat_pids(saida, 8181).is_empty());
    }
}
