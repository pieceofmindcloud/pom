//! Bancada do reuso de prefixo entre turnos: **um acerto de cache tem que
//! virar economia medida.**
//!
//! O `prefix_cache_lab` ao lado prova a contabilidade do cache em memória - a
//! *escolha*. Esta prova a *execução*: quantos tokens do prompt o pipeline
//! deixou de reprocessar, turno a turno, pelo mesmo caminho que a tela de chat
//! e um cliente OpenAI usam (`openai::render_prompt`,
//! `openai::conversation_cache_key`, `gateway::generate_stream_with_stops`,
//! `prefill::run_with_options`, `/v1/prefill`, `/v1/step`, `/v1/session/rewind`,
//! `/v1/session/release`). Era exatamente aí que o sistema informava "sessão
//! reusada" e executava outra coisa: em 100% dos turnos medidos em produção,
//! `cached_tokens` aproveitado era zero.
//!
//! ## Os três transportes, não um
//!
//! O prefill tem três caminhos - hub, anel e resposta direta - e cada um monta
//! o pedido do hop seguinte num lugar diferente. Um teste que exercite só um
//! deles passa enquanto os outros dois estão quebrados, que é a forma mais
//! fácil de esta bancada mentir. Cada cenário roda nos três, com dois ou mais
//! hops, para que cabeça e cauda sejam máquinas distintas como em produção e
//! a propagação não fique presa a uma topologia de três nós.
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
use crate::wire::{self, TensorShape, WireDtype};
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
const FAKE_PREFILL_MS_PER_TOKEN: u64 = 2;

// ── O motor falso, com contabilidade de KV ────────────────────────────────────

/// O KV de uma sessão neste hop, na mesma forma que `infer_server::Session`:
/// a sequência do prompt e, separada, a da geração.
#[derive(Default, Clone)]
struct FakeSession {
    /// Tokens do prompt presentes no KV da sequência do prompt.
    prompt_kv: Vec<i32>,
    /// Fronteira anotada quando o prompt acabou. O token depois dela e os
    /// tokens gerados vivem na sequência de geração e são removidos juntos.
    prompt_len: u32,
    /// Quantas posições existem na sequência de geração além do prefixo
    /// estável. Inclui a cauda do prompt que não pode ser retida e a saída.
    gen_len: u32,
    /// Fronteira que a cabeça mediu para este prompt.
    stable_len: u32,
    /// O prompt-alvo deste turno, na cabeça (que é quem tokeniza).
    target: Vec<i32>,
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
    partial_removals: Vec<String>,
    /// Sessões que sumiram antes de o gateway conseguir preservá-las.
    rewind_on_unknown_session: u32,
    /// Cada frame de prefill efetivamente ingerido pelo peer. O teste não pode
    /// inferir a economia a partir do pedido do gateway: precisa comparar a
    /// posição declarada com o contador remoto antes e depois do decode.
    frames: Vec<ObservedFrame>,
    /// Falha controlada da bancada: remove a sessão antes de um chunk e faz o
    /// peer responder como o servidor real quando o chamador ainda não pediu
    /// um reset. O contador permite testar tanto a recuperação única quanto o
    /// limite de uma tentativa.
    unknown_session_at_chunk: Option<u32>,
    unknown_session_failures_remaining: usize,
    /// Falha HTTP transitória controlada para simular a ligação cortada depois
    /// de a cabeça aceitar um pedaço, mas antes de o hop seguinte o aceitar.
    transport_failure_at_chunk: Option<u32>,
    transport_failures_remaining: usize,
}

#[derive(Clone, Copy)]
enum FaultKind {
    UnknownSession,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedFrame {
    session_id: String,
    chunk_index: u32,
    start_pos: u32,
    next_pos: u32,
    n_past_before: u32,
    n_past_after: u32,
    reuse_prefix: bool,
}

#[derive(Clone)]
struct Hop {
    is_head: bool,
    engine: Arc<Mutex<Engine>>,
    local_prefill_order: crate::infer_server::DirectForwardOrder,
    direct_forward_order: crate::infer_server::DirectForwardOrder,
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
        .map(|word| {
            let mut h: i32 = 7;
            for b in word.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as i32);
            }
            h
        })
        .collect()
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

fn error(msg: String) -> Response {
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
    tensor: Option<(Vec<f32>, TensorShape)>,
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
    tensor: (&[f32], TensorShape),
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
    .map_err(|e| error(e.to_string()))?;

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
struct Ingested {
    n_tokens: usize,
    is_final_chunk: bool,
    local: Value,
}

/// Toda a contabilidade de KV, **síncrona**: o lock do motor falso nasce e
/// morre aqui, longe de qualquer `await`.
fn ingest(hop: &Hop, inc: &Incoming, sid: &str) -> Result<Ingested, String> {
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let reuse_requested = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);
    let start_pos = inc.meta["start_pos"].as_u64().map(|v| v as u32);
    let is_final_chunk_received = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);
    let stable_from_head = inc.meta["stable_len"].as_u64().map(|v| v as u32);

    let mut engine = hop.engine.lock().unwrap();

    if engine.unknown_session_at_chunk == Some(chunk_index)
        && engine.unknown_session_failures_remaining > 0
    {
        engine.unknown_session_failures_remaining -= 1;
        engine.sessions.remove(sid);
        return Err(format!(
            "sessão '{sid}' desconhecida - envie reset=true antes"
        ));
    }

    let mut reused = false;
    let mut cached_tokens = 0u32;
    if chunk_index == 0 {
        let wants_reuse = reuse_requested && engine.sessions.contains_key(sid);
        if !wants_reuse {
            // Reset: limpeza total, que todo motor aceita.
            engine
                .sessions
                .insert(sid.to_string(), FakeSession::default());
        }
        if hop.is_head {
            let tokens = tokenize(inc.meta["prompt"].as_str().unwrap_or_default());
            if wants_reuse {
                // A regra de produção em pessoa, não uma cópia: se ela mudar,
                // a bancada muda junto.
                let session = engine.sessions.get(sid).cloned().unwrap_or_default();
                let decision = crate::infer_server::plan_prefix_reuse(
                    &session.prompt_kv,
                    session.n_past(),
                    &tokens,
                );
                reused = decision.reused;
                cached_tokens = decision.cached_tokens;
                if !reused {
                    engine
                        .sessions
                        .insert(sid.to_string(), FakeSession::default());
                }
            }
            let session = engine.sessions.entry(sid.to_string()).or_default();
            session.gen_len = 0;
            // O último token representa a cauda do template. Assim a bancada
            // exercita a mesma separação que o servidor real faz com
            // `stable_prefix_len`: o prompt retido nunca recebe a geração.
            session.stable_len = tokens.len().saturating_sub(1) as u32;
            session.target = tokens;
        } else {
            let session = engine.sessions.entry(sid.to_string()).or_default();
            session.gen_len = 0;
            // O hop intermediário não tokeniza, portanto a decisão autoritativa
            // vem no frame e precisa ser ecoada para o próximo hop.
            reused = reuse_requested;
            if let Some(stable) = stable_from_head {
                session.stable_len = stable;
            }
        }
    }

    let n_past = engine.sessions.entry(sid.to_string()).or_default().n_past();
    let expected = start_pos.unwrap_or(if chunk_index == 0 {
        n_past
    } else {
        chunk_index * chunk_size
    });
    if expected < n_past {
        // Um motor recorrente responderia `false` aqui. Anotar e recusar é o
        // que faz a bancada falhar em vez de esconder o defeito.
        engine.partial_removals.push(format!(
            "pedaço {chunk_index} pediu posição {expected} com a sessão em {n_past}"
        ));
        return Err("este modelo não aceita descartar posições do meio do cache".into());
    }
    if expected > n_past {
        return Err(format!(
            "chunk fora de ordem: pos {expected} com a sessão em {n_past}"
        ));
    }

    let (n_tokens, more, total) = if hop.is_head {
        let target = engine.sessions[sid].target.clone();
        let remaining = target.len().saturating_sub(expected as usize);
        let n = remaining.min(chunk_size as usize);
        if n == 0 {
            return Err(format!("chunk {chunk_index} não tem tokens"));
        }
        let new_items = target[expected as usize..expected as usize + n].to_vec();
        let session = engine.sessions.get_mut(sid).unwrap();
        let stable = session.stable_len as usize;
        let retained = new_items.len().min(stable.saturating_sub(expected as usize));
        session.prompt_kv.extend_from_slice(&new_items[..retained]);
        session.gen_len += new_items.len().saturating_sub(retained) as u32;
        (n, expected as usize + n < target.len(), target.len() as u32)
    } else {
        // Hop do meio/cauda: ingere o que a cabeça mandou, sem tokenizar.
        let n = inc.tensor.as_ref().map(|(_, s)| s[0]).unwrap_or(1);
        let session = engine.sessions.get_mut(sid).unwrap();
        let retained = n.min(session.stable_len.saturating_sub(expected) as usize);
        session.prompt_kv.extend(std::iter::repeat_n(0i32, retained));
        session.gen_len += (n - retained) as u32;
        (n, false, 0)
    };
    engine.prompt_chunks.push(n_tokens as u32);

    let is_final_chunk = if hop.is_head {
        !more
    } else {
        is_final_chunk_received
    };
    if is_final_chunk {
        let session = engine.sessions.get_mut(sid).unwrap();
        session.prompt_len = session.stable_len;
    }

    let n_past_after = engine.sessions[sid].n_past();
    engine.frames.push(ObservedFrame {
        session_id: sid.to_string(),
        chunk_index,
        start_pos: expected,
        next_pos: expected + n_tokens as u32,
        n_past_before: n_past,
        n_past_after,
        reuse_prefix: reused,
    });

    let mut local = json!({
        "processing_ms": 0,
        "node_timings": {},
        "start_pos": expected,
        "next_pos": expected + n_tokens as u32,
    });
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(total);
    }
    local["stable_len"] = json!(engine.sessions[sid].stable_len);
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
    Ok(Ingested {
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
    let reuse_requested = inc.meta["reuse_prefix"].as_bool().unwrap_or(false);
    let prefill_sequence_id = inc
        .meta
        .get("prefill_sequence_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let local_turn = if let Some(sequence_id) = &prefill_sequence_id {
        if chunk_index == 0 {
            hop.local_prefill_order
                .ensure_sequence(sequence_id)
                .expect("initialize fake node prefill order");
        }
        Some(
            hop.local_prefill_order
                .wait_turn(sequence_id, chunk_index)
                .await
                .expect("fake node receives chunks in session order"),
        )
    } else {
        None
    };

    {
        let mut engine = hop.engine.lock().unwrap();
        if engine.transport_failure_at_chunk == Some(chunk_index)
            && engine.transport_failures_remaining > 0
        {
            engine.transport_failures_remaining -= 1;
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "ligação de prefill cortada durante o pedaço",
            )
                .into_response();
        }
    }

    let Ingested {
        n_tokens,
        is_final_chunk,
        mut local,
    } = match ingest(&hop, &inc, &sid) {
        Ok(ok) => {
            if let Some(turn) = local_turn {
                turn.finish(true, ok.is_final_chunk);
            }
            ok
        }
        Err(msg) if msg.contains("sessão") && msg.contains("desconhecida") => {
            return (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(msg) => return error(msg),
    };

    // Make the executed work visible in TTFT. The real engine's prefill cost
    // is proportional to the tokens it actually ingests; this deterministic
    // delay prevents scheduler jitter from turning the regression into a test
    // of the chosen cache flag instead of the work that followed it.
    tokio::time::sleep(std::time::Duration::from_millis(
        n_tokens as u64 * FAKE_PREFILL_MS_PER_TOKEN,
    ))
    .await;
    local["processing_ms"] = json!(n_tokens as u64 * FAKE_PREFILL_MS_PER_TOKEN);

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

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
        let progress = local
            .get("owns_tokens")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| crate::infer_server::prompt_progress_for_forward(&inc.meta, &local));
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
            if let Some(sequence_id) = &prefill_sequence_id {
                if chunk_index == 0 {
                    hop.direct_forward_order
                        .ensure_sequence(sequence_id)
                        .expect("initialize fake direct forwarding order");
                }
            }
            let mut forward = json!({
                "session_id": sid,
                "chunk_index": chunk_index,
                "chunk_size": chunk_size,
                // As funções de produção, para o repasse ser o de produção.
                "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                    &local, reuse_requested, chunk_index),
                "is_final_chunk": is_final_chunk,
                "start_pos": local["start_pos"].clone(),
                "next_pos": local["next_pos"].clone(),
                "stable_len": local["stable_len"].clone(),
                "prompt_progress":
                    crate::infer_server::prompt_progress_for_forward(&inc.meta, &local),
                "hop_timings": hop_timings,
            });
            if let Some(sequence_id) = &prefill_sequence_id {
                forward["prefill_sequence_id"] = json!(sequence_id);
            }
            tokio::spawn(crate::infer_server::forward_direct_ordered(
                inc.next_hops,
                "/v1/prefill",
                forward,
                residual,
                shape,
                crate::wire::WireDtype::F16,
                1,
                reply_to,
                prefill_sequence_id
                    .as_ref()
                    .map(|_| hop.direct_forward_order.clone()),
                prefill_sequence_id,
                Some(chunk_index),
            ));
        }
        return crate::infer_server::direct_ack_with_progress(progress);
    }

    // ── Anel ────────────────────────────────────────────────────────────────
    if !inc.next_hops.is_empty() {
        let base = json!({
            "session_id": sid,
            "chunk_index": chunk_index,
            "chunk_size": chunk_size,
            "reuse_prefix": crate::infer_server::reuse_prefix_for_forward(
                &local, reuse_requested, chunk_index),
            "is_final_chunk": is_final_chunk,
            "start_pos": local["start_pos"].clone(),
            "next_pos": local["next_pos"].clone(),
            "stable_len": local["stable_len"].clone(),
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

    let generated = {
        let mut engine = hop.engine.lock().unwrap();
        let Some(session) = engine.sessions.get_mut(&sid) else {
            return error("sessão desconhecida — faça o prefill antes".into());
        };
        session.gen_len += 1;
        session.gen_len
    };

    // Três tokens por turno, depois EOS: o que se mede aqui é o prefill.
    let done = generated >= 3;
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        local["done"] = json!(done);
        local["token_id"] = json!(2);
        local["text"] = json!(if done { "" } else { "x" });
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
            tokio::spawn(crate::infer_server::forward_direct(
                inc.next_hops,
                "/v1/step",
                json!({"session_id": sid, "hop_timings": hop_timings}),
                residual,
                shape,
                crate::wire::WireDtype::F16,
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
    let request = req["n_tokens"].as_u64().unwrap_or(0) as u32;
    let mut engine = hop.engine.lock().unwrap();
    let Some(session) = engine.sessions.get_mut(&sid) else {
        engine.rewind_on_unknown_session += 1;
        return Json(json!({"rewound": false, "reason": "sessão desconhecida"}));
    };
    // A cauda não tokeniza, então o prompt dela é o número de posições que a
    // cabeça mandou - o mesmo `prompt_len` do `infer_server`.
    if request != session.prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!("prompt não bate: {request} != {}", session.prompt_len),
        }));
    }
    session.gen_len = 0;
    Json(json!({"rewound": true, "n_past": session.prompt_len}))
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
enum Transport {
    Hub,
    Ring,
    Direct,
}

impl Transport {
    fn caps(self) -> Vec<String> {
        match self {
            Transport::Hub => Vec::new(),
            Transport::Ring => vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()],
            Transport::Direct => vec![
                crate::pipeline::CAP_RELAY_CHAIN.to_string(),
                crate::pipeline::CAP_RELAY_DIRECT.to_string(),
            ],
        }
    }

    fn all() -> [Transport; 3] {
        [Transport::Hub, Transport::Ring, Transport::Direct]
    }
}

fn msg(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: Content::Text(text.into()),
        tool_calls: None,
    }
}

#[derive(Debug)]
struct Turn {
    /// Tokens de prompt que a cabeça reprocessou neste turno.
    reprocessed: u32,
    /// Pedaços que atravessaram o pipeline neste turno, na cabeça.
    chunks: u32,
    /// O que o gateway reportou como aproveitado do cache.
    cached_tokens: u32,
    prompt_tokens: u32,
    /// Tempo de prefill declarado pelo caminho de inferência. É a métrica
    /// determinística do trabalho simulado; `first_word_us` inclui escalonamento
    /// do processo e serve apenas para verificar que a telemetria existe.
    prefill_ms: u64,
    first_word_us: u128,
    /// Maior profundidade do pipeline de prefill observada e o teto efetivo.
    pipeline_depth: u32,
    max_inflight: u32,
    overlap_enabled: bool,
    failed: Option<String>,
}

struct Measurement {
    turns: Vec<Turn>,
    head: Arc<Mutex<Engine>>,
    tail: Arc<Mutex<Engine>>,
    hops: Vec<Arc<Mutex<Engine>>>,
}

/// Roda `turns` turnos de uma conversa que só cresce, pelo mesmo caminho que
/// `/v1/chat/completions` usa - inclusive `render_prompt` e
/// `conversation_cache_key`, que é onde a assimetria de renderização morava.
///
/// Sem pausa nenhuma entre os turnos: é o comportamento normal de um chat, e
/// era o que fazia o turno seguinte chegar antes da liberação do anterior.
async fn conversation(transport: Transport, turns: usize) -> Measurement {
    conversation_with(transport, turns, true).await
}

/// `wait_for_end = false` manda o turno seguinte assim que o evento final
/// chega ao cliente, sem esperar a tarefa do gateway terminar - que é o que um
/// chat de verdade faz e o que expôs a corrida do fim do turno: o `[DONE]` saía
/// antes de o rewind rodar, e o turno seguinte chegava no meio da arrumação.
async fn conversation_with(transport: Transport, turns: usize, wait_for_end: bool) -> Measurement {
    conversation_with_hops(transport, turns, wait_for_end, 2).await
}

async fn conversation_with_hops(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
) -> Measurement {
    conversation_with_hops_fault(transport, turns, wait_for_end, hop_count, None).await
}

async fn conversation_with_hops_fault(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
) -> Measurement {
    conversation_with_hops_fault_kind(
        transport,
        turns,
        wait_for_end,
        hop_count,
        fault,
        FaultKind::UnknownSession,
    )
    .await
}

async fn conversation_with_hops_transport_fault(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
) -> Measurement {
    conversation_with_hops_fault_kind(
        transport,
        turns,
        wait_for_end,
        hop_count,
        fault,
        FaultKind::Transport,
    )
    .await
}

async fn conversation_with_hops_fault_kind(
    transport: Transport,
    turns: usize,
    wait_for_end: bool,
    hop_count: usize,
    fault: Option<(usize, u32, usize)>,
    fault_kind: FaultKind,
) -> Measurement {
    // Os peers falsos só servem HTTP; `PeerInfo::inference_candidates` lê a
    // política TLS global, que outros testes alternam em paralelo.
    let _scheme_lock = crate::peer_client::GLOBAL_POLICY.lock();
    crate::peer_client::set_tls_enabled(false);

    assert!(hop_count >= 1);
    let engines: Vec<Arc<Mutex<Engine>>> = (0..hop_count)
        .map(|_| Arc::new(Mutex::new(Engine::default())))
        .collect();
    let mut urls = Vec::new();
    for (i, engine) in engines.iter().enumerate() {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                engine: engine.clone(),
                local_prefill_order: crate::infer_server::DirectForwardOrder::default(),
                direct_forward_order: crate::infer_server::DirectForwardOrder::default(),
            })
            .await,
        );
    }

    if let Some((hop_index, chunk_index, failures)) = fault {
        assert!(hop_index < hop_count);
        let mut engine = engines[hop_index].lock().unwrap();
        match fault_kind {
            FaultKind::UnknownSession => {
                engine.unknown_session_at_chunk = Some(chunk_index);
                engine.unknown_session_failures_remaining = failures;
            }
            FaultKind::Transport => {
                engine.transport_failure_at_chunk = Some(chunk_index);
                engine.transport_failures_remaining = failures;
            }
        }
    }

    let caps = transport.caps();
    let peers: Vec<crate::network::PeerInfo> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = crate::gateway::testkit::peer(
                &format!("hop{i}"),
                &format!("m{i}"),
                9000 + i as u16,
            );
            p.addr = "127.0.0.1:42777".into();
            p.inference_port = Some(port);
            p.model = "lab".into();
            p.start_layer = i as u32;
            p.end_layer = i as u32;
            p.total_layers = hop_count as u32;
            p.inference_frame_capabilities = caps.clone();
            if matches!(fault_kind, FaultKind::Transport) {
                // Mantém o endereço anunciado antigo na frente e deixa o
                // `addr` atual como fallback. O cenário prova tanto a
                // recuperação do prefill quanto o release pela rota viva.
                p.addr_candidates = vec!["127.0.0.1:1".into()];
            }
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

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut measured_turns = Vec::new();
    for turn in 0..turns {
        // The real discovery loop refreshes `last_seen`; this fixture has no
        // UDP traffic, so refresh the synthetic peers before every request.
        // Otherwise a busy full test process can let the 30-second liveness
        // window expire between turns and make the direct path fail before it
        // reaches the cache code.
        {
            let now = chrono::Utc::now();
            let mut peers = state.network.peers.write().await;
            for peer in peers.values_mut() {
                peer.last_seen = now;
            }
        }
        let question = if turn == 0 {
            "pergunta inicial com bastante contexto para aquecer o prompt e medir o custo frio"
                .to_string()
        } else {
            format!("followup curto {turn}")
        };
        history.push(msg("user", &question));
        let prompt = crate::openai::render_prompt(&history, &[]);
        let cache_key = crate::openai::conversation_cache_key(&history, &[]);

        let before = engines[0].lock().unwrap().prompt_chunks.len();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let bg = tokio::spawn(crate::gateway::generate_stream_with_stops(
            crate::gateway::GenerationInput {
                probe: None,
                state: state.clone(),
                inference_token: "test-api-key".into(),
                model: "lab".into(),
                prompt,
                images: Vec::new(),
                chat_template: true,
                thinking_enabled: true,
                budget: crate::gateway::OutputBudget::single(8),
                stops: Vec::new(),
                cache_key: Some(cache_key),
                preferred_head: None,
                request_details: serde_json::json!({}),
                trace_id: None,
            },
            tx,
        ));

        let mut response = String::new();
        let mut measured = Turn {
            reprocessed: 0,
            chunks: 0,
            cached_tokens: 0,
            prompt_tokens: 0,
            prefill_ms: 0,
            first_word_us: 0,
            pipeline_depth: 0,
            max_inflight: 0,
            overlap_enabled: false,
            failed: None,
        };
        let started = std::time::Instant::now();
        let mut saw_first_word = false;
        while let Some(event) = rx.recv().await {
            let terminal = match event {
                crate::gateway::GenEvent::Token(t) => {
                    if !saw_first_word && !t.is_empty() {
                        measured.first_word_us = started.elapsed().as_micros();
                        saw_first_word = true;
                    }
                    response.push_str(&t);
                    false
                }
                crate::gateway::GenEvent::Done(s) => {
                    if let Some(trace) = &s.trace {
                        measured.cached_tokens = trace.cached_tokens;
                        measured.prompt_tokens = trace.input_tokens;
                        measured.prefill_ms = s.timings.prefill_ms;
                        measured.pipeline_depth = trace.prefill_pipeline_depth_max;
                        measured.max_inflight = trace.prefill_max_inflight_chunks;
                        measured.overlap_enabled = trace.prefill_overlap_enabled;
                    }
                    true
                }
                crate::gateway::GenEvent::Failed(e) => {
                    measured.failed = Some(e);
                    true
                }
                crate::gateway::GenEvent::FailedWithTelemetry { error, .. } => {
                    measured.failed = Some(error);
                    true
                }
            };
            if terminal && !wait_for_end {
                break;
            }
        }
        if wait_for_end {
            let _ = bg.await;
        }
        {
            let e = engines[0].lock().unwrap();
            let this_turn = &e.prompt_chunks[before..];
            measured.reprocessed = this_turn.iter().sum();
            measured.chunks = this_turn.len() as u32;
        }
        history.push(msg(
            "assistant",
            if response.is_empty() { "ok" } else { &response },
        ));
        measured_turns.push(measured);
    }
    Measurement {
        turns: measured_turns,
        head: engines[0].clone(),
        tail: engines.last().unwrap().clone(),
        hops: engines,
    }
}

// ── O que a bancada afirma ────────────────────────────────────────────────────

/// A reprodução do relatório, virada de cabeça para baixo: o que era 0 em 100%
/// dos turnos tem que ser positivo do segundo em diante, e o custo do prefill
/// tem que parar de crescer com a conversa.
#[tokio::test]
async fn from_the_second_turn_on_the_prompt_is_not_reprocessed() {
    for transport in Transport::all() {
        let measurement = conversation(transport, 4).await;
        let turns = &measurement.turns;
        let context = format!("{transport:?}");

        eprintln!(
            "[{context}] prefix-cache metrics: {}",
            turns
                .iter()
                .enumerate()
                .map(|(i, t)| format!(
                    "turn={i} cached={} prefill_ms={} ttft_us={} chunks={} prompt={} reprocessed={}",
                    t.cached_tokens, t.prefill_ms, t.first_word_us, t.chunks, t.prompt_tokens, t.reprocessed
                ))
                .collect::<Vec<_>>()
                .join("; ")
        );

        for (i, t) in turns.iter().enumerate() {
            assert!(
                t.failed.is_none(),
                "[{context}] turno {i} falhou: {:?}",
                t.failed
            );
        }

        assert_eq!(
            turns[0].cached_tokens, 0,
            "[{context}] o primeiro turno é frio por definição"
        );
        assert_eq!(
            turns[0].reprocessed, turns[0].prompt_tokens,
            "[{context}] o primeiro turno ingere o prompt inteiro"
        );

        for (i, t) in turns.iter().enumerate().skip(1) {
            assert!(
                t.cached_tokens > 0,
                "[{context}] turno {i} não aproveitou nada do prefixo: {turns:?}"
            );
            assert_eq!(
                t.cached_tokens + t.reprocessed,
                t.prompt_tokens,
                "[{context}] turno {i}: cache + decodificado tem que dar o prompt"
            );
            assert_eq!(
                t.cached_tokens,
                turns[i - 1].prompt_tokens.saturating_sub(1),
                "[{context}] turno {i} devia aproveitar o prefixo estável do turno anterior"
            );
            assert!(
                t.first_word_us > 0,
                "[{context}] turno {i} não registrou o tempo até a primeira palavra"
            );
        }

        // A propriedade que o captain vê como tempo até a primeira palavra: o
        // trabalho de um turno é do tamanho do texto **novo**, não da conversa
        // inteira. Sem o reuso esta série cresce a cada turno - foi o TTFT
        // plano de 3.8s com o prompt crescendo, e os 171s para acrescentar
        // 1043 caracteres, do relatório.
        let work: Vec<u32> = turns.iter().map(|t| t.reprocessed).collect();
        let prompts: Vec<u32> = turns.iter().map(|t| t.prompt_tokens).collect();
        let chunks: Vec<u32> = turns.iter().map(|t| t.chunks).collect();
        assert!(
            prompts.windows(2).all(|w| w[1] > w[0]),
            "[{context}] a conversa tinha que estar crescendo: {prompts:?}"
        );
        assert!(
            work[1..].iter().all(|t| *t < prompts[1]),
            "[{context}] algum turno reprocessou o prompt quase inteiro: \
             trabalho={work:?} prompts={prompts:?}"
        );
        assert!(
            work[1..].windows(2).all(|w| w[1] <= w[0] + 2),
            "[{context}] o custo por turno cresce com a conversa: {work:?}"
        );
        assert!(
            turns[1..]
                .iter()
                .all(|t| t.prefill_ms < turns[0].prefill_ms),
            "[{context}] o prefill medido não caiu depois do primeiro turno: {:?}",
            turns.iter().map(|t| t.prefill_ms).collect::<Vec<_>>()
        );
        // Menos tokens para ingerir são menos pedaços atravessando o pipeline,
        // que é o número que o log de produção mostra.
        assert!(
            chunks[1..].iter().all(|p| *p <= chunks[0] + 1),
            "[{context}] o número de pedaços cresceu com a conversa: {chunks:?}"
        );

        for (name, engine) in [("cabeça", &measurement.head), ("cauda", &measurement.tail)] {
            let engine = engine.lock().unwrap();
            assert!(
                engine.partial_removals.is_empty(),
                "[{context}] o reuso dependeu de remoção parcial de KV na {name}, \
                 que o desenho proíbe: {:?}",
                engine.partial_removals
            );
            assert_eq!(
                engine.rewind_on_unknown_session, 0,
                "[{context}] a sessão da {name} foi destruída antes de o gateway \
                 preservar o prefixo"
            );
            for session in engine.sessions.values() {
                assert_eq!(
                    session.prompt_kv.len() as u32,
                    session.prompt_len,
                    "[{context}] {name} reteve tokens da geração na sequência do prompt"
                );
                assert_eq!(
                    session.gen_len, 0,
                    "[{context}] {name} deixou a sequência de geração viva após o rewind"
                );
            }
        }
    }
}

/// O prefill de um prompt de vários pedaços tem que **encher o pipeline**: com
/// a cadeia mais lenta que a cabeça, a bomba da cabeça corre à frente e vários
/// pedaços ficam em voo ao mesmo tempo. O modo anel continua serializado.
#[tokio::test]
async fn a_multi_chunk_prefill_fills_the_pipeline() {
    // 4 hops: o downstream (3 hops) é ~3x o custo da cabeça, então a bomba da
    // cabeça enche a janela. O prompt frio do turno 0 tem vários pedaços de 4
    // tokens.
    for transport in [Transport::Hub, Transport::Direct] {
        let measurement = conversation_with_hops(transport, 1, true, 4).await;
        let t = &measurement.turns[0];
        assert!(
            t.failed.is_none(),
            "[{transport:?}] turno falhou: {:?}",
            t.failed
        );
        assert!(
            t.chunks >= 2,
            "[{transport:?}] o prompt frio tinha que ter mais de um pedaço: {}",
            t.chunks
        );
        assert!(
            t.overlap_enabled,
            "[{transport:?}] a sobreposição devia estar ligada num plano de vários hops"
        );
        assert!(
            t.pipeline_depth >= 2,
            "[{transport:?}] o pipeline não encheu: profundidade {} (teto {})",
            t.pipeline_depth,
            t.max_inflight
        );
        assert!(
            t.pipeline_depth <= t.max_inflight,
            "[{transport:?}] profundidade {} acima do teto {}",
            t.pipeline_depth,
            t.max_inflight
        );
    }

    // No anel a sobreposição é dos hops (`ring_overlap`): o gateway continua
    // com um pedaço por vez, e cada hop responde ao intermediário depois do
    // próprio compute e repassa em segundo plano, em ordem.
    let ring = conversation_with_hops(Transport::Ring, 1, true, 4).await;
    let t = &ring.turns[0];
    assert!(t.failed.is_none(), "[Anel] turno falhou: {:?}", t.failed);
    assert!(
        t.overlap_enabled,
        "[Anel] a sobreposição dos hops devia estar pedida"
    );
    assert!(
        t.pipeline_depth <= 1,
        "[Anel] o gateway não manda mais de um pedaço por vez: profundidade {}",
        t.pipeline_depth
    );
}

/// Um peer pode perder o estado entre dois chunks de um prefill frio. O
/// chamador tem o prompt inteiro e precisa reconstruir todos os hops desde o
/// chunk 0; repetir apenas o hidden state atual deixaria o peer sem contexto.
#[tokio::test]
async fn unknown_session_during_prefill_is_recovered() {
    for transport in Transport::all() {
        let measurement = conversation_with_hops_fault(transport, 1, true, 2, Some((1, 1, 1))).await;
        let turn = &measurement.turns[0];
        assert!(
            turn.failed.is_none(),
            "{transport:?} falhou depois de perder a sessão: {:?}",
            turn.failed
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            assert_eq!(
                engine
                    .frames
                    .iter()
                    .filter(|frame| frame.chunk_index == 0)
                    .count(),
                2,
                "{transport:?} não reiniciou o prefill desde o chunk 0 no hop {i}: {:?}",
                engine.frames
            );
        }
        assert!(
            turn.reprocessed > turn.prompt_tokens,
            "{transport:?} não registrou o custo da reconstrução fria: {turn:?}"
        );

        let hop = measurement.hops[1].lock().unwrap();
        assert_eq!(
            hop.rewind_on_unknown_session, 0,
            "{transport:?} terminou sem uma sessão preservável no peer recuperado"
        );
    }
}

/// Uma perda persistente não pode fazer o gateway repetir o prefill para
/// sempre. Depois de uma reconstrução, a falha chega ao cliente sem a
/// instrução interna que só um peer poderia executar.
#[tokio::test]
async fn unknown_session_recovery_is_bounded() {
    for transport in Transport::all() {
        let measurement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation_with_hops_fault(transport, 1, true, 2, Some((1, 1, usize::MAX))),
        )
        .await
        .expect("a recuperação de sessão desconhecida não pode ficar em loop");
        let turn = &measurement.turns[0];
        let message = turn
            .failed
            .as_deref()
            .expect("a recusa persistente devia chegar ao cliente");
        assert!(
            !message.contains("reset=true"),
            "mensagem interna vazou: {message}"
        );
        assert!(
            message.contains("o pedido não foi executado"),
            "mensagem final não explica o impacto para o usuário: {message}"
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            let cold_attempts = engine
                .frames
                .iter()
                .filter(|frame| frame.chunk_index == 0)
                .count();
            assert!(
                cold_attempts <= 2,
                "{transport:?} fez uma terceira tentativa fria no hop {i}: {cold_attempts}",
            );
        }
    }
}

/// Se a ligação cai depois de a cabeça aceitar o chunk, um motor recorrente
/// não pode receber a repetição parcial: ele não sabe remover o meio do KV.
/// A recuperação deve falhar sem pedir essa remoção e liberar as sessões que
/// ficaram nos hops alcançados.
#[tokio::test]
async fn mid_prefill_transport_failure_does_not_corrupt_recurrent_session() {
    for transport in Transport::all() {
        let measurement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conversation_with_hops_transport_fault(transport, 1, true, 2, Some((1, 1, 2))),
        )
        .await
        .expect("a falha de transporte não pode deixar a sessão pendurada");
        let turn = &measurement.turns[0];
        assert!(
            turn.failed.is_some(),
            "{transport:?} devia devolver a falha persistente ao cliente"
        );

        for (i, hop) in measurement.hops.iter().enumerate() {
            let engine = hop.lock().unwrap();
            assert!(
                engine.partial_removals.is_empty(),
                "{transport:?} pediu remoção parcial no hop {i}: {:?}",
                engine.partial_removals
            );
            assert!(
                engine.sessions.is_empty(),
                "{transport:?} deixou a sessão no hop {i}: {:?}",
                engine.sessions.keys().collect::<Vec<_>>()
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
    for transport in Transport::all() {
        let measurement = conversation_with(transport, 4, false).await;
        let cached: Vec<u32> = measurement.turns.iter().map(|t| t.cached_tokens).collect();
        for (i, t) in measurement.turns.iter().enumerate() {
            assert!(
                t.failed.is_none(),
                "[{transport:?}] turno {i} falhou: {:?}",
                t.failed
            );
        }
        assert!(
            cached[1..].iter().all(|c| *c > 0),
            "[{transport:?}] turno imediato perdeu o cache: {cached:?}"
        );
    }
}

/// O defeito D, isolado: a transição do turno 1 para o 2 é a que **nunca**
/// acertava, porque o turno 1 renderizava cru e o 2 rotulado. Ela é a primeira
/// que um chat de verdade encontra.
#[tokio::test]
async fn the_first_to_second_turn_transition_hits() {
    for transport in Transport::all() {
        let measurement = conversation(transport, 2).await;
        assert!(
            measurement.turns[1].cached_tokens > 0,
            "[{transport:?}] a transição 1→2 não aproveitou nada: {:?}",
            measurement.turns
        );
    }
}

/// A conversa real usa três máquinas no pipeline da ocorrência. O estado que
/// vale para o contrato não é o índice lógico do pedaço, mas o contador remoto
/// do peer que acabou de executar o frame.
#[tokio::test]
async fn three_hop_follow_ups_keep_each_remote_position_in_sync() {
    for transport in [Transport::Ring, Transport::Direct] {
        for hop_count in [3, 5] {
            let measurement = conversation_with_hops(transport, 4, true, hop_count).await;

            for (hop_index, engine) in measurement.hops.iter().enumerate() {
                let engine = engine.lock().unwrap();
                assert!(
                    !engine.frames.is_empty(),
                    "{transport:?}/{hop_count} hops não executou prefill no hop {hop_index}"
                );
                for frame in &engine.frames {
                    assert_eq!(
                        frame.start_pos, frame.n_past_before,
                        "{transport:?}/{hop_count} hops, sessão {}: chunk {} chegou na posição {} mas o peer estava em {}",
                        frame.session_id,
                        frame.chunk_index,
                        frame.start_pos,
                        frame.n_past_before
                    );
                    assert_eq!(frame.next_pos, frame.n_past_after);
                }
                assert_eq!(
                    engine
                        .frames
                        .iter()
                        .map(|frame| frame.session_id.as_str())
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    1,
                    "{transport:?}/{hop_count} hops usou mais de uma sessão na mesma conversa no hop {hop_index}"
                );
            }

            assert!(
                measurement.turns[1..]
                    .iter()
                    .all(|turn| turn.cached_tokens > 0),
                "{transport:?}/{hop_count} hops perdeu o cache nos follow-ups: {:?}",
                measurement.turns
            );
        }
    }
}

/// Um nó inteiro também precisa avançar por posição absoluta. O caso reproduz
/// a sessão retida em 42 tokens e um prompt seguinte que exige um segundo
/// pedaço: `chunk_index * 512` não pode substituir o `next_pos` confirmado.
#[tokio::test]
async fn single_node_long_follow_up_does_not_send_chunk_by_index() {
    let measurement = conversation_with_hops(Transport::Direct, 2, true, 1).await;
    let engine = measurement.head.lock().unwrap();
    let first_session = engine
        .frames
        .first()
        .map(|frame| frame.session_id.clone())
        .expect("o nó não recebeu o primeiro frame");
    let follow_up_frames: Vec<_> = engine
        .frames
        .iter()
        .filter(|frame| frame.session_id == first_session && frame.chunk_index > 0)
        .collect();
    assert!(
        follow_up_frames
            .iter()
            .all(|frame| frame.start_pos == frame.n_past_before),
        "um chunk foi enviado pela posição lógica, não pela posição remota: {follow_up_frames:?}"
    );
}
