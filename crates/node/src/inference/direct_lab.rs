//! Bancada da lei: **o custo por token não pode depender de onde está a
//! cabeça da pipeline**.
//!
//! Sobe hops falsos que falam o protocolo real ([`crate::wire`],
//! [`crate::relay::send_frame`], o encadeamento `next_hops`) e são dirigidos
//! pelo código de orquestração real ([`crate::prefill::run`] e
//! [`crate::prefill::step`]). O repasse e a entrega da resposta direta são as
//! funções de produção em pessoa
//! ([`crate::infer_server::forward_direct`], [`crate::direct::deliver_result`]),
//! e o gateway falso serve o handler de produção de `/v1/relay-result`. O
//! único fingimento é o motor: cada hop dorme em vez de decodificar.
//!
//! A rede é emulada por *sítio*: cada hop e o gateway moram num sítio, e toda
//! mensagem entre sítios diferentes custa [`LINK_MS`] de latência **de mão
//! única** — cobrada na chegada e de novo na volta, porque as duas direções
//! atravessam o enlace. É isso que faz a bancada medir travessias em vez de
//! medir a máquina onde ela roda: com o cálculo em zero, o tempo por token é o
//! número de pernas de rede vezes `LINK_MS`.
//!
//! Herda da investigação de 2026-08-14 (`labsse.rs`), que provou a assimetria
//! e refutou a hipótese do SSE por contrafactual. Aqui a bancada deixou de ser
//! experimento solto e passou a guardar a correção.

#![cfg(test)]

use crate::pipeline::{PipelinePlan, PipelineStep};
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Igual ao Qwen3.8-27B do cluster do captain.
const HIDDEN_DIM: usize = 5120;
/// Latência de mão única entre dois sítios.
const LINK_MS: u64 = 50;
/// Tokens de prompt no fake. Maior que `CHUNK` de propósito: um prompt de mais
/// de um pedaço é o que denuncia o `prompt_progress` mal repassado.
const PROMPT_TOKENS: u32 = 3;
const CHUNK: u32 = 2;
/// Tokens de decode medidos por cenário.
const DECODE_TOKENS: u32 = 6;

// ── O hop falso ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Hop {
    /// Onde este hop mora.
    site: usize,
    /// Onde mora quem o chama. A cadeia é fixa, então isto é conhecido ao
    /// montar o cenário: quem chama o hop `i` é o hop `i-1`, e o hop 0 é
    /// chamado pelo gateway.
    caller_site: usize,
    /// Este hop tokenizou o prompt (é a cabeça).
    is_head: bool,
    /// Posições recebidas pela cabeça. A bancada recusa a ausência de posição
    /// absoluta nos chunks seguintes, como `expected_prefill_pos` faz em
    /// produção, e o teste compara os valores com a progressão publicada.
    received_starts: Arc<Mutex<Vec<(u32, Option<u32>, u32)>>>,
    auth: Option<AuthProbe>,
}

/// Observa a fronteira de autenticação sem tocar na rede real. Cada hop deve
/// receber somente a `connection_key`; a regressão fazia o primeiro hop usar a
/// chave da sessão e deixava o hop seguinte sem credencial.
#[derive(Clone)]
struct AuthProbe {
    accepted_tokens: Vec<String>,
    missing: Arc<AtomicU64>,
    invalid: Arc<AtomicU64>,
}

fn auth_failure(headers: &HeaderMap, probe: Option<&AuthProbe>) -> Option<Response> {
    let probe = probe?;
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|value| value.to_str().ok()));
    let internal = headers
        .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
        .and_then(|value| value.to_str().ok())
        == Some("1");
    match token {
        None => {
            probe.missing.fetch_add(1, Ordering::Relaxed);
            Some((StatusCode::UNAUTHORIZED, "missing inference credential").into_response())
        }
        Some(token)
            if internal && probe.accepted_tokens.iter().any(|accepted| accepted == token) =>
        {
            None
        }
        Some(_) => {
            probe.invalid.fetch_add(1, Ordering::Relaxed);
            Some((StatusCode::FORBIDDEN, "invalid inference API key").into_response())
        }
    }
}

impl Hop {
    /// Chegada de uma mensagem: paga o enlace se ela veio de outro sítio.
    async fn arrive(&self) {
        if self.site != self.caller_site {
            tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
        }
    }

    /// Volta da resposta pelo mesmo enlace. No anel isto está no caminho
    /// crítico (a resposta é o resultado); no modo direto é só o aceite, e sai
    /// do caminho sozinho — ninguém o espera.
    async fn depart(&self) {
        if self.site != self.caller_site {
            tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
        }
    }
}

fn frame(meta: &Value, tensor: Option<(&[f32], TensorShape)>) -> Response {
    match wire::encode(meta, tensor, WireDtype::F16) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn fake_health() -> impl IntoResponse {
    Json(json!({"status": "ok", "model_loaded": true, "hidden_dim": HIDDEN_DIM}))
}

async fn fake_release() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

/// O que todo handler precisa saber do quadro que chegou, lido com as mesmas
/// regras do `infer_server` real.
struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    next_hops: Vec<PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
}

fn read(body: &Bytes) -> Incoming {
    let (meta, _tensor) = wire::decode(body).expect("quadro válido");
    let declared: Option<Vec<PipelineStep>> = meta
        .get("next_hops")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let is_last = declared.as_ref().map(|h| h.is_empty()).unwrap_or_else(|| {
        meta.get("is_last")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });
    let reply_to = crate::infer_server::direct_reply_to(&meta);
    Incoming {
        inbound_bytes: body.len(),
        next_hops: declared.unwrap_or_default(),
        is_last,
        reply_to,
        meta,
    }
}

/// Espelha `infer_server::relay_and_bubble` — o caminho antigo, que serve de
/// linha de base contra a qual a correção é medida.
async fn bubble(
    hops: &[PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], TensorShape),
    local_processing_ms: u64,
) -> Result<Value, Response> {
    let next = &hops[0];
    let remaining = &hops[1..];
    base_meta["is_last"] = json!(remaining.is_empty());
    base_meta["scale"] = json!(1.0);
    base_meta["next_hops"] = json!(remaining);

    let sent = crate::relay::send_frame(
        &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
        &next.node_id,
        &next.inference_url,
        path,
        &base_meta,
        Some(tensor),
        WireDtype::F16,
        next.frame_encoding(),
        1,
        &next.node_id,
        |_| {},
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(local_processing_ms);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.arrive().await;
    let inc = read(&body);
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(CHUNK.into()) as u32;
    let is_final_chunk = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);

    // Só o chunk 0 pode omitir `start_pos`: a sessão é a referência nesse
    // caso. Todo chunk posterior precisa carregar a posição absoluta que a
    // cabeça acabou de publicar - derivar pelo índice esconderia exatamente o
    // defeito que esta bancada deve capturar.
    let supplied_start = inc
        .meta
        .get("start_pos")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let cached_start =
        if hop.is_head && chunk_index == 0 && inc.meta["reuse_prefix"].as_bool().unwrap_or(false) {
            1
        } else {
            0
        };
    let start_pos = match (supplied_start, chunk_index) {
        (Some(actual), _) => actual,
        (None, 0) => cached_start,
        (None, _) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("chunk {chunk_index} sem start_pos absoluto"),
            )
                .into_response();
        }
    };
    let n_tokens = PROMPT_TOKENS
        .saturating_sub(start_pos)
        .min(chunk_size)
        .max(1) as usize;
    let next_pos = start_pos + n_tokens as u32;
    if hop.is_head {
        hop.received_starts
            .lock()
            .unwrap()
            .push((chunk_index, supplied_start, start_pos));
    }
    let more = hop.is_head && next_pos < PROMPT_TOKENS;

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    local["start_pos"] = json!(start_pos);
    local["next_pos"] = json!(next_pos);
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(PROMPT_TOKENS);
        local["reuse_prefix"] = inc.meta["reuse_prefix"].clone();
        local["cached_tokens"] = json!(cached_start);
    }

    if inc.is_last {
        local["done"] = json!(false);
        if is_final_chunk {
            local["token_id"] = json!(1234);
            local["text"] = json!("x");
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];
    let progress = hop
        .is_head
        .then(|| crate::infer_server::prompt_progress_for_forward(&inc.meta, &local));

    if let Some(reply_to) = inc.reply_to {
        let hop_timings = crate::direct::accumulate(
            &inc.meta,
            crate::direct::hop_entry(
                &local["processing_ms"],
                &local["node_timings"],
                inc.inbound_bytes,
            ),
        );
        if inc.next_hops.is_empty() {
            let mut result = local;
            crate::infer_server::apply_prompt_progress(&inc.meta, &mut result);
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(
                    &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
                    &reply_to,
                    result,
                )
                .await;
            });
        } else {
            let forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                "reuse_prefix": inc.meta["reuse_prefix"].clone(),
                "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
                "start_pos": local["start_pos"].clone(),
                "next_pos": local["next_pos"].clone(),
                "prompt_progress": crate::infer_server::prompt_progress_for_forward(&inc.meta, &local),
                "hop_timings": hop_timings,
            });
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/prefill",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
            ));
        }
        hop.depart().await;
        return crate::infer_server::direct_ack_with_progress(progress);
    }

    // ── Caminho antigo (anel borbulhado ou hub), para comparação ────────────
    let response = if inc.next_hops.is_empty() {
        if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        }
    } else {
        let base = json!({
            "session_id": inc.meta["session_id"].clone(),
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": inc.meta["reuse_prefix"].clone(),
            "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
            "start_pos": local["start_pos"].clone(),
            "next_pos": local["next_pos"].clone(),
        });
        match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape), 0).await {
            Ok(mut bubbled) => {
                crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
                frame(&bubbled, None)
            }
            Err(resp) => resp,
        }
    };
    hop.depart().await;
    response
}

async fn auth_prefill(
    State(hop): State<Hop>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = auth_failure(&headers, hop.auth.as_ref()) {
        return response;
    }
    fake_prefill(State(hop), body).await
}

async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.arrive().await;
    let inc = read(&body);
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        local["token_id"] = json!(1234);
        local["text"] = json!("x");
        local["done"] = json!(false);
    }
    let residual = vec![0.0f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    if let Some(reply_to) = inc.reply_to {
        let hop_timings = crate::direct::accumulate(
            &inc.meta,
            crate::direct::hop_entry(
                &local["processing_ms"],
                &local["node_timings"],
                inc.inbound_bytes,
            ),
        );
        if inc.next_hops.is_empty() {
            let mut result = local;
            result["hop_timings"] = json!(hop_timings);
            tokio::spawn(async move {
                crate::direct::deliver_result(
                    &crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)),
                    &reply_to,
                    result,
                )
                .await;
            });
        } else {
            let forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "hop_timings": hop_timings,
            });
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/step",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
            ));
        }
        hop.depart().await;
        return crate::infer_server::direct_ack();
    }

    let response = if inc.next_hops.is_empty() {
        if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        }
    } else {
        let base = json!({"session_id": inc.meta["session_id"].clone()});
        match bubble(&inc.next_hops, "/v1/step", base, (&residual, shape), 0).await {
            Ok(bubbled) => frame(&bubbled, None),
            Err(resp) => resp,
        }
    };
    hop.depart().await;
    response
}

async fn spawn_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(fake_prefill))
        .route("/v1/step", post(fake_step))
        .route("/v1/session/release", post(fake_release))
        .route("/v1/session/rollback", post(fake_release))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

async fn spawn_auth_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(auth_prefill))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── O gateway falso ───────────────────────────────────────────────────────────

/// Serve o handler de produção de `/v1/relay-result`, pagando o enlace entre a
/// cauda e o sítio do gateway — a perna nova, a que substitui a subida pela
/// pilha aninhada.
#[derive(Clone)]
struct GatewaySite {
    inbox: crate::direct::ReplyInbox,
    site: usize,
    tail_site: usize,
    deliveries: Arc<AtomicU64>,
    prefill_deliveries: Arc<AtomicU64>,
    auth: Option<AuthProbe>,
}

async fn fake_relay_result(
    State(gw): State<GatewaySite>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = auth_failure(&headers, gw.auth.as_ref()) {
        return response;
    }
    let (meta, _) = wire::decode(&body).expect("resultado direto válido");
    let is_transport_update = meta
        .get("transport_update")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !is_transport_update && gw.site != gw.tail_site {
        tokio::time::sleep(Duration::from_millis(LINK_MS)).await;
    }
    if !is_transport_update {
        gw.deliveries.fetch_add(1, Ordering::Relaxed);
        if meta.get("next_pos").is_some() {
            gw.prefill_deliveries.fetch_add(1, Ordering::Relaxed);
        }
    }
    crate::direct::handle_relay_result(&gw.inbox, body)
}

async fn spawn_gateway(gw: GatewaySite) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route(crate::direct::RELAY_RESULT_PATH, post(fake_relay_result))
        .with_state(gw);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── Cenário ───────────────────────────────────────────────────────────────────

fn step_for(node_id: &str, url: &str, start: u32, end: u32, direct: bool) -> PipelineStep {
    // The fake endpoint below implements the single-frame wire contract. Do
    // not advertise batching unless the fixture also implements the batch
    // request and response envelope.
    let mut caps = vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()];
    if direct {
        caps.push(crate::pipeline::CAP_RELAY_DIRECT.to_string());
    }
    PipelineStep {
        node_id: node_id.into(),
        inference_url: url.into(),
        inference_candidates: vec![],
        start_layer: start,
        end_layer: end,
        exec_start: start,
        exec_end: end,
        inference_frame_capabilities: caps,
        libp2p_peer_id: None,
        libp2p_addr_candidates: Vec::new(),
    }
}

struct Outcome {
    ms_per_token: f64,
    deliveries: u64,
    prefill_deliveries: u64,
    prefill_chunks: u32,
    head_starts: Vec<(u32, Option<u32>, u32)>,
}

/// Roda `n_hops` hops, cada um no seu sítio, com o gateway no sítio do hop
/// `entry`. `direct` escolhe entre a correção e o anel borbulhado.
async fn run_scenario(n_hops: usize, entry: usize, direct: bool, reuse_prefix: bool) -> Outcome {
    let gateway_site = entry;
    let deliveries = Arc::new(AtomicU64::new(0));
    let prefill_deliveries = Arc::new(AtomicU64::new(0));

    let inbox = crate::direct::ReplyInbox::new();
    let gateway_url = spawn_gateway(GatewaySite {
        inbox: inbox.clone(),
        site: gateway_site,
        tail_site: n_hops - 1,
        deliveries: deliveries.clone(),
        prefill_deliveries: prefill_deliveries.clone(),
        auth: None,
    })
    .await;

    let mut steps = Vec::new();
    let mut head_starts = Arc::new(Mutex::new(Vec::new()));
    for i in 0..n_hops {
        // Quem chama o hop `i` é o hop `i-1`; o hop 0 é chamado pelo gateway.
        let caller_site = if i == 0 { gateway_site } else { i - 1 };
        let received_starts = Arc::new(Mutex::new(Vec::new()));
        if i == 0 {
            head_starts = received_starts.clone();
        }
        let url = spawn_hop(Hop {
            site: i,
            caller_site,
            is_head: i == 0,
            received_starts,
            auth: None,
        })
        .await;
        steps.push(step_for(
            &format!("hop{i}"),
            &url,
            i as u32,
            i as u32,
            direct,
        ));
    }

    let plan = PipelinePlan {
        steps,
        edges: vec![],
    };
    let session = uuid::Uuid::new_v4().to_string();
    let reply = crate::direct::DirectReply::new(
        inbox,
        vec![gateway_url],
        "gateway".into(),
    );
    let http = crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600));
    let routes = crate::route::RouteTable::new();

    let first = crate::prefill::run_with_options(
        &plan,
        "lab",
        &session,
        "um prompt qualquer",
        &[],
        true,
        CHUNK,
        WireDtype::F16,
        &http,
        &routes,
        reply.as_ref(),
        None,
        &crate::prefill::Cancellation::new(),
        reuse_prefix,
        crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS,
        None,
        None,
    )
    .await
    .expect("prefill");
    assert_eq!(
        first.prompt_tokens, PROMPT_TOKENS,
        "a contagem do prompt tem que chegar da cabeça ao gateway"
    );
    assert_eq!(
        first.chunks,
        if reuse_prefix {
            PROMPT_TOKENS.saturating_sub(1).div_ceil(CHUNK)
        } else {
            PROMPT_TOKENS.div_ceil(CHUNK)
        },
        "o prompt tem que atravessar em todos os pedaços — um `prompt_progress` \
         perdido trunca no primeiro"
    );

    let mut next = first.token_id;
    let started = Instant::now();
    for _ in 1..DECODE_TOKENS {
        let step = crate::prefill::step(
            &plan,
            "lab",
            &session,
            next,
            WireDtype::F16,
            &http,
            &routes,
            reply.as_ref(),
            None,
            None,
            None,
        )
        .await
        .expect("step");
        next = step.accepted[0].0;
    }

    let head_starts = head_starts.lock().unwrap().clone();
    Outcome {
        ms_per_token: started.elapsed().as_millis() as f64 / (DECODE_TOKENS - 1) as f64,
        deliveries: deliveries.load(Ordering::Relaxed),
        prefill_deliveries: prefill_deliveries.load(Ordering::Relaxed),
        prefill_chunks: first.chunks,
        head_starts,
    }
}

/// Reproduz a regressão de `f9d5632`: o gateway autentica o primeiro salto com
/// a chave da sessão, mas o hop intermediário não recebe credencial para falar
/// com o seguinte. Ao denunciar a falha, a cauda também chega ao callback sem
/// API key e encontra o segundo 401. O cenário permanece ponta a ponta para
/// garantir que o quadro chega à cauda e o resultado é entregue ao inbox.
#[tokio::test(flavor = "multi_thread")]
async fn direct_pipeline_reproduces_both_missing_credentials() {
    let session_key = format!("sk-session-{}", uuid::Uuid::new_v4().simple());
    let connection_key = "cluster-secret".to_string();
    let tail_missing = Arc::new(AtomicU64::new(0));
    let accepted_tokens = vec![connection_key.clone()];

    let inbox = crate::direct::ReplyInbox::new();
    let gateway_url = spawn_gateway(GatewaySite {
        inbox: inbox.clone(),
        site: 1,
        tail_site: 1,
        deliveries: Arc::new(AtomicU64::new(0)),
        prefill_deliveries: Arc::new(AtomicU64::new(0)),
        // The callback is authenticated by the per-frame Ticket in the body.
        auth: None,
    })
    .await;

    let head_missing = Arc::new(AtomicU64::new(0));
    let head_url = spawn_auth_hop(Hop {
        site: 0,
        caller_site: 1,
        is_head: true,
        received_starts: Arc::new(Mutex::new(Vec::new())),
        auth: Some(AuthProbe {
            accepted_tokens: accepted_tokens.clone(),
            missing: head_missing.clone(),
            invalid: Arc::new(AtomicU64::new(0)),
        }),
    })
    .await;
    let tail_url = spawn_auth_hop(Hop {
        site: 1,
        caller_site: 0,
        is_head: false,
        received_starts: Arc::new(Mutex::new(Vec::new())),
        auth: Some(AuthProbe {
            accepted_tokens,
            missing: tail_missing.clone(),
            invalid: Arc::new(AtomicU64::new(0)),
        }),
    })
    .await;

    let steps = vec![
        step_for("head", &head_url, 0, 0, true),
        step_for("tail", &tail_url, 1, 1, true),
    ];
    let session = uuid::Uuid::new_v4().to_string();
    let reply = crate::direct::DirectReply::new(
        inbox.clone(),
        vec![gateway_url],
        "gateway".into(),
    )
    .expect("gateway de teste precisa ser alcançável");
    let ticket = reply.ticket_for_hops(steps.len());
    let meta = json!({
        "session_id": session,
        "chunk_index": 0,
        "chunk_size": CHUNK,
        "is_final_chunk": true,
        "next_hops": [steps[1].clone()],
        "reply_to": reply.reply_to(ticket.id()),
    });

    let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
    {
        let mut config = state.auth.write().await;
        config.connection_key = connection_key;
        config.api_keys.push(crate::config::ApiKey {
            id: "session-key".into(),
            key: session_key.clone(),
            label: "direct-lab".into(),
            node_id: "gateway".into(),
            origin_node_id: "gateway".into(),
            active: true,
            created_at: "2026-09-07T00:00:00Z".into(),
        });
    }
    let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));
    http.set_auth_state(state.auth.clone());
    crate::infer_server::http_client().set_auth_state(state.auth.clone());

    let sent = crate::relay::send_frame_or_tunnel(
        &http,
        &steps[0],
        None,
        "/v1/prefill",
        &meta,
        None,
        WireDtype::F16,
        WireDtype::F16.frame_encoding(),
        1,
        "head",
        |_| {},
    )
    .await
    .expect("o primeiro hop deve aceitar o quadro");
    assert_eq!(sent.meta["accepted"], true);

    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if inbox.outstanding() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(observed.is_ok(), "o resultado não chegou ao gateway");
    assert_eq!(head_missing.load(Ordering::Relaxed), 0);
    assert_eq!(tail_missing.load(Ordering::Relaxed), 0);
}

/// Pernas de rede pagas por token, lidas do relógio. Com o cálculo em zero, o
/// tempo por token é o número de travessias vezes `LINK_MS`.
fn legs(o: &Outcome) -> f64 {
    o.ms_per_token / LINK_MS as f64
}

/// O mínimo possível: o gateway tem que alcançar a cabeça e ouvir a cauda, e
/// entre os hops não há atalho. Uma perna a menos em cada ponta quando o
/// gateway mora lá.
fn minimum_legs(n_hops: usize, entry: usize) -> f64 {
    let inner = (n_hops - 1) as f64;
    let to_head = if entry == 0 { 0.0 } else { 1.0 };
    let from_tail = if entry == n_hops - 1 { 0.0 } else { 1.0 };
    inner + to_head + from_tail
}

// ── A lei ─────────────────────────────────────────────────────────────────────

/// **O custo por token não depende de onde está o ponto de entrada.**
///
/// Esta é a correção inteira, medida. Para cada tamanho de cadeia, com o
/// gateway na cabeça, no meio e na cauda, o tempo por token tem que ser o
/// mínimo teórico — uma travessia por enlace da cadeia, mais a ida até a
/// cabeça e a volta da cauda quando o gateway não mora numa das pontas.
///
/// A prova de que isso é uma correção, e não o comportamento de sempre, está
/// na última asserção: no anel borbulhado a mesma cadeia com o gateway na
/// cauda paga o dobro das travessias, que foi a queda de 25 para 14 tok/s
/// medida no cluster do captain em 2026-08-14.
///
/// Tolerância de 0,45 perna (≈23 ms com `LINK_MS` 50) absorve o custo de
/// quatro chamadas HTTP em loopback com tensores de 5120 posições; ela é bem
/// menor que a perna inteira que separa um resultado certo de um errado.
#[tokio::test(flavor = "multi_thread")]
async fn the_cost_per_token_does_not_depend_on_where_the_entry_point_is() {
    const TOLERANCE: f64 = 0.45;

    for n_hops in 2..=4usize {
        let mut measured = Vec::new();
        for entry in 0..n_hops {
            let outcome = run_scenario(n_hops, entry, true, false).await;
            let expected = minimum_legs(n_hops, entry);
            let got = legs(&outcome);
            println!(
                "direto  N={n_hops} entrada={entry}  {:6.1} ms/token  = {got:.2} pernas \
                 (mínimo {expected:.0})",
                outcome.ms_per_token
            );
            assert!(
                (got - expected).abs() < TOLERANCE,
                "N={n_hops} entrada={entry}: {got:.2} pernas por token, esperado {expected:.0}"
            );
            assert_eq!(
                outcome.deliveries,
                DECODE_TOKENS as u64 - 1 + outcome.prefill_chunks as u64,
                "cada quadro tem que ser entregue direto exatamente uma vez, \
                 no decode e no prefill"
            );
            measured.push(got);
        }

        // As duas pontas são o caso do captain, e têm que empatar: com N=2 é
        // exatamente a ordem A contra a ordem B.
        let head = measured[0];
        let tail = measured[n_hops - 1];
        assert!(
            (head - tail).abs() < TOLERANCE,
            "N={n_hops}: entrar pela cabeça custou {head:.2} pernas e pela cauda {tail:.2}"
        );
        // Entrar pelo meio custa uma perna a mais e nunca mais que isso — é o
        // preço irredutível de não estar em nenhuma das duas pontas.
        let worst = measured.iter().cloned().fold(f64::MIN, f64::max);
        assert!(
            worst - head < 1.0 + TOLERANCE,
            "N={n_hops}: a pior entrada custou {worst:.2} pernas contra {head:.2} na cabeça"
        );
    }

    // A linha de base: o anel borbulhado, com o gateway na cauda, paga o dobro.
    let ring_tail = run_scenario(2, 1, false, false).await;
    let direct_tail = run_scenario(2, 1, true, false).await;
    println!(
        "anel    N=2 entrada=1  {:6.1} ms/token  = {:.2} pernas",
        ring_tail.ms_per_token,
        legs(&ring_tail)
    );
    assert!(
        (legs(&ring_tail) - 4.0).abs() < TOLERANCE,
        "o anel com a cabeça remota deveria pagar 4 travessias, pagou {:.2}",
        legs(&ring_tail)
    );
    assert!(
        legs(&direct_tail) < legs(&ring_tail) - 1.0,
        "a resposta direta tem que ser mais barata que o anel no caso que doía"
    );
    assert_eq!(ring_tail.deliveries, 0, "o anel não usa a entrega direta");
}

/// O prompt longo precisa carregar a posição absoluta que a cabeça publicou
/// dentro do ACK direto, tanto numa sessão fria quanto após reaproveitar um
/// prefixo. Este cenário exercita a mesma cadeia de produção e faz os hops
/// falsos recusarem qualquer continuação sem `start_pos`.
#[tokio::test(flavor = "multi_thread")]
async fn direct_prefill_preserva_posicao_absoluta_com_e_sem_reuso() {
    for reuse_prefix in [false, true] {
        let outcome = run_scenario(2, 1, true, reuse_prefix).await;
        assert_eq!(
            outcome.deliveries,
            DECODE_TOKENS as u64 - 1 + outcome.prefill_chunks as u64
        );
        assert_eq!(
            outcome.prefill_deliveries, outcome.prefill_chunks as u64,
            "cada chunk de prefill deve chegar ao gateway exatamente uma vez"
        );
        let mut starts = outcome.head_starts;
        starts.sort_by_key(|(chunk, _, _)| *chunk);
        if reuse_prefix {
            assert_eq!(starts, vec![(0, None, 1)]);
        } else {
            assert_eq!(starts[0], (0, None, 0));
            assert_eq!(starts[1], (1, Some(2), 2));
        }
    }
}
