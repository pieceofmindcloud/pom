//! Bancada do reuso de prefixo entre turnos: **um acerto de cache tem que
//! virar economia medida.**
//!
//! O `prefix_cache_lab` ao lado prova a contabilidade do cache em memória - a
//! *escolha*. Esta prova a *execução*: quantos tokens do prompt o pipeline
//! deixou de reprocessar, turno a turno, pelo mesmo caminho que a tela de chat
//! e um cliente OpenAI usam (`openai::render_prompt`,
//! `openai::conversation_cache_key`, `gateway::generate_stream_with_stops`,
//! `prefill::run`, `/v1/prefill`, `/v1/step`, `/v1/session/rewind`,
//! `/v1/session/release`). Era exatamente aí que o sistema informava "sessão
//! reusada" e executava outra coisa: em 100% dos turnos medidos em produção,
//! `cached_tokens` aproveitado era zero.
//!
//! ## Os três transportes, não um
//!
//! O prefill tem três caminhos - hub, anel e resposta direta - e cada um monta
//! o pedido do hop seguinte num lugar diferente. Um teste que exercite só um
//! deles passa enquanto os outros dois estão quebrados, que é a forma mais
//! fácil de esta bancada mentir. Cada cenário roda nos três, com dois hops,
//! para que cabeça e cauda sejam máquinas distintas como em produção.
//!
//! ## O motor que os hops falsos imitam
//!
//! O hop falso não é um motor complacente. Ele é o **mais restrito** que o
//! produto tem que atender: um modelo recorrente/híbrido (`gated_delta_net` do
//! Qwen3-Next, o Ornith de produção), onde `llama_memory_seq_rm` **recusa**
//! qualquer remoção parcial maior que a janela de snapshots e só aceita apagar
//! a sequência inteira - a causa da recusa está em
//! `llama.cpp/src/llama-memory-recurrent.cpp::seq_rm`. Pedir a este hop que
//! descarte posições do meio do KV é falha do teste, não recusa do modelo:
//! foi essa dependência que a decisão do produto mandou tirar do desenho, e a
//! bancada existe para que ela não volte por descuido.
//!
//! **Limite declarado:** sem GGUF nesta máquina não há llama.cpp de verdade.
//! O que a bancada prova é o contrato entre gateway e peers - onde os quatro
//! defeitos moravam. O que ela não prova é o que o llama.cpp faz com o lote;
//! isso é medição em cluster, registrada no relato da tarefa.

#![cfg(test)]

use crate::openai::{ChatMessage, Content};
use crate::wire::{self, WireDtype};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const HIDDEN_DIM: usize = 8;

// ── O motor falso, com contabilidade de KV ────────────────────────────────────

/// O KV de uma sessão neste hop, na mesma forma que `infer_server::Session`:
/// a sequência do prompt e, separada, a da geração.
#[derive(Default, Clone)]
struct FakeSession {
    /// Tokens do prompt presentes no KV da sequência do prompt.
    prompt_kv: Vec<i32>,
    /// Fronteira anotada quando o prompt acabou.
    prompt_len: u32,
    /// Quantas posições de geração existem além do prompt.
    gen_len: u32,
    /// O prompt-alvo deste turno, na cabeça (que é quem tokeniza).
    alvo: Vec<i32>,
}

impl FakeSession {
    fn n_past(&self) -> u32 {
        self.prompt_kv.len() as u32 + self.gen_len
    }
}

#[derive(Default)]
struct Engine {
    sessions: HashMap<String, FakeSession>,
    /// Um item por pedaço de prompt ingerido, com quantos tokens custou.
    prompt_chunks: Vec<u32>,
    /// Pedidos que exigiriam remover posições do meio do KV. Um motor
    /// recorrente recusaria; a bancada anota para o teste falhar por isso.
    remocoes_parciais: Vec<String>,
    /// Sessões que sumiram antes de o gateway conseguir preservá-las.
    rewind_em_sessao_desconhecida: u32,
}

#[derive(Clone)]
struct Hop {
    is_head: bool,
    engine: Arc<Mutex<Engine>>,
}

/// O "tokenizador" da bancada: uma palavra, um token.
///
/// Estável e aditivo, como o de verdade num template de chat: acrescentar
/// texto no fim do prompt acrescenta tokens no fim da lista, sem mexer nos
/// anteriores. É a premissa que o reuso exige, e a bancada a torna explícita
/// em vez de deixá-la implícita num GGUF ausente.
fn tokenize(prompt: &str) -> Vec<i32> {
    prompt
        .split_whitespace()
        .map(|palavra| {
            let mut h: i32 = 7;
            for b in palavra.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as i32);
            }
            h
        })
        .collect()
}

fn frame(meta: &Value, tensor: Option<(&[f32], [usize; 2])>) -> Response {
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

fn erro(msg: String) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
}

async fn fake_health() -> impl IntoResponse {
    Json(json!({"status": "ok", "model_loaded": true, "hidden_dim": HIDDEN_DIM}))
}

async fn fake_rollback() -> impl IntoResponse {
    Json(json!({"rolled_back": true}))
}

struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    tensor: Option<(Vec<f32>, [usize; 2])>,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
}

/// Lido com as mesmas regras do `infer_server` real.
fn read(body: &Bytes) -> Incoming {
    let (meta, tensor) = wire::decode(body).expect("quadro válido");
    let declared: Option<Vec<crate::pipeline::PipelineStep>> = meta
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
        tensor,
        meta,
    }
}

/// Espelha `infer_server::relay_and_bubble`.
async fn bubble(
    hops: &[crate::pipeline::PipelineStep],
    path: &str,
    mut base_meta: Value,
    tensor: (&[f32], [usize; 2]),
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
    .map_err(|e| erro(e.to_string()))?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(0);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

/// `/v1/prefill`, com a mesma árvore de decisão do `infer_server` real:
/// `reuse_prefix` no pedaço 0, `start_pos` mandando a partir de onde ingerir, e
/// a resposta declarando o que **foi executado** (`reuse_prefix`,
/// `cached_tokens`), que é o que os hops seguintes obedecem.
/// O que a ingestão de um pedaço deixou pronto para a resposta.
struct Ingerido {
    n_tokens: usize,
    is_final_chunk: bool,
    local: Value,
}

/// Toda a contabilidade de KV, **síncrona**: o lock do motor falso nasce e
/// morre aqui, longe de qualquer `await`.
fn ingerir(hop: &Hop, inc: &Incoming, sid: &str) -> Result<Ingerido, String> {
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let reuse_pedido = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);
    let start_pos = inc.meta["start_pos"].as_u64().map(|v| v as u32);
    let is_final_chunk_recebido = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);

    let mut engine = hop.engine.lock().unwrap();

    let mut reused = false;
    let mut cached_tokens = 0u32;
    if chunk_index == 0 {
        let quer_reusar = reuse_pedido && engine.sessions.contains_key(sid);
        if !quer_reusar {
            // Reset: limpeza total, que todo motor aceita.
            engine
                .sessions
                .insert(sid.to_string(), FakeSession::default());
        }
        if hop.is_head {
            let tokens = tokenize(inc.meta["prompt"].as_str().unwrap_or_default());
            if quer_reusar {
                // A regra de produção em pessoa, não uma cópia: se ela mudar,
                // a bancada muda junto.
                let sessao = engine.sessions.get(sid).cloned().unwrap_or_default();
                let decisao = crate::infer_server::plan_prefix_reuse(
                    &sessao.prompt_kv,
                    sessao.n_past(),
                    &tokens,
                );
                reused = decisao.reused;
                cached_tokens = decisao.cached_tokens;
                if !reused {
                    engine
                        .sessions
                        .insert(sid.to_string(), FakeSession::default());
                }
            }
            let sessao = engine.sessions.entry(sid.to_string()).or_default();
            sessao.gen_len = 0;
            sessao.alvo = tokens;
        } else {
            engine.sessions.entry(sid.to_string()).or_default().gen_len = 0;
        }
    }

    let n_past = engine.sessions.entry(sid.to_string()).or_default().n_past();
    let esperado = start_pos.unwrap_or(if chunk_index == 0 {
        n_past
    } else {
        chunk_index * chunk_size
    });
    if esperado < n_past {
        // Um motor recorrente responderia `false` aqui. Anotar e recusar é o
        // que faz a bancada falhar em vez de esconder o defeito.
        engine.remocoes_parciais.push(format!(
            "pedaço {chunk_index} pediu posição {esperado} com a sessão em {n_past}"
        ));
        return Err("este modelo não aceita descartar posições do meio do cache".into());
    }
    if esperado > n_past {
        return Err(format!(
            "chunk fora de ordem: pos {esperado} com a sessão em {n_past}"
        ));
    }

    let (n_tokens, mais, total) = if hop.is_head {
        let alvo = engine.sessions[sid].alvo.clone();
        let restante = alvo.len().saturating_sub(esperado as usize);
        let n = restante.min(chunk_size as usize);
        if n == 0 {
            return Err(format!("chunk {chunk_index} não tem tokens"));
        }
        let novos = alvo[esperado as usize..esperado as usize + n].to_vec();
        engine
            .sessions
            .get_mut(sid)
            .unwrap()
            .prompt_kv
            .extend_from_slice(&novos);
        (n, esperado as usize + n < alvo.len(), alvo.len() as u32)
    } else {
        // Hop do meio/cauda: ingere o que a cabeça mandou, sem tokenizar.
        let n = inc.tensor.as_ref().map(|(_, s)| s[0]).unwrap_or(1);
        engine
            .sessions
            .get_mut(sid)
            .unwrap()
            .prompt_kv
            .extend(std::iter::repeat_n(0i32, n));
        (n, false, 0)
    };
    engine.prompt_chunks.push(n_tokens as u32);

    let is_final_chunk = if hop.is_head {
        !mais
    } else {
        is_final_chunk_recebido
    };
    if is_final_chunk {
        let sessao = engine.sessions.get_mut(sid).unwrap();
        sessao.prompt_len = sessao.prompt_kv.len() as u32;
    }

    let mut local = json!({
        "processing_ms": 0,
        "node_timings": {},
        "start_pos": esperado,
        "next_pos": esperado + n_tokens as u32,
    });
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(mais);
        local["n_prompt_tokens"] = json!(total);
    }
    if chunk_index == 0 {
        local["reuse_prefix"] = json!(reused);
        local["cached_tokens"] = json!(cached_tokens);
    }
    if inc.is_last {
        local["done"] = json!(false);
        local["more_chunks"] = json!(!is_final_chunk);
        if is_final_chunk {
            local["token_id"] = json!(1);
            local["text"] = json!("ok");
        }
    }
    Ok(Ingerido {
        n_tokens,
        is_final_chunk,
        local,
    })
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let sid = inc.meta["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let reuse_pedido = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);

    let Ingerido {
        n_tokens,
        is_final_chunk,
        local,
    } = match ingerir(&hop, &inc, &sid) {
        Ok(ok) => ok,
        Err(msg) => return erro(msg),
    };

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = [n_tokens, HIDDEN_DIM];

    // ── Resposta direta ─────────────────────────────────────────────────────
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
                "session_id": sid,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                // As funções de produção, para o repasse ser o de produção.
                "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                    &local, reuse_pedido, chunk_index),
                "is_final_chunk": is_final_chunk,
                "start_pos": local["start_pos"].clone(),
                "next_pos": local["next_pos"].clone(),
                "prompt_progress":
                    crate::infer_server::prompt_progress_for_forward(&inc.meta, &local),
                "hop_timings": hop_timings,
            });
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/prefill",
                forward,
                residual,
                shape,
                1,
                reply_to,
            ));
        }
        return crate::infer_server::direct_ack();
    }

    // ── Anel ────────────────────────────────────────────────────────────────
    if !inc.next_hops.is_empty() {
        let base = json!({
            "session_id": sid,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                &local, reuse_pedido, chunk_index),
            "is_final_chunk": is_final_chunk,
            "start_pos": local["start_pos"].clone(),
            "next_pos": local["next_pos"].clone(),
        });
        return match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape)).await {
            Ok(mut bubbled) => {
                crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
                frame(&bubbled, None)
            }
            Err(resp) => resp,
        };
    }

    // ── Hub ─────────────────────────────────────────────────────────────────
    if inc.is_last {
        frame(&local, None)
    } else {
        frame(&local, Some((&residual, shape)))
    }
}

/// `/v1/step`: um token gerado. Ele **não** entra na sequência do prompt.
async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let sid = inc.meta["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let gerados = {
        let mut engine = hop.engine.lock().unwrap();
        let Some(sessao) = engine.sessions.get_mut(&sid) else {
            return erro("sessão desconhecida — faça o prefill antes".into());
        };
        sessao.gen_len += 1;
        sessao.gen_len
    };

    // Três tokens por turno, depois EOS: o que se mede aqui é o prefill.
    let done = gerados >= 3;
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        local["done"] = json!(done);
        local["token_id"] = json!(2);
        local["text"] = json!(if done { "" } else { "x" });
    }
    let residual = vec![0.0f32; HIDDEN_DIM];
    let shape = [1usize, HIDDEN_DIM];

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
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/step",
                json!({"session_id": sid, "hop_timings": hop_timings}),
                residual,
                shape,
                1,
                reply_to,
            ));
        }
        return crate::infer_server::direct_ack();
    }

    if !inc.next_hops.is_empty() {
        return match bubble(
            &inc.next_hops,
            "/v1/step",
            json!({"session_id": sid}),
            (&residual, shape),
        )
        .await
        {
            Ok(bubbled) => frame(&bubbled, None),
            Err(resp) => resp,
        };
    }

    if inc.is_last {
        frame(&local, None)
    } else {
        frame(&local, Some((&residual, shape)))
    }
}

/// `/v1/session/rewind`: a retenção, do lado do peer. Apaga a sequência da
/// geração inteira - remoção total - e devolve a sessão ao fim do prompt.
async fn fake_rewind(State(hop): State<Hop>, Json(req): Json<Value>) -> impl IntoResponse {
    let sid = req["session_id"].as_str().unwrap_or_default().to_string();
    let pedido = req["n_tokens"].as_u64().unwrap_or(0) as u32;
    let mut engine = hop.engine.lock().unwrap();
    let Some(sessao) = engine.sessions.get_mut(&sid) else {
        engine.rewind_em_sessao_desconhecida += 1;
        return Json(json!({"rewound": false, "reason": "sessão desconhecida"}));
    };
    // A cauda não tokeniza, então o prompt dela é o número de posições que a
    // cabeça mandou - o mesmo `prompt_len` do `infer_server`.
    if pedido != sessao.prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!("prompt não bate: {pedido} != {}", sessao.prompt_len),
        }));
    }
    sessao.gen_len = 0;
    Json(json!({"rewound": true, "n_past": sessao.prompt_len}))
}

async fn fake_release(State(hop): State<Hop>, Json(req): Json<Value>) -> impl IntoResponse {
    let sid = req["session_id"].as_str().unwrap_or_default().to_string();
    hop.engine.lock().unwrap().sessions.remove(&sid);
    Json(json!({"released": true}))
}

async fn spawn_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(fake_prefill))
        .route("/v1/step", post(fake_step))
        .route("/v1/session/release", post(fake_release))
        .route("/v1/session/rewind", post(fake_rewind))
        .route("/v1/session/rollback", post(fake_rollback))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── A conversa, pelo caminho do usuário final ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transporte {
    Hub,
    Anel,
    Direto,
}

impl Transporte {
    fn caps(self) -> Vec<String> {
        match self {
            Transporte::Hub => Vec::new(),
            Transporte::Anel => vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()],
            Transporte::Direto => vec![
                crate::pipeline::CAP_RELAY_CHAIN.to_string(),
                crate::pipeline::CAP_RELAY_DIRECT.to_string(),
            ],
        }
    }

    fn todos() -> [Transporte; 3] {
        [Transporte::Hub, Transporte::Anel, Transporte::Direto]
    }
}

fn msg(role: &str, texto: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: Content::Text(texto.into()),
        tool_calls: None,
    }
}

#[derive(Debug)]
struct Turno {
    /// Tokens de prompt que a cabeça reprocessou neste turno.
    reprocessados: u32,
    /// Pedaços que atravessaram o pipeline neste turno, na cabeça.
    pedacos: u32,
    /// O que o gateway reportou como aproveitado do cache.
    cached_tokens: u32,
    prompt_tokens: u32,
    falhou: Option<String>,
}

struct Medida {
    turnos: Vec<Turno>,
    cabeca: Arc<Mutex<Engine>>,
    cauda: Arc<Mutex<Engine>>,
}

/// Roda `turnos` turnos de uma conversa que só cresce, pelo mesmo caminho que
/// `/v1/chat/completions` usa - inclusive `render_prompt` e
/// `conversation_cache_key`, que é onde a assimetria de renderização morava.
///
/// Sem pausa nenhuma entre os turnos: é o comportamento normal de um chat, e
/// era o que fazia o turno seguinte chegar antes da liberação do anterior.
async fn conversa(transporte: Transporte, turnos: usize) -> Medida {
    conversa_com(transporte, turnos, true).await
}

/// `esperar_o_fim = false` manda o turno seguinte assim que o evento final
/// chega ao cliente, sem esperar a tarefa do gateway terminar - que é o que um
/// chat de verdade faz e o que expôs a corrida do fim do turno: o `[DONE]` saía
/// antes de o rewind rodar, e o turno seguinte chegava no meio da arrumação.
async fn conversa_com(transporte: Transporte, turnos: usize, esperar_o_fim: bool) -> Medida {
    let motores: Vec<Arc<Mutex<Engine>>> = (0..2)
        .map(|_| Arc::new(Mutex::new(Engine::default())))
        .collect();
    let mut urls = Vec::new();
    for (i, engine) in motores.iter().enumerate() {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                engine: engine.clone(),
            })
            .await,
        );
    }

    let caps = transporte.caps();
    let peers: Vec<crate::network::PeerInfo> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let porta: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = crate::gateway::testkit::peer(
                &format!("hop{i}"),
                &format!("m{i}"),
                9000 + i as u16,
            );
            p.addr = "127.0.0.1:42777".into();
            p.inference_port = Some(porta);
            p.model = "lab".into();
            p.start_layer = i as u32;
            p.end_layer = i as u32;
            p.total_layers = 2;
            p.inference_frame_capabilities = caps.clone();
            p
        })
        .collect();

    let state = crate::gateway::testkit::state_with("eu", peers).await;
    let mut config = crate::gateway::testkit::spec_config();
    config.speculative_decoding_enabled = false;
    // Pedaços pequenos: é assim que a economia aparece como *menos pedaços*, e
    // não só como menos tokens.
    config.prefill_chunk_tokens = 4;
    state.runtime.seed_config_for_test(config).await;
    let state = crate::gateway::testkit::with_self_reachable_gateway(state).await;

    let mut historico: Vec<ChatMessage> = Vec::new();
    let mut medidos = Vec::new();
    for turno in 0..turnos {
        historico.push(msg(
            "user",
            &format!("pergunta numero {turno} com um pouco de texto para dar corpo ao prompt"),
        ));
        let prompt = crate::openai::render_prompt(&historico, &[]);
        let cache_key = crate::openai::conversation_cache_key(&historico, &[]);

        let antes = motores[0].lock().unwrap().prompt_chunks.len();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let bg = tokio::spawn(crate::gateway::generate_stream_with_stops(
            crate::gateway::GenerationInput {
                state: state.clone(),
                model: "lab".into(),
                prompt,
                chat_template: true,
                max_tokens: 8,
                stops: Vec::new(),
                cache_key: Some(cache_key),
            },
            tx,
        ));

        let mut resposta = String::new();
        let mut medido = Turno {
            reprocessados: 0,
            pedacos: 0,
            cached_tokens: 0,
            prompt_tokens: 0,
            falhou: None,
        };
        while let Some(evento) = rx.recv().await {
            let terminal = match evento {
                crate::gateway::GenEvent::Token(t) => {
                    resposta.push_str(&t);
                    false
                }
                crate::gateway::GenEvent::Done(s) => {
                    if let Some(trace) = &s.trace {
                        medido.cached_tokens = trace.cached_tokens;
                        medido.prompt_tokens = trace.input_tokens;
                    }
                    true
                }
                crate::gateway::GenEvent::Failed(e) => {
                    medido.falhou = Some(e);
                    true
                }
            };
            if terminal && !esperar_o_fim {
                break;
            }
        }
        if esperar_o_fim {
            let _ = bg.await;
        }
        {
            let e = motores[0].lock().unwrap();
            let deste_turno = &e.prompt_chunks[antes..];
            medido.reprocessados = deste_turno.iter().sum();
            medido.pedacos = deste_turno.len() as u32;
        }
        historico.push(msg(
            "assistant",
            if resposta.is_empty() { "ok" } else { &resposta },
        ));
        medidos.push(medido);
    }
    Medida {
        turnos: medidos,
        cabeca: motores[0].clone(),
        cauda: motores[1].clone(),
    }
}

// ── O que a bancada afirma ────────────────────────────────────────────────────

/// A reprodução do relatório, virada de cabeça para baixo: o que era 0 em 100%
/// dos turnos tem que ser positivo do segundo em diante, e o custo do prefill
/// tem que parar de crescer com a conversa.
#[tokio::test]
async fn from_the_second_turn_on_the_prompt_is_not_reprocessed() {
    for transporte in Transporte::todos() {
        let medida = conversa(transporte, 4).await;
        let turnos = &medida.turnos;
        let contexto = format!("{transporte:?}");

        for (i, t) in turnos.iter().enumerate() {
            assert!(
                t.falhou.is_none(),
                "[{contexto}] turno {i} falhou: {:?}",
                t.falhou
            );
        }

        assert_eq!(
            turnos[0].cached_tokens, 0,
            "[{contexto}] o primeiro turno é frio por definição"
        );
        assert_eq!(
            turnos[0].reprocessados, turnos[0].prompt_tokens,
            "[{contexto}] o primeiro turno ingere o prompt inteiro"
        );

        for (i, t) in turnos.iter().enumerate().skip(1) {
            assert!(
                t.cached_tokens > 0,
                "[{contexto}] turno {i} não aproveitou nada do prefixo: {turnos:?}"
            );
            assert_eq!(
                t.cached_tokens + t.reprocessados,
                t.prompt_tokens,
                "[{contexto}] turno {i}: cache + decodificado tem que dar o prompt"
            );
            assert_eq!(
                t.cached_tokens,
                turnos[i - 1].prompt_tokens,
                "[{contexto}] turno {i} devia aproveitar o prompt inteiro do turno anterior"
            );
        }

        // A propriedade que o captain vê como tempo até a primeira palavra: o
        // trabalho de um turno é do tamanho do texto **novo**, não da conversa
        // inteira. Sem o reuso esta série cresce a cada turno - foi o TTFT
        // plano de 3.8s com o prompt crescendo, e os 171s para acrescentar
        // 1043 caracteres, do relatório.
        let trabalho: Vec<u32> = turnos.iter().map(|t| t.reprocessados).collect();
        let prompts: Vec<u32> = turnos.iter().map(|t| t.prompt_tokens).collect();
        let pedacos: Vec<u32> = turnos.iter().map(|t| t.pedacos).collect();
        assert!(
            prompts.windows(2).all(|w| w[1] > w[0]),
            "[{contexto}] a conversa tinha que estar crescendo: {prompts:?}"
        );
        assert!(
            trabalho[1..].iter().all(|t| *t < prompts[1]),
            "[{contexto}] algum turno reprocessou o prompt quase inteiro: \
             trabalho={trabalho:?} prompts={prompts:?}"
        );
        assert!(
            trabalho[1..].windows(2).all(|w| w[1] <= w[0] + 2),
            "[{contexto}] o custo por turno cresce com a conversa: {trabalho:?}"
        );
        // Menos tokens para ingerir são menos pedaços atravessando o pipeline,
        // que é o número que o log de produção mostra.
        assert!(
            pedacos[1..].iter().all(|p| *p <= pedacos[0] + 1),
            "[{contexto}] o número de pedaços cresceu com a conversa: {pedacos:?}"
        );

        for (nome, engine) in [("cabeça", &medida.cabeca), ("cauda", &medida.cauda)] {
            let engine = engine.lock().unwrap();
            assert!(
                engine.remocoes_parciais.is_empty(),
                "[{contexto}] o reuso dependeu de remoção parcial de KV na {nome}, \
                 que o desenho proíbe: {:?}",
                engine.remocoes_parciais
            );
            assert_eq!(
                engine.rewind_em_sessao_desconhecida, 0,
                "[{contexto}] a sessão da {nome} foi destruída antes de o gateway \
                 preservar o prefixo"
            );
        }
    }
}

/// O defeito adicional do §7 do relatório: um chat manda o turno seguinte assim
/// que o stream fecha, e a arrumação do turno anterior tinha que estar pronta
/// antes disso. Aqui o cliente não espera a tarefa do gateway - ele age no
/// evento final, como a tela de chat age no `[DONE]`.
#[tokio::test]
async fn a_follow_up_sent_the_instant_the_stream_closes_still_hits() {
    for transporte in Transporte::todos() {
        let medida = conversa_com(transporte, 4, false).await;
        let cached: Vec<u32> = medida.turnos.iter().map(|t| t.cached_tokens).collect();
        for (i, t) in medida.turnos.iter().enumerate() {
            assert!(
                t.falhou.is_none(),
                "[{transporte:?}] turno {i} falhou: {:?}",
                t.falhou
            );
        }
        assert!(
            cached[1..].iter().all(|c| *c > 0),
            "[{transporte:?}] turno imediato perdeu o cache: {cached:?}"
        );
    }
}

/// O defeito D, isolado: a transição do turno 1 para o 2 é a que **nunca**
/// acertava, porque o turno 1 renderizava cru e o 2 rotulado. Ela é a primeira
/// que um chat de verdade encontra.
#[tokio::test]
async fn the_first_to_second_turn_transition_hits() {
    for transporte in Transporte::todos() {
        let medida = conversa(transporte, 2).await;
        assert!(
            medida.turnos[1].cached_tokens > 0,
            "[{transporte:?}] a transição 1→2 não aproveitou nada: {:?}",
            medida.turnos
        );
    }
}
