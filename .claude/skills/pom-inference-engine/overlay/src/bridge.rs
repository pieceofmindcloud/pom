//! The generation route the POM gateway calls (`engine.upstream`), on
//! loopback, in front of the engine's own server.
//!
//! The POM renders a conversation into one prompt text and posts
//! `POST /pom/generate` with `{prompt, chat_template, thinking, max_tokens,
//! stop, temperature, top_p, top_k, min_p, presence_penalty,
//! frequency_penalty, seed, session?, images?, probe?}` and
//! `Authorization: Bearer <token>`. The answer is one JSON object per line:
//!
//! - `{"t": text}` answer text and `{"r": text}` reasoning, in order;
//! - `{"p": [read, total]}` prompt progress (optional);
//! - `{"done": {finish, prompt_tokens, completion_tokens, reasoning_tokens,
//!   cached_tokens, timings, stop_sequence}}`, or `{"error": message}`.
//!
//! This first level translates to the engine's OpenAI-compatible streaming
//! API: `chat_template: true` becomes one user turn on `/v1/chat/completions`
//! (the engine applies the model's template), `false` a raw
//! `/v1/completions`. Reasoning must stay structured (`reasoning_content`),
//! never `<think>` text in `t`. Tool calls stay in the answer text in the
//! model's own form: the POM parses them. A capability the engine lacks is a
//! clear `{"error"}` line, never a silent fallback. The session routes answer
//! 501 until the engine has sessions: the POM then syncs nothing.

use crate::http::{self, NdjsonWriter, Request};
use serde_json::{json, Map, Value};
use std::io::BufRead;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// The numeric sampling fields the POM forwards from the client.
pub const SAMPLING_KEYS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "presence_penalty",
    "frequency_penalty",
    "seed",
];

/// How long the engine may stay silent inside one generation.
const ENGINE_SILENCE: Duration = Duration::from_secs(600);

/// Where the bridge reaches the engine, while it is ready: its loopback port
/// and the model name its API expects.
pub type Target = Arc<dyn Fn() -> Option<(u16, String)> + Send + Sync>;

pub struct Bridge {
    pub port: u16,
    pub token: String,
    pub in_flight: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl Bridge {
    pub fn start(target: Target) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let token = http::random_hex(24);
        let in_flight = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (secret, count, stopping) = (token.clone(), Arc::clone(&in_flight), Arc::clone(&stop));
        thread::Builder::new()
            .name(format!("pom-plugin-{}-bridge", crate::PLUGIN_CODE))
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    let (target, secret, count) =
                        (Arc::clone(&target), secret.clone(), Arc::clone(&count));
                    let _ = thread::Builder::new()
                        .name(format!("pom-plugin-{}-request", crate::PLUGIN_CODE))
                        .spawn(move || serve(&stream, &target, &secret, &count));
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            port,
            token,
            in_flight,
            stop,
        })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag and releases the port.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

struct InFlight(Arc<AtomicU32>);

impl InFlight {
    fn enter(count: &Arc<AtomicU32>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve(stream: &TcpStream, target: &Target, secret: &str, count: &Arc<AtomicU32>) {
    let Ok(request) = http::read_request(stream) else {
        return;
    };
    let presented = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !http::same_secret(presented, secret) {
        let _ = http::respond_json(stream, 401, &json!({"error": "unauthorized"}));
        return;
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/pom/generate") => {
            let _guard = InFlight::enter(count);
            generate(stream, target, &request);
        }
        ("GET", "/pom/status") => {
            let ready = target().is_some();
            let _ = http::respond_json(
                stream,
                200,
                &json!({"loaded": ready, "in_flight": count.load(Ordering::SeqCst)}),
            );
        }
        (_, path) if path.starts_with("/pom/session") => {
            // Conversation sessions are an engine feature (feature-levels.md, L4).
            let _ = http::respond_json(
                stream,
                501,
                &json!({"error": "this engine keeps no conversation sessions"}),
            );
        }
        _ => {
            let _ = http::respond_json(stream, 404, &json!({"error": "not found"}));
        }
    }
}

/// The engine's request for one POM generation, and its route.
pub fn engine_request(body: &Value, model: &str) -> Result<(&'static str, Value), String> {
    let prompt = body["prompt"].as_str().ok_or("'prompt' must be a string")?;
    let chat = body["chat_template"].as_bool().unwrap_or(true);
    if body.get("probe").is_some_and(|probe| !probe.is_null()) {
        return Err("this engine does not answer typed decisions (probe)".into());
    }
    if body["prefill_only"] == true {
        return Err("this engine has no prefill/decode split".into());
    }
    let images = body["images"].as_array().cloned().unwrap_or_default();
    if !images.is_empty() && !crate::launch::VISION {
        return Err("this engine does not read images".into());
    }
    let mut request = Map::new();
    request.insert("model".into(), json!(model));
    request.insert("stream".into(), json!(true));
    request.insert("stream_options".into(), json!({"include_usage": true}));
    if let Some(max) = body["max_tokens"].as_u64().filter(|max| *max > 0) {
        request.insert("max_tokens".into(), json!(max));
    }
    if let Some(stop) = body["stop"].as_array().filter(|stop| !stop.is_empty()) {
        request.insert("stop".into(), json!(stop));
    }
    for key in SAMPLING_KEYS {
        if body[*key].is_number() {
            request.insert((*key).into(), body[*key].clone());
        }
    }
    if !chat {
        request.insert("prompt".into(), json!(prompt));
        return Ok(("/v1/completions", Value::Object(request)));
    }
    let content = if images.is_empty() {
        json!(prompt)
    } else {
        let mut parts: Vec<Value> = images
            .iter()
            .map(|url| json!({"type": "image_url", "image_url": {"url": url}}))
            .collect();
        parts.push(json!({"type": "text", "text": prompt}));
        Value::Array(parts)
    };
    request.insert(
        "messages".into(),
        json!([{"role": "user", "content": content}]),
    );
    // The thinking switch most chat templates read (llama.cpp, vLLM, SGLang).
    let thinking = body["thinking"].as_bool().unwrap_or(true);
    request.insert(
        "chat_template_kwargs".into(),
        json!({"enable_thinking": thinking}),
    );
    Ok(("/v1/chat/completions", Value::Object(request)))
}

/// What one streamed engine event adds to the answer.
#[derive(Debug, Default, PartialEq)]
pub struct Delta {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub finish: Option<String>,
    pub usage: Option<Value>,
    pub timings: Option<Value>,
}

/// One `data:` payload of the engine's server-sent events.
pub fn parse_event(data: &str) -> Result<Delta, String> {
    let event: Value = serde_json::from_str(data).map_err(|error| error.to_string())?;
    if let Some(error) = event.get("error") {
        return Err(error["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string()));
    }
    let choice = &event["choices"][0];
    let nonempty = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let delta = &choice["delta"];
    Ok(Delta {
        text: nonempty(&delta["content"]).or_else(|| nonempty(&choice["text"])),
        reasoning: nonempty(&delta["reasoning_content"]).or_else(|| nonempty(&delta["reasoning"])),
        finish: choice["finish_reason"].as_str().map(str::to_owned),
        usage: event
            .get("usage")
            .filter(|usage| usage.is_object())
            .cloned(),
        timings: event
            .get("timings")
            .filter(|timings| timings.is_object())
            .cloned(),
    })
}

/// The POM's `done` line from what the stream said.
pub fn done_line(
    finish: Option<&str>,
    usage: &Value,
    timings: Option<Value>,
    first_token: Option<Duration>,
    total: Duration,
) -> Value {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let prompt_tokens = count("prompt_tokens");
    let completion_tokens = count("completion_tokens");
    let reasoning_tokens = usage["completion_tokens_details"]["reasoning_tokens"]
        .as_u64()
        .unwrap_or(0);
    let cached_tokens = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    // llama.cpp-style timings: the engine's own when it sends them, else the
    // bridge's measure (prompt = time to the first token).
    let timings = timings.unwrap_or_else(|| {
        let prompt_ms = first_token.unwrap_or(total).as_secs_f64() * 1000.0;
        json!({
            "prompt_n": prompt_tokens,
            "prompt_ms": prompt_ms,
            "predicted_n": completion_tokens,
            "predicted_ms": total.as_secs_f64() * 1000.0 - prompt_ms,
        })
    });
    json!({"done": {
        "finish": match finish {
            Some("length") => "length",
            _ => "stop",
        },
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "reasoning_tokens": reasoning_tokens,
        "cached_tokens": cached_tokens,
        "timings": timings,
        "stop_sequence": Value::Null,
    }})
}

fn generate(stream: &TcpStream, target: &Target, request: &Request) {
    let fail = |message: String| {
        if let Ok(mut out) = NdjsonWriter::start(stream) {
            let _ = out.line(&json!({"error": message}));
            let _ = out.finish();
        }
    };
    let Some((port, model)) = target() else {
        return fail("the engine is not ready".into());
    };
    let body: Value = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => return fail(format!("the body is not JSON: {error}")),
    };
    let (path, engine_body) = match engine_request(&body, &model) {
        Ok(request) => request,
        Err(error) => return fail(error),
    };
    let started = Instant::now();
    let payload = engine_body.to_string();
    let response = match http::request(
        "POST",
        port,
        path,
        &[
            ("content-type", "application/json"),
            ("accept", "text/event-stream"),
        ],
        Some(payload.as_bytes()),
        ENGINE_SILENCE,
    ) {
        Ok(response) => response,
        Err(error) => return fail(format!("the engine did not answer: {error}")),
    };
    let Ok(mut out) = NdjsonWriter::start(stream) else {
        return;
    };
    let mut body = response.body;
    if response.status != 200 {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut body, &mut text);
        let _ = out.line(
            &json!({"error": format!("the engine answered {}: {}", response.status, text.trim())}),
        );
        let _ = out.finish();
        return;
    }
    let (mut finish, mut usage, mut timings, mut first) = (None, json!({}), None, None);
    let mut line = String::new();
    loop {
        line.clear();
        match body.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                let _ = out.line(&json!({"error": format!("the engine stream broke: {error}")}));
                let _ = out.finish();
                return;
            }
        }
        let Some(data) = line.trim_end().strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let delta = match parse_event(data) {
            Ok(delta) => delta,
            Err(error) => {
                let _ = out.line(&json!({"error": error}));
                let _ = out.finish();
                return;
            }
        };
        if delta.text.is_some() || delta.reasoning.is_some() {
            first.get_or_insert_with(|| started.elapsed());
        }
        let sent = delta
            .reasoning
            .map(|text| out.line(&json!({"r": text})))
            .unwrap_or(Ok(()))
            .and_then(|()| {
                delta
                    .text
                    .map(|text| out.line(&json!({"t": text})))
                    .unwrap_or(Ok(()))
            });
        if sent.is_err() {
            // The POM went away: dropping the engine's connection cancels it.
            return;
        }
        finish = delta.finish.or(finish);
        usage = delta.usage.unwrap_or(usage);
        timings = delta.timings.or(timings);
    }
    let _ = out.line(&done_line(
        finish.as_deref(),
        &usage,
        timings,
        first,
        started.elapsed(),
    ));
    let _ = out.finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_chat_turn_becomes_one_user_message_with_the_thinking_switch() {
        let (path, request) = engine_request(
            &json!({"prompt": "hi", "chat_template": true, "thinking": false, "max_tokens": 9,
                    "stop": ["</s>"], "temperature": 0.2, "top_k": 40, "unknown": 1}),
            "m",
        )
        .unwrap();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(
            request["messages"],
            json!([{"role": "user", "content": "hi"}])
        );
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(request["max_tokens"], 9);
        assert_eq!(request["stop"], json!(["</s>"]));
        assert_eq!(request["temperature"], 0.2);
        assert_eq!(request["top_k"], 40);
        assert!(request.get("unknown").is_none());
    }

    #[test]
    fn a_raw_completion_keeps_the_prompt_as_it_is() {
        let (path, request) =
            engine_request(&json!({"prompt": "<s>x", "chat_template": false}), "m").unwrap();
        assert_eq!(path, "/v1/completions");
        assert_eq!(request["prompt"], "<s>x");
        assert!(request.get("messages").is_none());
    }

    #[test]
    fn what_the_engine_cannot_do_is_refused_by_name() {
        let probe = engine_request(&json!({"prompt": "x", "probe": {"labels": ["a"]}}), "m");
        assert!(probe.unwrap_err().contains("probe"));
        let prefill = engine_request(&json!({"prompt": "x", "prefill_only": true}), "m");
        assert!(prefill.unwrap_err().contains("prefill"));
        assert!(engine_request(&json!({}), "m").is_err());
    }

    #[test]
    fn reasoning_and_answer_stay_apart() {
        let delta = parse_event(r#"{"choices":[{"delta":{"reasoning_content":"why"}}]}"#).unwrap();
        assert_eq!(delta.reasoning.as_deref(), Some("why"));
        assert_eq!(delta.text, None);
        let end = parse_event(
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"length"}],"usage":{"prompt_tokens":3}}"#,
        )
        .unwrap();
        assert_eq!(end.text.as_deref(), Some("ok"));
        assert_eq!(end.finish.as_deref(), Some("length"));
        assert!(parse_event(r#"{"error":{"message":"boom"}}"#)
            .unwrap_err()
            .contains("boom"));
    }

    #[test]
    fn the_done_line_carries_counts_and_timings() {
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 4,
            "completion_tokens_details": {"reasoning_tokens": 2},
            "prompt_tokens_details": {"cached_tokens": 6}});
        let done = done_line(
            Some("stop"),
            &usage,
            None,
            Some(Duration::from_millis(100)),
            Duration::from_millis(300),
        );
        assert_eq!(done["done"]["finish"], "stop");
        assert_eq!(done["done"]["reasoning_tokens"], 2);
        assert_eq!(done["done"]["cached_tokens"], 6);
        assert_eq!(done["done"]["timings"]["prompt_n"], 10);
        assert!((done["done"]["timings"]["prompt_ms"].as_f64().unwrap() - 100.0).abs() < 1.0);
        let own = done_line(
            None,
            &usage,
            Some(json!({"draft_n": 5})),
            None,
            Duration::ZERO,
        );
        assert_eq!(own["done"]["timings"]["draft_n"], 5);
    }

    /// An engine server that streams a fixed answer, chunked.
    fn fake_engine() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let request = http::read_request(&stream).unwrap();
                assert_eq!(request.path, "/v1/chat/completions");
                let events = [
                    r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
                    r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
                ];
                let mut stream = &stream;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n")
                    .unwrap();
                for data in events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .chain(["data: [DONE]\n\n".to_owned()])
                {
                    write!(stream, "{:x}\r\n{data}\r\n", data.len()).unwrap();
                }
                stream.write_all(b"0\r\n\r\n").unwrap();
            }
        });
        port
    }

    fn post(port: u16, token: &str, body: &Value) -> (u16, Vec<Value>) {
        let payload = body.to_string();
        let auth = format!("Bearer {token}");
        let mut response = http::request(
            "POST",
            port,
            "/pom/generate",
            &[("authorization", &auth)],
            Some(payload.as_bytes()),
            Duration::from_secs(10),
        )
        .unwrap();
        let mut text = String::new();
        response.body.read_to_string(&mut text).unwrap();
        let lines = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        (response.status, lines)
    }

    #[test]
    fn the_route_streams_the_engine_answer_as_pom_lines() {
        let engine = fake_engine();
        let bridge = Bridge::start(Arc::new(move || Some((engine, "m".to_owned())))).unwrap();
        let (status, lines) = post(
            bridge.port,
            &bridge.token,
            &json!({"prompt": "hi", "chat_template": true}),
        );
        assert_eq!(status, 200);
        assert_eq!(lines[0], json!({"r": "think"}));
        assert_eq!(lines[1], json!({"t": "Hello"}));
        assert_eq!(lines[2]["done"]["completion_tokens"], 2);
        assert_eq!(lines.len(), 3);
        let (status, _) = post(bridge.port, "wrong", &json!({"prompt": "hi"}));
        assert_eq!(status, 401);
    }

    #[test]
    fn a_bridge_without_an_engine_answers_an_error_line() {
        let bridge = Bridge::start(Arc::new(|| None)).unwrap();
        let (status, lines) = post(bridge.port, &bridge.token, &json!({"prompt": "hi"}));
        assert_eq!(status, 200);
        assert!(lines[0]["error"].as_str().unwrap().contains("not ready"));
    }
}
