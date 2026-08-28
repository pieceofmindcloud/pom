//! Bancada do laço de continuação: **o teto de uma geração de backend não pode
//! virar o fim da resposta do cliente.**
//!
//! ## O que ela reproduz
//!
//! Medido no cluster vivo em 2026-08-27, `POST /v1/chat/completions` com
//! `max_tokens: 4000` contra uma implantação de `max_new_tokens: 1000`:
//!
//! ```text
//! finish_reason: "length"      usage.completion_tokens: 1000
//! x_pom.output_limits: {requested: 4000, deployment: 1000, effective: 1000}
//! ```
//!
//! O corte era calado: o cliente recebia menos do que pediu e o único lugar
//! onde isso aparecia era o bloco não-padrão `x_pom`, que cliente
//! OpenAI-compatível ignora. Na mesma medição, com uma ferramenta declarada, o
//! corte caiu **dentro** da chamada: a resposta voltou sem `tool_calls`, com o
//! `<tool_call>` inacabado degradado em 4366 caracteres de texto solto. É o
//! caso do captain: o opencode lê `max_tokens` como condição terminal, larga o
//! turno e o arquivo fica pela metade.
//!
//! ## O que ela prova
//!
//! Cada teste sobe dois hops falsos, um `GatewayState` de verdade e serve as
//! rotas `/v1` de verdade num socket TCP, e fala com elas por HTTP com
//! `reqwest` - inclusive o SSE, lido byte a byte como um cliente lê. O caminho
//! exercitado é o de produção inteiro: `openai::chat_completions`,
//! `gateway::generate_stream_with_stops`, `prefill::run`, `prefill::step`,
//! `relay::send_frame`, `tool_call::TagScanner`.
//!
//! **Limite declarado:** o motor é falso. Ele lê um roteiro de pedaços de texto
//! em vez de decodificar, então o que a bancada prova é o contrato entre
//! gateway, adaptador e peers - que é onde o defeito morava. O que o llama.cpp
//! faz com o lote não está aqui; isso é medição em cluster.

#![cfg(test)]

use crate::gateway::testkit;
use crate::wire::{self, TensorShape, WireDtype};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Pequeno de propósito: a bancada mede protocolo, não memória.
const HIDDEN_DIM: usize = 8;
/// Tokens que o prompt vale para a cabeça falsa.
const PROMPT_TOKENS: u32 = 6;

// ── O motor falso ─────────────────────────────────────────────────────────────

/// O roteiro que a cauda falsa "amostra", um pedaço por token.
///
/// Uma chamada de ferramenta inteira, deliberadamente mais longa que o
/// orçamento de uma perna: nenhum pedaço isolado é uma chamada válida, e só a
/// saída costurada fecha as tags. É a forma exata do corte que o captain viu.
fn tool_call_script() -> Vec<String> {
    let mut script = vec![
        "Vou escrever o arquivo.\n".to_string(),
        "<tool_call>\n".to_string(),
        "<function=write_file>\n".to_string(),
        "<parameter=path>\n".to_string(),
        "/tmp/calc.py".to_string(),
        "\n</parameter>\n".to_string(),
        "<parameter=content>\n".to_string(),
    ];
    for linha in 0..40 {
        script.push(format!("linha_{linha} = {linha} * 2\n"));
    }
    script.push("\n</parameter>\n".to_string());
    script.push("</function>\n".to_string());
    script.push("</tool_call>".to_string());
    script
}

/// Texto corrido, também mais longo que uma perna. O `TagScanner` não acha
/// tag nenhuma aqui, então este roteiro mede o caminho de texto puro sem o
/// caminho de ferramenta por perto.
fn plain_script() -> Vec<String> {
    (0..51).map(|i| format!("palavra{i} ")).collect()
}

/// Um roteiro sem fim: o modelo que entra em laço, contra o qual o teto
/// absoluto existe.
fn runaway_script() -> Vec<String> {
    Vec::new()
}

/// Um peer de inferência falso.
#[derive(Clone)]
struct Hop {
    is_head: bool,
    /// Pedaços de texto que a cauda entrega, em ordem. Vazio quer dizer
    /// "nunca para": a cauda repete um pedaço fixo e jamais sinaliza EOS.
    script: Arc<Vec<String>>,
    /// Quantos pedaços já saíram. Compartilhado entre prefill (o primeiro
    /// token) e step (o resto), como no motor de verdade.
    emitted: Arc<AtomicUsize>,
}

impl Hop {
    /// O próximo token do roteiro e se ele foi o último.
    fn next_token(&self) -> (usize, String, bool) {
        let i = self.emitted.fetch_add(1, Ordering::Relaxed);
        if self.script.is_empty() {
            return (i, "e ".to_string(), false);
        }
        let text = self
            .script
            .get(i)
            .cloned()
            .unwrap_or_else(|| String::from(""));
        (i, text, i + 1 >= self.script.len())
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

/// `gateway::rewind_sessions` lê o corpo, não o status: só `rewound: true` com
/// o `n_past` pedido conta como sessão arrumada para o turno seguinte.
async fn fake_rewind(body: Bytes) -> impl IntoResponse {
    let pedido: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let n = pedido.get("n_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    Json(json!({"rewound": true, "n_past": n}))
}

struct Incoming {
    meta: Value,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
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
    Incoming {
        next_hops: declared.unwrap_or_default(),
        is_last,
        meta,
    }
}

/// Espelha `infer_server::relay_and_bubble`: o anel, que é o transporte que
/// esta bancada exercita.
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
            let (id, text, last) = hop.next_token();
            local["token_id"] = json!(id as u32);
            local["text"] = json!(text);
            local["done"] = json!(last);
        }
    }

    let residual = vec![0.0f32; n_tokens * HIDDEN_DIM];
    let shape = vec![n_tokens, HIDDEN_DIM];

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

async fn fake_step(State(hop): State<Hop>, body: Bytes) -> Response {
    let inc = read(&body);
    let mut local = json!({"processing_ms": 0, "node_timings": {}});
    if inc.is_last {
        let (id, text, last) = hop.next_token();
        local["done"] = json!(last);
        local["token_id"] = json!(id as u32);
        local["text"] = json!(text);
    } else {
        local["done"] = json!(false);
    }

    let residual = vec![0.0f32; HIDDEN_DIM];
    let shape = vec![1usize, HIDDEN_DIM];

    if inc.next_hops.is_empty() {
        return if inc.is_last {
            frame(&local, None)
        } else {
            frame(&local, Some((&residual, shape)))
        };
    }
    let base = json!({"session_id": inc.meta["session_id"].clone()});
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
        .route("/v1/session/release", post(fake_release))
        .route("/v1/session/rewind", post(fake_rewind))
        .with_state(hop);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).tcp_nodelay(true).await;
    });
    format!("http://{addr}")
}

// ── O nó, com as rotas `/v1` de verdade num socket ────────────────────────────

const CHAVE: &str = "chave";

struct No {
    base: String,
    /// Vivo enquanto o teste roda; largar isto derruba o nó.
    _servidor: tokio::task::JoinHandle<()>,
}

/// Sobe dois hops falsos e um nó de verdade servindo `/v1` por HTTP.
///
/// `deployment_max_new_tokens` é o teto de **uma** geração de backend - o
/// número do captain, que a tarefa proíbe levantar. `absolute_ceiling` é o
/// `max_output_tokens` do `pom.json`, o freio da requisição inteira.
async fn no(script: Vec<String>, deployment_max_new_tokens: u32, absolute_ceiling: u32) -> No {
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

    let script = Arc::new(script);
    let emitted = Arc::new(AtomicUsize::new(0));
    let mut urls = Vec::new();
    for i in 0..2 {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                script: script.clone(),
                emitted: emitted.clone(),
            })
            .await,
        );
    }

    // Só o anel: é o transporte que os hops falsos implementam, e anunciar
    // outro faria a bancada prometer o que não cumpre.
    let caps = vec![crate::pipeline::CAP_RELAY_CHAIN.to_string()];
    let peers: Vec<crate::network::PeerInfo> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let porta: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = testkit::peer(&format!("hop{i}"), &format!("m{i}"), 9000 + i as u16);
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

    let state = testkit::state_with("eu", peers).await;
    let mut config = testkit::spec_config();
    config.speculative_decoding_enabled = false;
    config.max_new_tokens = deployment_max_new_tokens;
    state.runtime.seed_config_for_test(config).await;
    state.auth.write().await.max_output_tokens = absolute_ceiling;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .merge(crate::openai::routes())
        .with_state(state);
    let servidor = tokio::spawn(async move {
        let _ = axum::serve(listener, app).tcp_nodelay(true).await;
    });

    No {
        base: format!("http://{addr}"),
        _servidor: servidor,
    }
}

fn pedido(max_tokens: u32, stream: bool, com_ferramenta: bool) -> Value {
    let mut body = json!({
        "model": "lab",
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": [{"role": "user", "content": "escreva o arquivo"}],
    });
    if com_ferramenta {
        body["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Escreve um arquivo",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"],
                },
            },
        }]);
    }
    body
}

async fn post_json(no: &No, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(CHAVE)
        .json(&body)
        .send()
        .await
        .expect("resposta HTTP")
        .json()
        .await
        .expect("corpo JSON")
}

/// O que um cliente SSE vê: os quadros na ordem em que chegaram, num único
/// fluxo. Nada aqui reabre conexão - se a costura fechasse o stream, o `while`
/// abaixo terminaria cedo e os testes veriam menos texto.
struct Fluxo {
    /// Cada `data:` do stream, já desembrulhado, exceto `[DONE]`.
    chunks: Vec<Value>,
    /// Quantos `[DONE]` chegaram.
    done_sentinels: usize,
}

impl Fluxo {
    /// Todo `finish_reason` não-nulo que atravessou o fluxo. Mais de um quer
    /// dizer que o cliente viu o fim da resposta antes da hora.
    fn finish_reasons(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["finish_reason"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn texto(&self) -> String {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    /// Os deltas de `tool_calls`, na ordem.
    fn tool_call_deltas(&self) -> Vec<&Value> {
        self.chunks
            .iter()
            .filter(|c| c["choices"][0]["delta"].get("tool_calls").is_some())
            .collect()
    }

    fn ultimo_com_x_pom(&self) -> &Value {
        self.chunks
            .iter()
            .rev()
            .find(|c| c.get("x_pom").is_some())
            .expect("o quadro final carrega x_pom")
    }
}

async fn post_sse(no: &No, body: Value) -> Fluxo {
    let resposta = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(CHAVE)
        .json(&body)
        .send()
        .await
        .expect("resposta HTTP");
    assert!(resposta.status().is_success(), "{:?}", resposta.status());

    let mut bytes = resposta.bytes_stream();
    let mut buffer = String::new();
    let mut fluxo = Fluxo {
        chunks: Vec::new(),
        done_sentinels: 0,
    };
    while let Some(pedaco) = bytes.next().await {
        buffer.push_str(&String::from_utf8_lossy(&pedaco.expect("pedaço SSE")));
        while let Some(fim) = buffer.find("\n\n") {
            let evento: String = buffer.drain(..fim + 2).collect();
            for linha in evento.lines() {
                let Some(dado) = linha.strip_prefix("data: ") else {
                    continue;
                };
                if dado == "[DONE]" {
                    fluxo.done_sentinels += 1;
                } else {
                    fluxo
                        .chunks
                        .push(serde_json::from_str(dado).expect("chunk JSON"));
                }
            }
        }
    }
    fluxo
}

// ── O que a bancada afirma ────────────────────────────────────────────────────

/// A reprodução, virada de cabeça para baixo. Pedindo 100 contra uma
/// implantação de 8, o cliente recebia 8 tokens e `finish_reason: "length"`;
/// agora recebe a resposta inteira e um `stop` honesto - **sem** que o
/// `max_new_tokens` da implantação tenha sido alterado.
#[tokio::test]
async fn a_request_above_the_deployment_ceiling_comes_back_whole() {
    let script = plain_script();
    let esperado: String = script.concat();
    let no = no(script.clone(), 8, 8_192).await;

    let corpo = post_json(&no, pedido(100, false, false)).await;
    let escolha = &corpo["choices"][0];

    assert_eq!(escolha["finish_reason"], "stop");
    assert_eq!(escolha["message"]["content"], esperado);
    assert_eq!(corpo["usage"]["completion_tokens"], script.len() as u64);
    // O número da implantação continua o que era: o laço passou por cima dele
    // sem levantá-lo.
    assert_eq!(corpo["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(corpo["x_pom"]["output_limits"]["leg_max_tokens"], 8);
    assert_eq!(corpo["x_pom"]["output_limits"]["effective_max_tokens"], 100);
    assert_eq!(corpo["x_pom"]["output_limits"]["requested_max_tokens"], 100);
    // E a costura é inspecionável: 51 tokens em pernas de 8.
    assert_eq!(
        corpo["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
    assert_eq!(corpo["x_pom"]["partial"], false);
}

/// O requisito central: um fluxo só. A costura entre pernas não pode aparecer
/// como fim de resposta - nem como `finish_reason` intermediário, nem como um
/// segundo `[DONE]`, nem como conexão fechada no meio.
#[tokio::test]
async fn the_stream_is_one_continuous_flow_with_a_single_final_chunk() {
    let script = plain_script();
    let esperado: String = script.concat();
    let no = no(script.clone(), 8, 8_192).await;

    let fluxo = post_sse(&no, pedido(100, true, false)).await;

    assert_eq!(
        fluxo.finish_reasons(),
        vec!["stop".to_string()],
        "a costura vazou um fim de resposta para o cliente"
    );
    assert_eq!(fluxo.done_sentinels, 1, "mais de um [DONE] é outro fluxo");
    assert_eq!(fluxo.texto(), esperado);
    assert_eq!(
        fluxo.ultimo_com_x_pom()["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
}

/// O caso do captain, ponta a ponta: a chamada de ferramenta atravessa o teto
/// da implantação e chega com o JSON de argumentos inteiro e válido. Nenhum
/// pedaço intermediário é JSON: só a saída costurada fecha as tags.
#[tokio::test]
async fn a_tool_call_that_crosses_the_ceiling_arrives_complete_and_parseable() {
    let script = tool_call_script();
    let no = no(script, 8, 8_192).await;

    let corpo = post_json(&no, pedido(100, false, true)).await;
    let escolha = &corpo["choices"][0];
    assert_eq!(escolha["finish_reason"], "tool_calls");

    let chamadas = escolha["message"]["tool_calls"]
        .as_array()
        .expect("a resposta traz tool_calls");
    assert_eq!(chamadas.len(), 1);
    assert_eq!(chamadas[0]["function"]["name"], "write_file");

    let bruto = chamadas[0]["function"]["arguments"]
        .as_str()
        .expect("arguments é string JSON");
    let argumentos: Value = serde_json::from_str(bruto).expect("arguments é JSON válido");
    assert_eq!(argumentos["path"], "/tmp/calc.py");
    let conteudo = argumentos["content"].as_str().expect("content");
    assert!(conteudo.starts_with("linha_0 = 0 * 2"), "{conteudo:?}");
    assert!(
        conteudo.trim_end().ends_with("linha_39 = 39 * 2"),
        "o conteúdo chegou cortado: {:?}",
        &conteudo[conteudo.len().saturating_sub(60)..]
    );
}

/// A mesma chamada, em streaming: o cliente recebe um `tool_calls` com
/// argumentos completos, não um `<tool_call>` inacabado degradado em texto -
/// que é exatamente o que o cluster devolvia.
#[tokio::test]
async fn the_streamed_tool_call_carries_complete_arguments() {
    let script = tool_call_script();
    let no = no(script, 8, 8_192).await;

    let fluxo = post_sse(&no, pedido(100, true, true)).await;
    assert_eq!(fluxo.finish_reasons(), vec!["tool_calls".to_string()]);

    let deltas = fluxo.tool_call_deltas();
    assert_eq!(deltas.len(), 2, "abertura e argumentos");
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    let bruto = deltas[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments é string JSON");
    let argumentos: Value = serde_json::from_str(bruto).expect("arguments é JSON válido");
    assert_eq!(argumentos["path"], "/tmp/calc.py");
    assert!(argumentos["content"]
        .as_str()
        .unwrap()
        .contains("linha_39 = 39 * 2"));
    // Nada do `<tool_call>` sobrou como texto solto.
    assert!(!fluxo.texto().contains("<tool_call>"), "{}", fluxo.texto());
}

/// O freio obrigatório: sem ele um modelo em laço ocupa o nó para sempre. O
/// teto absoluto é o único que produz `length`, e ele é do `pom.json`, não da
/// implantação.
#[tokio::test]
async fn the_absolute_ceiling_stops_a_runaway_model_and_is_the_only_length() {
    let no = no(runaway_script(), 8, 37).await;

    let corpo = post_json(&no, pedido(100_000, false, false)).await;
    let escolha = &corpo["choices"][0];
    assert_eq!(escolha["finish_reason"], "length");
    assert_eq!(corpo["usage"]["completion_tokens"], 37);
    assert_eq!(corpo["x_pom"]["output_limits"]["absolute_max_tokens"], 37);
    assert_eq!(corpo["x_pom"]["output_limits"]["effective_max_tokens"], 37);
    assert_eq!(corpo["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(corpo["x_pom"]["finish_reason"], "length");
    assert_eq!(corpo["x_pom"]["continuation_legs"], 5);
}

/// O mesmo estouro em streaming: um `length` só, no fim, e um `[DONE]` só.
#[tokio::test]
async fn the_absolute_ceiling_ends_the_stream_exactly_once() {
    let no = no(runaway_script(), 8, 37).await;

    let fluxo = post_sse(&no, pedido(100_000, true, false)).await;
    assert_eq!(fluxo.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(fluxo.done_sentinels, 1);
    assert_eq!(fluxo.texto().matches("e ").count(), 37);
}

/// Quando o pedido cabe numa perna nada muda: uma perna, sem costura, e o
/// `length` continua sendo dito na hora certa.
#[tokio::test]
async fn a_request_that_fits_one_leg_is_untouched() {
    let no = no(runaway_script(), 8, 8_192).await;

    let corpo = post_json(&no, pedido(5, false, false)).await;
    assert_eq!(corpo["choices"][0]["finish_reason"], "length");
    assert_eq!(corpo["usage"]["completion_tokens"], 5);
    assert_eq!(corpo["x_pom"]["continuation_legs"], 1);
    assert_eq!(corpo["x_pom"]["output_limits"]["leg_max_tokens"], 5);
}

/// `/v1/models` anuncia o teto da requisição, não o de uma perna: anunciar o
/// da implantação faria o cliente pedir menos do que este nó entrega.
#[tokio::test]
async fn the_advertised_completion_ceiling_is_the_request_ceiling() {
    let no = no(runaway_script(), 8, 4_096).await;

    let corpo: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(CHAVE)
        .send()
        .await
        .expect("resposta HTTP")
        .json()
        .await
        .expect("corpo JSON");

    let modelo = &corpo["data"][0];
    assert_eq!(modelo["top_provider"]["max_completion_tokens"], 4_096);
}
