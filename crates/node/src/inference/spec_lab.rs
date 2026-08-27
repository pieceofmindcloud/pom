//! Bancada da especulação: **uma rodada especulativa que não rascunha ainda
//! tem que gerar texto**.
//!
//! O contrato do fio já diz que a cabeça pode recusar o rascunho — está escrito
//! no doc de `step_blocking::incoming_draft`: `draft` ausente significa "esta
//! rodada não é especulativa, vale o formato antigo de um token".
//!
//! **Correção de 2026-08-16, depois desta bancada ter sido escrita**: a cabeça
//! achava que recusava "sempre que o cache KV do modelo não desloca"
//! (`kv_can_shift`) e chamava isso de propriedade de todo modelo híbrido ou
//! recorrente. Era engano nos dois pontos — `kv_can_shift`
//! (`llama_memory_can_shift`) é sobre o K-shift do `--ctx-shift` de referência
//! do llama.cpp, nada a ver com desfazer um rascunho, e nada neste projeto liga
//! `--ctx-shift`; e o Qwen3.5/3.8 (`LLM_ARCH_QWEN35`) tem suporte dedicado a
//! rollback recorrente parcial no llama.cpp
//! (`llm_arch_supports_rs_rollback`/`cparams.n_rs_seq`), que só precisava ser
//! ligado (`EngineConfig::rs_seq_window`). `should_draft` não recebe mais
//! `kv_can_shift` - hoje toda invocação de lookup rascunha, e a única recusa
//! que sobra é MTP sem cabeça `nextn` local e sem rascunho pronto. O que esta
//! bancada media (recusa antes de rascunhar, seguida de resposta direta) não
//! tem mais como acontecer no caminho de lookup; o defeito real que pode
//! acontecer agora é *depois* de rascunhar - `gateway::rollback_sessions`
//! recusado -, que é uma bancada diferente.
//!
//! O que faltava era a outra metade do contrato: quem pediu a especulação não
//! sabia ler a resposta antiga, e tratava uma resposta legítima como erro fatal.
//! O efeito visível é a requisição morrer com **um** token e `partial: true`,
//! que foi o que o captain viu em 2026-08-15 no Qwen3.8-27B. Essa metade
//! continua valendo e é o que os testes abaixo ainda provam.
//!
//! Aqui o caminho de orquestração é o de produção inteiro —
//! `generate_stream_with_stops`,
//! `prefill::run`, `prefill::step_speculative`, `relay::send_frame`,
//! `infer_server::forward_direct`, `direct::deliver_result` — e os hops falsos
//! decidem com as funções de produção em pessoa (`should_draft`, `find_draft`,
//! `longest_match`). O único fingimento é o motor: em vez de decodificar, o hop
//! devolve tensores zerados e tokens de um alfabeto fixo.
//!
//! **Limite desta bancada:** sem GGUF e sem llama.cpp nesta máquina, o motor de
//! verdade não entra. O que ela prova é o contrato entre os processos, que é
//! onde o defeito mora; o que ela não prova é o que o llama.cpp faz com o lote.

#![cfg(test)]

use crate::network::PeerInfo;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Pequeno de propósito: a bancada mede protocolo, não memória.
const HIDDEN_DIM: usize = 8;
/// Tokens que o prompt do teste vale para a cabeça falsa.
const PROMPT_TOKENS: u32 = 6;

// ── O hop falso ───────────────────────────────────────────────────────────────

/// Um peer de inferência falso.
#[derive(Clone)]
struct Hop {
    is_head: bool,
    /// Estado por sessão que a bancada precisa: quantos tokens já saíram, para
    /// o "motor" devolver texto diferente a cada rodada e parar no fim.
    emitted: Arc<AtomicU64>,
    /// Quantas vezes `/v1/step` foi servido neste hop — é o `decode.calls` dos
    /// hops, visto de dentro.
    step_calls: Arc<AtomicU64>,
    /// Quantos desses passos vieram com `reply_to`, ou seja pelo caminho da
    /// resposta direta. `step_speculative` nunca manda esse campo, então este
    /// contador é a prova de que a cadeia saiu do anel.
    direct_steps: Arc<AtomicU64>,
}

/// Tokens que o motor falso "amostra", em ordem. Termina antes de acabar para
/// que um teste de `max_tokens` alto ainda feche por EOS.
const ALFABETO: [&str; 8] = ["A", "B", "C", "D", "E", "F", "G", "H"];

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

async fn fake_ok() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

/// A bancada não tem cache de verdade para um rollback poder falhar contra -
/// só o llama.cpp real decide isso (`n_rs_seq`/`seq_rm`, ver o doc do módulo).
/// A forma da resposta importa: `gateway::rollback_sessions` lê o corpo, não
/// só o status HTTP, então `fake_ok` (`{"ok": true}`) leria como recusado.
async fn fake_rollback() -> impl IntoResponse {
    Json(json!({"rolled_back": true}))
}

/// Lido com as mesmas regras do `infer_server` real.
struct Incoming {
    meta: Value,
    inbound_bytes: usize,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
    reply_to: Option<Value>,
}

fn read(body: &Bytes) -> Incoming {
    let (meta, _tensor) = wire::decode(body).expect("quadro válido");
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
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())?;

    let mut bubbled = sent.meta.clone();
    bubbled["hop_timings"] = json!(crate::relay::bubble_hop_timings(&sent));
    bubbled["processing_ms"] = json!(0);
    bubbled["node_timings"] = json!({});
    Ok(bubbled)
}

async fn fake_prefill(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let chunk_index = inc.meta["chunk_index"].as_u64().unwrap_or(0) as u32;
    let chunk_size = inc.meta["chunk_size"].as_u64().unwrap_or(512) as u32;
    let is_final_chunk = inc.meta["is_final_chunk"].as_bool().unwrap_or(false);

    let done_before = chunk_index * chunk_size;
    let n_tokens = if hop.is_head {
        PROMPT_TOKENS.saturating_sub(done_before).min(chunk_size)
    } else {
        chunk_size
            .min(PROMPT_TOKENS.saturating_sub(done_before))
            .max(1)
    } as usize;
    let more = hop.is_head && (done_before + n_tokens as u32) < PROMPT_TOKENS;

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if hop.is_head {
        local["owns_tokens"] = json!(true);
        local["more_chunks"] = json!(more);
        local["n_prompt_tokens"] = json!(PROMPT_TOKENS);
    }
    if inc.is_last {
        local["done"] = json!(false);
        if is_final_chunk {
            // O primeiro token da geração, amostrado pela cauda.
            let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
            local["token_id"] = json!(i as u32);
            local["text"] = json!(ALFABETO[i % ALFABETO.len()]);
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

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
        return crate::infer_server::direct_ack();
    }

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let base = json!({
        "session_id": inc.meta["session_id"].clone(),
        "chunk_index": chunk_index,
        "chunk_size": chunk_size,
        "reuse_prefix": inc.meta["reuse_prefix"].clone(),
        "is_final_chunk": if hop.is_head { !more } else { is_final_chunk },
    });
    match bubble(&inc.next_hops, "/v1/prefill", base, (&residual, shape)).await {
        Ok(mut bubbled) => {
            crate::infer_server::overlay_head_authoritative_fields(&local, &mut bubbled);
            frame(&bubbled, None)
        }
        Err(resp) => resp,
    }
}

/// Mesma árvore de decisão de `step_blocking`, com as funções de produção nos
/// pontos que importam: `should_draft` decide se a cabeça rascunha,
/// `find_draft` monta o rascunho e `longest_match` verifica na cauda.
async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    hop.step_calls.fetch_add(1, Ordering::Relaxed);
    let inc = read(&body);
    if inc.reply_to.is_some() {
        hop.direct_steps.fetch_add(1, Ordering::Relaxed);
    }

    let speculate = crate::infer_server::Speculate::from_meta(&inc.meta);
    let incoming_draft: Option<Vec<i32>> = inc
        .meta
        .get("draft")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // ── Cabeça: decide o rascunho, exatamente como `step_blocking` ───────────
    let mut head_draft: Option<Vec<i32>> = None;
    let mut n_positions = 1usize;
    if hop.is_head {
        // `mtp_ready` é falso nesta bancada: ela move quadros entre processos
        // fingidos, sem motor nenhum, então a única fonte de rascunho que ela
        // pode exercer de verdade é o lookup.
        if crate::infer_server::should_draft(speculate.clone(), false) {
            let spec = speculate.expect("should_draft implica speculate");
            // Histórico fabricado com um padrão que se repete, para
            // `find_draft` ter o que achar.
            let history: Vec<i32> = (0..24).map(|i| i % 4).collect();
            let draft = crate::infer_server::find_draft(&history, spec.m as usize, spec.k);
            n_positions = 1 + draft.len();
            head_draft = Some(draft);
        }
    } else {
        n_positions = incoming_draft.as_ref().map(|d| 1 + d.len()).unwrap_or(1);
    }
    let draft = head_draft.or(incoming_draft);

    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        if let Some(draft) = draft {
            // Verificação: o motor falso "amostra" o alfabeto em ordem, então
            // nenhum rascunho casa — `longest_match` devolve 0 e a rodada
            // confirma um token, que é o pior caso legítimo.
            let mut sampled: Vec<i32> = Vec::new();
            let mut texts: Vec<String> = Vec::new();
            for _ in 0..n_positions {
                let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
                sampled.push(i as i32);
                texts.push(ALFABETO[i % ALFABETO.len()].to_string());
            }
            let keep =
                (crate::infer_server::longest_match(&sampled, &draft) + 1).min(sampled.len());
            // O que passou do aceite não saiu: devolve ao contador.
            hop.emitted
                .fetch_sub((n_positions - keep) as u64, Ordering::Relaxed);
            let accepted: Vec<Value> = sampled[..keep]
                .iter()
                .zip(&texts[..keep])
                .map(|(id, text)| json!({"token_id": id, "text": text}))
                .collect();
            local["done"] = json!(false);
            local["accepted"] = json!(accepted);
            local["accept_count"] = json!(keep as u32);
            local["batch_width"] = json!(n_positions as u32);
        } else {
            // Formato antigo, de um token — é o que o contrato manda quando
            // `draft` está ausente.
            let i = hop.emitted.fetch_add(1, Ordering::Relaxed) as usize;
            local["done"] = json!(false);
            local["token_id"] = json!(i as u32);
            local["text"] = json!(ALFABETO[i % ALFABETO.len()]);
        }
    } else {
        local["done"] = json!(false);
        if let Some(draft) = &draft {
            local["draft"] = json!(draft);
        }
    }

    let residual = vec![0.0f32; n_positions * HIDDEN_DIM];
    let shape = vec![n_positions, HIDDEN_DIM];

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
            let mut forward = json!({
                "session_id": inc.meta["session_id"].clone(),
                "hop_timings": hop_timings,
            });
            if let Some(d) = local.get("draft") {
                forward["draft"] = d.clone();
            }
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
        return crate::infer_server::direct_ack();
    }

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let mut base = json!({"session_id": inc.meta["session_id"].clone()});
    if let Some(d) = local.get("draft") {
        base["draft"] = d.clone();
    }
    match bubble(&inc.next_hops, "/v1/step", base, (&residual, shape)).await {
        Ok(bubbled) => frame(&bubbled, None),
        Err(resp) => resp,
    }
}

async fn spawn_hop(hop: Hop) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/health", get(fake_health))
        .route("/v1/prefill", post(fake_prefill))
        .route("/v1/step", post(fake_step))
        .route("/v1/session/release", post(fake_ok))
        .route("/v1/session/rollback", post(fake_rollback))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── O cenário, montado no caminho do usuário final ────────────────────────────

/// O que o cliente vê no fim de um `generate_stream_with_stops`.
struct Visto {
    texto: String,
    tokens: u32,
    partial: bool,
    timings: Option<crate::gateway::GenerationTimings>,
    /// `decode.calls` de cada hop do trace — o número que o captain leu no
    /// `x_pom` e viu zerado nos dois.
    decode_calls: Vec<u32>,
    /// Quantas vezes cada hop serviu `/v1/step`, visto de dentro dos hops.
    step_calls: Vec<u64>,
    /// Desses, quantos chegaram pelo caminho da resposta direta.
    direct_steps: Vec<u64>,
    falhou: Option<String>,
}

/// Sobe dois hops falsos, um gateway de verdade com a config do captain e roda
/// uma geração pelo mesmo caminho que `/v1/chat/completions` usa.
async fn gerar(direto: bool, max_tokens: u32, n_batch: u32) -> Visto {
    // Com `RUST_LOG=info` a bancada imprime as mesmas linhas que o captain leu
    // no servidor — é assim que se confere que ela reproduz o caso dele, e não
    // um primo.
    static LOG: std::sync::Once = std::sync::Once::new();
    LOG.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });

    let emitted = Arc::new(AtomicU64::new(0));
    let step_calls: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let direct_steps: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();

    let mut urls = Vec::new();
    for i in 0..2 {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                emitted: emitted.clone(),
                step_calls: step_calls[i].clone(),
                direct_steps: direct_steps[i].clone(),
            })
            .await,
        );
    }

    // This fixture exposes only the transports it actually implements. The
    // fake hop serves one wire frame at a time, so advertising frame batching
    // would route the request through BatchCoordinator without a batch-aware
    // fake endpoint and make the fixture claim a capability it cannot honor.
    let mut caps = vec![
        crate::pipeline::CAP_RELAY_CHAIN.to_string(),
        crate::pipeline::CAP_SPECULATE_LOOKUP.to_string(),
    ];
    if direto {
        caps.push(crate::pipeline::CAP_RELAY_DIRECT.to_string());
    }

    let peers: Vec<PeerInfo> = urls
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
    config.n_batch = n_batch;
    state.runtime.seed_config_for_test(config).await;

    // O gateway de verdade, na loopback, para a cauda ter onde entregar a
    // resposta direta. Sem ele o modo direto esperaria o prazo inteiro.
    let state = crate::gateway::testkit::with_self_reachable_gateway(state).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let bg = tokio::spawn(crate::gateway::generate_stream_with_stops(
        crate::gateway::GenerationInput {
            state,
            model: "lab".into(),
            prompt: "um prompt qualquer para a bancada".into(),
            chat_template: true,
            max_tokens,
            stops: Vec::new(),
            cache_key: None,
            request_details: serde_json::json!({}),
        },
        tx,
    ));

    let mut visto = Visto {
        texto: String::new(),
        tokens: 0,
        partial: false,
        timings: None,
        decode_calls: Vec::new(),
        step_calls: Vec::new(),
        direct_steps: Vec::new(),
        falhou: None,
    };
    while let Some(ev) = rx.recv().await {
        match ev {
            crate::gateway::GenEvent::Token(t) => visto.texto.push_str(&t),
            crate::gateway::GenEvent::Done(sum) => {
                visto.tokens = sum.tokens_generated;
                visto.partial = sum.partial;
                visto.timings = Some(sum.timings);
                visto.decode_calls = sum
                    .trace
                    .as_ref()
                    .map(|t| t.hops.iter().map(|h| h.decode.calls).collect())
                    .unwrap_or_default();
            }
            crate::gateway::GenEvent::Failed(e) => visto.falhou = Some(e),
        }
    }
    let _ = bg.await;
    visto.step_calls = step_calls
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    visto.direct_steps = direct_steps
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .collect();
    visto
}

// ── A lei ─────────────────────────────────────────────────────────────────────
//
// As três leis que provavam a recusa-antes-de-rascunhar via `kv_can_shift`
// saíram daqui em 2026-08-16 (ver o doc do módulo): a única recusa que
// `should_draft` ainda faz é MTP sem cabeça `nextn` local e sem rascunho
// pronto, que esta bancada (só lookup, sem MTP) não constrói. O que
// continua valendo — toda invocação de lookup rascunha, direto ou pelo
// anel — está provado abaixo.

/// Quem rascunha continua no anel, que é o único caminho que
/// `step_speculative` conhece — sem resposta direta negociada.
#[tokio::test]
async fn quem_rascunha_no_anel_gera_o_texto_inteiro() {
    let visto = gerar(false, 5, 512).await;

    assert_eq!(visto.falhou, None, "a geração não pode falhar");
    assert_eq!(visto.tokens, 5);
    assert!(!visto.partial);
    assert_eq!(visto.texto, "ABCDE");
    let timings = visto.timings.expect("a resposta deve trazer os tempos");
    assert_eq!(timings.generation_tokens, visto.tokens);
    assert!(timings.ttft_ms >= timings.prefill_ms);
    assert!(timings.prefill_tokens_per_sec.is_some());
    assert!(
        visto.direct_steps.iter().all(|c| *c == 0),
        "sem resposta direta negociada, nenhum passo pode sair por ali: {:?}",
        visto.direct_steps
    );
    assert!(visto.decode_calls.iter().all(|c| *c > 0));
}

/// E com resposta direta negociada: toda rodada especula, então nenhuma
/// pode ser desviada — sem isto o teste anterior passaria por acidente.
#[tokio::test]
async fn quem_rascunha_com_resposta_direta_nao_e_desviado_para_ela() {
    let visto = gerar(true, 5, 512).await;
    assert_eq!(visto.falhou, None);
    assert_eq!(visto.tokens, 5);
    assert!(!visto.partial);
    assert_eq!(visto.texto, "ABCDE");
    let timings = visto.timings.expect("a resposta deve trazer os tempos");
    assert_eq!(timings.generation_tokens, visto.tokens);
    assert!(timings.ttft_ms >= timings.prefill_ms);
    assert!(timings.prefill_tokens_per_sec.is_some());
    assert_eq!(
        visto.direct_steps[0], 0,
        "toda rodada especulou, então nenhuma podia sair pelo caminho direto"
    );
    assert!(visto.decode_calls.iter().all(|c| *c > 0));
}

#[tokio::test]
async fn resposta_de_um_token_mantem_as_taxas_finitas() {
    let visto = gerar(false, 1, 512).await;

    assert_eq!(visto.falhou, None);
    assert_eq!(visto.tokens, 1);
    let timings = visto.timings.expect("a resposta deve trazer os tempos");
    assert_eq!(timings.generation_tokens, 1);
    assert!(timings.total_tokens_per_sec.is_finite());
    assert!(timings.generation_tokens_per_sec.is_none_or(f64::is_finite));
}

/// `n_batch=512` limita apenas a largura de uma rodada no motor, não o
/// orçamento de saída pedido ao gateway.
#[tokio::test]
async fn um_orcamento_acima_de_n_batch_nao_e_truncado() {
    for n_batch in [512, 8] {
        let visto = gerar(false, 2048, n_batch).await;

        assert_eq!(visto.falhou, None, "a geração não pode falhar");
        assert_eq!(visto.tokens, 2048, "n_batch={n_batch} não pode ser o teto");
        assert!(!visto.partial);
        assert_eq!(visto.step_calls, vec![2047, 2047]);
    }
}
