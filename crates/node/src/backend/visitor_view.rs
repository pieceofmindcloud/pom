//! What a visitor learns of the network it is connected to: the models it
//! may choose and the admin entry nodes it talks to, nothing about the
//! machines that serve them - not their names, ids, addresses, how many there
//! are or how the layers are split among them.
//!
//! **The admin removes it, never the visitor.** Every filter here runs on the
//! node that holds the information (an admin, or any member answering a
//! visitor), before the bytes leave for the visitor. A visitor's own code
//! keeps no redaction of its own: a visitor running other code must not learn
//! more than an honest one.
//!
//! - [`redact_answer`]: the answer an entry node sends back for a delegated
//!   request (`delegation::delegate_entry`). `x_pom.source` becomes
//!   [`HIDDEN_SOURCE`], `x_pom.trace` keeps only the request-level counters
//!   ([`TRACE_KEEP`]), `x_pom.error` becomes a fixed sentence, and the
//!   identifiers of every hidden machine are replaced in error messages.
//! - [`servable_catalog`]: the models list an admin hands a visitor
//!   (`/api/internal/catalog/servable`), without the `<name>@<node_id>` head
//!   pins.

use crate::gateway::GatewayState;
use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::{Map, Value};

/// What `x_pom.source` says to a visitor: the request ran on the network's
/// distributed pipeline, with no hop named.
pub(crate) const HIDDEN_SOURCE: &str = "pipeline:network";
/// What a visitor reads in place of a generation error's text, which names
/// hops and addresses.
pub(crate) const HIDDEN_ERROR: &str = "the network could not finish the generation";
/// What stands in an error message for an identifier of a hidden machine.
const HIDDEN_MACHINE: &str = "[a machine of the network]";
/// The `RequestTrace` fields a visitor keeps: counters of its own request.
/// An allowlist on purpose - a field added to the trace later stays with the
/// members until someone decides a visitor may read it.
pub(crate) const TRACE_KEEP: &[&str] = &[
    "trace_id",
    "model",
    "profile",
    "request_state",
    "complete",
    "input_tokens",
    "cached_tokens",
    "prefix_match_tokens",
    "output_tokens",
    "generated_tokens",
    "server_ttft_ms",
    "engine_ttft_ms",
    "client_ttft_ms",
    "mtp_draft_tokens_proposed",
    "mtp_draft_tokens_accepted",
    "mtp_draft_tokens_rejected",
    "mtp_acceptance_rate",
];
/// `GenerationTimings` fields that say where the work ran.
const TIMINGS_DROP: &[&str] = &["local_engine_first_event_ms"];
/// The largest non-streamed answer the redaction reads whole.
const MAX_ANSWER: usize = 64 << 20;

/// The identifiers of the machines a visitor must not learn: every live peer
/// that is not an admin of the chain (workers and other visitors), with its
/// node id (whole and the prefixes the logs and `hop_chain` print), host
/// name, endpoint id and addresses.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hidden {
    needles: Vec<String>,
}

impl Hidden {
    pub(crate) async fn of(state: &GatewayState) -> Self {
        let mut needles = Vec::new();
        for peer in state.network.alive_peers().await {
            if peer.node_id == state.network.node_id || state.network.peer_is_admin(&peer) {
                continue;
            }
            needles.extend(Self::of_peer(&peer));
        }
        Self::from_needles(needles)
    }

    fn of_peer(peer: &crate::network::PeerInfo) -> Vec<String> {
        let mut out = vec![peer.node_id.clone()];
        for len in [16, 12, 8] {
            if peer.node_id.len() > len {
                out.push(peer.node_id[..len].to_string());
            }
        }
        out.push(peer.hostname.clone());
        out.extend(peer.endpoint_id.clone());
        for addr in std::iter::once(&peer.addr)
            .chain(&peer.addr_candidates)
            .chain(&peer.endpoint_addrs)
        {
            out.push(addr.clone());
            if let Ok(socket) = addr.parse::<std::net::SocketAddr>() {
                out.push(socket.ip().to_string());
            }
        }
        out
    }

    pub(crate) fn from_needles(needles: Vec<String>) -> Self {
        // Short strings would replace common words; a host name of three
        // letters is not worth mangling every error message.
        let mut needles: Vec<String> = needles
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| n.chars().count() >= 4)
            .collect();
        needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup();
        Self { needles }
    }

    /// `text` with every hidden identifier replaced, longest first so a node
    /// id is not left half replaced by its own prefix.
    pub(crate) fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for needle in &self.needles {
            if out.contains(needle.as_str()) {
                out = out.replace(needle.as_str(), HIDDEN_MACHINE);
            }
        }
        out
    }
}

/// The trace a visitor reads: [`TRACE_KEEP`] only.
fn slim_trace(trace: &Value) -> Value {
    let Some(object) = trace.as_object() else {
        return Value::Null;
    };
    Value::Object(
        object
            .iter()
            .filter(|(key, _)| TRACE_KEEP.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

/// A report of how the generation ran (`x_pom`, or the top of an
/// `/api/generate` answer), as a visitor reads it.
fn redact_report(report: &mut Map<String, Value>) {
    if let Some(source) = report.get_mut("source") {
        if source.as_str().is_some_and(|s| s != "none") {
            *source = Value::String(HIDDEN_SOURCE.into());
        }
    }
    if let Some(trace) = report.get_mut("trace") {
        if !trace.is_null() {
            *trace = slim_trace(trace);
        }
    }
    if let Some(error) = report.get_mut("error") {
        if error.as_str().is_some_and(|e| !e.is_empty()) {
            *error = Value::String(HIDDEN_ERROR.into());
        }
    }
    if let Some(Value::Object(timings)) = report.get_mut("timings") {
        for key in TIMINGS_DROP {
            timings.remove(*key);
        }
    }
}

/// One JSON document of an answer (the whole body, or one SSE/NDJSON frame),
/// as a visitor reads it. Only the report and the error object are touched:
/// the generated text and tool calls are the model's, never rewritten.
pub(crate) fn redact_document(document: &mut Value, route: &str, ok: bool, hidden: &Hidden) {
    let Some(object) = document.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(report)) = object.get_mut("x_pom") {
        redact_report(report);
    }
    if route == "/api/generate" {
        redact_report(object);
        if !ok {
            if let Some(response) = object.get_mut("response") {
                if response.is_string() {
                    *response = Value::String(HIDDEN_ERROR.into());
                }
            }
        }
    }
    match object.get_mut("error") {
        Some(Value::Object(error)) => {
            if let Some(Value::String(message)) = error.get_mut("message") {
                *message = hidden.scrub(message);
            }
        }
        Some(Value::String(message)) => *message = hidden.scrub(message),
        _ => {}
    }
}

/// One line of a streamed answer: `data: {json}` (SSE) or `{json}`
/// (NDJSON). Anything else passes as it came.
fn redact_line(line: &[u8], route: &str, ok: bool, hidden: &Hidden) -> Vec<u8> {
    let (body, ending) = match line.strip_suffix(b"\r\n") {
        Some(body) => (body, &b"\r\n"[..]),
        None => match line.strip_suffix(b"\n") {
            Some(body) => (body, &b"\n"[..]),
            None => (line, &b""[..]),
        },
    };
    let (prefix, payload) = match body.strip_prefix(b"data:") {
        Some(rest) => (&b"data: "[..], rest.strip_prefix(b" ").unwrap_or(rest)),
        None => (&b""[..], body),
    };
    if !payload.starts_with(b"{") {
        return line.to_vec();
    }
    let Ok(mut document) = serde_json::from_slice::<Value>(payload) else {
        return line.to_vec();
    };
    redact_document(&mut document, route, ok, hidden);
    let mut out = prefix.to_vec();
    out.extend(serde_json::to_vec(&document).unwrap_or_else(|_| payload.to_vec()));
    out.extend_from_slice(ending);
    out
}

/// The answer of a delegated request as it leaves an entry node for a
/// visitor: the same status and headers, the body redacted. A streamed
/// answer is redacted line by line as it flows; a JSON one is read whole.
pub(crate) async fn redact_answer(response: Response, route: &str, hidden: Hidden) -> Response {
    let (mut parts, body) = response.into_parts();
    let ok = parts.status.is_success();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let route = route.to_string();
    if content_type.starts_with("text/event-stream") || content_type.contains("ndjson") {
        parts.headers.remove(header::CONTENT_LENGTH);
        let stream = line_stream(body, route, ok, hidden);
        return Response::from_parts(parts, Body::from_stream(stream));
    }
    if !content_type.contains("json") {
        return Response::from_parts(parts, body);
    }
    let bytes = match axum::body::to_bytes(body, MAX_ANSWER).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let mut response = Response::new(Body::from(
                serde_json::json!({ "error": { "message": HIDDEN_ERROR } }).to_string(),
            ));
            *response.status_mut() = StatusCode::BAD_GATEWAY;
            return response;
        }
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    let out = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut document) => {
            redact_document(&mut document, &route, ok, &hidden);
            Bytes::from(serde_json::to_vec(&document).unwrap_or_else(|_| bytes.to_vec()))
        }
        Err(_) => bytes,
    };
    Response::from_parts(parts, Body::from(out))
}

fn line_stream(
    body: Body,
    route: String,
    ok: bool,
    hidden: Hidden,
) -> impl futures::Stream<Item = Result<Bytes, axum::Error>> + Send {
    use futures::StreamExt;
    struct Lines {
        input: axum::body::BodyDataStream,
        buffer: Vec<u8>,
        done: bool,
    }
    let state = Lines {
        input: body.into_data_stream(),
        buffer: Vec::new(),
        done: false,
    };
    futures::stream::unfold(state, move |mut state| {
        let route = route.clone();
        let hidden = hidden.clone();
        async move {
            loop {
                if let Some(end) = state.buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = state.buffer.drain(..=end).collect();
                    let out = redact_line(&line, &route, ok, &hidden);
                    return Some((Ok(Bytes::from(out)), state));
                }
                if state.done {
                    if state.buffer.is_empty() {
                        return None;
                    }
                    let rest = std::mem::take(&mut state.buffer);
                    let out = redact_line(&rest, &route, ok, &hidden);
                    return Some((Ok(Bytes::from(out)), state));
                }
                match state.input.next().await {
                    Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                    Some(Err(error)) => {
                        state.done = true;
                        state.buffer.clear();
                        return Some((Err(error), state));
                    }
                    None => state.done = true,
                }
            }
        }
    })
}

/// The models a visitor may choose, as `/v1/models` objects: what the admin's
/// own `/v1/models` lists, without the `<name>@<node_id>` pins of a
/// deployment with several heads (they name the machines that hold layer 0).
pub(crate) async fn servable_catalog(state: &GatewayState) -> Vec<Value> {
    crate::openai::model_objects(state, false)
        .await
        .into_iter()
        .filter_map(|model| serde_json::to_value(model).ok())
        .collect()
}

/// `POST /api/internal/catalog/servable`: on an admin, the models a visitor
/// may choose ([`servable_catalog`]). Any member may read it; only the admin
/// answers, since a worker's view is not the network's.
pub(crate) const CATALOG_PATH: &str = "/api/internal/catalog/servable";
/// How long a visitor reuses the catalog it read.
const CATALOG_TTL: std::time::Duration = std::time::Duration::from_secs(5);
const CATALOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) async fn internal_servable_catalog(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
) -> Response {
    use axum::response::IntoResponse;
    if auth.principal().is_none() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "signed_caller_required" })),
        )
            .into_response();
    }
    if !crate::webui::is_admin(&state) {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "error": "not_an_entry_node" })),
        )
            .into_response();
    }
    axum::Json(servable_catalog(&state).await).into_response()
}

fn catalog_cache() -> &'static std::sync::Mutex<Option<(std::time::Instant, Vec<Value>)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, Vec<Value>)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// On a visitor: the catalog of the nearest admin entry node that answers,
/// reused for [`CATALOG_TTL`]. With no entry node answering, the last list
/// read stays for a minute and then the list is empty: a visitor has no
/// other source, its peer table holds no serving machine.
pub(crate) async fn catalog_from_entry(state: &GatewayState) -> Vec<Value> {
    let cached = catalog_cache().lock().ok().and_then(|c| c.clone());
    if let Some((at, models)) = &cached {
        if at.elapsed() < CATALOG_TTL {
            return models.clone();
        }
    }
    for entry in crate::delegation::entry_nodes(state).await {
        let answer = crate::admin::post_internal_json(
            state,
            &entry,
            CATALOG_PATH,
            &serde_json::json!({}),
            CATALOG_TIMEOUT,
        )
        .await;
        let Ok(answer) = answer else { continue };
        if !answer.status().is_success() {
            continue;
        }
        if let Ok(models) = answer.json::<Vec<Value>>().await {
            if let Ok(mut cache) = catalog_cache().lock() {
                *cache = Some((std::time::Instant::now(), models.clone()));
            }
            return models;
        }
    }
    match cached {
        Some((at, models)) if at.elapsed() < std::time::Duration::from_secs(60) => models,
        _ => Vec::new(),
    }
}

/// Whether the caller this node knows by `node_id` is a visitor - its
/// roster vouch, else its announce (`Network::peer_is_visitor`). A caller
/// this node does not know is treated as one: it learns nothing it would
/// not tell a visitor.
pub(crate) async fn caller_is_visitor(state: &GatewayState, node_id: Option<&str>) -> bool {
    let Some(node_id) = node_id else {
        return true;
    };
    if node_id == state.network.node_id {
        return false;
    }
    let document = state.admin_trust.document();
    if crate::admin_trust::is_admin_node(&document, node_id) {
        return false;
    }
    match state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
    {
        Some(peer) => crate::gateway::peer_is_visitor(state, &peer),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hidden() -> Hidden {
        Hidden::from_needles(vec![
            "e5f6a7b8c9d0e1f2a3b4".into(),
            "e5f6a7b8".into(),
            "gpu-box-7".into(),
            "10.0.0.5".into(),
            "box".into(),
        ])
    }

    fn full_report() -> Value {
        json!({
            "source": "pipeline:a1b2c3d4→e5f6a7b8",
            "tokens_per_sec": 4.5,
            "error": "hop e5f6a7b8 failed: connect 10.0.0.5:8182",
            "timings": { "ttft_ms": 900, "local_engine_first_event_ms": 12 },
            "payment": { "charged_micro": 12 },
            "trace": {
                "trace_id": "t1",
                "model": "qwen",
                "input_tokens": 40,
                "cached_tokens": 33,
                "hops": [{ "node_id": "e5f6a7b8c9d0e1f2a3b4", "start_layer": 0, "end_layer": 20 }],
                "spans": [{ "host": "gpu-box-7" }],
                "stage_balance": { "decode": { "node_id": "e5f6a7b8c9d0e1f2a3b4" } },
                "issues": [{ "node_id": "e5f6a7b8c9d0e1f2a3b4", "message": "x" }],
                "session_transfer": { "source_node_id": "a", "target_node_id": "b" },
                "total_bytes_sent": 1024,
                "future_field": 1
            }
        })
    }

    #[test]
    fn an_x_pom_report_keeps_the_counters_and_names_no_machine() {
        let mut document = json!({ "choices": [], "x_pom": full_report() });
        redact_document(&mut document, "/v1/chat/completions", true, &hidden());
        let report = &document["x_pom"];
        assert_eq!(report["source"], HIDDEN_SOURCE);
        assert_eq!(report["error"], HIDDEN_ERROR);
        assert_eq!(report["tokens_per_sec"], 4.5);
        assert_eq!(report["payment"]["charged_micro"], 12);
        assert_eq!(report["timings"]["ttft_ms"], 900);
        assert!(report["timings"]
            .get("local_engine_first_event_ms")
            .is_none());
        assert_eq!(report["trace"]["cached_tokens"], 33);
        assert_eq!(report["trace"]["input_tokens"], 40);
        assert_eq!(report["trace"]["model"], "qwen");
        for gone in [
            "hops",
            "spans",
            "stage_balance",
            "issues",
            "session_transfer",
            "total_bytes_sent",
            "future_field",
        ] {
            assert!(
                report["trace"].get(gone).is_none(),
                "{gone} reached a visitor"
            );
        }
        let text = document.to_string();
        assert!(!text.contains("e5f6a7b8"));
        assert!(!text.contains("gpu-box-7"));
        assert!(!text.contains("10.0.0.5"));
    }

    #[test]
    fn the_generated_text_is_never_rewritten() {
        let mut document = json!({
            "choices": [{ "message": { "content": "my box gpu-box-7 at 10.0.0.5" } }],
        });
        let before = document.clone();
        redact_document(&mut document, "/v1/chat/completions", true, &hidden());
        assert_eq!(document, before);
    }

    #[test]
    fn an_error_message_loses_the_hidden_identifiers_only() {
        let mut document = json!({
            "error": { "message": "no plan: e5f6a7b8c9d0e1f2a3b4 (gpu-box-7) is loading", "type": "server_error" }
        });
        redact_document(&mut document, "/v1/chat/completions", false, &hidden());
        let message = document["error"]["message"].as_str().unwrap();
        assert!(!message.contains("e5f6a7b8"));
        assert!(!message.contains("gpu-box-7"));
        assert!(message.starts_with("no plan: "));
        // A needle shorter than four characters is not scrubbed.
        let mut document = json!({ "error": { "message": "box" } });
        redact_document(&mut document, "/v1/messages", false, &hidden());
        assert_eq!(document["error"]["message"], "box");
    }

    #[test]
    fn an_api_generate_answer_is_its_own_report() {
        let mut document = full_report();
        document["response"] = json!("Inference failed. Error: hop e5f6a7b8 timed out.");
        redact_document(&mut document, "/api/generate", false, &hidden());
        assert_eq!(document["source"], HIDDEN_SOURCE);
        assert_eq!(document["response"], HIDDEN_ERROR);
        assert!(document["trace"].get("hops").is_none());
        let mut ok = json!({ "response": "hello e5f6a7b8", "source": "none" });
        redact_document(&mut ok, "/api/generate", true, &hidden());
        assert_eq!(ok["response"], "hello e5f6a7b8");
        assert_eq!(ok["source"], "none");
    }

    #[test]
    fn sse_and_ndjson_lines_are_redacted_and_keep_their_framing() {
        let frame = format!("data: {}\n", json!({ "x_pom": full_report() }));
        let out = String::from_utf8(redact_line(
            frame.as_bytes(),
            "/v1/chat/completions",
            true,
            &hidden(),
        ))
        .unwrap();
        assert!(out.starts_with("data: {"));
        assert!(out.ends_with('\n'));
        assert!(!out.contains("e5f6a7b8"));
        for line in [
            "data: [DONE]\n",
            "event: message_delta\n",
            "\n",
            ": keepalive\r\n",
        ] {
            assert_eq!(
                redact_line(line.as_bytes(), "/v1/messages", true, &hidden()),
                line.as_bytes()
            );
        }
        let ndjson = format!("{}\n", json!({ "x_pom": full_report() }));
        let out = String::from_utf8(redact_line(
            ndjson.as_bytes(),
            "/v1/messages",
            true,
            &hidden(),
        ))
        .unwrap();
        assert!(out.contains(HIDDEN_SOURCE));
    }

    #[tokio::test]
    async fn a_streamed_answer_is_redacted_across_chunk_boundaries() {
        use futures::StreamExt;
        let frame = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({ "x_pom": full_report() })
        );
        let (a, b) = frame.split_at(frame.len() / 2);
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(a.to_string())),
            Ok(Bytes::from(b.to_string())),
        ];
        let response = axum::http::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures::stream::iter(chunks)))
            .unwrap();
        let redacted = redact_answer(response, "/v1/chat/completions", hidden()).await;
        let mut stream = redacted.into_body().into_data_stream();
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(HIDDEN_SOURCE));
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(!text.contains("e5f6a7b8"));
    }

    #[tokio::test]
    async fn a_json_answer_is_redacted_whole() {
        let body = json!({ "x_pom": full_report() }).to_string();
        let response = axum::http::Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap();
        let redacted = redact_answer(response, "/v1/chat/completions", hidden()).await;
        assert!(redacted.headers().get(header::CONTENT_LENGTH).is_none());
        let bytes = axum::body::to_bytes(redacted.into_body(), usize::MAX)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(document["x_pom"]["source"], HIDDEN_SOURCE);
    }
}
