//! HTTP inference server — the Rust replacement for `inference_server.py`.
//!
//! Exposes:
//!
//!   POST /v1/prefill          — one chunk of the prompt
//!   POST /v1/step             — one autoregressive token step (optionally
//!                                speculative — see `step_blocking`)
//!   POST /v1/session/release  — free a session's KV slot
//!   POST /v1/session/rollback — discard an unconfirmed speculative batch
//!   GET  /health              — readiness probe
//!
//! Wire format is defined in [`crate::wire`] (binary framing, f16 on the wire).
//! Decode calls run in `spawn_blocking` so they don't starve the tokio runtime.

use crate::llm::{sample_token_profiled, EngineConfig, LlmEngine, SamplingConfig};
use crate::pipeline::PipelineStep;
use crate::runtime::NodeConfig;
use crate::wire::{self, WireDtype};
use anyhow::{bail, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

const SESSION_IDLE_TIMEOUT_SECS: u64 = 900;
/// Upper bound for one binary f16 hidden-state frame received from a peer.
///
/// A normal 512-token chunk with a 4096-wide hidden state is 4 MiB before its
/// small wire header. Keeping this limit explicit at 16 MiB allows the
/// documented 8 MiB frame while still rejecting unbounded peer input.
const INFERENCE_FRAME_BODY_LIMIT: usize = 16 * 1024 * 1024;
const BATCH_CHILD_HEADER: &str = "x-pom-batch-child";

/// Timings measured on the inference node itself. The names deliberately name
/// the boundary being measured instead of guessing whether a CUDA call is
/// asynchronous. In particular, `residual_ms` contains the GPU -> host copy
/// and the CUDA stream synchronization performed by `take_residual()`.
#[derive(Default)]
struct NodeTimings {
    frame_decode_us: u64,
    input_prepare_us: u64,
    queue_us: u64,
    /// Espera pelo mutex do motor, antes de qualquer trabalho.
    ///
    /// O `llama_context` não é seguro para uso concorrente, então todo decode
    /// deste nó serializa aqui. Sem este contador a espera caía no vão entre a
    /// entrada do handler e o primeiro medidor, e aparecia na tela só como
    /// "não atribuído" - 194 ms por token numa medição de 2026-08-14, 63% do
    /// tempo da requisição, invisível.
    lock_wait_us: u64,
    setup_us: u64,
    decode_us: u64,
    backend_enqueue_us: u64,
    backend_completion_us: u64,
    residual_capture_d2h_sync_us: u64,
    residual_us: u64,
    logits_access_us: u64,
    logits_clone_us: u64,
    repetition_penalty_us: u64,
    softmax_us: u64,
    top_k_us: u64,
    top_p_us: u64,
    selection_us: u64,
    token_text_us: u64,
    sampling_us: u64,
    encode_us: u64,
    tp: Option<crate::tp::TpDecodeTelemetry>,
}

impl NodeTimings {
    fn json(&self) -> Value {
        // Counters are kept in microseconds so a stage costing less than a
        // millisecond is not truncated to zero on every call and lost in the
        // sum. The wire stays in milliseconds, which is what the screen reads.
        let ms = |us: u64| us / 1_000;
        json!({
            "frame_decode_ms": ms(self.frame_decode_us),
            "input_prepare_ms": ms(self.input_prepare_us),
            "queue_ms": ms(self.queue_us),
            "lock_wait_ms": ms(self.lock_wait_us),
            "setup_ms": ms(self.setup_us),
            "decode_ms": ms(self.decode_us),
            "backend_enqueue_ms": ms(self.backend_enqueue_us),
            "backend_completion_ms": ms(self.backend_completion_us),
            "residual_capture_d2h_sync_ms": ms(self.residual_capture_d2h_sync_us),
            "residual_ms": ms(self.residual_us),
            "logits_access_ms": ms(self.logits_access_us),
            "logits_clone_ms": ms(self.logits_clone_us),
            "repetition_penalty_ms": ms(self.repetition_penalty_us),
            "softmax_ms": ms(self.softmax_us),
            "top_k_ms": ms(self.top_k_us),
            "top_p_ms": ms(self.top_p_us),
            "selection_ms": ms(self.selection_us),
            "token_text_ms": ms(self.token_text_us),
            "sampling_ms": ms(self.sampling_us),
            "encode_ms": ms(self.encode_us),
            "tp": self
                .tp
                .as_ref()
                .map(|tp| serde_json::to_value(tp).unwrap_or(Value::Null)),
        })
    }
}

struct TimedReply {
    bytes: Vec<u8>,
    timings: NodeTimings,
}

struct PrefillJob {
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    chunk_tokens: Option<Vec<i32>>,
    queued_at: Instant,
}

struct StepJob {
    sid_key: String,
    is_last: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    speculate: Option<Speculate>,
    incoming_draft: Option<Vec<i32>>,
    batch_tokens: Option<Vec<i32>>,
    mtp_k: Option<u32>,
    queued_at: Instant,
}

enum InferenceCommand {
    Prefill {
        job: PrefillJob,
        reply: oneshot::Sender<Result<TimedReply>>,
        _permit: OwnedSemaphorePermit,
    },
    Step {
        job: StepJob,
        reply: oneshot::Sender<Result<TimedReply>>,
        _permit: OwnedSemaphorePermit,
    },
}

impl InferenceCommand {
    fn into_queue_item(self) -> crate::scheduler::QueueItem<Self> {
        let (session_id, class, tokens, queued_at) = match &self {
            Self::Prefill { job, .. } => (
                job.sid_key.clone(),
                if job.tensor.is_some() {
                    crate::scheduler::WorkClass::PrefillHidden
                } else {
                    crate::scheduler::WorkClass::PrefillTokens
                },
                job.tensor
                    .as_ref()
                    .map(|(_, shape)| shape[0])
                    .unwrap_or(job.chunk_size as usize)
                    .max(1),
                job.queued_at,
            ),
            Self::Step { job, .. } => (
                job.sid_key.clone(),
                if job.tensor.is_some() {
                    crate::scheduler::WorkClass::DecodeHidden
                } else {
                    crate::scheduler::WorkClass::DecodeTokens
                },
                job.tensor
                    .as_ref()
                    .map(|(_, shape)| shape[0])
                    .unwrap_or(1)
                    .max(1),
                job.queued_at,
            ),
        };
        crate::scheduler::QueueItem {
            session_id,
            class,
            tokens,
            enqueued_at: queued_at,
            payload: self,
        }
    }
}

#[derive(Debug)]
enum SchedulerSubmitError {
    Full { limit: usize },
    Closed,
}

#[derive(Clone)]
struct SchedulerHandle {
    tx: mpsc::Sender<InferenceCommand>,
    limit: usize,
    admission: Arc<Semaphore>,
    stats: Arc<SchedulerStats>,
    config: crate::scheduler::SchedulerConfig,
}

#[derive(Default)]
struct SchedulerStats {
    queue_depth: AtomicUsize,
    last_batch_size: AtomicUsize,
    last_batch_tokens: AtomicUsize,
    total_batches: AtomicU64,
    total_items: AtomicU64,
}

impl SchedulerStats {
    fn json(&self, config: &crate::scheduler::SchedulerConfig) -> Value {
        json!({
            "enabled": config.enabled,
            "queue_depth": self.queue_depth.load(Ordering::Relaxed),
            "queue_limit": config.max_queue_requests,
            "batch_window_ms": config.batch_window.as_millis(),
            "max_batch_sequences": config.max_batch_sequences,
            "max_batch_tokens": config.max_batch_tokens,
            "last_batch_size": self.last_batch_size.load(Ordering::Relaxed),
            "last_batch_tokens": self.last_batch_tokens.load(Ordering::Relaxed),
            "total_batches": self.total_batches.load(Ordering::Relaxed),
            "total_items": self.total_items.load(Ordering::Relaxed),
        })
    }
}

impl SchedulerHandle {
    fn submit_prefill(
        &self,
        job: PrefillJob,
    ) -> std::result::Result<oneshot::Receiver<Result<TimedReply>>, SchedulerSubmitError> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| SchedulerSubmitError::Full { limit: self.limit })?;
        let (reply, receiver) = oneshot::channel();
        self.tx
            .try_send(InferenceCommand::Prefill {
                job,
                reply,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    SchedulerSubmitError::Full { limit: self.limit }
                }
                mpsc::error::TrySendError::Closed(_) => SchedulerSubmitError::Closed,
            })?;
        Ok(receiver)
    }

    fn submit_step(
        &self,
        job: StepJob,
    ) -> std::result::Result<oneshot::Receiver<Result<TimedReply>>, SchedulerSubmitError> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| SchedulerSubmitError::Full { limit: self.limit })?;
        let (reply, receiver) = oneshot::channel();
        self.tx
            .try_send(InferenceCommand::Step {
                job,
                reply,
                _permit: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    SchedulerSubmitError::Full { limit: self.limit }
                }
                mpsc::error::TrySendError::Closed(_) => SchedulerSubmitError::Closed,
            })?;
        Ok(receiver)
    }
}

#[derive(Clone)]
struct InferenceAppState {
    state: SharedState,
    scheduler: SchedulerHandle,
    /// TP exposes one global decode/barrier identity per process. Serialize
    /// complete inbound batches so a second request cannot replace that id
    /// while the first batch is waiting for its workers.
    tp_batch_gate: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait::async_trait]
impl crate::relay::InProcessInference for InferenceAppState {
    async fn dispatch(&self, path: &str, body: Bytes) -> Result<Response> {
        let headers = axum::http::HeaderMap::new();
        Ok(match path {
            "/v1/prefill" => handle_prefill(State(self.clone()), headers, body).await,
            "/v1/step" => handle_step(State(self.clone()), headers, body).await,
            _ => {
                return Err(anyhow::anyhow!(
                    "rota de inferência em processo desconhecida: {path}"
                ))
            }
        })
    }
}

fn spawn_scheduler(state: SharedState, cfg: crate::scheduler::SchedulerConfig) -> SchedulerHandle {
    let limit = cfg.max_queue_requests;
    let (tx, rx) = mpsc::channel(limit);
    let admission = Arc::new(Semaphore::new(limit));
    let stats = Arc::new(SchedulerStats::default());
    let handle = SchedulerHandle {
        tx,
        limit,
        admission,
        stats: stats.clone(),
        config: cfg.clone(),
    };
    tokio::spawn(run_scheduler(rx, state, cfg, stats));
    handle
}

async fn run_scheduler(
    mut rx: mpsc::Receiver<InferenceCommand>,
    state: SharedState,
    cfg: crate::scheduler::SchedulerConfig,
    stats: Arc<SchedulerStats>,
) {
    let mut queue = crate::scheduler::SchedulerQueue::new(cfg.clone());
    loop {
        if queue.is_empty() {
            let Some(command) = rx.recv().await else {
                return;
            };
            let _ = queue.try_push(command.into_queue_item());
        }

        while queue.len() < cfg.max_queue_requests {
            let Ok(command) = rx.try_recv() else {
                break;
            };
            if queue.try_push(command.into_queue_item()).is_err() {
                unreachable!("a fila foi verificada antes de inserir o comando");
            }
        }
        stats.queue_depth.store(queue.len(), Ordering::Relaxed);

        if cfg.batch_window > std::time::Duration::ZERO && queue.len() < cfg.max_batch_sequences {
            tokio::time::sleep(cfg.batch_window).await;
            while queue.len() < cfg.max_queue_requests {
                let Ok(command) = rx.try_recv() else {
                    break;
                };
                if queue.try_push(command.into_queue_item()).is_err() {
                    unreachable!("a fila foi verificada antes de inserir o comando");
                }
            }
            stats.queue_depth.store(queue.len(), Ordering::Relaxed);
        }

        let Some(class) = queue.front_class() else {
            continue;
        };
        let selection = queue.take_batch(Instant::now(), class);
        if selection.items.is_empty() {
            continue;
        }
        stats.queue_depth.store(queue.len(), Ordering::Relaxed);
        stats
            .last_batch_size
            .store(selection.items.len(), Ordering::Relaxed);
        stats
            .last_batch_tokens
            .store(selection.total_tokens, Ordering::Relaxed);
        stats.total_batches.fetch_add(1, Ordering::Relaxed);
        stats
            .total_items
            .fetch_add(selection.items.len() as u64, Ordering::Relaxed);
        let state = state.clone();
        let enabled = cfg.enabled;
        let _ = tokio::task::spawn_blocking(move || {
            execute_scheduled_batch(state, selection.items, enabled);
        })
        .await;
    }
}

fn execute_scheduled_batch(
    state: SharedState,
    items: Vec<crate::scheduler::QueueItem<InferenceCommand>>,
    enabled: bool,
) {
    if !enabled || items.len() == 1 {
        for item in items {
            execute_one_command(state.clone(), item.payload);
        }
        return;
    }
    let class = items[0].class;
    match class {
        crate::scheduler::WorkClass::PrefillTokens | crate::scheduler::WorkClass::PrefillHidden => {
            let mut jobs = Vec::with_capacity(items.len());
            for item in items {
                match item.payload {
                    InferenceCommand::Prefill { job, reply, .. } => jobs.push((job, reply)),
                    InferenceCommand::Step { reply, .. } => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "scheduler recebeu fase de inferência incompatível"
                        )));
                    }
                }
            }
            for (reply, result) in prefill_batch_blocking(state, jobs) {
                let _ = reply.send(result);
            }
        }
        crate::scheduler::WorkClass::DecodeTokens | crate::scheduler::WorkClass::DecodeHidden => {
            let mut jobs = Vec::with_capacity(items.len());
            for item in items {
                match item.payload {
                    InferenceCommand::Step { job, reply, .. } => jobs.push((job, reply)),
                    InferenceCommand::Prefill { reply, .. } => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "scheduler recebeu fase de inferência incompatível"
                        )));
                    }
                }
            }
            for (reply, result) in step_batch_blocking(state, jobs) {
                let _ = reply.send(result);
            }
        }
    }
}

fn execute_one_command(state: SharedState, command: InferenceCommand) {
    match command {
        InferenceCommand::Prefill { job, reply, .. } => {
            let result = prefill_blocking(
                state,
                job.sid_key,
                job.chunk_index,
                job.chunk_size,
                job.is_last,
                job.is_final_chunk,
                job.wire_dtype,
                job.prompt,
                job.tensor,
                job.chunk_tokens,
                job.queued_at,
            );
            let _ = reply.send(result);
        }
        InferenceCommand::Step { job, reply, .. } => {
            let result = step_blocking(
                state,
                job.sid_key,
                job.is_last,
                job.wire_dtype,
                job.token_id,
                job.tensor,
                job.speculate,
                job.incoming_draft,
                job.batch_tokens,
                job.mtp_k,
                job.queued_at,
            );
            let _ = reply.send(result);
        }
    }
}

fn inference_frame_body_limit() -> usize {
    INFERENCE_FRAME_BODY_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::DefaultBodyLimit,
        http::Request,
    };
    use tower::ServiceExt;

    async fn accept_frame(body: Bytes) -> StatusCode {
        assert_eq!(body.len(), 4 * 1024 * 1024);
        StatusCode::NO_CONTENT
    }

    #[test]
    fn resposta_de_item_unico_preserva_identidade_do_batch() {
        let request = json!({
            "item_id": "request-item",
            "batch_id": "request-batch",
        });
        let mut response = json!({"done": true});

        preserve_batch_identity(&request, &mut response);

        assert_eq!(response["item_id"], "request-item");
        assert_eq!(response["batch_id"], "request-batch");
    }

    #[tokio::test]
    async fn prefill_route_accepts_a_default_f16_frame_larger_than_axums_two_mib_default() {
        let app = Router::new()
            .route("/v1/prefill", post(accept_frame))
            .layer(DefaultBodyLimit::max(inference_frame_body_limit()));
        let request = Request::post("/v1/prefill")
            .body(Body::from(vec![0_u8; 4 * 1024 * 1024]))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }

    /// O socket tem que atender IPv4 mesmo estando ligado em `[::]`, e é isso
    /// que `IPV6_V6ONLY=0` compra. O teste conecta por 127.0.0.1 de verdade,
    /// porque o defeito que ele guarda não aparece na leitura do código: o
    /// caminho antigo desligava a opção *depois* do bind, o que não tem efeito
    /// nenhum. No Linux passava por acidente (o padrão do sistema já é 0) e no
    /// Windows deixaria o servidor mudo para todo peer IPv4 da LAN.
    #[test]
    fn listener_em_ipv6_tambem_atende_ipv4() {
        let l = match listener_dual_stack(0) {
            Ok(l) => l,
            // Máquina sem IPv6 cai no IPv4 puro, e aí não há o que provar.
            Err(_) => return,
        };
        let port = l.local_addr().unwrap().port();
        if l.local_addr().unwrap().is_ipv4() {
            return;
        }

        let aceito = std::thread::spawn(move || l.accept().is_ok());
        let conectou = std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();

        assert!(conectou, "um cliente IPv4 não conseguiu conectar em [::]");
        assert!(aceito.join().unwrap(), "a conexão IPv4 não foi aceita");
    }

    /// O socket tem que pedir `SO_REUSEADDR`, como o `TcpListener::bind` da std
    /// pede sozinho.
    ///
    /// Sem isso, toda conexao que ficou em TIME_WAIT naquela porta - e sempre
    /// ha, porque o gateway sonda `/health` e manda inferencia o tempo todo -
    /// impede o proximo servidor de ligar por ate um minuto. O sintoma e
    /// "Address already in use" com `lsof` sem achar dono nenhum: TIME_WAIT nao
    /// tem processo. Foi o que travou toda implantacao de versao nova, dez
    /// tentativas seguidas.
    ///
    /// A flag se perdeu quando o socket passou a ser montado a mao com
    /// `socket2` para o dual-stack; um socket cru nao herda o padrao da std.
    #[test]
    fn o_listener_pede_reuse_address() {
        let l = match listener_dual_stack(0) {
            Ok(l) => l,
            Err(_) => return,
        };
        let sock = socket2::Socket::from(l);
        assert!(
            sock.reuse_address().unwrap(),
            "sem SO_REUSEADDR, TIME_WAIT segura a porta por ~60s a cada troca de versao"
        );
    }

    /// Porta ocupada tem que falhar *antes* de o modelo ser carregado.
    ///
    /// O `run` ligava o socket depois do `LlmEngine::load`, então cada uma das
    /// 10 tentativas do supervisor carregava um modelo inteiro na VRAM para
    /// descobrir, no fim, que a porta estava tomada - minutos por tentativa, e
    /// memória de GPU presa em cargas descartadas. O sintoma no log era
    /// "encerrou com erro: Address already in use" logo depois de
    /// "llama context ready".
    ///
    /// Aqui isso vira uma garantia estrutural: `serve_port` é o primeiro passo
    /// do `run`, e devolve `Err` sem que nada caro tenha acontecido.
    #[tokio::test]
    async fn porta_ocupada_falha_de_imediato() {
        let ocupante = match listener_dual_stack(0) {
            Ok(l) => l,
            Err(_) => return,
        };
        let port = ocupante.local_addr().unwrap().port();

        // O caminho de um modelo que nao existe: se o `run` carregasse antes de
        // ligar, o erro falaria do arquivo (ou o processo morreria dentro do
        // llama.cpp). Falar de porta ocupada e a prova de que a ordem esta certa.
        let mut cfg = crate::runtime::NodeConfig {
            deploy_id: String::new(),
            model_path: "/nao/existe/modelo.gguf".into(),
            model_label: "qwen35".into(),
            deployment_name: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_port: port,
            n_gpu_layers: 99,
            n_ctx: 8192,
            offload_kqv: true,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            max_sessions: 1,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
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
            tp: None,
        };
        cfg.inference_port = port;
        let (_tx, rx) = tokio::sync::watch::channel(0u64);

        let comecou = std::time::Instant::now();
        let erro = run(
            cfg,
            rx,
            0,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
            None,
        )
        .await
        .expect_err("porta ocupada devia falhar");

        assert!(
            erro.to_string().to_lowercase().contains("address") || erro.to_string().contains("use"),
            "o erro tinha que ser o da porta, e foi: {erro}"
        );
        assert!(
            comecou.elapsed() < std::time::Duration::from_secs(1),
            "falhou depois de carregar alguma coisa: levou {:?}",
            comecou.elapsed()
        );
    }
}

/// `relay_and_bubble` against a real (in-process, on a loopback port) fake
/// next hop — a real `reqwest` round trip, not an in-process `oneshot` call,
/// since that's what a relaying hop actually does. No `LlmEngine` involved:
/// this is exactly the "new glue code" the plan flagged as the concentrated
/// risk (the wire meta assembled by hand here, `more_chunks`-style, is what
/// a bug like the prefill overlay issue would hide in).
#[cfg(test)]
mod relay_tests {
    use super::*;
    use axum::{body::to_bytes, routing::post as axum_post};

    fn step(node_id: &str, url: String) -> PipelineStep {
        PipelineStep {
            node_id: node_id.into(),
            inference_url: url,
            inference_candidates: Vec::new(),
            start_layer: 16,
            end_layer: 31,
            inference_frame_capabilities: vec!["relay/v1-chain".into()],
            libp2p_peer_id: None,
            libp2p_addr_candidates: Vec::new(),
        }
    }

    async fn spawn_fake_hop(
        handler: impl Fn() -> Response + Clone + Send + Sync + 'static,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/v1/step",
            axum_post(move |_body: Bytes| {
                let handler = handler.clone();
                async move { handler() }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn prepends_the_called_hops_own_entry_onto_the_bubbled_response() {
        let url = spawn_fake_hop(|| {
            let meta = json!({
                "done": true,
                "token_id": 42,
                "text": "!",
                "processing_ms": 5,
                "node_timings": {"decode_ms": 3},
            });
            let bytes = wire::encode(&meta, None, WireDtype::F16).unwrap();
            (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response()
        })
        .await;

        let hops = vec![step("tail", url)];
        let residual = vec![0.0f32; 8];
        let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));

        let meta = relay_and_bubble(
            &http,
            &hops,
            "/v1/step",
            json!({"session_id": "sess-1"}),
            (&residual, [1, 8]),
            1,
            json!(7),
            json!({"decode_ms": 2}),
        )
        .await
        .expect("relay should succeed");

        assert_eq!(meta["done"], true);
        assert_eq!(meta["token_id"], 42);
        assert_eq!(meta["text"], "!");
        // Ours, not the tail's — the tail reported processing_ms=5.
        assert_eq!(meta["processing_ms"], 7);
        assert_eq!(meta["node_timings"]["decode_ms"], 2);

        let hop_timings = meta["hop_timings"].as_array().expect("hop_timings array");
        assert_eq!(hop_timings.len(), 1);
        assert_eq!(
            hop_timings[0]["processing_ms"], 5,
            "the tail's self-reported processing_ms"
        );
        assert_eq!(hop_timings[0]["node_timings"]["decode_ms"], 3);
    }

    #[tokio::test]
    async fn a_downstream_relay_failure_is_propagated_unchanged() {
        let url = spawn_fake_hop(|| {
            let failure = crate::relay::RelayFailure {
                failed_node_id: "deep-hop".into(),
                kind: "timeout".into(),
                message: "no response".into(),
            };
            (StatusCode::BAD_GATEWAY, Json(failure)).into_response()
        })
        .await;

        let hops = vec![step("directly-called-hop", url)];
        let residual = vec![0.0f32; 8];
        let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));

        let response = relay_and_bubble(
            &http,
            &hops,
            "/v1/step",
            json!({"session_id": "sess-1"}),
            (&residual, [1, 8]),
            1,
            json!(7),
            json!({}),
        )
        .await
        .expect_err("the fake hop always fails");

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let failure: crate::relay::RelayFailure = serde_json::from_slice(&body).unwrap();
        // Not "directly-called-hop": the failure must still name the peer
        // that actually died, several hops further down the chain.
        assert_eq!(failure.failed_node_id, "deep-hop");
        assert_eq!(failure.kind, "timeout");
    }

    /// The verified bug this ring implementation exists to not repeat: a
    /// bubbled ring response's `more_chunks`/`n_prompt_tokens` come from the
    /// *tail* (always `false`/absent, since only the head owns the prompt)
    /// unless the head overwrites them. Without `overlay_head_authoritative_fields`,
    /// a multi-chunk prompt on a ring pipeline would silently stop after one
    /// chunk — this proves the head's values win.
    #[test]
    fn the_heads_more_chunks_overwrites_the_tails_bubbled_false() {
        let local_meta = json!({
            "owns_tokens": true,
            "more_chunks": true,
            "n_prompt_tokens": 900,
        });
        // What actually bubbled back from a tail that isn't the head.
        let mut bubbled = json!({"done": false, "more_chunks": false, "hop_timings": []});

        overlay_head_authoritative_fields(&local_meta, &mut bubbled);

        assert_eq!(bubbled["more_chunks"], true);
        assert_eq!(bubbled["n_prompt_tokens"], 900);
    }

    /// A middle hop (never the head) must leave the bubbled response alone —
    /// its own `more_chunks`/`n_prompt_tokens` are meaningless placeholders,
    /// not a second, competing source of truth.
    #[test]
    fn a_non_head_hop_never_overlays_its_own_placeholder_fields() {
        let local_meta = json!({"owns_tokens": false, "more_chunks": false, "n_prompt_tokens": 0});
        let mut bubbled = json!({"done": true, "token_id": 7, "more_chunks": false});
        let before = bubbled.clone();

        overlay_head_authoritative_fields(&local_meta, &mut bubbled);

        assert_eq!(bubbled, before);
    }

    #[test]
    fn is_final_chunk_for_forward_uses_the_heads_own_determination() {
        let head = json!({"owns_tokens": true, "more_chunks": true});
        assert!(!is_final_chunk_for_forward(&head, false));

        let head_last_chunk = json!({"owns_tokens": true, "more_chunks": false});
        assert!(is_final_chunk_for_forward(&head_last_chunk, false));
    }

    #[test]
    fn is_final_chunk_for_forward_passes_through_for_a_non_head_hop() {
        let middle = json!({"owns_tokens": false, "more_chunks": false});
        assert!(is_final_chunk_for_forward(&middle, true));
        assert!(!is_final_chunk_for_forward(&middle, false));
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

struct Session {
    seq_id: u32,
    n_past: u32,
    recent_tokens: Vec<i32>,
    /// Stored by the first peer so subsequent chunks don't re-tokenize.
    prompt_tokens: Vec<i32>,
    last_used: Instant,
}

impl Session {
    fn new(seq_id: u32) -> Self {
        Self {
            seq_id,
            n_past: 0,
            recent_tokens: Vec::new(),
            prompt_tokens: Vec::new(),
            last_used: Instant::now(),
        }
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

struct InferState {
    engine: LlmEngine,
    sessions: HashMap<String, Session>,
    seq_pool: Vec<u32>,
    max_sessions: u32,
    sampling: SamplingConfig,
    no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    chat_template: bool,
    start_layer: u32,
    end_layer: u32,
    original_block_count: Option<u32>,
    /// Corte de confiança do rascunho MTP — ver `NodeConfig::mtp_min_probability`.
    mtp_min_probability: f32,
    /// Duração do último handshake TLS aceito por este servidor, em ms.
    ///
    /// Medida no laço de accept (`serve.rs`), que é onde o `TlsAcceptor` roda e
    /// o tempo é exato. Sai daqui para o `meta` da resposta, e de lá para a
    /// telemetria por salto do gateway. `None` quando o nó não tem TLS.
    tls_handshake_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Identidade TP deste nó. O rank 0 precisa da lista de membros para pôr
    /// os workers no mesmo decode; sem isso a barreira nunca fecha.
    tp: Option<crate::network::TpIdentity>,
}

type SharedState = Arc<Mutex<InferState>>;

/// A porta de inferência aceita a credencial compartilhada do cluster ou uma
/// API key ativa. O gateway usa a connection_key nas chamadas internas; a
/// alternativa de API key mantém o contrato dos clientes OpenAI/Anthropic.
async fn require_inference_auth(
    State(config): State<crate::auth::AuthState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.strip_prefix("Bearer ").unwrap_or(value))
        .or_else(|| {
            request
                .headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        });

    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing inference credential").into_response();
    };

    let valid = {
        let cfg = config.read().await;
        crate::auth::is_valid_inference_token(&cfg, token)
    };
    if !valid {
        return (StatusCode::FORBIDDEN, "invalid inference credential").into_response();
    }

    next.run(request).await
}

impl InferState {
    fn new(engine: LlmEngine, cfg: &NodeConfig) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        Self {
            tls_handshake_ms: None,
            engine,
            sessions: HashMap::new(),
            seq_pool,
            max_sessions,
            sampling: SamplingConfig {
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                top_k: cfg.top_k as i32,
                repeat_penalty: cfg.repeat_penalty,
                repeat_last_n: cfg.repeat_last_n as usize,
            },
            no_thinking: cfg.no_thinking,
            chat_template: cfg.no_thinking, // --no-thinking implies --chat-template
            start_layer: cfg.start_layer,
            end_layer: cfg.end_layer,
            original_block_count: None, // filled in after engine load
            mtp_min_probability: cfg.mtp_min_probability,
            tp: cfg.tp.clone(),
        }
    }

    fn alloc_seq(&mut self) -> Option<u32> {
        if self.seq_pool.is_empty() {
            let reaped = self.reap_idle();
            if reaped == 0 {
                return None;
            }
        }
        Some(self.seq_pool.remove(0))
    }

    fn free_seq(&mut self, seq_id: u32) {
        if !self.seq_pool.contains(&seq_id) {
            self.seq_pool.push(seq_id);
        }
    }

    fn reap_idle(&mut self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(SESSION_IDLE_TIMEOUT_SECS))
            .unwrap_or_else(Instant::now);
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.last_used < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for sid in stale {
            if let Some(s) = self.sessions.remove(&sid) {
                info!(
                    "reclaiming idle session {}… (seq={})",
                    &sid[..8.min(sid.len())],
                    s.seq_id
                );
                // Limpeza total da sequência, que todo modelo aceita.
                let _ = self.engine.kv_remove(s.seq_id, 0, -1);
                self.free_seq(s.seq_id);
            }
        }
        count
    }

    fn get_or_create(&mut self, sid: &str, reset: bool) -> Result<&mut Session> {
        if reset {
            if let Some(old) = self.sessions.remove(sid) {
                let _ = self.engine.kv_remove(old.seq_id, 0, -1);
                self.free_seq(old.seq_id);
            }
            let seq = self.alloc_seq().ok_or_else(|| {
                anyhow::anyhow!(
                    "seq pool esgotado (max {} sessões simultâneas)",
                    self.max_sessions
                )
            })?;
            self.sessions.insert(sid.to_string(), Session::new(seq));
        }
        self.sessions.get_mut(sid).ok_or_else(|| {
            anyhow::anyhow!("sessão '{}' desconhecida — envie reset=true antes", sid)
        })
    }

    fn finish(&mut self, sid: &str) {
        if let Some(s) = self.sessions.remove(sid) {
            let _ = self.engine.kv_remove(s.seq_id, 0, -1);
            self.free_seq(s.seq_id);
        }
    }
}

// ── Start ─────────────────────────────────────────────────────────────────────

/// Load the model and serve the inference HTTP server.
///
/// Blocks until the server exits or `stop_rx` fires.
pub async fn run(
    cfg: NodeConfig,
    mut stop_rx: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    mtp_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    batching_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tls: Option<crate::serve::TlsServer>,
    auth: Option<crate::auth::AuthState>,
) -> Result<()> {
    // Derivado, não copiado do pedido: o caminho de linha de comando nunca
    // preenche `no_embeddings`, então sem isto o nó da última fatia instalava o
    // callback de captura e pagava ~8 ms por token para capturar um residual
    // que ninguém lê. Ver `NodeConfig::needs_residual_export`.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    let port = cfg.inference_port;
    let listener = bind_dual_stack(port).await?;
    info!("porta {port} ligada; carregando o modelo");

    let need_embeddings = cfg.needs_residual_export();
    // Maior largura de lote especulativo que este nó pode receber, dos dois
    // mecanismos (lookup e MTP) - é o que `cparams.n_rs_seq` precisa cobrir
    // para um modelo híbrido/recorrente aceitar desfazer um rascunho
    // recusado. Ver o doc de `EngineConfig::rs_seq_window`.
    let rs_seq_window = cfg.speculative_k.max(cfg.mtp_draft_tokens + 1);
    let engine_cfg = EngineConfig {
        n_ctx: crate::runtime::total_context_tokens(cfg.n_ctx, cfg.max_sessions)?,
        offload_kqv: cfg.offload_kqv,
        cache_type_k: cfg.cache_type_k,
        cache_type_v: cfg.cache_type_v,
        fit_memory: cfg.fit_memory && cfg.tp.is_none(),
        n_batch: cfg.n_batch,
        n_gpu_layers: cfg.n_gpu_layers as i32,
        n_threads: 0,
        max_sessions: cfg.max_sessions.max(1),
        need_embeddings,
        mtp: cfg.mtp_effective(),
        rs_seq_window,
    };

    let model_path = cfg.model_path.clone();
    let engine = tokio::task::spawn_blocking(move || LlmEngine::load(&model_path, &engine_cfg))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    // Só agora se sabe se a cabeça `nextn` existe: depende do GGUF, não da
    // configuração. Ver `Network::mtp_ready`.
    mtp_ready.store(engine.mtp_ready(), std::sync::atomic::Ordering::Relaxed);

    let mut state_inner = InferState::new(engine, &cfg);
    state_inner.tls_handshake_ms = tls.as_ref().map(|t| t.last_handshake_ms.clone());
    state_inner.original_block_count =
        crate::gguf::read_info(std::path::Path::new(&cfg.model_path))
            .ok()
            .and_then(|info| info.original_block_count);

    // Fora do mutex do estado: o decode segura aquele lock e é de dentro dele
    // que o reduce espera a soma. Ver `handle_tp_reduce`.
    let tp_barrier = crate::collective::ReduceBarrier::new();
    let tp_size = cfg.tp.as_ref().map(|t| t.size).unwrap_or(1);
    let local_barrier_handle = crate::tp::set_local_barrier(tp_barrier.clone(), tp_size);

    let state: SharedState = Arc::new(Mutex::new(state_inner));
    let scheduler_cfg = crate::scheduler::SchedulerConfig::from_node_config(&cfg);
    // TP participates in the same canonical batch frame. The handler opens
    // one shared decode id for the whole frame, broadcasts that frame to the
    // workers and then submits all items to this local scheduler together.
    batching_ready.store(scheduler_cfg.enabled, std::sync::atomic::Ordering::Relaxed);
    let scheduler = spawn_scheduler(state.clone(), scheduler_cfg);
    let app_state = InferenceAppState {
        state,
        scheduler,
        tp_batch_gate: Arc::new(tokio::sync::Mutex::new(())),
    };
    crate::relay::install_local_inference(generation, Arc::new(app_state.clone())).await;

    if let Some(auth) = auth.clone() {
        http_client().set_auth_state(auth);
    }

    let inference_routes = Router::new()
        .route(
            "/v1/prefill",
            post(handle_prefill).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step",
            post(handle_step).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route("/v1/session/release", post(handle_release))
        .route("/v1/session/rollback", post(handle_rollback));
    let inference_routes = if let Some(auth) = auth.clone() {
        inference_routes.layer(middleware::from_fn_with_state(auth, require_inference_auth))
    } else {
        inference_routes
    };
    let inference_routes = inference_routes.with_state(app_state.clone());

    let tp_routes = Router::new().route(
        "/v1/tp/reduce",
        post(handle_tp_reduce).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
    );
    let tp_routes = if let Some(auth) = auth {
        tp_routes.layer(middleware::from_fn_with_state(auth, require_inference_auth))
    } else {
        tp_routes
    };
    let tp_routes = tp_routes.with_state((tp_barrier, tp_size));

    let router = inference_routes
        .merge(
            Router::new()
                .route("/health", get(handle_health))
                .with_state(app_state.clone()),
        )
        .merge(tp_routes);

    info!("inference server pronto em [::]:{port}");

    // Ver a nota gêmea em `gateway::start_gateway`: sem `tcp_nodelay`, toda
    // resposta de hop sai com Nagle ligado, e um ack de resposta direta é
    // exatamente a escrita pequena que ele segura. Quem o aplica agora é o
    // `serve`, que também decide entre TLS e texto claro por conexão.
    //
    // O desligamento gracioso virou uma corrida entre o laço de accept e o
    // watch de geração: quando a geração muda, paramos de aceitar. Conexão em
    // voo termina sozinha, como antes.
    // O desligamento gracioso é sequencial de propósito: primeiro o `serve`
    // para de aceitar e encerra as conexões que restaram, só depois a VRAM é
    // devolvida. Na ordem inversa uma requisição em voo tocaria um contexto já
    // liberado.
    let resultado = crate::serve::serve_with_shutdown(listener, router, tls, async move {
        loop {
            if stop_rx.changed().await.is_err() {
                break;
            }
            if *stop_rx.borrow() != generation {
                break;
            }
        }
    })
    .await;
    crate::relay::clear_local_inference(generation).await;

    if let Some(handle) = local_barrier_handle {
        crate::tp::clear_local_barrier(handle);
    }

    // Explícito, e não pela contagem do `Arc`: uma cópia esquecida em qualquer
    // canto — foi o que as conexões desgarradas faziam — adiava o `Drop` para
    // sempre, e o `undeploy` respondia "pronto" com o modelo ainda na GPU.
    // Depois do drenar acima ninguém mais decodifica, então isto é seguro.
    // `into_inner` no envenenado: um handler que entrou em pânico deixa o
    // mutex marcado, e recusar a liberação por causa disso seria justamente
    // deixar a VRAM presa no caso em que ela mais precisa voltar.
    app_state
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .engine
        .unload();
    info!("memória do modelo devolvida na porta {port}");

    resultado?;
    batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// ── Socket ────────────────────────────────────────────────────────────────────

/// Um listener que atende IPv6 e IPv4 no mesmo socket, caindo para IPv4 puro
/// quando não há IPv6 na máquina.
///
/// `IPV6_V6ONLY` só vale se for desligado **antes** do `bind`, e é por isso que
/// o socket é montado à mão em vez de sair do `TcpListener::bind`. O caminho
/// antigo chamava `setsockopt` depois do bind: no Linux passava despercebido
/// porque `net.ipv6.bindv6only` já é 0 por padrão, mas no Windows o padrão é 1
/// e o servidor de inferência simplesmente não aceitaria nenhum peer IPv4 -
/// que é como todo o cluster da LAN fala.
///
/// `SO_REUSEADDR` continua fora de propósito: no Windows ele deixa outro
/// processo tomar uma porta já ligada, e é justamente o "address already in
/// use" que denuncia um servidor velho ainda de pé.
fn listener_dual_stack(port: u16) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    // Backlog: o mesmo que o `TcpListener::bind` da std usa.
    const BACKLOG: i32 = 128;

    let v6 = (|| -> std::io::Result<std::net::TcpListener> {
        let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        sock.set_only_v6(false)?;
        // O `TcpListener::bind` da std pede isto sozinho no Unix; um socket cru
        // nao. Sem a flag, cada conexao em TIME_WAIT nesta porta - e o gateway
        // deixa varias, sondando `/health` e mandando inferencia - impede o
        // proximo servidor de ligar por ate um minuto, que e mais que as dez
        // tentativas do supervisor. Toda implantacao de versao nova morria ai.
        sock.set_reuse_address(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.listen(BACKLOG)?;
        Ok(sock.into())
    })();

    match v6 {
        Ok(l) => Ok(l),
        Err(e) => {
            // Sem IPv6 na máquina: IPv4 puro ainda serve o cluster inteiro.
            tracing::debug!("bind IPv6 em [::]:{port} falhou ({e}); tentando IPv4");
            let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            sock.set_reuse_address(true)?;
            sock.bind(&SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port).into())?;
            sock.listen(BACKLOG)?;
            Ok(sock.into())
        }
    }
}

async fn bind_dual_stack(port: u16) -> Result<TcpListener> {
    let std_sock = listener_dual_stack(port)?;
    std_sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_sock)?)
}

// ── Error helpers ─────────────────────────────────────────────────────────────

/// One shared outbound client cache, used only when relaying to this node's own
/// next hop (`PipelineStep::supports(CAP_RELAY_CHAIN)`) — this server has no
/// other reason to make outbound HTTP calls. Same lazy-static pattern as
/// `webui::catalog_cache`.
///
/// É um `PeerClients` e não um `reqwest::Client` porque o próximo salto é um
/// peer: com o cluster em TLS ele precisa do certificado daquele node_id
/// fixado. O esquema global (`peer_client::tls_ativo`) é quem diz se há TLS -
/// este processo é o mesmo do nó, então a política já está aplicada aqui.
fn http_client() -> &'static crate::peer_client::PeerClients {
    static CLIENT: std::sync::OnceLock<crate::peer_client::PeerClients> =
        std::sync::OnceLock::new();
    let c = CLIENT
        .get_or_init(|| crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)));
    // A cada uso, não só na criação: o `OnceLock` nasce uma vez e o operador
    // pode virar a flag depois. `set_tls` só faz trabalho quando o valor muda,
    // então isto custa uma leitura atômica no caminho comum.
    c.set_tls(crate::peer_client::tls_ativo());
    c
}

/// Duração do último handshake TLS que este servidor aceitou, em ms.
///
/// Zero quando não há TLS, e zero também quando o salto reaproveitou uma
/// conexão do pool - que é a maioria deles. Ver o doc de
/// `SentFrame::tls_handshake_ms` sobre por que isso é a verdade e não uma
/// falha de medição.
fn last_handshake_ms(state: &SharedState) -> u64 {
    state
        .lock()
        .unwrap()
        .tls_handshake_ms
        .as_ref()
        .map(|v| v.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

async fn handle_prefill_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/prefill", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_prefill(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

async fn handle_step_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/step", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_step(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

fn err400(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn err500(msg: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

// ── /health ───────────────────────────────────────────────────────────────────

async fn handle_health(State(app): State<InferenceAppState>) -> impl IntoResponse {
    let scheduler = app.scheduler;
    let state = app.state;
    let s = state.lock().unwrap();
    // Keep in sync with network.rs's `self_announce`, gateway.rs's local peer
    // entry, and the capability constants in routing/pipeline.rs.
    let mut caps = vec![
        crate::pipeline::CAP_FRAME_BATCH,
        "relay/v1-chain",
        "relay/v2-direct",
        "speculate/v1-lookup",
        "speculate/v1-mtp-sliced",
    ];
    if s.engine.mtp_ready() {
        caps.push("speculate/v1-mtp");
    }
    if scheduler.config.enabled {
        caps.push(crate::pipeline::CAP_FRAME_BATCH);
    }
    Json(json!({
        "status": "ok",
        "model_loaded": true,
        "start_layer": s.start_layer,
        "end_layer": s.end_layer,
        "original_block_count": s.original_block_count,
        "hidden_dim": s.engine.n_embd,
        "vocab_size": s.engine.n_vocab,
        "max_sessions": s.max_sessions,
        "sessions_in_use": s.sessions.len(),
        "free_slots": s.seq_pool.len(),
        "n_ctx_total": s.engine.n_ctx(),
        "n_ctx_per_session": s.engine.n_ctx() / s.max_sessions.max(1),
        "offload_kqv": s.engine.offload_kqv(),
        "cache_type_k": s.engine.cache_type_k().as_str(),
        "cache_type_v": s.engine.cache_type_v().as_str(),
        "fit_memory_applied": s.engine.fit_memory_applied(),
        "n_gpu_layers_effective": s.engine.effective_n_gpu_layers(),
        "n_ubatch_effective": s.engine.effective_n_ubatch(),
        // Onde as camadas e o cache KV ficaram de verdade, direto do
        // llama.cpp. `null` quando a ponte não respondeu - a interface diz
        // que não sabe, em vez de derivar um mapa de `n_gpu_layers`, que
        // erraria sempre que um override de buffer type entrasse.
        "placement": s.engine.placement(),
        "context_memory": s.engine.context_memory(),
        "inference_frame_capabilities": caps,
        "scheduler": scheduler.stats.json(&scheduler.config),
        // Diagnóstico puro — `llama_memory_can_shift`, o K-shift do
        // `--ctx-shift` de referência do llama.cpp. Não decide mais se a
        // especulação roda (ver o doc de `should_draft`); mantido aqui
        // porque é informação real do motor.
        "kv_can_shift": s.engine.kv_can_shift(),
        // A cabeça de rascunho `nextn` foi mesmo anexada? Depende do GGUF
        // carregado (fatia não-final não tem o bloco) e do interruptor do
        // operador, então nem a configuração nem o nome do arquivo respondem.
        "mtp_ready": s.engine.mtp_ready(),
    }))
}

// ── /v1/tp/reduce ────────────────────────────────────────────────────────────

/// Uma parcial de um reduce de tensor parallelism (caminho C). Cada rank do
/// grupo faz um POST desta com a própria parcial e o mesmo `barrier_id`; o
/// endpoint (no primary/rank 0) acumula e devolve a soma quando todos chegam.
/// **Nunca** toca no mutex do `InferState`, e isso é a correção, não estilo.
///
/// `handle_prefill` segura esse mutex durante o decode inteiro, e é de dentro
/// desse decode que o callback do reduce bloqueia esperando a soma. Enquanto
/// este handler pegava o mesmo mutex para ler a barreira, a parcial que chegava
/// - do próprio rank 0 ou do worker - ficava presa esperando um lock que só
/// seria liberado quando o decode terminasse; e o decode só terminaria com a
/// soma que estava presa na porta. Deadlock exato de 18/08: os dois ranks
/// logavam o primeiro reduce, com 0,2 ms de diferença, e o pipeline parava ali.
///
/// A barreira vive fora do mutex (`ReduceBarrier` já é `Clone` e sincronizada
/// por dentro), então o caminho da coletiva nunca disputa com o decode.
///
/// A parcial viaja como **f32 cru em little-endian**, não como JSON. Um float
/// de 4 bytes vira ~12 caracteres em JSON, e a parcial de um prefill de 100
/// tokens tem 410 mil floats: ~5 MB de texto por reduce, 64 vezes por decode.
/// Isso estourava o limite de corpo do axum (413) e, quando cabia, gastava mais
/// tempo formatando número que somando. Cru é 4x menor e sem parse.
///
/// `barrier_id` e `rank` vão em cabeçalho, que é onde metadado de tamanho fixo
/// pertence.
pub(crate) async fn handle_tp_reduce(
    State((barrier, size)): State<(crate::collective::ReduceBarrier, u32)>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let barrier_id = match headers
        .get(crate::tp::TP_BARRIER_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) => v.to_string(),
        None => {
            return err400(format!(
                "falta o cabeçalho {}",
                crate::tp::TP_BARRIER_HEADER
            ))
        }
    };
    let rank: u32 = match headers
        .get(crate::tp::TP_RANK_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(v) => v,
        None => return err400(format!("falta o cabeçalho {}", crate::tp::TP_RANK_HEADER)),
    };
    if body.len() % 4 != 0 {
        return err400(format!(
            "parcial de {} bytes não é múltiplo de 4",
            body.len()
        ));
    }
    let data: Vec<f32> = body
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    match barrier.contribute(&barrier_id, rank, size, data) {
        Ok(rx) => match rx.await {
            Ok(sum) => {
                let mut out = Vec::with_capacity(sum.len() * 4);
                for v in &sum {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                (
                    [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                    out,
                )
                    .into_response()
            }
            Err(e) => err500(format!("barreira abandonada: {e}")),
        },
        Err(e) => err400(e),
    }
}

/// Repassa uma chamada de ciclo de vida de sessão aos demais ranks do grupo.
///
/// O corpo é remontado (não é um quadro binário como o decode), e o cabeçalho
/// de réplica evita fan-out infinito: quem recebe marcado não repassa.
fn replicate_session_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    rota: &str,
    session_id: &str,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // já é a réplica
    }
    let tp = {
        let s = state.lock().unwrap();
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let corpo = json!({ "session_id": session_id }).to_string().into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        rota,
        "session",
        corpo,
        Some("application/json"),
    );
}

// ── /v1/session/release ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReleaseRequest {
    session_id: String,
}

async fn handle_release(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ReleaseRequest>,
) -> impl IntoResponse {
    // A réplica não encerra sessão por conta própria (ver `tp::is_replica`),
    // então quem a libera é este release - que o gateway manda **só ao rank 0**,
    // porque o worker não é hop do pipeline. Sem repassar, a sessão do worker
    // fica segurando o slot para sempre e a requisição seguinte morre com
    // "seq pool esgotado (max 1 sessões simultâneas)" (18/08).
    replicate_session_call(&app.state, &headers, "/v1/session/release", &req.session_id);
    let mut s = app.state.lock().unwrap();
    let existed = s.sessions.contains_key(&req.session_id);
    s.finish(&req.session_id);
    let free = s.seq_pool.len();
    let max = s.max_sessions;
    drop(s);
    Json(json!({
        "released": existed,
        "free_slots": free,
        "max_sessions": max,
    }))
}

// ── /v1/session/rollback ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RollbackRequest {
    session_id: String,
    /// How many of the batch's positions are confirmed valid — see
    /// `rollback_from`'s doc comment.
    accept_count: u32,
    /// How many positions the speculative decode actually wrote.
    batch_width: u32,
}

/// Discard the KV cache entries for whatever part of a speculative batch
/// didn't get confirmed. Fanned out by the gateway to *every* hop in the
/// plan after each speculative round (`gateway::rollback_sessions`, mirroring
/// `release_sessions`'s existing shape) — every hop decoded the full batch
/// into its own cache, so every hop needs this, not just the tail. Same
/// best-effort ethos as `/v1/session/release`: an unknown or already-reaped
/// `session_id` is not an error, just a no-op.
async fn handle_rollback(
    State(app): State<InferenceAppState>,
    Json(req): Json<RollbackRequest>,
) -> impl IntoResponse {
    let mut s = app.state.lock().unwrap();
    let Some((seq_id, n_past_after_batch)) = s
        .sessions
        .get(&req.session_id)
        .map(|session| (session.seq_id, session.n_past))
    else {
        return Json(json!({"rolled_back": false}));
    };
    let new_n_past = rollback_from(n_past_after_batch, req.batch_width, req.accept_count);
    if new_n_past < n_past_after_batch && !s.engine.kv_remove(seq_id, new_n_past as i32, -1) {
        // O cache não recuou. Ajustar `n_past` mesmo assim é o que produz o
        // "inconsistent sequence positions" da rodada seguinte: a contabilidade
        // andaria para trás com o cache parado. Melhor manter os dois de acordo
        // e dizer que não deu.
        return Json(json!({
            "rolled_back": false,
            "n_past": n_past_after_batch,
            "reason": "este modelo não aceita descartar posições do meio do cache",
        }));
    }
    if let Some(session) = s.sessions.get_mut(&req.session_id) {
        session.n_past = new_n_past;
    }
    Json(json!({"rolled_back": true, "n_past": new_n_past}))
}

// ── /v1/prefill ───────────────────────────────────────────────────────────────

/// Abre o decode nos **dois** papéis e, no rank 0, põe os workers para rodar o
/// mesmo lote.
///
/// TP só funciona em lockstep: todo rank percorre o mesmo grafo sobre os mesmos
/// tokens, e as parciais se encontram na barreira. O gateway só fala com o rank
/// 0 (o worker é excluído do pipeline de propósito), então é aqui que os outros
/// ranks entram no decode. Sem isto o rank 0 postava a parcial e esperava para
/// sempre - o travamento de 18/08.
///
/// O quadro é repassado byte a byte, com o id do decode no cabeçalho. Quem
/// recebe o cabeçalho é réplica: abre o decode com o id recebido e **não**
/// retransmite, senão o grupo se multiplicaria a cada salto.
fn open_tp_decode(state: &SharedState, headers: &axum::http::HeaderMap, rota: &str, body: &Bytes) {
    if headers.get(BATCH_CHILD_HEADER).is_some() {
        return;
    }
    let tp = {
        let s = state.lock().unwrap();
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1) else {
        return;
    };
    if let Some(id) = headers
        .get(crate::tp::TP_DECODE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        crate::tp::begin_replica_decode(id);
        return;
    }
    if tp.rank != 0 {
        // Worker recebendo quadro sem cabeçalho: veio de fora do grupo. Abrir
        // um decode aqui produziria reduces com id que ninguém mais conhece.
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    crate::tp::begin_decode(&id);
    crate::tp::broadcast_decode(&tp.members, rota, &id, body.to_vec());
}

async fn handle_prefill(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if let Ok(batch) = wire::decode_batch(&body) {
        if batch.items.len() > 1 {
            return handle_prefill_batch(app, headers, batch).await;
        }
    }
    // A one-item frame is still a batch frame: the gateway's
    // `BatchCoordinator` deliberately sends it through the same path. TP has
    // one decode id and one collective barrier per request, so keep the
    // complete top-level request under the same gate as a multi-item batch.
    // Fan-out children already hold the gate in `handle_prefill_batch` and
    // mark themselves with `BATCH_CHILD_HEADER`, otherwise they would try to
    // acquire the same non-reentrant mutex again.
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_request_guard = if tp_active && headers.get(BATCH_CHILD_HEADER).is_none() {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let state = app.state.clone();
    let scheduler = app.scheduler.clone();
    let processing_started = Instant::now();
    open_tp_decode(&state, &headers, "/v1/prefill", &body);
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__session__")
        .to_string();
    let chunk_index = meta
        .get("chunk_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let chunk_size = meta
        .get("chunk_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(512) as u32;
    // Same next_hops-derives-is_last rule as handle_step — see its comment.
    let next_hops: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = next_hops
        .as_ref()
        .map(|hops| hops.is_empty())
        .unwrap_or_else(|| {
            meta.get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    // What *this* request says about chunking — correct as received for a
    // middle/tail hop (which just passes it through); wrong for the head,
    // which always gets `false` here (the sender can't know in advance) and
    // must override it with its own determination before relaying onward —
    // see the relay branch below and `owns_tokens` in `prefill_blocking`.
    let is_final_chunk = meta
        .get("is_final_chunk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let wire_dtype = WireDtype::F16;
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let prompt = meta
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // A composição dos tokens deste pedaço, gerada na cabeça (que tokenizou o
    // prompt) e ecoada até a cauda — é o que alimenta o `observe` da cabeça
    // `nextn` no `decode_hidden` (ver `prefill_blocking`). Ausente é peer antigo.
    let chunk_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Scale the hidden state once before moving into spawn_blocking.
    let input_prepare_started = Instant::now();
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };
    let input_prepare_us = input_prepare_started.elapsed().as_micros() as u64;
    // Neither of `prefill_blocking`'s non-final replies echoes `session_id`
    // back — a relay branch that forwards to the next hop needs its own
    // copy, taken before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    let queued_at = Instant::now();
    let result = match scheduler.submit_prefill(PrefillJob {
        sid_key,
        chunk_index,
        chunk_size,
        is_last,
        is_final_chunk,
        wire_dtype,
        prompt,
        tensor,
        chunk_tokens,
        queued_at,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("fila de inferência cheia (limite {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("scheduler de inferência encerrado");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida")
                || e.to_string().contains("fora de ordem")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (code, e.to_string()).into_response();
        }
        Err(e) => return err500(e),
    };
    reply.timings.frame_decode_us = frame_decode_us;
    reply.timings.input_prepare_us = input_prepare_us;

    // Same reasoning as handle_step: decode once so the relay branch below
    // can inspect what came back, patch in this hop's own processing_ms/
    // node_timings either way, and only then decide whether to relay or
    // just re-encode and return.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor) = match wire::decode(&reply.bytes) {
        Ok(v) => v,
        Err(e) => return err500(e),
    };
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta — ver o bloco gêmeo em `handle_step`. A diferença é o
    // `prompt_progress`: no anel borbulhado a cabeça sobrescrevia
    // `more_chunks`/`n_prompt_tokens` na resposta que passava de volta por ela
    // (`overlay_head_authoritative_fields`). Sem volta, ela manda os dois
    // **adiante**, e quem entrega copia — só a cabeça tokenizou o prompt, e sem
    // isso todo prompt de mais de um pedaço seria truncado no primeiro.
    if let Some(reply_to) = direct_reply_to(&meta) {
        let hop_timings = crate::direct::accumulate(
            &meta,
            crate::direct::hop_entry(
                &local_meta["processing_ms"],
                &local_meta["node_timings"],
                inbound_bytes,
            ),
        );
        let hops = next_hops.unwrap_or_default();

        if hops.is_empty() {
            let mut result = local_meta;
            apply_prompt_progress(&meta, &mut result);
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            let failure = crate::relay::RelayFailure {
                failed_node_id: hops[0].node_id.clone(),
                kind: "protocol".into(),
                message: "hop intermediário não produziu estado oculto".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = json!({
            "session_id": sid_for_forward,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_final_chunk": is_final_chunk_for_forward(&local_meta, is_final_chunk),
            "prompt_progress": prompt_progress_for_forward(&meta, &local_meta),
            "hop_timings": hop_timings,
        });
        preserve_batch_identity(&meta, &mut forward);
        if let Some(tokens) = local_meta.get("tokens") {
            forward["tokens"] = tokens.clone();
        }
        tokio::spawn(forward_direct(
            hops,
            "/v1/prefill",
            forward,
            residual,
            shape,
            crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
            reply_to,
        ));
        return direct_ack();
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode_batch_single(
            &local_meta,
            local_tensor.as_ref().map(|(v, s)| (v.as_slice(), *s)),
            WireDtype::F16,
        ) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    // Relay branch: this hop is not the tail, and the sender wants the rest
    // of the chain reached directly instead of relaying back through it.
    let hops = next_hops.expect("should_relay implies next_hops is Some");
    let (residual, shape) = local_tensor.expect("should_relay implies local_tensor is Some");
    let is_final_chunk_for_forward = is_final_chunk_for_forward(&local_meta, is_final_chunk);

    let mut bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/prefill",
        {
            let mut relay_meta = json!({
                "session_id": sid_for_forward,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "is_final_chunk": is_final_chunk_for_forward,
            });
            if let Some(tokens) = local_meta.get("tokens") {
                relay_meta["tokens"] = tokens.clone();
            }
            preserve_batch_identity(&meta, &mut relay_meta);
            relay_meta
        },
        (&residual, shape),
        crate::relay::CHUNK_ATTEMPTS, // prefill chunks are KV-rollback-safe to retry
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };

    overlay_head_authoritative_fields(&local_meta, &mut bubbled);

    match wire::encode_batch_single(&bubbled, None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

fn owns_tokens(local_meta: &Value) -> bool {
    local_meta
        .get("owns_tokens")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Only the head (`owns_tokens`) can know whether this chunk was the last
/// one — everyone else just passes through whatever `is_final_chunk` it was
/// told, since it has no `prompt_tokens` of its own to check that against.
fn is_final_chunk_for_forward(local_meta: &Value, received_is_final_chunk: bool) -> bool {
    if owns_tokens(local_meta) {
        !local_meta
            .get("more_chunks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    } else {
        received_is_final_chunk
    }
}

/// The verified fix: only the head's own `more_chunks`/`n_prompt_tokens` are
/// authoritative. A bubbled ring response's `more_chunks`/`n_prompt_tokens`
/// came from the *tail* (always `false`/absent there, since it never owns
/// the prompt) unless overwritten — leaving it as-is would silently
/// truncate every multi-chunk prompt on a ring pipeline after one chunk.
pub(crate) fn overlay_head_authoritative_fields(local_meta: &Value, bubbled: &mut Value) {
    if owns_tokens(local_meta) {
        bubbled["more_chunks"] = local_meta["more_chunks"].clone();
        bubbled["n_prompt_tokens"] = local_meta["n_prompt_tokens"].clone();
    }
}

/// Os mesmos dois campos autoritativos, empurrados **adiante** em vez de
/// sobrepostos na volta — é o que a resposta direta precisa, já que a cabeça
/// nunca mais vê o resultado. Quem não é a cabeça repassa intacto o que
/// recebeu.
pub(crate) fn prompt_progress_for_forward(inbound_meta: &Value, local_meta: &Value) -> Value {
    if owns_tokens(local_meta) {
        return json!({
            "more_chunks": local_meta["more_chunks"].clone(),
            "n_prompt_tokens": local_meta["n_prompt_tokens"].clone(),
        });
    }
    inbound_meta
        .get("prompt_progress")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A ponta da cadeia copia para o resultado o que a cabeça apurou do prompt.
/// Sem isto o gateway leria o `more_chunks: false` que a cauda sempre responde
/// — ela não tokenizou nada — e pararia o prefill no primeiro pedaço.
pub(crate) fn apply_prompt_progress(inbound_meta: &Value, result: &mut Value) {
    let Some(progress) = inbound_meta.get("prompt_progress") else {
        return;
    };
    for field in ["more_chunks", "n_prompt_tokens"] {
        if let Some(value) = progress.get(field) {
            if !value.is_null() {
                result[field] = value.clone();
            }
        }
    }
}

/// Preserve the identity assigned by `BatchCoordinator` when a one-item
/// frame takes the regular handler path. `encode_batch_single` cannot infer
/// that identity from the scheduler reply, so dropping these fields makes the
/// coordinator unable to match the response to its waiting request.
pub(crate) fn preserve_batch_identity(request_meta: &Value, response_meta: &mut Value) {
    for field in ["item_id", "batch_id"] {
        if let Some(value) = request_meta.get(field) {
            response_meta[field] = value.clone();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prefill_blocking(
    state: SharedState,
    sid_key: String,
    chunk_index: u32,
    chunk_size: u32,
    is_last: bool,
    is_final_chunk: bool,
    wire_dtype: WireDtype,
    prompt: Option<String>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    // A composição dos tokens deste pedaço, vinda da cabeça via `tokens` na
    // wire. Só tem sentido no branch de estado oculto (a cabeça tokeniza o
    // prompt; hops do meio ecoam): alimenta o `observe` da cabeça `nextn`
    // dentro do `decode_hidden`. `None` é peer antigo que não encaminha o
    // campo — o `decode_hidden` desliga o MTP com aviso.
    chunk_tokens: Option<Vec<i32>>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut is_final_chunk = is_final_chunk;
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    // O prefill espera no mesmo mutex do decode: um prompt grande de outra
    // sessão segura este aqui, e é a mesma espera invisível.
    let lock_started = Instant::now();
    let mut s = state.lock().unwrap();
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;

    // ── Session setup ────────────────────────────────────────────────────────

    let setup_started = Instant::now();
    s.get_or_create(&sid_key, chunk_index == 0)?;

    // First peer: tokenize the prompt on chunk 0.
    // We must not hold a &mut Session borrow while calling engine methods, so
    // compute everything we need, store results, then re-borrow.
    if let Some(ref text) = prompt {
        if chunk_index == 0 {
            let use_tmpl = s.chat_template;
            let no_think = s.no_thinking;
            let formatted = if use_tmpl {
                s.engine
                    .apply_chat_template(&[("user", text)], true, no_think)
            } else {
                text.clone()
            };
            let add_bos = !use_tmpl;
            let tokens = s.engine.tokenize(&formatted, add_bos, true);
            s.sessions.get_mut(&sid_key).unwrap().prompt_tokens = tokens;
        }
    }
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    // ── Rollback / order check ───────────────────────────────────────────────
    let expected_pos = chunk_index * chunk_size;
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        if session.n_past > expected_pos {
            warn!(
                "refazendo chunk {chunk_index} da sessão {}… (pos {expected_pos})",
                &sid_key[..8.min(sid_key.len())]
            );
            let seq_id = session.seq_id;
            if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                bail!(
                    "este modelo não aceita descartar posições do meio do cache, \
                     então refazer o pedaço {chunk_index} corromperia a sessão"
                );
            }
            s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
        } else if session.n_past < expected_pos {
            bail!(
                "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                session.n_past
            );
        }
    }

    // ── Decode ───────────────────────────────────────────────────────────────
    // Extract the session values we need as plain integers, drop the borrow,
    // then call the engine (which needs &mut s.engine).
    let (n_past, seq_id, tokens_for_chunk) = {
        let session = &s.sessions[&sid_key];
        let toks = if tensor.is_none() {
            let start = expected_pos as usize;
            let end = (expected_pos + chunk_size) as usize;
            session
                .prompt_tokens
                .get(start..end.min(session.prompt_tokens.len()))
                .unwrap_or(&[])
                .to_vec()
        } else {
            vec![]
        };
        (session.n_past, session.seq_id, toks)
    };

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    if let Some((hidden, shape)) = tensor {
        n_tokens = shape[0];
        engine_timing = s.engine.decode_hidden(
            &hidden,
            n_tokens,
            n_past,
            seq_id,
            !is_last,
            chunk_tokens.as_deref(),
        )?;
    } else {
        if tokens_for_chunk.is_empty() {
            bail!("chunk {chunk_index} não tem tokens");
        }
        n_tokens = tokens_for_chunk.len();
        engine_timing = s
            .engine
            .decode_tokens(&tokens_for_chunk, n_past, seq_id, !is_last)?;
    }
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    timings.tp = crate::tp::take_decode_telemetry();
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    // ── Session update ───────────────────────────────────────────────────────
    {
        let session = s.sessions.get_mut(&sid_key).unwrap();
        session.n_past += n_tokens as u32;
        session.last_used = Instant::now();
    }

    let (total_tokens, n_past_now) = {
        let session = &s.sessions[&sid_key];
        (session.prompt_tokens.len() as u32, session.n_past)
    };
    let owns_tokens = total_tokens > 0; // this peer has prompt_tokens → token path
    let more = owns_tokens && (n_past_now < total_tokens);
    if owns_tokens && !more {
        is_final_chunk = true;
    }

    // ── Sampling / residual ──────────────────────────────────────────────────
    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        if !is_final_chunk {
            let encode_started = Instant::now();
            let bytes = wire::encode(
                &json!({"done": false, "more_chunks": more}),
                None,
                wire_dtype,
            )?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            return Ok(TimedReply { bytes, timings });
        }
        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_profiled(logits, &recent, &sampling);
        let token_text_started = Instant::now();
        let text = s.engine.token_to_piece(next_id, false);
        let token_text_us = token_text_started.elapsed().as_micros() as u64;
        let done = next_id == eos;
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        // A réplica de um grupo TP não encerra sessão: quem decide o fim da
        // geração é o rank 0. Ver `tp::is_replica`.
        if done && !crate::tp::is_replica() {
            s.finish(&sid_key);
        }
        timings.sampling_us += logits_access_us
            + sampling_timing.logits_clone_us
            + sampling_timing.repetition_penalty_us
            + sampling_timing.softmax_us
            + sampling_timing.top_k_us
            + sampling_timing.top_p_us
            + sampling_timing.selection_us
            + token_text_us;
        timings.logits_access_us += logits_access_us;
        timings.logits_clone_us += sampling_timing.logits_clone_us;
        timings.repetition_penalty_us += sampling_timing.repetition_penalty_us;
        timings.softmax_us += sampling_timing.softmax_us;
        timings.top_k_us += sampling_timing.top_k_us;
        timings.top_p_us += sampling_timing.top_p_us;
        timings.selection_us += sampling_timing.selection_us;
        timings.token_text_us += token_text_us;
        let encode_started = Instant::now();
        let bytes = wire::encode(
            &json!({"done": done, "token_id": next_id, "text": text, "more_chunks": false}),
            None,
            wire_dtype,
        )?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let residual = s.engine.take_residual(n_tokens)?;
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let n_prompt_tokens = s.sessions[&sid_key].prompt_tokens.len();
    let n_embd = s.engine.n_embd;
    let mut reply_meta = json!({
        "done": false,
        "more_chunks": more,
        "n_prompt_tokens": n_prompt_tokens,
        // Only meaningful when true: whether *this* peer is the one that
        // tokenized the prompt, i.e. whether `more_chunks`/`n_prompt_tokens`
        // above are its own authoritative determination rather than the
        // `false`/`0` placeholder every other peer replies with. Ring
        // mode's relay-and-bubble uses this to know which hop's reply is
        // allowed to overwrite what the tail bubbles back — see
        // `handle_prefill`.
        "owns_tokens": owns_tokens,
    });
    // A composição real deste pedaço, para a cauda manter a cabeça `nextn` em
    // dia (ver o doc de `chunk_tokens`). A cabeça conhece os tokens; hops do
    // meio ecoam o que receberam.
    if owns_tokens {
        reply_meta["tokens"] = json!(tokens_for_chunk);
    } else if let Some(tokens) = &chunk_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual, [n_tokens, n_embd])),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}

struct PreparedPrefill {
    sid_key: String,
    seq_id: u32,
    n_past: u32,
    n_tokens: usize,
    is_last: bool,
    is_final_chunk: bool,
    owns_tokens: bool,
    tokens_for_chunk: Vec<i32>,
    chunk_tokens: Option<Vec<i32>>,
    input: PreparedPrefillInput,
    wire_dtype: WireDtype,
    queued_at: Instant,
}

enum PreparedPrefillInput {
    Tokens(Vec<i32>),
    Hidden(Vec<f32>),
}

fn prefill_batch_blocking(
    state: SharedState,
    jobs: Vec<(PrefillJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = state.lock().unwrap();
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let PrefillJob {
            sid_key,
            chunk_index,
            chunk_size,
            is_last,
            is_final_chunk,
            wire_dtype,
            prompt,
            tensor,
            chunk_tokens,
            queued_at,
        } = job;

        let result = (|| -> Result<PreparedPrefill> {
            s.get_or_create(&sid_key, chunk_index == 0)?;
            if let Some(text) = prompt.as_deref() {
                if chunk_index == 0 {
                    let formatted = if s.chat_template {
                        s.engine
                            .apply_chat_template(&[("user", text)], true, s.no_thinking)
                    } else {
                        text.to_string()
                    };
                    let tokens = s.engine.tokenize(&formatted, !s.chat_template, true);
                    s.sessions.get_mut(&sid_key).unwrap().prompt_tokens = tokens;
                }
            }

            let expected_pos = chunk_index
                .checked_mul(chunk_size)
                .ok_or_else(|| anyhow::anyhow!("posição do chunk excede o limite numérico"))?;
            {
                let session = s.sessions.get(&sid_key).unwrap();
                if session.n_past > expected_pos {
                    let seq_id = session.seq_id;
                    if !s.engine.kv_remove(seq_id, expected_pos as i32, -1) {
                        bail!(
                            "este modelo não aceita descartar posições do meio do cache, \
                             então refazer o pedaço {chunk_index} corromperia a sessão"
                        );
                    }
                    s.sessions.get_mut(&sid_key).unwrap().n_past = expected_pos;
                } else if session.n_past < expected_pos {
                    bail!(
                        "chunk fora de ordem: recebi {chunk_index} (pos {expected_pos}) mas a sessão está em {}",
                        session.n_past
                    );
                }
            }

            let (seq_id, n_past, prompt_tokens, input, n_tokens) = {
                let session = &s.sessions[&sid_key];
                if let Some((hidden, shape)) = tensor {
                    if shape[0] == 0 || shape[1] != s.engine.n_embd {
                        bail!(
                            "hidden state de {} tem shape {:?}, esperado [tokens, {}]",
                            sid_key,
                            shape,
                            s.engine.n_embd
                        );
                    }
                    let n_tokens = shape[0];
                    if hidden.len() != n_tokens.saturating_mul(s.engine.n_embd) {
                        bail!(
                            "hidden state de {} tem {} floats, esperado {}",
                            sid_key,
                            hidden.len(),
                            n_tokens.saturating_mul(s.engine.n_embd)
                        );
                    }
                    (
                        session.seq_id,
                        session.n_past,
                        Vec::new(),
                        PreparedPrefillInput::Hidden(hidden),
                        n_tokens,
                    )
                } else {
                    let start = expected_pos as usize;
                    let end = start.saturating_add(chunk_size as usize);
                    let tokens = session
                        .prompt_tokens
                        .get(start..end.min(session.prompt_tokens.len()))
                        .unwrap_or(&[])
                        .to_vec();
                    if tokens.is_empty() {
                        bail!("chunk {chunk_index} não tem tokens");
                    }
                    let n_tokens = tokens.len();
                    (
                        session.seq_id,
                        session.n_past,
                        tokens.clone(),
                        PreparedPrefillInput::Tokens(tokens),
                        n_tokens,
                    )
                }
            };
            let owns_tokens =
                !prompt_tokens.is_empty() || !s.sessions[&sid_key].prompt_tokens.is_empty();

            Ok(PreparedPrefill {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                is_last,
                is_final_chunk,
                owns_tokens,
                tokens_for_chunk: prompt_tokens,
                chunk_tokens,
                input,
                wire_dtype,
                queued_at,
            })
        })();

        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }

    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    let decode_result = match &prepared[0].1.input {
        PreparedPrefillInput::Tokens(_) => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedPrefillInput::Tokens(tokens) => tokens.clone(),
                        PreparedPrefillInput::Hidden(_) => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedPrefillInput::Hidden(_) => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedPrefillInput::Hidden(hidden) => hidden.clone(),
                        PreparedPrefillInput::Tokens(_) => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };

    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let session = s.sessions.get_mut(&item.sid_key).unwrap();
            session.n_past = session.n_past.saturating_add(item.n_tokens as u32);
            session.last_used = Instant::now();
            let total_tokens = session.prompt_tokens.len() as u32;
            let n_past_now = session.n_past;
            let more = item.owns_tokens && n_past_now < total_tokens;
            let mut is_final_chunk = item.is_final_chunk;
            if item.owns_tokens && !more {
                is_final_chunk = true;
            }

            let mut timings = NodeTimings {
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us: lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };

            if item.is_last {
                if !is_final_chunk {
                    let started = Instant::now();
                    let bytes = wire::encode(
                        &json!({"done": false, "more_chunks": more}),
                        None,
                        item.wire_dtype,
                    )?;
                    timings.encode_us = started.elapsed().as_micros() as u64;
                    return Ok(TimedReply { bytes, timings });
                }

                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let text = s.engine.token_to_piece(next_id, false);
                let done = next_id == s.engine.eos_token();
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                if done && !crate::tp::is_replica() {
                    s.finish(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us;
                let token_text_started = Instant::now();
                let bytes = wire::encode(
                    &json!({"done": done, "token_id": next_id, "text": text, "more_chunks": false}),
                    None,
                    item.wire_dtype,
                )?;
                timings.token_text_us = token_text_started.elapsed().as_micros() as u64;
                timings.encode_us = 0;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let residual = output.residual_for(&item.sid_key, s.engine.n_embd)?;
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let mut meta = json!({
                "done": false,
                "more_chunks": more,
                "n_prompt_tokens": total_tokens,
                "owns_tokens": item.owns_tokens,
            });
            if item.owns_tokens {
                meta["tokens"] = json!(item.tokens_for_chunk);
            } else if let Some(tokens) = &item.chunk_tokens {
                meta["tokens"] = json!(tokens);
            }
            let started = Instant::now();
            let bytes = wire::encode(
                &meta,
                Some((&residual, [item.n_tokens, s.engine.n_embd])),
                item.wire_dtype,
            )?;
            timings.encode_us = started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

fn step_batch_blocking(
    state: SharedState,
    jobs: Vec<(StepJob, oneshot::Sender<Result<TimedReply>>)>,
) -> Vec<(oneshot::Sender<Result<TimedReply>>, Result<TimedReply>)> {
    if jobs.iter().any(|(job, _)| {
        job.speculate.is_some() || job.incoming_draft.is_some() || job.mtp_k.is_some()
    }) {
        return jobs
            .into_iter()
            .map(|(job, reply)| {
                let result = step_blocking(
                    state.clone(),
                    job.sid_key,
                    job.is_last,
                    job.wire_dtype,
                    job.token_id,
                    job.tensor,
                    job.speculate,
                    job.incoming_draft,
                    job.batch_tokens,
                    job.mtp_k,
                    job.queued_at,
                );
                (reply, result)
            })
            .collect();
    }

    let mut replies = Vec::with_capacity(jobs.len());
    let lock_started = Instant::now();
    let mut s = state.lock().unwrap();
    let lock_wait_us = lock_started.elapsed().as_micros() as u64;
    let mut prepared = Vec::with_capacity(jobs.len());

    for (job, reply) in jobs {
        let StepJob {
            sid_key,
            is_last,
            wire_dtype,
            token_id,
            tensor,
            queued_at,
            ..
        } = job;
        let result = (|| -> Result<PreparedStep> {
            let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
                anyhow::anyhow!("sessão '{}' desconhecida - faça o prefill antes", sid_key)
            })?;
            session.last_used = Instant::now();
            let seq_id = session.seq_id;
            let n_past = session.n_past;
            let input = if let Some((hidden, shape)) = tensor {
                if shape[0] == 0 || shape[1] != s.engine.n_embd {
                    bail!(
                        "hidden state de {} tem shape {:?}, esperado [tokens, {}]",
                        sid_key,
                        shape,
                        s.engine.n_embd
                    );
                }
                let n_tokens = shape[0];
                if hidden.len() != n_tokens.saturating_mul(s.engine.n_embd) {
                    bail!(
                        "hidden state de {} tem {} floats, esperado {}",
                        sid_key,
                        hidden.len(),
                        n_tokens.saturating_mul(s.engine.n_embd)
                    );
                }
                PreparedStepInput::Hidden { hidden, n_tokens }
            } else {
                let token =
                    token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
                s.sessions
                    .get_mut(&sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(token);
                PreparedStepInput::Tokens { token }
            };
            let n_tokens = match &input {
                PreparedStepInput::Tokens { .. } => 1,
                PreparedStepInput::Hidden { n_tokens, .. } => *n_tokens,
            };
            Ok(PreparedStep {
                sid_key,
                seq_id,
                n_past,
                n_tokens,
                is_last,
                wire_dtype,
                input,
                queued_at,
            })
        })();
        match result {
            Ok(item) => prepared.push((reply, item)),
            Err(error) => replies.push((reply, Err(error))),
        }
    }

    if prepared.is_empty() {
        return replies;
    }
    let need_residual = prepared.iter().any(|(_, item)| !item.is_last);
    let decode_result = match &prepared[0].1.input {
        PreparedStepInput::Tokens { .. } => {
            let entries: Vec<crate::llm::TokenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::TokenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    tokens: match &item.input {
                        PreparedStepInput::Tokens { token } => vec![*token],
                        PreparedStepInput::Hidden { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_tokens_batch(&entries, need_residual)
        }
        PreparedStepInput::Hidden { .. } => {
            let entries: Vec<crate::llm::HiddenBatchEntry> = prepared
                .iter()
                .map(|(_, item)| crate::llm::HiddenBatchEntry {
                    descriptor: crate::llm::BatchEntry {
                        item_id: item.sid_key.clone(),
                        seq_id: item.seq_id,
                        pos: item.n_past,
                        tokens: item.n_tokens,
                        want_logits: item.is_last,
                    },
                    hidden: match &item.input {
                        PreparedStepInput::Hidden { hidden, .. } => hidden.clone(),
                        PreparedStepInput::Tokens { .. } => Vec::new(),
                    },
                })
                .collect();
            s.engine.decode_hidden_batch(&entries, need_residual)
        }
    };
    let (output, engine_timings) = match decode_result {
        Ok(value) => value,
        Err(error) => {
            let message = error.to_string();
            for (reply, _) in prepared {
                replies.push((reply, Err(anyhow::anyhow!(message.clone()))));
            }
            return replies;
        }
    };

    for (reply, item) in prepared {
        let result = (|| -> Result<TimedReply> {
            let new_n_past = s.sessions[&item.sid_key]
                .n_past
                .saturating_add(item.n_tokens as u32);
            s.sessions.get_mut(&item.sid_key).unwrap().n_past = new_n_past;
            let mut timings = NodeTimings {
                queue_us: item.queued_at.elapsed().as_micros() as u64,
                lock_wait_us,
                decode_us: engine_timings.enqueue_us,
                backend_completion_us: engine_timings.backend_completion_us,
                residual_capture_d2h_sync_us: engine_timings.residual_capture_d2h_sync_us,
                ..Default::default()
            };

            if item.is_last {
                let logits_index = output
                    .layout
                    .range(&item.sid_key)
                    .and_then(|range| range.logits_index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("batch não produziu logits para {}", item.sid_key)
                    })?;
                let recent = s.sessions[&item.sid_key].recent_tokens.clone();
                let logits = s.engine.logits_ith(logits_index as i32);
                let (next_id, sampling_timing) =
                    sample_token_profiled(logits, &recent, &s.sampling);
                let token_text_started = Instant::now();
                let text = s.engine.token_to_piece(next_id, false);
                let token_text_us = token_text_started.elapsed().as_micros() as u64;
                let done = next_id == s.engine.eos_token();
                s.sessions
                    .get_mut(&item.sid_key)
                    .unwrap()
                    .recent_tokens
                    .push(next_id);
                if done && !crate::tp::is_replica() {
                    s.finish(&item.sid_key);
                }
                timings.logits_clone_us = sampling_timing.logits_clone_us;
                timings.repetition_penalty_us = sampling_timing.repetition_penalty_us;
                timings.softmax_us = sampling_timing.softmax_us;
                timings.top_k_us = sampling_timing.top_k_us;
                timings.top_p_us = sampling_timing.top_p_us;
                timings.selection_us = sampling_timing.selection_us;
                timings.token_text_us = token_text_us;
                timings.sampling_us = sampling_timing.logits_clone_us
                    + sampling_timing.repetition_penalty_us
                    + sampling_timing.softmax_us
                    + sampling_timing.top_k_us
                    + sampling_timing.top_p_us
                    + sampling_timing.selection_us
                    + token_text_us;
                let encode_started = Instant::now();
                let bytes = wire::encode(
                    &json!({"done": done, "token_id": next_id, "text": text}),
                    None,
                    item.wire_dtype,
                )?;
                timings.encode_us = encode_started.elapsed().as_micros() as u64;
                return Ok(TimedReply { bytes, timings });
            }

            let residual_started = Instant::now();
            let residual = output.residual_for(&item.sid_key, s.engine.n_embd)?;
            timings.residual_us = residual_started.elapsed().as_micros() as u64;
            let tokens = match &item.input {
                PreparedStepInput::Tokens { token } => vec![*token],
                PreparedStepInput::Hidden { .. } => Vec::new(),
            };
            let mut meta = json!({"done": false});
            if !tokens.is_empty() {
                meta["tokens"] = json!(tokens);
            }
            let encode_started = Instant::now();
            let bytes = wire::encode(
                &meta,
                Some((&residual, [item.n_tokens, s.engine.n_embd])),
                item.wire_dtype,
            )?;
            timings.encode_us = encode_started.elapsed().as_micros() as u64;
            Ok(TimedReply { bytes, timings })
        })();
        replies.push((reply, result));
    }
    replies
}

struct PreparedStep {
    sid_key: String,
    seq_id: u32,
    n_past: u32,
    n_tokens: usize,
    is_last: bool,
    wire_dtype: WireDtype,
    input: PreparedStepInput,
    queued_at: Instant,
}

enum PreparedStepInput {
    Tokens { token: i32 },
    Hidden { hidden: Vec<f32>, n_tokens: usize },
}

// ── /v1/step ─────────────────────────────────────────────────────────────────

async fn handle_step(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if let Ok(batch) = wire::decode_batch(&body) {
        if batch.items.len() > 1 {
            return handle_step_batch(app, headers, batch).await;
        }
    }
    // See the matching guard in `handle_prefill`: a one-item batch must not
    // overwrite the decode/barrier state of another TP request while its
    // ranks are still in the collective.
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_request_guard = if tp_active && headers.get(BATCH_CHILD_HEADER).is_none() {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let state = app.state.clone();
    let scheduler = app.scheduler.clone();
    open_tp_decode(&state, &headers, "/v1/step", &body);
    let processing_started = Instant::now();
    // Lido no começo porque `state` é consumido mais abaixo, e o valor é o
    // mesmo em qualquer ponto do handler.
    let tls_handshake_ms = last_handshake_ms(&state);
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode_inference_request(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__session__")
        .to_string();
    // Present and non-empty only when the sender negotiated relay mode
    // (`PipelinePlan::supports_relay_chain`) — the ordered chain from *our*
    // next hop through the tail. An empty list still parses as `Some(vec![])`
    // and behaves exactly like `None`: this hop is the tail either way.
    let next_hops: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = next_hops
        .as_ref()
        .map(|hops| hops.is_empty())
        .unwrap_or_else(|| {
            meta.get("is_last")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    let wire_dtype = WireDtype::F16;
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let token_id = meta
        .get("token_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    // Present only on the request to the head — see `step_blocking`'s
    // `speculate` param doc.
    let speculate = Speculate::from_meta(&meta);
    // Present on every non-head request in a speculative round — see
    // `step_blocking`'s `incoming_draft` param doc for why `None` and
    // `Some(vec![])` mean different things here.
    let incoming_draft: Option<Vec<i32>> = meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A composição do lote decodificado na cabeça, ecoada de hop em hop até a
    // cauda — é o que alimenta o `observe` da cabeça `nextn` (ver o doc de
    // `batch_tokens` em `step_blocking`). Ausente é peer antigo.
    let batch_tokens: Option<Vec<i32>> = meta
        .get("tokens")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // A largura de lote que o gateway quer na rodada seguinte, gerada na
    // cabeça (do `speculate`) e ecoada até a cauda (ver o doc de `mtp_k`).
    let mtp_k: Option<u32> = meta.get("mtp_k").and_then(|v| v.as_u64()).map(|v| v as u32);

    let input_prepare_started = Instant::now();
    let tensor = match tensor {
        Some((mut values, shape)) => {
            if scale != 1.0 {
                values.iter_mut().for_each(|v| *v *= scale);
            }
            Some((values, shape))
        }
        None => None,
    };
    let input_prepare_us = input_prepare_started.elapsed().as_micros() as u64;
    // Neither of `step_blocking`'s replies echoes `session_id` back — a
    // relay branch that forwards to the next hop needs its own copy, taken
    // before `sid_key` moves into the blocking closure below.
    let sid_for_forward = sid_key.clone();

    let queued_at = Instant::now();
    let result = match scheduler.submit_step(StepJob {
        sid_key,
        is_last,
        wire_dtype,
        token_id,
        tensor,
        speculate,
        incoming_draft,
        batch_tokens,
        mtp_k,
        queued_at,
    }) {
        Ok(receiver) => receiver.await,
        Err(SchedulerSubmitError::Full { limit }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, "1")],
                format!("fila de inferência cheia (limite {limit})"),
            )
                .into_response();
        }
        Err(SchedulerSubmitError::Closed) => {
            return err500("scheduler de inferência encerrado");
        }
    };

    let mut reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            let code = if e.to_string().contains("desconhecida") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (code, e.to_string()).into_response();
        }
        Err(e) => return err500(e),
    };
    reply.timings.frame_decode_us = frame_decode_us;
    reply.timings.input_prepare_us = input_prepare_us;

    // Decode what step_blocking already encoded so we can tell a residual
    // (needs relaying, if `next_hops` says so) from a final answer (the
    // tail's `{token_id, text, done}`, with no tensor) — and, either way, to
    // patch in this hop's own `processing_ms`/`node_timings` before it goes
    // any further. `handle_prefill` does the identical patch for the same
    // reason.
    let elapsed = processing_started.elapsed();
    let (mut local_meta, local_tensor) = match wire::decode(&reply.bytes) {
        Ok(v) => v,
        Err(e) => return err500(e),
    };
    local_meta["processing_ms"] = json!(elapsed.as_millis() as u64);
    local_meta["node_timings"] = reply.timings.json();
    local_meta["tls_handshake_ms"] = json!(tls_handshake_ms);
    preserve_batch_identity(&meta, &mut local_meta);

    // Resposta direta: quem terminar entrega ao gateway, e o que sai daqui é
    // só um aceite. Cada hop descreve a si mesmo numa lista que viaja na ida.
    if let Some(reply_to) = direct_reply_to(&meta) {
        let hop_timings = crate::direct::accumulate(
            &meta,
            crate::direct::hop_entry(
                &local_meta["processing_ms"],
                &local_meta["node_timings"],
                inbound_bytes,
            ),
        );
        let hops = next_hops.unwrap_or_default();

        if hops.is_empty() {
            // Cauda: o resultado sai daqui direto para quem pediu.
            let mut result = local_meta;
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(http_client(), &reply_to, result).await;
            });
            return direct_ack();
        }

        let Some((residual, shape)) = local_tensor else {
            // Não somos a cauda e não temos residual: a cadeia não anda. Dizer
            // isso ao gateway agora vale mais que deixá-lo esperar o prazo.
            let failure = crate::relay::RelayFailure {
                failed_node_id: hops[0].node_id.clone(),
                kind: "protocol".into(),
                message: "hop intermediário não produziu estado oculto".into(),
            };
            tokio::spawn(async move {
                crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
            });
            return direct_ack();
        };

        let mut forward = json!({
            "session_id": sid_for_forward,
            "hop_timings": hop_timings,
        });
        preserve_batch_identity(&meta, &mut forward);
        if let Some(draft) = local_meta.get("draft") {
            forward["draft"] = draft.clone();
        }
        if let Some(tokens) = local_meta.get("tokens") {
            forward["tokens"] = tokens.clone();
        }
        if let Some(mtp_k) = local_meta.get("mtp_k") {
            forward["mtp_k"] = mtp_k.clone();
        }
        tokio::spawn(forward_direct(
            hops, "/v1/step", forward, residual, shape,
            1, // a decode step has never retried — see relay_and_bubble's doc comment
            reply_to,
        ));
        return direct_ack();
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode_batch_single(
            &local_meta,
            local_tensor.as_ref().map(|(v, s)| (v.as_slice(), *s)),
            WireDtype::F16,
        ) {
            Ok(bytes) => (
                StatusCode::OK,
                [("content-type", "application/octet-stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err500(e),
        };
    }

    // Relay branch: this hop is not the tail, and the sender wants the rest
    // of the chain reached directly instead of relaying back through it.
    let hops = next_hops.expect("should_relay implies next_hops is Some");
    let (residual, shape) = local_tensor.expect("should_relay implies local_tensor is Some");
    let mut forward_meta = json!({"session_id": sid_for_forward});
    preserve_batch_identity(&meta, &mut forward_meta);
    // Keep threading the draft toward the tail — absent entirely (not just
    // an empty array) when this round isn't speculative, same distinction
    // `incoming_draft` relies on above.
    if let Some(draft) = local_meta.get("draft") {
        forward_meta["draft"] = draft.clone();
    }
    // Idem para `tokens`/`mtp_k` — o que mantém a cauda observando o lote e
    // rascunhando a medida certa (ver os docs de `batch_tokens`/`mtp_k`).
    if let Some(tokens) = local_meta.get("tokens") {
        forward_meta["tokens"] = tokens.clone();
    }
    if let Some(mtp_k) = local_meta.get("mtp_k") {
        forward_meta["mtp_k"] = mtp_k.clone();
    }

    let bubbled = match relay_and_bubble(
        http_client(),
        &hops,
        "/v1/step",
        forward_meta,
        (&residual, shape),
        1, // a decode step has never retried — see relay_and_bubble's doc comment
        local_meta["processing_ms"].clone(),
        local_meta["node_timings"].clone(),
    )
    .await
    {
        Ok(bubbled) => bubbled,
        Err(resp) => return resp,
    };
    match wire::encode_batch_single(&bubbled, None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Relay a residual to `hops[0]` and bubble its reply back: the eventual
/// tail's real answer, plus this hop's own already-known contribution
/// (`local_processing_ms`/`local_node_timings`, computed by the caller from
/// its own `NodeTimings`) prepended onto whatever `hop_timings` bubbled up
/// from further down the chain. Returns the merged `meta` — *not* yet
/// re-encoded — so a caller with phase-specific fields to overlay (prefill's
/// `more_chunks`/`n_prompt_tokens`, see `handle_prefill`) can add them
/// before the final `wire::encode`.
///
/// `base_meta` carries whatever the phase needs beyond what every relay call
/// needs (`is_last`/`scale`/`next_hops`, added here): just `session_id` for
/// `/v1/step`, plus `chunk_index`/`chunk_size`/`is_final_chunk` for
/// `/v1/prefill`. `attempts` follows the same rule `send_chunk`/`step_ring`
/// already do at the gateway: `CHUNK_ATTEMPTS` for a prefill chunk (retry-safe,
/// KV-rollback protected), `1` for a decode step (not).
///
/// Split out from `handle_step`/`handle_prefill` (both call this with the
/// same `hops`/tensor shape, differing only in `path` and `base_meta`) so
/// it's directly testable against a fake next-hop server — see the
/// `relay_tests` module — without needing a loaded `LlmEngine`.
async fn relay_and_bubble(
    http: &crate::peer_client::PeerClients,
    hops: &[PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], [usize; 2]),
    attempts: u32,
    local_processing_ms: Value,
    local_node_timings: Value,
) -> std::result::Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http,
        next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some(tensor),
        WireDtype::F16,
        next.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    match sent {
        Ok(sent) => {
            if sent.tensor.is_some() {
                // A fully-bubbled reply is always the tail's final answer,
                // which never carries a tensor — only a *forward* (not yet
                // relayed) residual does. Getting here means some hop in the
                // chain didn't relay when it should have; surviving loud
                // rather than silently dropping the tensor.
                warn!(
                    "resposta de {} veio com tensor num retransmissão já borbulhada — descartando",
                    next.node_id
                );
            }
            let hop_timings = crate::relay::bubble_hop_timings(&sent);
            let mut bubbled = sent.meta.clone();
            bubbled["hop_timings"] = json!(hop_timings);
            // Ours, not the next hop's: our caller needs these to build our
            // own entry, the same way we just built the next hop's.
            bubbled["processing_ms"] = local_processing_ms;
            bubbled["node_timings"] = local_node_timings;
            Ok(bubbled)
        }
        Err(e) => {
            let failure = crate::relay::extract_relay_failure(&e).unwrap_or_else(|| {
                crate::relay::RelayFailure {
                    failed_node_id: next.node_id.clone(),
                    kind: issue_kind_label(last_issue_kind).to_string(),
                    message: e.to_string(),
                }
            });
            Err((StatusCode::BAD_GATEWAY, Json(failure)).into_response())
        }
    }
}

// ── Resposta direta (`relay/v2-direct`) ──────────────────────────────────────

/// Para onde o resultado desta cadeia deve ir, quando quem despachou pediu
/// resposta direta. Ausente em todo quadro do modo hub e do anel borbulhado —
/// é este campo, e só ele, que liga o caminho novo.
pub(crate) fn direct_reply_to(meta: &Value) -> Option<Value> {
    meta.get("reply_to").filter(|v| !v.is_null()).cloned()
}

/// O que este hop devolve a quem o chamou quando a cadeia responde direto: um
/// aceite, e nada mais. A resposta de verdade não passa por aqui — ver
/// [`crate::direct`].
pub(crate) fn direct_ack() -> Response {
    match wire::encode_batch_single(&json!({"accepted": true}), None, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}

/// Repassa o residual ao próximo hop e some. Roda numa task própria: quem nos
/// chamou já foi aceito e não espera mais nada daqui.
///
/// Falhou o repasse, quem denuncia somos nós — direto ao gateway, nomeando o
/// nó que realmente morreu. É o caminho que substitui o `RelayFailure` subindo
/// pela pilha aninhada do anel borbulhado.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_direct(
    hops: Vec<PipelineStep>,
    path: &'static str,
    mut base_meta: Value,
    residual: Vec<f32>,
    shape: [usize; 2],
    attempts: u32,
    reply_to: Value,
) {
    let next = hops[0].clone();
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);
    base_meta["reply_to"] = reply_to.clone();

    let mut last_issue_kind = None;
    let sent = crate::relay::send_frame_or_tunnel(
        http_client(),
        &next,
        crate::infer_tunnel::tunnel_sender(),
        path,
        &base_meta,
        Some((&residual, shape)),
        WireDtype::F16,
        next.frame_encoding(),
        attempts,
        &next.node_id,
        |issue| last_issue_kind = Some(issue.kind),
    )
    .await;

    if let Err(e) = sent {
        let failure =
            crate::relay::extract_relay_failure(&e).unwrap_or_else(|| crate::relay::RelayFailure {
                failed_node_id: next.node_id.clone(),
                kind: issue_kind_label(last_issue_kind).to_string(),
                message: e.to_string(),
            });
        warn!(
            "repasse direto para {} falhou: {} — avisando o gateway",
            failure.failed_node_id, failure.message
        );
        crate::direct::deliver_failure(http_client(), &reply_to, failure).await;
    }
}

fn issue_kind_label(kind: Option<crate::relay::SendIssueKind>) -> &'static str {
    match kind {
        Some(crate::relay::SendIssueKind::Timeout) => "timeout",
        Some(crate::relay::SendIssueKind::Transport) => "transport",
        Some(crate::relay::SendIssueKind::Http) => "http",
        Some(crate::relay::SendIssueKind::Protocol) => "protocol",
        None => "unknown",
    }
}

/// Search `history` for the most recent prior occurrence of its own trailing
/// `m` tokens, and propose whatever followed there as a draft continuation
/// of up to `k - 1` tokens. An empty result (no history, no match, or the
/// match runs off the end of `history` with nothing after it) means "draft
/// nothing" — the caller then decodes exactly `[confirmed_token]`, identical
/// to today's non-speculative behavior. This never looks at model weights or
/// probabilities — it's a plain array search — which is exactly why
/// verification (`longest_match`, below) can never trust it: the draft is
/// only ever a guess about what to *batch*, never a substitute for sampling.
/// `m` is the **longest** n-gram tried, not the only one: the search walks
/// `m, m-1, … 1` and stops at the first length that matches, so a window that
/// finds nothing degrades into a shorter, weaker guess instead of into no
/// guess at all. Longer wins because it is stronger evidence; how much of the
/// continuation is trusted scales with the matched length (`m` tokens of
/// context authorise at most `m + 1` guesses), because a one-token match
/// pointing anywhere in the history is nearly a coin flip and every extra
/// drafted token widens the batch on every hop of the ring.
/// Vale rascunhar nesta rodada?
///
/// Especular é apostar: um rascunho recusado tem de ser desfeito do cache
/// antes da rodada seguinte (`gateway::rollback_sessions`, via
/// `/v1/session/rollback` -> `kv_remove` -> `llama_memory_seq_rm`).
///
/// **Não é `kv_can_shift`/`llama_memory_can_shift` quem decide isso** - essa
/// flag é sobre outra coisa (o "K-shift" de reposicionar RoPE quando o
/// contexto some por cima, `common.cpp`'s `ctx_shift` no llama.cpp de
/// referência; nenhum código deste projeto ativa esse recurso). A
/// especulação de verdade só depende de `seq_rm` aceitar remover o fim da
/// sequência, e isso já é decidido - e reportado de volta - em tempo real
/// por `handle_rollback`; não há necessidade de adivinhar antes. Um modelo
/// híbrido/recorrente (Qwen3.5+) só aceita esse `seq_rm` parcial se o
/// contexto foi criado com `n_rs_seq > 0` (`EngineConfig::rs_seq_window`,
/// `llm_arch_supports_rs_rollback` no llama.cpp) - sem isso `seq_rm`
/// simplesmente recusa, `rollback_sessions` percebe pela resposta e encerra
/// a geração com o que já foi confirmado (ver seu call site em
/// `gateway::generate_stream`) em vez de deixar o `n_past` do nó divergir do
/// cache, que é o que produzia "inconsistent sequence positions".
pub(crate) fn should_draft(speculate: Option<Speculate>, mtp_ready: bool) -> bool {
    match speculate {
        None => false,
        Some(s) if s.source == DraftSource::Mtp => {
            // O rascunho pode nascer aqui (bloco `nextn` local, máquina
            // única) ou vir pronto do gateway (a cauda de um pipeline
            // fatiado o produziu na rodada anterior).
            mtp_ready || s.draft.is_some()
        }
        Some(_) => true,
    }
}

/// O convite a rascunhar que a cabeça recebe numa rodada.
///
/// `source` é o campo que separa os dois mecanismos, e ele existe porque eles
/// **não** são a mesma coisa com números diferentes: `Lookup` é uma busca por
/// repetição no texto já gerado (`find_draft`, nenhum peso envolvido) e `Mtp`
/// é o bloco `nextn` treinado dentro do GGUF (`crate::mtp`). Um `speculate`
/// sem `source` vem de um gateway antigo e vale `Lookup`, que é o que ele
/// sempre quis dizer.
#[derive(Debug, Clone)]
pub(crate) struct Speculate {
    /// Largura do lote: o token confirmado mais até `k - 1` palpites.
    pub k: u32,
    /// Janela de história do `find_draft`. Ignorada pelo `Mtp`, que não olha
    /// história nenhuma.
    pub m: u32,
    pub source: DraftSource,
    /// Rascunho pronto, produzido pela cauda num pipeline fatiado. A cabeça usa
    /// este em vez de chamar `mtp_draft`. `None` significa que a cabeça deve
    /// produzir o próprio rascunho (modelo inteiro) ou que não há rascunho.
    pub draft: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftSource {
    Lookup,
    Mtp,
}

impl DraftSource {
    /// O nome no fio (wire), espelhando o `DraftSource::wire` de `prefill.rs`.
    pub(crate) fn wire(&self) -> &'static str {
        match self {
            DraftSource::Lookup => "lookup",
            DraftSource::Mtp => "mtp",
        }
    }
}

impl Speculate {
    /// Lê o convite do campo `speculate` do quadro.
    ///
    /// Público ao crate porque a bancada `spec_lab` fala o protocolo de
    /// verdade: uma segunda cópia desta leitura ali passaria a valer como
    /// contrato sem nunca ter sido conferida contra esta.
    pub(crate) fn from_meta(meta: &Value) -> Option<Self> {
        let v = meta.get("speculate")?;
        let draft = v.get("draft").and_then(|d| {
            d.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_i64())
                    .map(|v| v as i32)
                    .collect()
            })
        });
        Some(Speculate {
            k: v.get("k")?.as_u64()? as u32,
            m: v.get("m")?.as_u64()? as u32,
            source: match v.get("source").and_then(|s| s.as_str()) {
                Some("mtp") => DraftSource::Mtp,
                _ => DraftSource::Lookup,
            },
            draft,
        })
    }

    /// Serializa o convite para o campo `speculate` do quadro — a contraparte
    /// de `from_meta`. O gateway a usa para injetar, no `step` de um pipeline
    /// fatiado, o rascunho que a cauda produziu na rodada anterior
    /// (`draft` preenchido).
    pub(crate) fn to_meta(&self) -> Value {
        let mut v = json!({"k": self.k, "m": self.m, "source": self.source.wire()});
        if let Some(draft) = &self.draft {
            v["draft"] = json!(draft);
        }
        v
    }
}

pub(crate) fn find_draft(history: &[i32], m: usize, k: u32) -> Vec<i32> {
    let extra = k.saturating_sub(1) as usize;
    if extra == 0 {
        return Vec::new();
    }
    for len in (1..=m.max(1)).rev() {
        if history.len() <= len {
            continue;
        }
        let needle = &history[history.len() - len..];
        let allowed = extra.min(len + 1);
        // Most recent first: `start` walks backward from the last position a
        // *prior* (non-overlapping-with-needle) occurrence could begin.
        for start in (0..history.len() - len).rev() {
            if &history[start..start + len] != needle {
                continue;
            }
            let continuation_start = start + len;
            if continuation_start >= history.len() {
                continue;
            }
            let continuation_end = (continuation_start + allowed).min(history.len());
            return history[continuation_start..continuation_end].to_vec();
        }
    }
    Vec::new()
}

/// How many leading positions `sampled` and `drafted` agree on. `sampled`
/// holds one real, freshly-sampled token per decoded position (always
/// computed, whether or not it ends up "accepted"); `drafted` holds what was
/// guessed for positions `1..`, one shorter than `sampled` since position 0
/// is the already-confirmed seed token with nothing in `drafted` to compare
/// it against. The accepted output is always `sampled[..=result]` — the
/// matched prefix plus one more real sample, which is the genuine next token
/// whether it means "the draft's next unverified guess" (full match) or
/// "the correction at the first place the draft was wrong" (partial match).
pub(crate) fn longest_match(sampled: &[i32], drafted: &[i32]) -> usize {
    sampled
        .iter()
        .zip(drafted)
        .take_while(|(a, b)| a == b)
        .count()
}

/// New `n_past` after discarding the speculative positions that weren't
/// confirmed. `batch_width` positions were decoded into the KV cache;
/// `accept_count` of them (the confirmed seed token plus every draft token
/// that matched — see `longest_match`) are valid and kept, the rest must be
/// rolled back with `LlmEngine::kv_remove`. Saturating throughout: a caller
/// passing a degenerate `accept_count > batch_width` gets "roll back
/// nothing" rather than a panic or a wrapped position.
fn rollback_from(n_past_after_batch: u32, batch_width: u32, accept_count: u32) -> u32 {
    n_past_after_batch.saturating_sub(batch_width.saturating_sub(accept_count))
}

#[cfg(test)]
mod speculative_tests {
    use super::*;

    fn lookup(k: u32, m: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m,
            source: DraftSource::Lookup,
            draft: None,
        })
    }

    fn mtp(k: u32) -> Option<Speculate> {
        Some(Speculate {
            k,
            m: 0,
            source: DraftSource::Mtp,
            draft: None,
        })
    }

    /// Um pedido de lookup sempre rascunha - a decisão de aceitar o desfazer
    /// não é mais estática (`kv_can_shift`), é o `seq_rm` de verdade em
    /// `handle_rollback`, checada em tempo real. Ver o doc de `should_draft`.
    #[test]
    fn a_lookup_round_always_drafts_when_asked() {
        assert!(should_draft(lookup(4, 3), false));
        assert!(should_draft(lookup(4, 3), true));
    }

    /// Pedir MTP a um motor sem a cabeça `nextn` e sem rascunho pronto é o
    /// caso normal de uma fatia que não é a final: recusar é a resposta
    /// certa, e o gateway lê a recusa (`speculated: false`) e para de pedir.
    /// Rascunhar por lookup no lugar seria trocar o mecanismo por baixo de
    /// quem pediu.
    #[test]
    fn mtp_asked_of_an_engine_without_the_nextn_head_is_refused() {
        assert!(!should_draft(mtp(4), false));
        assert!(should_draft(mtp(4), true));
    }

    /// Um rascunho MTP já pronto (a cauda de um pipeline fatiado produziu na
    /// rodada anterior) faz a cabeça aceitar mesmo sem a cabeça `nextn` local
    /// - ela só precisa verificar o lote, não gerá-lo.
    #[test]
    fn a_premade_mtp_draft_is_accepted_without_the_local_nextn_head() {
        let mut spec = mtp(4).unwrap();
        spec.draft = Some(vec![1, 2, 3]);
        assert!(should_draft(Some(spec), false));
    }

    /// Sem especulacao pedida, o resto nao importa.
    #[test]
    fn without_speculation_asked_there_is_no_draft() {
        assert!(!should_draft(None, true));
        assert!(!should_draft(None, false));
    }

    /// O campo `source` é o que separa os dois mecanismos no fio. Um quadro
    /// de gateway antigo não o traz, e "ausente" tem que continuar querendo
    /// dizer lookup: era a única especulação que existia quando ele foi
    /// escrito.
    #[test]
    fn a_frame_without_a_draft_source_still_means_lookup() {
        let spec =
            Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 3}})).expect("k e m bastam");
        assert_eq!(spec.source, DraftSource::Lookup);
        assert_eq!((spec.k, spec.m), (4, 3));
    }

    #[test]
    fn a_frame_asking_for_mtp_says_so() {
        let spec = Speculate::from_meta(&json!({"speculate": {"k": 4, "m": 0, "source": "mtp"}}))
            .expect("quadro completo");
        assert_eq!(spec.source, DraftSource::Mtp);
    }

    #[test]
    fn a_frame_without_speculate_asks_for_no_draft_at_all() {
        assert!(Speculate::from_meta(&json!({"session_id": "x"})).is_none());
    }

    #[test]
    fn find_draft_returns_nothing_without_a_match() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 3, 4), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_proposes_what_followed_the_match() {
        // Trailing 3 tokens [1,2,3] last occurred at index 1..4; [7,1]
        // followed it there.
        assert_eq!(find_draft(&[9, 1, 2, 3, 7, 1, 2, 3], 3, 3), vec![7, 1]);
    }

    #[test]
    fn find_draft_needs_more_history_than_just_the_needle() {
        assert_eq!(find_draft(&[1, 2, 3], 3, 5), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_k_of_one_never_drafts_anything() {
        assert_eq!(find_draft(&[1, 2, 3, 4, 5], 2, 1), Vec::<i32>::new());
    }

    #[test]
    fn find_draft_picks_the_most_recent_occurrence() {
        // [1,2,3] occurs at 0..3 and 4..7; the earlier one is followed by
        // 99, the later (more recent) one by 88. The more recent one must win.
        let history = [1, 2, 3, 99, 1, 2, 3, 88, 1, 2, 3];
        assert_eq!(find_draft(&history, 3, 2), vec![88]);
    }

    /// A janela é o **maior** n-grama tentado, não o único. Sem o recuo, um
    /// texto onde só o último token se repete não rascunha nada e a
    /// especulação fica ociosa justamente onde ela é barata (verificar K
    /// tokens num MoE custa quase o mesmo que verificar 1).
    #[test]
    fn find_draft_falls_back_to_a_shorter_ngram_when_the_window_does_not_match() {
        // Janela 3 = [7,8,9], que nunca ocorreu antes. O sufixo [9] ocorreu,
        // seguido de 42.
        let history = [1, 9, 42, 43, 7, 8, 9];
        assert_eq!(find_draft(&history, 3, 4), vec![42, 43]);
    }

    /// O n-grama mais longo continua tendo precedência: ele é a evidência mais
    /// forte, e um casamento curto que aponte para outro lugar não pode
    /// atropelá-lo.
    #[test]
    fn find_draft_prefers_the_longest_ngram_available() {
        //           0  1  2  3   4  5  6  7
        let history = [5, 6, 7, 90, 9, 70, 5, 6, 7];
        // Janela 3 ([5,6,7]) casa em 0..3, seguido de 90. O sufixo [7] casa
        // mais tarde (índice 2) — mas o de 3 vence.
        assert_eq!(find_draft(&history, 3, 2), vec![90]);
    }

    /// Um casamento de um token só é evidência fraca; ele rascunha menos que um
    /// casamento longo. Rejeição não corrompe nada (a verificação é sempre
    /// contra o token amostrado de verdade), mas cada token rascunhado a mais
    /// alarga o batch de todo hop do anel.
    #[test]
    fn find_draft_trusts_a_short_match_for_fewer_tokens() {
        let history = [4, 1, 2, 3, 9, 9, 9, 4];
        // Sufixo [4] casa no índice 0; k=8 pediria 7 tokens, mas um casamento
        // de comprimento 1 só autoriza 2.
        assert_eq!(find_draft(&history, 1, 8), vec![1, 2]);
    }

    #[test]
    fn longest_match_stops_at_the_first_disagreement() {
        assert_eq!(longest_match(&[1, 2, 9, 4], &[1, 2, 3]), 2);
    }

    #[test]
    fn longest_match_of_a_full_match_is_the_drafts_whole_length() {
        assert_eq!(longest_match(&[1, 2, 3, 4], &[1, 2, 3]), 3);
    }

    #[test]
    fn longest_match_against_an_empty_draft_is_zero() {
        assert_eq!(longest_match(&[1, 2, 3], &[]), 0);
    }

    #[test]
    fn rollback_from_a_full_match_is_a_no_op() {
        assert_eq!(rollback_from(100, 4, 4), 100);
    }

    #[test]
    fn rollback_from_a_partial_match_removes_the_gap() {
        // n_past was 96 before the batch, 4 positions were decoded, only 2
        // (the seed + one draft token) were confirmed — 2 must come off.
        assert_eq!(rollback_from(100, 4, 2), 98);
    }

    #[test]
    fn rollback_from_saturates_instead_of_underflowing() {
        assert_eq!(rollback_from(100, 4, 0), 96);
        // Degenerate: accept_count larger than batch_width still can't
        // remove more than the whole batch.
        assert_eq!(rollback_from(100, 4, 9), 100);
    }
}

#[allow(clippy::too_many_arguments)]
fn step_blocking(
    state: SharedState,
    sid_key: String,
    is_last: bool,
    wire_dtype: WireDtype,
    token_id: Option<i32>,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    // Present only for the head — everyone else either forwards
    // `incoming_draft` untouched (not the tail) or verifies against it (the
    // tail). Ver `Speculate` para por que a fonte do rascunho viaja junto.
    speculate: Option<Speculate>,
    // What a previous hop already decided, forwarded via the wire `draft`
    // field. `None` means this round isn't speculative at all (the
    // pre-existing, single-token wire format applies); `Some(vec![])` means
    // it is speculative but nothing matched — still a batch of one, but
    // through the *new* wire format, since the caller committed to it by
    // asking to speculate in the first place.
    incoming_draft: Option<Vec<i32>>,
    // A composição do lote decodificado na cabeça, viajando pela wire (`tokens`)
    // para a cauda. É o que alimenta o `observe` da cabeça `nextn` dentro do
    // `decode_hidden`: a cauda sabe *quais* tokens o tronco decodificou, e sem
    // eles a cabeça de rascunho não se mantém em dia. `None` é peer antigo que
    // não encaminha o campo — aí o `decode_hidden` desliga o MTP com aviso.
    batch_tokens: Option<Vec<i32>>,
    // Largura de lote que o gateway quer na rodada seguinte, vinda da cabeça
    // via `mtp_k` na wire. Só a cauda usa (para rascunhar a medida certa); a
    // cabeça a ecoa a partir do próprio `speculate` (`mtp_k_hint`).
    mtp_k: Option<u32>,
    queued_at: Instant,
) -> Result<TimedReply> {
    let mut timings = NodeTimings {
        queue_us: queued_at.elapsed().as_micros() as u64,
        ..Default::default()
    };
    let lock_started = Instant::now();
    let mut s = state.lock().unwrap();
    timings.lock_wait_us = lock_started.elapsed().as_micros() as u64;

    let setup_started = Instant::now();
    let session = s.sessions.get_mut(&sid_key).ok_or_else(|| {
        anyhow::anyhow!("sessão '{}' desconhecida — faça o prefill antes", sid_key)
    })?;
    session.last_used = Instant::now();
    let n_past = session.n_past;
    let seq_id = session.seq_id;
    timings.setup_us += setup_started.elapsed().as_micros() as u64;

    let n_tokens;
    let decode_started = Instant::now();
    let engine_timing;
    let mut head_draft: Option<Vec<i32>> = None;
    // A composição do lote que este hop decodificou, para o reply não-cauda.
    // Inicializada nos dois branches (tensor e cabeça) antes de qualquer
    // leitura, por isso sem valor inicial — ver o uso no reply_meta.
    let reply_tokens;
    // Ecoado no reply não-cauda: na cabeça nasce do `speculate` (mtp_k_hint),
    // nos hops do meio repassa o que chegou da wire. É como a cauda descobre a
    // largura de lote que o gateway quer na rodada seguinte.
    let mut mtp_k_echo: Option<u32> = None;
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state. `decode_hidden` already
        // handles any `n_tokens` — a speculative batch needs no special
        // handling here beyond what chunked prefill already exercises. A cauda
        // de pipeline fatiado tem o bloco `nextn`: o `decode_hidden` alimenta
        // o `observe` com a composição do lote (`batch_tokens`, vinda da
        // cabeça via `tokens` na wire); sem ela, ele desliga o MTP com aviso.
        n_tokens = shape[0];
        let logits_all = incoming_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_hidden(
            &hidden,
            n_tokens,
            n_past,
            seq_id,
            logits_all,
            batch_tokens.as_deref(),
        )?;
        // `batch_tokens` segue vivo: a cauda usa-o no gate do `next_draft`
        // (verificação) e no caminho plain.
        reply_tokens = batch_tokens.clone();
        // `Option<u32>` é Copy — o `mtp_k` do parâmetro continua vivo para o
        // caminho plain da cauda.
        mtp_k_echo = mtp_k;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
        // Only the tail pushed to `recent_tokens` before this fix, so on a
        // real (head != tail) multi-hop pipeline the head's own history was
        // permanently empty — harmless until something needed it (repetition
        // penalty already reads only the tail's copy, correctly), but
        // prompt-lookup drafting below needs the head's history to search.
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(tid);

        // A largura de lote que o gateway quer na rodada seguinte, extraída do
        // `speculate` que a cabeça recebeu e ecoada na resposta — é como a
        // cauda descobre a medida do rascunho (ver `mtp_k` no doc do parâmetro).
        let mtp_k_hint = speculate
            .as_ref()
            .filter(|s| s.source == DraftSource::Mtp)
            .map(|s| s.k);
        let pode_rascunhar = should_draft(speculate.clone(), s.engine.mtp_ready());
        let batch: Vec<i32> = if let Some(spec) = speculate.filter(|_| pode_rascunhar) {
            let draft: Vec<i32> = if let Some(premade) = spec.draft {
                premade
            } else {
                match spec.source {
                    DraftSource::Mtp => {
                        let p_min = s.mtp_min_probability;
                        s.engine
                            .mtp_draft(tid, n_past, seq_id, spec.k.saturating_sub(1), p_min)
                    }
                    DraftSource::Lookup => {
                        let session = &s.sessions[&sid_key];
                        let history: Vec<i32> = session
                            .prompt_tokens
                            .iter()
                            .chain(session.recent_tokens.iter())
                            .copied()
                            .collect();
                        find_draft(&history, spec.m as usize, spec.k)
                    }
                }
            };
            let mut batch = Vec::with_capacity(1 + draft.len());
            batch.push(tid);
            batch.extend_from_slice(&draft);
            head_draft = Some(draft);
            batch
        } else {
            vec![tid]
        };
        n_tokens = batch.len();
        let logits_all = head_draft.is_some() || !is_last;
        engine_timing = s.engine.decode_tokens(&batch, n_past, seq_id, logits_all)?;
        // A composição do lote decodificado na cabeça — o que a cauda precisa
        // para o `observe`. Viaja no reply não-cauda e é ecoada até ela.
        reply_tokens = Some(batch);
        // Ecoa a largura de lote pedida para a cauda rascunhar. `mtp_k` do
        // parâmetro fica vivo para o caminho plain da cauda (o `or` usa uma
        // cópia).
        if let Some(k) = mtp_k_hint.or(mtp_k) {
            mtp_k_echo = Some(k);
        }
    }
    timings.decode_us += decode_started.elapsed().as_micros() as u64;
    timings.backend_enqueue_us += engine_timing.enqueue_us;
    timings.backend_completion_us += engine_timing.backend_completion_us;
    timings.residual_capture_d2h_sync_us += engine_timing.residual_capture_d2h_sync_us;

    s.sessions.get_mut(&sid_key).unwrap().n_past += n_tokens as u32;

    // Whichever hop decided it — computed just now (the head) or forwarded
    // from one that did (everyone else) — this is what the tail verifies
    // against, and what a non-tail relay branch echoes onward.
    let draft = head_draft.or(incoming_draft);

    let sampling = s.sampling.clone();
    let eos = s.engine.eos_token();

    if is_last {
        if let Some(draft) = draft {
            // ── Speculative verification ─────────────────────────────────
            //
            // Never trusts the draft: every position is sampled for real,
            // in order, through the exact same `sample_token_profiled` a
            // plain step uses — the draft only ever decided what got
            // batched, never what got output. Accepting positions this way
            // makes the result identical in distribution to running the
            // single-token path `keep` times in a row (see the "prompt
            // lookup decoding" writeup this shipped with for the full
            // argument); it is not an approximation.
            let verification_started = Instant::now();
            let mut recent = s.sessions[&sid_key].recent_tokens.clone();
            let mut sampled: Vec<i32> = Vec::with_capacity(n_tokens);
            let mut texts: Vec<String> = Vec::with_capacity(n_tokens);
            for i in 0..n_tokens {
                let logits = s.engine.logits_ith(i as i32);
                let (id, _timing) = sample_token_profiled(logits, &recent, &sampling);
                recent.push(id);
                texts.push(s.engine.token_to_piece(id, false));
                let is_eos = id == eos;
                sampled.push(id);
                if is_eos {
                    // Nothing past EOS is worth sampling — generation is over
                    // regardless of what the rest of the draft guessed.
                    break;
                }
            }
            timings.sampling_us += verification_started.elapsed().as_micros() as u64;

            let accept_len = longest_match(&sampled, &draft);
            // A cabeça `nextn` guarda uma linha de estado oculto por posição
            // decodificada; qual delas alimenta a próxima rodada só se sabe
            // depois de a verificação dizer até onde o rascunho valeu. Sem
            // isto ela pareia o token confirmado com o `h` de uma posição que
            // foi descartada, e o rascunho seguinte degenera em silêncio.
            s.engine.mtp_accept(accept_len);

            let keep = (accept_len + 1).min(sampled.len());
            let accepted_ids = &sampled[..keep];
            let accepted_texts = &texts[..keep];
            let done = accepted_ids.last().copied() == Some(eos);

            // Rascunho para a rodada seguinte. Numa máquina só ele alimenta
            // o mesmo processo; num pipeline fatiado ele viaja na resposta
            // direta de volta ao gateway (`relay/v2-direct`), que o injeta
            // no `speculate.draft` da próxima rodada — zero travessia extra.
            //
            // O `batch_tokens` presente é o que garante que esta é a cauda de
            // um pipeline (a composição do lote veio de fora, via `tokens`):
            // numa máquina só a cabeça é a própria cauda e não há para onde
            // devolver o rascunho.
            let next_draft: Option<Vec<i32>> =
                if s.engine.mtp_ready() && !done && batch_tokens.is_some() {
                    // A sessão já avançou `n_tokens`; só as primeiras `keep`
                    // posições foram confirmadas, então o próximo slot livre do
                    // tronco é `n_past - (n_tokens - keep)`.
                    let session_n_past = s.sessions[&sid_key].n_past;
                    let n_past_next = session_n_past - (n_tokens as u32 - keep as u32);
                    let id0 = *accepted_ids.last().unwrap_or(&eos);
                    let p_min = s.mtp_min_probability;
                    let dr =
                        s.engine
                            .mtp_draft(id0, n_past_next, seq_id, draft.len() as u32, p_min);
                    if dr.is_empty() {
                        None
                    } else {
                        Some(dr)
                    }
                } else {
                    None
                };

            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .recent_tokens
                .extend_from_slice(accepted_ids);
            // Ver a nota gêmea abaixo: réplica não encerra sessão.
            if done && !crate::tp::is_replica() {
                s.finish(&sid_key);
            }

            let accepted: Vec<Value> = accepted_ids
                .iter()
                .zip(accepted_texts)
                .map(|(id, text)| json!({"token_id": id, "text": text}))
                .collect();
            let encode_started = Instant::now();
            let bytes = wire::encode(
                &json!({
                    "done": done,
                    "accepted": accepted,
                    "accept_count": keep as u32,
                    "batch_width": n_tokens as u32,
                    "next_draft": next_draft,
                }),
                None,
                wire_dtype,
            )?;
            timings.encode_us += encode_started.elapsed().as_micros() as u64;
            return Ok(TimedReply { bytes, timings });
        }

        let logits_access_started = Instant::now();
        let recent = s.sessions[&sid_key].recent_tokens.clone();
        let logits = s.engine.logits_ith(n_tokens as i32 - 1);
        let logits_access_us = logits_access_started.elapsed().as_micros() as u64;
        let (next_id, sampling_timing) = sample_token_profiled(logits, &recent, &sampling);
        let token_text_started = Instant::now();
        let text = s.engine.token_to_piece(next_id, false);
        let token_text_us = token_text_started.elapsed().as_micros() as u64;
        let done = next_id == eos;
        s.sessions
            .get_mut(&sid_key)
            .unwrap()
            .recent_tokens
            .push(next_id);
        // A réplica de um grupo TP não encerra sessão: quem decide o fim da
        // geração é o rank 0. Ver `tp::is_replica`.
        if done && !crate::tp::is_replica() {
            s.finish(&sid_key);
        }
        timings.sampling_us += logits_access_us
            + sampling_timing.logits_clone_us
            + sampling_timing.repetition_penalty_us
            + sampling_timing.softmax_us
            + sampling_timing.top_k_us
            + sampling_timing.top_p_us
            + sampling_timing.selection_us
            + token_text_us;
        timings.logits_access_us += logits_access_us;
        timings.logits_clone_us += sampling_timing.logits_clone_us;
        timings.repetition_penalty_us += sampling_timing.repetition_penalty_us;
        timings.softmax_us += sampling_timing.softmax_us;
        timings.top_k_us += sampling_timing.top_k_us;
        timings.top_p_us += sampling_timing.top_p_us;
        timings.selection_us += sampling_timing.selection_us;
        timings.token_text_us += token_text_us;

        // A cauda de um pipeline fatiado também rascunha na rodada em que a
        // cabeça recusou — a primeira, quando ainda não há rascunho pronto
        // para viajar. O `mtp_k` veio da cabeça (via `mtp_k` na wire): é a
        // largura de lote que o gateway quer na rodada seguinte. O
        // `mtp_accept(0)` confirma o `pending_h` da linha do token recém
        // decodificado; sem ele o draft seguinte usaria a linha de uma posição
        // anterior e parearia errado. Rodada de modelo inteiro recusada não
        // tem `mtp_k` — nada a devolver, e não há segunda máquina esperando.
        let next_draft: Option<Vec<i32>> = if s.engine.mtp_ready() && !done {
            if let Some(k) = mtp_k {
                s.engine.mtp_accept(0);
                let session_n_past = s.sessions[&sid_key].n_past;
                let p_min = s.mtp_min_probability;
                let dr =
                    s.engine
                        .mtp_draft(next_id, session_n_past, seq_id, k.saturating_sub(1), p_min);
                if dr.is_empty() {
                    None
                } else {
                    Some(dr)
                }
            } else {
                None
            }
        } else {
            None
        };
        let encode_started = Instant::now();
        let mut reply = json!({"done": done, "token_id": next_id, "text": text});
        if let Some(nd) = &next_draft {
            reply["next_draft"] = json!(nd);
        }
        let bytes = wire::encode(&reply, None, wire_dtype)?;
        timings.encode_us += encode_started.elapsed().as_micros() as u64;
        return Ok(TimedReply { bytes, timings });
    }

    let residual_started = Instant::now();
    let residual = s.engine.take_residual(n_tokens)?;
    timings.residual_us += residual_started.elapsed().as_micros() as u64;
    let mut reply_meta = json!({"done": false});
    // Whoever called us needs this to keep threading it toward the tail —
    // see `draft`'s own comment above for why it's carried this way rather
    // than recomputed at each hop.
    if let Some(draft) = &draft {
        reply_meta["draft"] = json!(draft);
    }
    // A composição do lote decodificado (ver `batch_tokens`) e a largura de
    // lote da próxima rodada (ver `mtp_k`) viajam da cabeça para a cauda —
    // são o que mantém a cabeça `nextn` da cauda em dia e rascunhando a
    // medida certa.
    if let Some(tokens) = &reply_tokens {
        reply_meta["tokens"] = json!(tokens);
    }
    if let Some(k) = mtp_k_echo {
        reply_meta["mtp_k"] = json!(k);
    }
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual, [n_tokens, s.engine.n_embd])),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}
