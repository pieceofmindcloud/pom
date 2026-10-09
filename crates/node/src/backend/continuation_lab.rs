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
//! ## As duas fronteiras
//!
//! O laço de continuação resolveu o corte na fronteira da **perna**: ela é
//! invisível, e costurar pernas para honrar o pedido do cliente é o certo. A
//! de fora - o teto que o cliente pediu - é outra coisa: ela é **imposta e
//! inviolável**, e atingi-la é parada terminal. Nada a ultrapassa, nem uma
//! chamada de ferramenta aberta no token do corte.
//!
//! O que sobrava a resolver era a **forma** da resposta quando o corte cai
//! dentro de uma chamada. Duas respostas erradas já foram medidas aqui: a
//! marcação crua vazada como texto (335 caracteres de `<tool_call>…` no
//! `content`, com `x_pom.partial: false` mentindo) e o descarte silencioso do
//! fragmento. A certa é a dos dois provedores, verificada na documentação viva
//! em 2026-08-29: a chamada volta **estruturada e incompleta**, ao lado do
//! motivo de parada terminal - `finish_reason: "length"` na OpenAI,
//! `stop_reason: "max_tokens"` com o último bloco `tool_use` na Anthropic. O
//! cliente lê o motivo e repete o pedido com um teto maior; nenhum dos dois
//! continua, transborda ou descarta.
//!
//! No lugar do transbordo os dois oferecem o **orçamento de tarefa**
//! (`output_config.task_budget`), que avisa o modelo em vez de esticar o teto.
//! Ele é advisory e está coberto aqui também.
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
    for line in 0..40 {
        script.push(format!("linha_{line} = {line} * 2\n"));
    }
    script.push("\n</parameter>\n".to_string());
    script.push("</function>\n".to_string());
    script.push("</tool_call>".to_string());
    script
}

/// A forma reduzida da reprodução do captain: 847 linhas de HTML, quebradas
/// em pedaços pequenos como um tokenizer real faria. O número de linhas é
/// deliberado - a bancada precisa atravessar tanto 8192 quanto 50000 tokens
/// sem confundir tamanho do arquivo com a capacidade de contexto do nó.
fn large_html_tool_script() -> Vec<String> {
    let mut script = vec![
        "<tool_call>\n".to_string(),
        "<function=write_file>\n".to_string(),
        "<parameter=path>\n".to_string(),
        "/tmp/index.html\n".to_string(),
        "</parameter>\n".to_string(),
        "<parameter=content>\n".to_string(),
    ];
    let html: String = (0..847)
        .map(|line| format!("<div data-line=\"{line}\">conteudo da linha {line}</div>\n"))
        .collect();
    script.extend(
        html.as_bytes()
            .chunks(5)
            .map(|chunk| String::from_utf8(chunk.to_vec()).expect("HTML ASCII")),
    );
    script.push("</parameter>\n".to_string());
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
    /// O prompt que a cabeça recebeu no quadro de prefill. É por aqui que a
    /// bancada confere que o marcador do orçamento de tarefa chegou mesmo à
    /// geração, e não só ao adaptador.
    seen_prompt: Arc<std::sync::Mutex<String>>,
    /// Reutilizado entre tokens para que a reprodução longa meça o gateway,
    /// não a exaustão de sockets causada por criar um cliente por passo.
    peer_http: Arc<crate::peer_client::PeerClients>,
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
    let request: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let n = request.get("n_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    Json(json!({"rewound": true, "n_past": n}))
}

struct Incoming {
    meta: Value,
    next_hops: Vec<crate::pipeline::PipelineStep>,
    is_last: bool,
}

fn read(body: &Bytes) -> Incoming {
    let (meta, _tensor) = wire::decode(body).expect("valid frame");
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
    http: &crate::peer_client::PeerClients,
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
        http,
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
    if hop.is_head {
        if let Some(prompt) = inc.meta["prompt"].as_str() {
            *hop.seen_prompt.lock().unwrap() = prompt.to_string();
        }
    }
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
    match bubble(
        &hop.peer_http,
        &inc.next_hops,
        "/v1/prefill",
        base,
        (&residual, shape),
    )
    .await
    {
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
    match bubble(
        &hop.peer_http,
        &inc.next_hops,
        "/v1/step",
        base,
        (&residual, shape),
    )
    .await
    {
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

const API_KEY: &str = "api-chave";

struct No {
    base: String,
    /// O prompt que a cabeça recebeu - ver `Hop::seen_prompt`.
    seen_prompt: Arc<std::sync::Mutex<String>>,
    /// Vivo enquanto o teste roda; largar isto derruba o nó.
    _server: tokio::task::JoinHandle<()>,
}

impl No {
    fn generation_prompt(&self) -> String {
        self.seen_prompt.lock().unwrap().clone()
    }
}

/// Sobe dois hops falsos e um nó de verdade servindo `/v1` por HTTP.
///
/// `deployment_max_new_tokens` é o teto de **uma** geração de backend. A
/// capacidade total deriva do contexto efetivo, sem um teto absoluto separado.
async fn no(script: Vec<String>, deployment_max_new_tokens: u32) -> No {
    no_with_context(script, deployment_max_new_tokens, 8_192).await
}

async fn no_with_context(script: Vec<String>, deployment_max_new_tokens: u32, n_ctx: u32) -> No {
    no_with_peer_capacities(script, deployment_max_new_tokens, n_ctx, Vec::new()).await
}

async fn no_with_peer_capacities(
    script: Vec<String>,
    deployment_max_new_tokens: u32,
    n_ctx: u32,
    capacities: Vec<crate::network::RuntimeCapacity>,
) -> No {
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
    let seen_prompt = Arc::new(std::sync::Mutex::new(String::new()));
    let peer_http = Arc::new(crate::peer_client::PeerClients::new(
        std::time::Duration::from_secs(600),
    ));
    let mut urls = Vec::new();
    for i in 0..2 {
        urls.push(
            spawn_hop(Hop {
                is_head: i == 0,
                script: script.clone(),
                emitted: emitted.clone(),
                seen_prompt: seen_prompt.clone(),
                peer_http: peer_http.clone(),
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
            let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
            let mut p = testkit::peer(&format!("hop{i}"), &format!("m{i}"), 9000 + i as u16);
            p.addr = "127.0.0.1:42777".into();
            p.inference_port = Some(port);
            p.model = "lab".into();
            p.start_layer = i as u32;
            p.end_layer = i as u32;
            p.total_layers = 2;
            p.inference_frame_capabilities = caps.clone();
            p.runtime_capacity = Some(capacities.get(i).copied().unwrap_or(
                crate::network::RuntimeCapacity {
                    n_ctx,
                    max_new_tokens: deployment_max_new_tokens,
                },
            ));
            p
        })
        .collect();

    let state = testkit::state_with("eu", peers).await;
    let mut config = testkit::spec_config();
    config.speculative_decoding_enabled = false;
    config.max_new_tokens = deployment_max_new_tokens;
    config.n_ctx = n_ctx;
    state.runtime.seed_config_for_test(config).await;
    let api_key = crate::config::ApiKey {
        id: "claude-code-contract".into(),
        key_hash: crate::auth::api_key_hash(API_KEY),
        key_hint: String::new(),
        label: "claude-code".into(),
        node_id: "eu".into(),
        origin_node_id: "eu".into(),
        active: true,
        created_at: String::new(),
    };
    state.database.insert_api_key(&api_key).unwrap();
    state.auth.write().await.api_keys.push(api_key);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .merge(crate::openai::routes())
        .merge(crate::messages::routes())
        .with_state(state);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).tcp_nodelay(true).await;
    });

    No {
        base: format!("http://{addr}"),
        seen_prompt,
        _server: server,
    }
}

fn request(max_tokens: u32, stream: bool, with_tool: bool) -> Value {
    let mut body = json!({
        "model": "lab",
        "max_tokens": max_tokens,
        "stream": stream,
        "messages": [{"role": "user", "content": "escreva o arquivo"}],
    });
    if with_tool {
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
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body")
}

/// `/v1/messages`, para o que só o adaptador Anthropic diz: o `stop_reason`,
/// os blocos `tool_use` e o `output_config`.
async fn post_messages(no: &No, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body")
}

async fn messages_events(no: &No, body: Value) -> Vec<Value> {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response");
    assert!(response.status().is_success(), "{:?}", response.status());

    let mut bytes = response.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = bytes.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("SSE chunk")));
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            for line in event.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload != "[DONE]" {
                    events.push(serde_json::from_str(payload).expect("JSON event"));
                }
            }
        }
    }
    events
}

/// O SSE da rota Anthropic, que tem nomes de evento próprios. Devolve os
/// `partial_json` de `input_json_delta`, na ordem - é onde os argumentos da
/// chamada de ferramenta chegam ali.
async fn messages_fragments(no: &No, body: Value) -> Vec<String> {
    let events = messages_events(no, body).await;
    let mut fragments = Vec::new();
    for v in events {
        if v["delta"]["type"] == "input_json_delta" {
            fragments.push(
                v["delta"]["partial_json"]
                    .as_str()
                    .expect("partial_json")
                    .to_string(),
            );
        }
    }
    fragments
}

/// O pedido Anthropic da bancada, com a ferramenta declarada. `eager` liga
/// `eager_input_streaming` nela.
fn messages_request(max_tokens: u32, eager: Option<bool>) -> Value {
    let mut tool = json!({
        "name": "write_file",
        "description": "Escreve um arquivo",
        "input_schema": {"type": "object", "properties": {}},
    });
    if let Some(v) = eager {
        tool["eager_input_streaming"] = json!(v);
    }
    json!({
        "model": "lab",
        "max_tokens": max_tokens,
        "stream": true,
        "messages": [{"role": "user", "content": "escreva o arquivo"}],
        "tools": [tool],
    })
}

/// O que um cliente SSE vê: os quadros na ordem em que chegaram, num único
/// fluxo. Nada aqui reabre conexão - se a costura fechasse o stream, o `while`
/// abaixo terminaria cedo e os testes veriam menos texto.
struct Flow {
    /// Cada `data:` do stream, já desembrulhado, exceto `[DONE]`.
    chunks: Vec<Value>,
    /// Quantos `[DONE]` chegaram.
    done_sentinels: usize,
    /// Em que instante cada quadro chegou, contado do começo do pedido. É o
    /// que separa "chegou em vários pedaços" de "chegou cedo".
    instants: Vec<std::time::Duration>,
}

impl Flow {
    /// Todo `finish_reason` não-nulo que atravessou o fluxo. Mais de um quer
    /// dizer que o cliente viu o fim da resposta antes da hora.
    fn finish_reasons(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["finish_reason"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn text(&self) -> String {
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

    /// Os fragmentos de argumento, na ordem, com o instante de cada um.
    fn argument_fragments(&self) -> Vec<(std::time::Duration, String)> {
        self.chunks
            .iter()
            .zip(&self.instants)
            .filter_map(|(c, t)| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| (*t, s.to_string()))
            })
            .collect()
    }

    fn last_with_x_pom(&self) -> &Value {
        self.chunks
            .iter()
            .rev()
            .find(|c| c.get("x_pom").is_some())
            .expect("the final frame carries x_pom")
    }
}

async fn post_sse(no: &No, body: Value) -> Flow {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&body)
        .send()
        .await
        .expect("HTTP response");
    assert!(response.status().is_success(), "{:?}", response.status());

    let start = std::time::Instant::now();
    let mut bytes = response.bytes_stream();
    let mut buffer = String::new();
    let mut flow = Flow {
        chunks: Vec::new(),
        done_sentinels: 0,
        instants: Vec::new(),
    };
    while let Some(chunk) = bytes.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("SSE chunk")));
        let now = start.elapsed();
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            for line in event.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload == "[DONE]" {
                    flow.done_sentinels += 1;
                } else {
                    flow
                        .chunks
                        .push(serde_json::from_str(payload).expect("chunk JSON"));
                    flow.instants.push(now);
                }
            }
        }
    }
    flow
}

// ── O que a bancada afirma ────────────────────────────────────────────────────

/// A reprodução, virada de cabeça para baixo. Pedindo 100 contra uma
/// implantação de 8, o cliente recebia 8 tokens e `finish_reason: "length"`;
/// agora recebe a resposta inteira e um `stop` honesto - **sem** que o
/// `max_new_tokens` da implantação tenha sido alterado.
#[tokio::test]
async fn a_request_above_the_deployment_ceiling_comes_back_whole() {
    let script = plain_script();
    let expected: String = script.concat();
    let no = no(script.clone(), 8).await;

    let body = post_json(&no, request(100, false, false)).await;
    let choice = &body["choices"][0];

    assert_eq!(choice["finish_reason"], "stop");
    assert_eq!(choice["message"]["content"], expected);
    assert_eq!(body["usage"]["completion_tokens"], script.len() as u64);
    // O número da implantação continua o que era: o laço passou por cima dele
    // sem levantá-lo.
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(body["x_pom"]["output_limits"]["leg_max_tokens"], 8);
    assert_eq!(body["x_pom"]["output_limits"]["effective_max_tokens"], 100);
    assert_eq!(body["x_pom"]["output_limits"]["requested_max_tokens"], 100);
    // E a costura é inspecionável: 51 tokens em pernas de 8.
    assert_eq!(
        body["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
    assert_eq!(body["x_pom"]["partial"], false);
}

/// O requisito central: um fluxo só. A costura entre pernas não pode aparecer
/// como fim de resposta - nem como `finish_reason` intermediário, nem como um
/// segundo `[DONE]`, nem como conexão fechada no meio.
#[tokio::test]
async fn the_stream_is_one_continuous_flow_with_a_single_final_chunk() {
    let script = plain_script();
    let expected: String = script.concat();
    let no = no(script.clone(), 8).await;

    let flow = post_sse(&no, request(100, true, false)).await;

    assert_eq!(
        flow.finish_reasons(),
        vec!["stop".to_string()],
        "the seam leaked an end of response to the client"
    );
    assert_eq!(flow.done_sentinels, 1, "more than one [DONE] is another stream");
    assert_eq!(flow.text(), expected);
    assert_eq!(
        flow.last_with_x_pom()["x_pom"]["continuation_legs"],
        script.len().div_ceil(8) as u64
    );
}

/// O caso do captain, ponta a ponta: a chamada de ferramenta atravessa o teto
/// da implantação e chega com o JSON de argumentos inteiro e válido. Nenhum
/// pedaço intermediário é JSON: só a saída costurada fecha as tags.
#[tokio::test]
async fn a_tool_call_that_crosses_the_ceiling_arrives_complete_and_parseable() {
    let script = tool_call_script();
    let no = no(script, 8).await;

    let body = post_json(&no, request(100, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");

    let calls = choice["message"]["tool_calls"]
        .as_array()
        .expect("the response carries tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "write_file");

    let raw = calls[0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("arguments is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    let content = arguments["content"].as_str().expect("content");
    assert!(content.starts_with("linha_0 = 0 * 2"), "{content:?}");
    assert!(
        content.trim_end().ends_with("linha_39 = 39 * 2"),
        "the content arrived cut: {:?}",
        &content[content.len().saturating_sub(60)..]
    );
}

/// A mesma chamada, em streaming: o cliente recebe um `tool_calls` com
/// argumentos completos, não um `<tool_call>` inacabado degradado em texto -
/// que é exatamente o que o cluster devolvia.
#[tokio::test]
async fn the_streamed_tool_call_carries_complete_arguments() {
    let script = tool_call_script();
    let no = no(script, 8).await;

    let flow = post_sse(&no, request(100, true, true)).await;
    assert_eq!(flow.finish_reasons(), vec!["tool_calls".to_string()]);

    let deltas = flow.tool_call_deltas();
    // A abertura traz o nome, e os argumentos vêm em MUITOS fragmentos.
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    let fragments = flow.argument_fragments();
    assert!(
        fragments.len() > 5,
        "the arguments came back held up: {} fragment(s)",
        fragments.len()
    );

    // Concatenar reproduz o argumento inteiro e válido.
    let whole: String = fragments.iter().map(|(_, s)| s.as_str()).collect();
    let arguments: Value = serde_json::from_str(&whole).expect("arguments is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    assert!(arguments["content"]
        .as_str()
        .unwrap()
        .ends_with("linha_39 = 39 * 2"));
    // Nada do `<tool_call>` sobrou como texto solto.
    assert!(!flow.text().contains("<tool_call>"), "{}", flow.text());
}

/// A reprodução grande do caso do captain: com contexto efetivo suficiente,
/// as 847 linhas cabem e a chamada chega completa mesmo atravessando várias
/// pernas de 1024 tokens.
#[tokio::test]
async fn a_large_html_tool_call_fits_the_context_capacity() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(50_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(body["x_pom"]["partial"], false);

    let raw = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("a complete call is JSON");
    assert_eq!(arguments["path"], "/tmp/index.html");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
}

/// O cenário do captain com a capacidade que o deployment pretende publicar:
/// 32000 tokens de uma requisição em contexto efetivo de 100000. O HTML não
/// pode sumir apenas porque atravessou várias pernas de 1024 tokens.
#[tokio::test]
async fn a_large_html_tool_call_at_32000_and_context_100k_is_complete() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;

    let body = post_json(&no, request(32_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert!(
        body["usage"]["completion_tokens"]
            .as_u64()
            .expect("completion_tokens")
            < 32_000
    );
    assert_eq!(body["x_pom"]["partial"], false);

    let raw = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    let arguments: Value = serde_json::from_str(raw).expect("a complete call is JSON");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
}

/// E o mesmo limite é verificável no caminho incremental que o Pi consome:
/// concatenar os fragmentos de argumentos ainda produz a chamada inteira.
#[tokio::test]
async fn a_large_html_tool_call_at_32000_streams_complete_arguments() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;

    let flow = post_sse(&no, request(32_000, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["tool_calls".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    let arguments: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, fragment)| fragment.as_str())
        .collect();
    let arguments: Value = serde_json::from_str(&arguments).expect("arguments is JSON");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));
    assert_eq!(flow.last_with_x_pom()["x_pom"]["partial"], false);
}

/// A mesma reprodução em 8192 tokens não pode ser "resolvida" com folga:
/// o cliente recebe exatamente o teto, a chamada volta estruturada e os
/// argumentos ficam deliberadamente incompletos para não serem executados.
#[tokio::test]
async fn a_large_openai_tool_call_at_8192_is_structured_and_terminal() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(8_192, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 8_192);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);

    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "write_file");
    let arguments = call["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    assert!(!arguments.contains("<tool_call>"), "raw markup leaked");
    assert!(serde_json::from_str::<Value>(arguments).is_err());
}

#[tokio::test]
async fn a_large_openai_tool_call_at_5000_is_structured_and_terminal_non_streaming() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let body = post_json(&no, request(5_000, false, true)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5_000);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert!(!body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .contains("<tool_call>"));

    let arguments = choice["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    assert!(serde_json::from_str::<Value>(arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));
}

/// A reprodução em streaming do write grande do Pi: quando o teto terminal
/// corta o parâmetro HTML, o cliente ainda recebe um `tool_calls` estruturado
/// e pode guardar o prefixo recuperável para uma nova tentativa. O fluxo não
/// pode fechar sem argumentos nem despejar `<tool_call>` em `content`.
#[tokio::test]
async fn a_large_openai_tool_call_cut_in_stream_is_recoverable() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let flow = post_sse(&no, request(8_192, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert!(!flow.text().contains("<tool_call>"), "{}", flow.text());

    let deltas = flow.tool_call_deltas();
    assert!(!deltas.is_empty(), "the cut lost the structured call");
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    let arguments: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, fragment)| fragment.as_str())
        .collect();
    assert!(arguments.starts_with(r#"{"path":"/tmp/index.html","content":"#));
    assert!(serde_json::from_str::<Value>(&arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));

    let x_pom = &flow.last_with_x_pom()["x_pom"];
    assert_eq!(x_pom["truncated_tool_call"], true);
    assert_eq!(x_pom["partial"], true);
}

/// O mesmo caminho com o limite local de 5000 tokens reproduz o corte visto
/// pelo Pi: não existe uma continuação escondida depois do evento terminal.
#[tokio::test]
async fn a_large_openai_tool_call_at_5000_has_one_recoverable_terminal_cut() {
    let no = no_with_context(large_html_tool_script(), 1_024, 65_536).await;

    let flow = post_sse(&no, request(5_000, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert!(!flow.text().contains("<tool_call>"));
    assert!(!flow.argument_fragments().is_empty());
    assert!(!flow
        .argument_fragments()
        .iter()
        .any(|(_, fragment)| fragment.contains("data-line=\"846\"")));
    assert_eq!(flow.last_with_x_pom()["x_pom"]["partial"], true);
}

/// O adaptador Anthropic expõe a mesma chamada grande como bloco `tool_use`:
/// completa quando cabe, e incompleta com `stop_reason: max_tokens` quando o
/// teto de 8192 cai dentro do parâmetro.
#[tokio::test]
async fn a_large_anthropic_tool_call_respects_50000_and_8192_separately() {
    let tool = json!({
        "name": "write_file",
        "description": "Escreve um arquivo",
        "input_schema": {"type": "object", "properties": {}},
    });

    let complete = no_with_context(large_html_tool_script(), 1_024, 65_536).await;
    let body = post_messages(
        &complete,
        json!({
            "model": "lab",
            "max_tokens": 50_000,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [tool.clone()],
        }),
    )
    .await;
    assert_eq!(body["stop_reason"], "tool_use");
    assert!(body["content"][0]["input"]["content"]
        .as_str()
        .expect("content")
        .contains("data-line=\"846\""));

    let truncated = no_with_context(large_html_tool_script(), 1_024, 65_536).await;
    let body = post_messages(
        &truncated,
        json!({
            "model": "lab",
            "max_tokens": 8_192,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [tool],
        }),
    )
    .await;
    assert_eq!(body["stop_reason"], "max_tokens");
    assert_eq!(body["usage"]["output_tokens"], 8_192);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["content"][0]["type"], "tool_use");
    let content = body["content"][0]["input"]["content"]
        .as_str()
        .expect("content");
    assert!(!content.contains("data-line=\"846\""));
}

/// Reproduz o limite configurado pelo Pi na mesma rota que o Claude Code usa:
/// `max_tokens: 5000` chega inteiro ao gateway, corta exatamente nesse total e
/// ainda fecha o stream com um `tool_use` estruturado. O contexto de 100k não
/// aumenta o teto de saída do cliente.
#[tokio::test]
async fn pi_5000_limit_is_forwarded_and_large_write_is_structured_when_cut() {
    let no = no_with_context(large_html_tool_script(), 1_024, 100_000).await;
    let events = messages_events(&no, messages_request(5_000, Some(false))).await;

    let delta = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .expect("message_delta final");
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    assert_eq!(delta["usage"]["output_tokens"], 5_000);
    assert_eq!(
        delta["x_pom"]["output_limits"]["requested_max_tokens"],
        5_000
    );
    assert_eq!(
        delta["x_pom"]["output_limits"]["effective_max_tokens"],
        5_000
    );
    assert_eq!(delta["x_pom"]["partial"], true);
    assert_eq!(delta["x_pom"]["truncated_tool_call"], true);

    let tool_start = events
        .iter()
        .find(|event| event["content_block"]["type"] == "tool_use")
        .expect("content_block_start do tool_use");
    assert_eq!(tool_start["content_block"]["name"], "write_file");

    let arguments: String = events
        .iter()
        .filter(|event| event["delta"]["type"] == "input_json_delta")
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert!(arguments.starts_with(r#"{"path":"/tmp/index.html","content":"#));
    assert!(serde_json::from_str::<Value>(&arguments).is_err());
    assert!(!arguments.contains("data-line=\"846\""));
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_stop")
            .count(),
        1
    );
}

/// **O defeito do captain, virado do avesso.** Medido no ksia vivo: 2 pedaços,
/// os dois aos 11,01 s - os 663 caracteres chegavam todos de uma vez, no fim,
/// e o usuário encarava uma tela parada. Agora o primeiro fragmento chega
/// muito antes do último.
#[tokio::test]
async fn the_first_argument_fragment_arrives_well_before_the_last() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(100, true, true)).await;
    let fragments = flow.argument_fragments();

    assert!(fragments.len() > 5, "{} fragmento(s)", fragments.len());
    let first = fragments.first().expect("first").0;
    let last = fragments.last().expect("last").0;
    assert!(
        first < last,
        "first and last at the same instant ({first:?}) - it is the defect again"
    );
    // O relógio de parede deste teste inclui escalonamento do processo, filas
    // do kernel e agrupamento de bytes pelo socket. Esses fatores podem fazer
    // o primeiro quadro parecer tardio mesmo quando o servidor já o emitiu
    // separado. A prova estável de que o streaming não represou a chamada é
    // haver muitos fragmentos e o primeiro ser observado antes do último.
    assert!(
        first < last,
        "the first frame did not arrive before the last"
    );
}

/// O freio contra o modelo em laço: o teto que o cliente pediu é o único que
/// produz `length`, e a implantação (8) não encerra nada - ela só divide a
/// resposta em pernas invisíveis, cinco delas aqui.
#[tokio::test]
async fn the_client_ceiling_stops_a_runaway_model_and_is_the_only_length() {
    let no = no(runaway_script(), 8).await;

    let body = post_json(&no, request(37, false, false)).await;
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 37);
    assert_eq!(body["x_pom"]["output_limits"]["effective_max_tokens"], 37);
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    assert_eq!(body["x_pom"]["finish_reason"], "length");
    assert_eq!(body["x_pom"]["continuation_legs"], 5);
}

/// O mesmo estouro em streaming: um `length` só, no fim, e um `[DONE]` só. A
/// costura das cinco pernas segue invisível.
#[tokio::test]
async fn the_client_ceiling_ends_the_stream_exactly_once() {
    let no = no(runaway_script(), 8).await;

    let flow = post_sse(&no, request(37, true, false)).await;
    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert_eq!(flow.text().matches("e ").count(), 37);
    assert_eq!(flow.last_with_x_pom()["x_pom"]["continuation_legs"], 5);
}

/// Quando o pedido cabe numa perna nada muda: uma perna, sem costura, e o
/// `length` continua sendo dito na hora certa.
#[tokio::test]
async fn a_request_that_fits_one_leg_is_untouched() {
    let no = no(runaway_script(), 8).await;

    let body = post_json(&no, request(5, false, false)).await;
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_eq!(body["x_pom"]["continuation_legs"], 1);
    assert_eq!(body["x_pom"]["output_limits"]["leg_max_tokens"], 5);
}

/// `/v1/models` anuncia o teto da requisição, não o de uma perna: anunciar o
/// da implantação faria o cliente pedir menos do que este nó entrega.
#[tokio::test]
async fn the_advertised_completion_ceiling_is_the_request_ceiling() {
    let no = no(runaway_script(), 8).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");

    let model = &body["data"][0];
    assert_eq!(model["top_provider"]["max_completion_tokens"], 8_192);
}

/// Anthropic SDKs can send both authentication headers. A bad Bearer header
/// must not mask a valid `x-api-key`, and the same rule applies to both API
/// surfaces. Two invalid credentials still remain a 403.
#[tokio::test]
async fn either_valid_auth_header_is_accepted_on_models_and_messages() {
    let no = no(plain_script(), 8).await;
    let client = reqwest::Client::new();

    let models = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(models.status(), reqwest::StatusCode::OK);

    let messages = client
        .post(format!("{}/v1/messages", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", API_KEY)
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
        }))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(messages.status(), reqwest::StatusCode::OK);

    let both_invalid = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", "invalid-api-key")
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(both_invalid.status(), reqwest::StatusCode::FORBIDDEN);

    let messages_invalid = client
        .post(format!("{}/v1/messages", no.base))
        .header("authorization", "Bearer invalid-bearer")
        .header("x-api-key", "invalid-api-key")
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
        }))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(messages_invalid.status(), reqwest::StatusCode::FORBIDDEN);

    let bearer = client
        .get(format!("{}/v1/models", no.base))
        .header("authorization", format!("Bearer {API_KEY}"))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(bearer.status(), reqwest::StatusCode::OK);

    let x_api_key = client
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(x_api_key.status(), reqwest::StatusCode::OK);
}

/// A lista precisa ser autodetectável por Claude Code e OpenCode ao mesmo
/// tempo. Os campos Anthropic extras são ignorados pelos clientes OpenAI.
#[tokio::test]
async fn model_list_is_authenticated_and_advertises_the_live_100k_context() {
    let no = no_with_context(runaway_script(), 8, 100_000).await;

    let body: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(body["object"], "list");
    assert_eq!(body["has_more"], false);
    let model = &body["data"][0];
    assert_eq!(model["id"], "lab");
    assert_eq!(model["type"], "model");
    assert_eq!(model["max_input_tokens"], 100_000);
    assert_eq!(model["max_tokens"], 100_000);
    assert_eq!(model["context_length"], 100_000);
    assert_eq!(model["top_provider"]["max_completion_tokens"], 100_000);
}

/// A capacidade derivada da rede é a capacidade do pipeline inteiro: um peer
/// com contexto 8192 não pode ser escondido por outro com contexto 100000.
/// A lista usa o menor valor entre os peers da pipeline planejada.
#[tokio::test]
async fn deployment_context_capacity_is_aggregated_across_all_live_peers() {
    let no = no_with_peer_capacities(
        plain_script(),
        32_000,
        100_000,
        vec![
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
            crate::network::RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 8_192,
            },
        ],
    )
    .await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 8_192);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        8_192
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    let status = response.status();
    let body = response.text().await.expect("error body");
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
}

/// Depois que todos os peers reiniciam com a nova configuração, o gateway
/// deixa de anunciar a capacidade antiga e aceita 32000 sem confundi-la com
/// o teto por perna.
#[tokio::test]
async fn deployment_capacity_updates_only_after_all_peers_report_ready() {
    let no = no_with_peer_capacities(
        plain_script(),
        32_000,
        100_000,
        vec![
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
            crate::network::RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            },
        ],
    )
    .await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 100_000);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        100_000
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("x-api-key", API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

/// Reproduz a configuração do captain: `max_new_tokens: 32000` continua sendo
/// uma perna, enquanto a capacidade publicada vem do contexto efetivo do
/// modelo e da rede, sem um segundo teto absoluto escondido.
#[tokio::test]
async fn a_32000_deployment_leg_uses_the_context_derived_capacity() {
    let no = no_with_context(plain_script(), 32_000, 100_000).await;

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", no.base))
        .bearer_auth(API_KEY)
        .send()
        .await
        .expect("HTTP response")
        .json()
        .await
        .expect("JSON body");
    assert_eq!(models["data"][0]["context_length"], 100_000);
    assert_eq!(
        models["data"][0]["top_provider"]["max_completion_tokens"],
        100_000
    );

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(32_000, false, false))
        .send()
        .await
        .expect("HTTP response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

/// OpenCode envia o nome novo do limite e pode acrescentar campos opcionais
/// do protocolo OpenAI. O gateway deve honrar `max_completion_tokens` sem
/// exigir que o cliente também mande o nome antigo.
#[tokio::test]
async fn openai_chat_completions_accepts_max_completion_tokens() {
    let no = no(runaway_script(), 8).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("authorization", format!("Bearer {API_KEY}"))
        .json(&json!({
            "model": "lab",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "oi"}]}],
            "max_completion_tokens": 5,
            "stream": false,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "response_format": {"type": "text"},
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["usage"]["completion_tokens"], 5);
    assert_eq!(body["x_pom"]["partial"], false);
}

#[tokio::test]
async fn max_completion_tokens_above_the_former_50k_ceiling_is_accepted() {
    let no = no_with_context(plain_script(), 8, 100_000).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "model": "lab",
            "messages": [{"role": "user", "content": "oi"}],
            "max_completion_tokens": 50_001,
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

// ── O teto do CLIENTE caindo dentro da chamada ───────────────────────────────

/// Teto escolhido para cair **dentro** da chamada de ferramenta.
///
/// `tool_call_script()` tem 50 pedaços: índice 0 é prosa, 1 abre
/// `<tool_call>`, 7..=46 são as 40 linhas do `content`, 49 fecha
/// `</tool_call>`. Com 20, a geração chega ao teto logo depois do pedaço 19 -
/// `linha_12` - dezoito pedaços depois da abertura e vinte e nove antes do
/// fechamento. `the_cut_lands_inside_the_call()` prova isso, para ninguém depois
/// achar que o ponto caiu em outro lugar por acidente.
const CAP_INSIDE_CALL: u32 = 20;

/// Prova aritmética do ponto de corte, independente dos testes de HTTP: com
/// `CAP_INSIDE_CALL` tokens gerados, a saída bruta abriu a chamada e
/// não a fechou.
#[test]
fn the_cut_lands_inside_the_call() {
    let script = tool_call_script();
    let truncated: String = script[..CAP_INSIDE_CALL as usize].concat();

    assert!(
        (CAP_INSIDE_CALL as usize) < script.len(),
        "the cap must be lower than the script, otherwise it cuts nothing"
    );
    assert!(
        truncated.contains("<tool_call>"),
        "the cut landed before the opening"
    );
    assert!(
        !truncated.contains("</tool_call>"),
        "the cut landed after the closing"
    );
    // E cai no miolo do `content`, não numa fronteira de tag.
    assert!(truncated.ends_with("linha_12 = 12 * 2\n"), "{truncated:?}");
}

/// **O teto do cliente é inviolável, inclusive com uma chamada aberta.**
///
/// Era aqui que uma folga removida deixava a geração ultrapassar o que o
/// cliente pediu. Nenhum dos dois provedores faz isso:
/// `max_tokens` é teto por resposta imposto, atingi-lo é parada terminal, e a
/// saída trunca no meio. O que eles oferecem no lugar do transbordo é o
/// orçamento de tarefa, coberto mais abaixo.
#[tokio::test]
async fn the_client_ceiling_is_never_exceeded_even_with_an_open_tool_call() {
    let no = no(tool_call_script(), 8).await;

    let body = post_json(&no, request(CAP_INSIDE_CALL, false, true)).await;

    assert_eq!(
        body["usage"]["completion_tokens"], CAP_INSIDE_CALL,
        "the generation went past the cap the client asked for"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "length");
}

/// O mesmo em streaming, e com a garantia que só o streaming dá: nem um token
/// a mais atravessou o fluxo.
#[tokio::test]
async fn the_streamed_client_ceiling_is_never_exceeded_with_an_open_tool_call() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(CAP_INSIDE_CALL, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);
    assert_eq!(
        flow.last_with_x_pom()["x_pom"]["tokens_generated"],
        CAP_INSIDE_CALL
    );
}

/// **A forma da resposta quando o corte cai dentro da chamada** (OpenAI).
///
/// Apurado na documentação viva em 2026-08-29: a chamada parcial volta
/// **estruturada**, com `function.arguments` sendo a string JSON inacabada, ao
/// lado de `finish_reason: "length"`. `arguments` é documentado como string
/// que "nem sempre é JSON válido", e o cliente é obrigado a validar antes de
/// executar - é isso que impede um harness de escrever meio arquivo.
///
/// As duas respostas erradas que já foram medidas aqui estão asseridas contra:
/// marcação crua vazada como texto, e o descarte silencioso do fragmento.
#[tokio::test]
async fn a_tool_call_cut_by_the_ceiling_comes_back_structured_and_incomplete() {
    let no = no(tool_call_script(), 8).await;

    let body = post_json(&no, request(CAP_INSIDE_CALL, false, true)).await;
    let choice = &body["choices"][0];

    // Parada terminal, com o motivo correspondente.
    assert_eq!(choice["finish_reason"], "length");

    // A chamada NÃO foi descartada: ela volta estruturada.
    let calls = choice["message"]["tool_calls"]
        .as_array()
        .expect("the cut call comes back structured, not discarded");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "write_file");

    // E os argumentos são a string JSON inacabada - de propósito: um cliente
    // que ignore o `length` e tente parsear falha, em vez de executar meia
    // chamada com o `content` pela metade.
    let raw = calls[0]["function"]["arguments"]
        .as_str()
        .expect("arguments is a string");
    assert!(
        serde_json::from_str::<Value>(raw).is_err(),
        "arguments of a cut call cannot parse: {raw:?}"
    );
    assert!(
        raw.starts_with(r#"{"path":"/tmp/calc.py","content":"#),
        "{raw:?}"
    );
    // O último parâmetro entra com a aspa de abertura e **sem** a de
    // fechamento, e nada do que o modelo escreveu é aparado: a quebra de linha
    // final é conteúdo, não sobra.
    assert!(raw.ends_with(r"linha_12 = 12 * 2\n"), "{raw:?}");

    // E nada de marcação crua no conteúdo - o defeito original.
    let content = choice["message"]["content"].as_str().unwrap_or("");
    assert_eq!(content, "Vou escrever o arquivo.\n");
    assert!(
        !content.contains("<tool_call>")
            && !content.contains("<function=")
            && !content.contains("<parameter="),
        "{content:?}"
    );

    // `x_pom` honesto: o corte caiu dentro da chamada, e a resposta é parcial.
    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["x_pom"]["partial"], true);
    assert_eq!(body["x_pom"]["finish_reason"], "length");
    assert_eq!(
        body["x_pom"]["output_limits"]["effective_max_tokens"],
        CAP_INSIDE_CALL
    );
}

/// A mesma forma em streaming: os dois deltas de sempre - abertura com o nome,
/// depois os argumentos -, só que os argumentos são a string inacabada. Nada
/// de dentro da tag vazou como `content` antes disso, porque o scanner segura
/// o corpo até a tag fechar.
#[tokio::test]
async fn the_streamed_tool_call_cut_by_the_ceiling_is_structured_and_incomplete() {
    let no = no(tool_call_script(), 8).await;

    let flow = post_sse(&no, request(CAP_INSIDE_CALL, true, true)).await;

    assert_eq!(flow.finish_reasons(), vec!["length".to_string()]);
    assert_eq!(flow.done_sentinels, 1);

    let deltas = flow.tool_call_deltas();
    assert_eq!(
        deltas[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "write_file"
    );
    // Os fragmentos que já tinham sido transmitidos contam: o corte não
    // repete o que o cliente já acumulou. A concatenação é a string inacabada.
    let raw: String = flow
        .argument_fragments()
        .iter()
        .map(|(_, s)| s.as_str())
        .collect();
    assert!(
        serde_json::from_str::<Value>(&raw).is_err(),
        "arguments of a cut call cannot parse: {raw:?}"
    );
    assert!(
        raw.starts_with(r#"{"path":"/tmp/calc.py","content":"#),
        "{raw:?}"
    );
    assert!(raw.ends_with(r"linha_12 = 12 * 2\n"), "{raw:?}");

    let text = flow.text();
    assert_eq!(text, "Vou escrever o arquivo.\n");
    assert!(
        !text.contains("<tool_call>")
            && !text.contains("<function=")
            && !text.contains("<parameter="),
        "{text:?}"
    );

    let x_pom = &flow.last_with_x_pom()["x_pom"];
    assert_eq!(x_pom["truncated_tool_call"], true);
    assert_eq!(x_pom["partial"], true);
}

/// A mesma forma no vocabulário da Anthropic: o **último bloco** é um
/// `tool_use` com o `input` incompleto, e o `stop_reason` é `max_tokens`. É
/// exatamente o par que a documentação manda o cliente conferir para saber que
/// precisa repetir o pedido com um teto maior.
#[tokio::test]
async fn the_anthropic_adapter_returns_the_cut_call_as_a_trailing_tool_use_block() {
    let no = no(tool_call_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": CAP_INSIDE_CALL,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "tools": [{
                "name": "write_file",
                "description": "Escreve um arquivo",
                "input_schema": {"type": "object", "properties": {}},
            }],
        }),
    )
    .await;

    assert_eq!(body["stop_reason"], "max_tokens");
    let blocks = body["content"].as_array().expect("content is a list");
    let last = blocks.last().expect("at least one block");
    assert_eq!(
        last["type"], "tool_use",
        "the last block must be the cut call: {blocks:?}"
    );
    assert_eq!(last["name"], "write_file");
    // `input` é objeto por contrato, com o que deu para recuperar.
    assert_eq!(last["input"]["path"], "/tmp/calc.py");
    assert!(last["input"]["content"]
        .as_str()
        .expect("partial content")
        .ends_with("linha_12 = 12 * 2\n"));

    // E nada de marcação crua nos blocos de texto.
    let text: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        !text.contains("<tool_call>") && !text.contains("<function="),
        "{text:?}"
    );

    assert_eq!(body["x_pom"]["truncated_tool_call"], true);
    assert_eq!(body["x_pom"]["partial"], true);
}

// ── Capacidade derivada do runtime ──────────────────────────────────────────

/// Não existe mais um teto absoluto paralelo ao deployment. Um pedido que
/// ultrapassaria o antigo default 8192 continua válido quando o contexto
/// efetivo comporta a saída; `max_new_tokens` só divide as pernas.
#[tokio::test]
async fn a_request_above_the_removed_absolute_default_uses_runtime_context() {
    let no = no_with_context(plain_script(), 8, 100_000).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", no.base))
        .bearer_auth(API_KEY)
        .json(&request(100_000, false, false))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(
        body["x_pom"]["output_limits"]["requested_max_tokens"],
        100_000
    );
    assert_eq!(
        body["x_pom"]["output_limits"]["effective_max_tokens"],
        99_992
    );
    assert_eq!(body["x_pom"]["output_limits"]["deployment_max_tokens"], 8);
    let limits = body["x_pom"]["output_limits"]
        .as_object()
        .expect("output limits");
    assert_eq!(limits.len(), 5);
    assert!(limits.contains_key("context_available_tokens"));
}

// ── Orçamento de tarefa ──────────────────────────────────────────────────────

/// **O marcador chega à geração.**
///
/// `output_config.task_budget` é o mecanismo que os provedores oferecem no
/// lugar do transbordo: o servidor injeta um marcador de contagem regressiva
/// que o modelo enxerga, para ele se organizar e terminar de forma graciosa
/// dentro do limite. Este teste confere o caminho inteiro - o marcador sai do
/// adaptador, atravessa o gateway e chega ao quadro de prefill que a cabeça
/// recebe, que é onde o modelo de verdade o leria.
#[tokio::test]
async fn a_task_budget_puts_a_countdown_marker_into_the_generation() {
    let no = no(plain_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 64_000}},
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(prompt.contains("<task_budget>"), "{prompt:?}");
    assert!(prompt.contains("remaining_tokens: 64000"), "{prompt:?}");
    assert!(prompt.contains("total_tokens: 64000"), "{prompt:?}");

    // Advisory: não corta nada. Quem corta é `max_tokens`, e o roteiro acabou
    // antes dele.
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["x_pom"]["task_budget"]["total"], 64_000);
    assert_eq!(
        body["x_pom"]["task_budget"]["remaining_at_turn_start"],
        64_000
    );
    assert_eq!(
        body["x_pom"]["task_budget_spent_this_turn"],
        plain_script().len() as u64
    );
}

/// `remaining` é o resto vindo de uma requisição anterior: é ele que o
/// marcador anuncia, não o `total`. É como o cliente costura o laço agêntico
/// deste lado, já que o nó não guarda estado entre requisições.
#[tokio::test]
async fn a_carried_over_remaining_is_what_the_marker_announces() {
    let no = no(plain_script(), 8).await;

    post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "continue"}],
            "output_config": {
                "task_budget": {"type": "tokens", "total": 128_000, "remaining": 42_000},
            },
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(prompt.contains("remaining_tokens: 42000"), "{prompt:?}");
    assert!(prompt.contains("total_tokens: 128000"), "{prompt:?}");
}

/// **Ausente, nada muda.** O marcador não aparece no prompt, e o `x_pom` não
/// inventa orçamento nenhum.
#[tokio::test]
async fn without_a_task_budget_the_generation_is_untouched() {
    let no = no(plain_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "escreva o arquivo"}],
        }),
    )
    .await;

    let prompt = no.generation_prompt();
    assert!(!prompt.contains("task_budget"), "{prompt:?}");
    assert_eq!(prompt, "User: escreva o arquivo");
    assert!(
        body["x_pom"]["task_budget"].is_null(),
        "{}",
        body["x_pom"]
    );
}

/// O mínimo é o dos provedores: abaixo dele a API responde 400, porque um
/// orçamento pequeno demais para a tarefa faz o modelo recusar ou parar cedo.
#[tokio::test]
async fn a_task_budget_below_the_minimum_is_refused() {
    let no = no(plain_script(), 8).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", no.base))
        .bearer_auth(API_KEY)
        .json(&json!({
            "model": "lab",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "oi"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 1_000}},
        }))
        .send()
        .await
        .expect("HTTP response");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("JSON body");
    let message = body["error"]["message"].as_str().expect("message");
    assert!(message.contains("20000"), "{message}");
}

/// O orçamento de tarefa **nunca corta**: quem corta é `max_tokens`. Um
/// orçamento generoso não estende o teto nem por um token - se estendesse,
/// teríamos reinventado a folga que esta tarefa removeu.
#[tokio::test]
async fn a_generous_task_budget_never_lifts_the_client_ceiling() {
    let no = no(runaway_script(), 8).await;

    let body = post_messages(
        &no,
        json!({
            "model": "lab",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": "oi"}],
            "output_config": {"task_budget": {"type": "tokens", "total": 1_000_000}},
        }),
    )
    .await;

    assert_eq!(body["stop_reason"], "max_tokens");
    assert_eq!(body["usage"]["output_tokens"], 30);
}

// ── `eager_input_streaming`, por ferramenta ─────────────────────────────────

/// **Ligado, os fragmentos saem conforme o modelo gera.** É a forma da
/// Anthropic: campo opcional na definição da ferramenta, sem cabeçalho beta.
#[tokio::test]
async fn eager_input_streaming_on_a_tool_streams_the_argument_in_fragments() {
    let no = no(tool_call_script(), 8).await;

    let fragments = messages_fragments(&no, messages_request(100, Some(true))).await;

    assert!(
        fragments.len() > 5,
        "the arguments came back held up: {} fragment(s)",
        fragments.len()
    );
    let whole: String = fragments.concat();
    let arguments: Value = serde_json::from_str(&whole).expect("the concatenation is valid JSON");
    assert_eq!(arguments["path"], "/tmp/calc.py");
    assert!(arguments["content"]
        .as_str()
        .expect("content")
        .ends_with("linha_39 = 39 * 2"));
}

/// **Ausente, o comportamento represado é preservado.** É o padrão da
/// Anthropic, e o represamento é por **valor de parâmetro**: o parâmetro
/// grande sai inteiro, num fragmento só.
#[tokio::test]
async fn without_eager_input_streaming_each_parameter_arrives_whole() {
    for absent in [None, Some(false)] {
        // Um nó por pedido: o motor falso tem um contador de tokens só, e ele
        // não volta ao começo entre requisições.
        let no = no(tool_call_script(), 8).await;
        let fragments = messages_fragments(&no, messages_request(100, absent)).await;
        assert_eq!(fragments.len(), 3, "{absent:?}: {fragments:?}");
        let large = fragments
            .iter()
            .find(|f| f.contains("linha_39"))
            .expect("the content went out");
        assert!(large.contains("linha_0 = 0 * 2"), "{large:?}");

        let whole: String = fragments.concat();
        let arguments: Value = serde_json::from_str(&whole).expect("valid JSON");
        assert_eq!(arguments["path"], "/tmp/calc.py");
    }
}
