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
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::net::TcpListener;
use tracing::{info, warn};

const SESSION_IDLE_TIMEOUT_SECS: u64 = 900;
/// Upper bound for one binary f16 hidden-state frame received from a peer.
///
/// A normal 512-token chunk with a 4096-wide hidden state is 4 MiB before its
/// small wire header. Keeping this limit explicit at 16 MiB allows the
/// documented 8 MiB frame while still rejecting unbounded peer input.
const INFERENCE_FRAME_BODY_LIMIT: usize = 16 * 1024 * 1024;

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
        })
    }
}

struct TimedReply {
    bytes: Vec<u8>,
    timings: NodeTimings,
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
            model_path: "/nao/existe/modelo.gguf".into(),
            model_label: "qwen35".into(),
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
        };
        cfg.inference_port = port;
        let (_tx, rx) = tokio::sync::watch::channel(0u64);

        let comecou = std::time::Instant::now();
        let erro = run(cfg, rx, 0).await.expect_err("porta ocupada devia falhar");

        assert!(
            erro.to_string().to_lowercase().contains("address")
                || erro.to_string().contains("use"),
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
        }
    }

    async fn spawn_fake_hop(handler: impl Fn() -> Response + Clone + Send + Sync + 'static) -> String {
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
        let http = reqwest::Client::new();

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
        assert_eq!(hop_timings[0]["processing_ms"], 5, "the tail's self-reported processing_ms");
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
        let http = reqwest::Client::new();

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
    max_new_tokens: u32,
    n_batch: u32,
    sampling: SamplingConfig,
    no_thinking: bool,
    /// Whether to apply the model's chat template before tokenising.
    chat_template: bool,
    start_layer: u32,
    end_layer: u32,
    original_block_count: Option<u32>,
}

type SharedState = Arc<Mutex<InferState>>;

impl InferState {
    fn new(engine: LlmEngine, cfg: &NodeConfig) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        Self {
            engine,
            sessions: HashMap::new(),
            seq_pool,
            max_sessions,
            max_new_tokens: cfg.max_new_tokens,
            n_batch: cfg.n_batch,
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
    let engine_cfg = EngineConfig {
        n_ctx: cfg.n_ctx,
        offload_kqv: cfg.offload_kqv,
        n_batch: cfg.n_batch,
        n_gpu_layers: cfg.n_gpu_layers as i32,
        n_threads: 0,
        max_sessions: cfg.max_sessions.max(1),
        need_embeddings,
    };

    let model_path = cfg.model_path.clone();
    let engine = tokio::task::spawn_blocking(move || LlmEngine::load(&model_path, &engine_cfg))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    let mut state_inner = InferState::new(engine, &cfg);
    state_inner.original_block_count =
        crate::gguf::read_info(std::path::Path::new(&cfg.model_path))
            .ok()
            .and_then(|info| info.original_block_count);

    let state: SharedState = Arc::new(Mutex::new(state_inner));

    let router = Router::new()
        .route(
            "/v1/prefill",
            post(handle_prefill).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step",
            post(handle_step).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route("/v1/session/release", post(handle_release))
        .route("/v1/session/rollback", post(handle_rollback))
        .route("/health", get(handle_health))
        .with_state(state);

    info!("inference server pronto em [::]:{port}");

    // Ver a nota gêmea em `gateway::start_gateway`: sem isto, toda resposta de
    // hop sai com Nagle ligado, e um ack de resposta direta é exatamente a
    // escrita pequena que ele segura.
    axum::serve(listener, router)
        .tcp_nodelay(true)
        .with_graceful_shutdown(async move {
            loop {
                if stop_rx.changed().await.is_err() {
                    break;
                }
                if *stop_rx.borrow() != generation {
                    break;
                }
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("axum: {e}"))
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

/// One shared outbound client, used only when relaying to this node's own
/// next hop (`PipelineStep::supports(CAP_RELAY_CHAIN)`) — this server has no
/// other reason to make outbound HTTP calls. Same lazy-static pattern as
/// `webui::catalog_cache`.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn err400(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn err500(msg: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

// ── /health ───────────────────────────────────────────────────────────────────

async fn handle_health(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.lock().unwrap();
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
        // Keep in sync with network.rs's `self_announce`, gateway.rs's local
        // peer entry, and the capability constants in routing/pipeline.rs.
        "inference_frame_capabilities": ["frame/v1-f16", "frame/v2-zstd-f16", "relay/v1-chain", "relay/v2-direct", "speculate/v1-lookup"],
        // Falso em modelo híbrido/recorrente. É a única forma de saber, sem
        // shell na máquina, por que a especulação não está acelerando nada.
        "kv_can_shift": s.engine.kv_can_shift(),
    }))
}

// ── /v1/session/release ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReleaseRequest {
    session_id: String,
}

async fn handle_release(
    State(state): State<SharedState>,
    Json(req): Json<ReleaseRequest>,
) -> impl IntoResponse {
    let mut s = state.lock().unwrap();
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
    State(state): State<SharedState>,
    Json(req): Json<RollbackRequest>,
) -> impl IntoResponse {
    let mut s = state.lock().unwrap();
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

async fn handle_prefill(State(state): State<SharedState>, body: Bytes) -> Response {
    let processing_started = Instant::now();
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__legacy__")
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
        .unwrap_or_else(|| meta.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false));
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
    let result = tokio::task::spawn_blocking(move || {
        prefill_blocking(
            state,
            sid_key,
            chunk_index,
            chunk_size,
            is_last,
            is_final_chunk,
            wire_dtype,
            prompt,
            tensor,
            queued_at,
        )
    })
    .await;

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

        let forward = json!({
            "session_id": sid_for_forward,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_final_chunk": is_final_chunk_for_forward(&local_meta, is_final_chunk),
            "prompt_progress": prompt_progress_for_forward(&meta, &local_meta),
            "hop_timings": hop_timings,
        });
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
        return match wire::encode(
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
        json!({
            "session_id": sid_for_forward,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "is_final_chunk": is_final_chunk_for_forward,
        }),
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

    match wire::encode(&bubbled, None, WireDtype::F16) {
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
fn overlay_head_authoritative_fields(local_meta: &Value, bubbled: &mut Value) {
    if owns_tokens(local_meta) {
        bubbled["more_chunks"] = local_meta["more_chunks"].clone();
        bubbled["n_prompt_tokens"] = local_meta["n_prompt_tokens"].clone();
    }
}

/// Os mesmos dois campos autoritativos, empurrados **adiante** em vez de
/// sobrepostos na volta — é o que a resposta direta precisa, já que a cabeça
/// nunca mais vê o resultado. Quem não é a cabeça repassa intacto o que
/// recebeu.
fn prompt_progress_for_forward(inbound_meta: &Value, local_meta: &Value) -> Value {
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
fn apply_prompt_progress(inbound_meta: &Value, result: &mut Value) {
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
        engine_timing = s
            .engine
            .decode_hidden(&hidden, n_tokens, n_past, seq_id, !is_last)?;
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
        if done {
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
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &json!({
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
        }),
        Some((&residual, [n_tokens, n_embd])),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}

// ── /v1/step ─────────────────────────────────────────────────────────────────

async fn handle_step(State(state): State<SharedState>, body: Bytes) -> Response {
    let processing_started = Instant::now();
    let inbound_bytes = body.len();
    let frame_decode_started = Instant::now();
    let (meta, tensor) = match wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return err400(format!("quadro inválido: {e}")),
    };
    let frame_decode_us = frame_decode_started.elapsed().as_micros() as u64;

    let sid_key = meta
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("__legacy__")
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
        .unwrap_or_else(|| meta.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false));
    let wire_dtype = WireDtype::F16;
    let scale = meta.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
    let token_id = meta
        .get("token_id")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    // Present only on the request to the head — see `step_blocking`'s
    // `speculate` param doc.
    let speculate: Option<(u32, u32)> = meta.get("speculate").and_then(|v| {
        Some((
            v.get("k")?.as_u64()? as u32,
            v.get("m")?.as_u64()? as u32,
        ))
    });
    // Present on every non-head request in a speculative round — see
    // `step_blocking`'s `incoming_draft` param doc for why `None` and
    // `Some(vec![])` mean different things here.
    let incoming_draft: Option<Vec<i32>> = meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

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
    let result = tokio::task::spawn_blocking(move || {
        step_blocking(
            state,
            sid_key,
            is_last,
            wire_dtype,
            token_id,
            tensor,
            speculate,
            incoming_draft,
            queued_at,
        )
    })
    .await;

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
        if let Some(draft) = local_meta.get("draft") {
            forward["draft"] = draft.clone();
        }
        tokio::spawn(forward_direct(
            hops,
            "/v1/step",
            forward,
            residual,
            shape,
            1, // a decode step has never retried — see relay_and_bubble's doc comment
            reply_to,
        ));
        return direct_ack();
    }

    let should_relay = next_hops.as_ref().is_some_and(|h| !h.is_empty()) && local_tensor.is_some();
    if !should_relay {
        return match wire::encode(
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
    // Keep threading the draft toward the tail — absent entirely (not just
    // an empty array) when this round isn't speculative, same distinction
    // `incoming_draft` relies on above.
    if let Some(draft) = local_meta.get("draft") {
        forward_meta["draft"] = draft.clone();
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
    match wire::encode(&bubbled, None, WireDtype::F16) {
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
    http: &reqwest::Client,
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
    let sent = crate::relay::send_frame(
        http,
        &next.inference_url,
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
fn direct_reply_to(meta: &Value) -> Option<Value> {
    meta.get("reply_to").filter(|v| !v.is_null()).cloned()
}

/// O que este hop devolve a quem o chamou quando a cadeia responde direto: um
/// aceite, e nada mais. A resposta de verdade não passa por aqui — ver
/// [`crate::direct`].
fn direct_ack() -> Response {
    match wire::encode(&json!({"accepted": true}), None, WireDtype::F16) {
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
async fn forward_direct(
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
    let sent = crate::relay::send_frame(
        http_client(),
        &next.inference_url,
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
/// Especular é apostar: o rascunho recusado tem de ser desfeito do cache. Num
/// modelo híbrido de atenção linear o llama.cpp não sabe desfazer (o estado
/// recorrente é acumulado, não indexado por posição), e insistir não dá erro na
/// hora - dá divergência entre o `n_past` do nó e o cache, que só explode na
/// rodada seguinte com "inconsistent sequence positions".
///
/// Então a decisão é tomada antes de apostar: sem rollback parcial, o nó
/// simplesmente não rascunha e a geração segue um token por rodada, correta e
/// mais lenta - em vez de morrer no quarto token.
fn should_draft(speculate: Option<(u32, u32)>, kv_can_shift: bool) -> bool {
    speculate.is_some() && kv_can_shift
}

fn find_draft(history: &[i32], m: usize, k: u32) -> Vec<i32> {
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
fn longest_match(sampled: &[i32], drafted: &[i32]) -> usize {
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

    /// Modelo hibrido nao rascunha. O rascunho recusado precisaria ser
    /// desfeito, e num modelo recorrente o llama.cpp nao sabe desfazer.
    #[test]
    fn a_model_that_cannot_shift_its_cache_never_drafts() {
        assert!(!should_draft(Some((4, 3)), false));
    }

    #[test]
    fn a_model_that_can_shift_drafts_when_asked() {
        assert!(should_draft(Some((4, 3)), true));
    }

    /// Sem especulacao pedida, o resto nao importa.
    #[test]
    fn without_speculation_asked_there_is_no_draft() {
        assert!(!should_draft(None, true));
        assert!(!should_draft(None, false));
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
    // (k, m): draft width and lookup window, present only for the head —
    // everyone else either forwards `incoming_draft` untouched (not the
    // tail) or verifies against it (the tail).
    speculate: Option<(u32, u32)>,
    // What a previous hop already decided, forwarded via the wire `draft`
    // field. `None` means this round isn't speculative at all (the
    // pre-existing, single-token wire format applies); `Some(vec![])` means
    // it is speculative but nothing matched — still a batch of one, but
    // through the *new* wire format, since the caller committed to it by
    // asking to speculate in the first place.
    incoming_draft: Option<Vec<i32>>,
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
    if let Some((hidden, shape)) = tensor {
        // Middle/last peer receiving a hidden state. `decode_hidden` already
        // handles any `n_tokens` — a speculative batch needs no special
        // handling here beyond what chunked prefill already exercises.
        n_tokens = shape[0];
        let logits_all = incoming_draft.is_some() || !is_last;
        engine_timing = s
            .engine
            .decode_hidden(&hidden, n_tokens, n_past, seq_id, logits_all)?;
    } else {
        // First peer: receives the token ID sampled by the previous last peer.
        let tid = token_id.ok_or_else(|| anyhow::anyhow!("primeiro peer precisa de token_id"))?;
        // Only the tail pushed to `recent_tokens` before this fix, so on a
        // real (head != tail) multi-hop pipeline the head's own history was
        // permanently empty — harmless until something needed it (repetition
        // penalty already reads only the tail's copy, correctly), but
        // prompt-lookup drafting below needs the head's history to search.
        s.sessions.get_mut(&sid_key).unwrap().recent_tokens.push(tid);

        let pode_rascunhar = should_draft(speculate, s.engine.kv_can_shift());
        let batch: Vec<i32> = if let Some((k, m)) = speculate.filter(|_| pode_rascunhar) {
            let draft = {
                let session = &s.sessions[&sid_key];
                let history: Vec<i32> = session
                    .prompt_tokens
                    .iter()
                    .chain(session.recent_tokens.iter())
                    .copied()
                    .collect();
                find_draft(&history, m as usize, k)
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
            let keep = (accept_len + 1).min(sampled.len());
            let accepted_ids = &sampled[..keep];
            let accepted_texts = &texts[..keep];
            let done = accepted_ids.last().copied() == Some(eos);

            s.sessions
                .get_mut(&sid_key)
                .unwrap()
                .recent_tokens
                .extend_from_slice(accepted_ids);
            if done {
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
        if done {
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
            &json!({"done": done, "token_id": next_id, "text": text}),
            None,
            wire_dtype,
        )?;
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
    let encode_started = Instant::now();
    let bytes = wire::encode(
        &reply_meta,
        Some((&residual, [n_tokens, s.engine.n_embd])),
        wire_dtype,
    )?;
    timings.encode_us += encode_started.elapsed().as_micros() as u64;
    Ok(TimedReply { bytes, timings })
}
