//! Biblioteca Enterprise carregada exclusivamente pelo `pom-plugin-host`.
//!
//! A biblioteca não conhece tipos do processo `pom`: recebe JSON pela ABI C,
//! grava somente no seu próprio `enterprise.db` e devolve JSON para consultas.
//! Cada callback captura panic antes de atravessar a fronteira C.

use enterprise_protocol::abi::{ByteBuffer, ByteSlice, HostCallbacks, PluginApiV1, PluginHandle};
use rusqlite::{params, types::Value as SqlValue, Connection};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

static VERSION: &[u8] = b"enterprise-plugin-0.1.0\0";
const CAPABILITY_AUDIT: u64 = 1;

struct PluginState {
    database: Mutex<Connection>,
}

fn input_bytes(input: ByteSlice) -> Result<&'static [u8], String> {
    if input.len == 0 {
        return Ok(&[]);
    }
    if input.ptr.is_null() {
        return Err("entrada ABI nula".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(input.ptr, input.len) })
}

fn database_path(config: ByteSlice) -> Result<PathBuf, String> {
    let bytes = input_bytes(config)?;
    let config = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice::<Value>(bytes).map_err(|error| error.to_string())?
    };
    if let Some(path) = config.get("database_path").and_then(Value::as_str) {
        if path.trim().is_empty() {
            return Err("database_path vazio".into());
        }
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("POM_ENTERPRISE_DB") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    Ok(std::env::current_dir()
        .map_err(|error| error.to_string())?
        .join("enterprise.db"))
}

fn open_database(config: ByteSlice) -> Result<Connection, String> {
    let path = database_path(config)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS requests (
                 id TEXT PRIMARY KEY,
                 received_at TEXT NOT NULL,
                 completed_at TEXT,
                 route TEXT NOT NULL,
                 model TEXT NOT NULL,
                 caller_json TEXT NOT NULL,
                 entry_node_id TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 request_details_json TEXT NOT NULL,
                 result TEXT,
                 telemetry_json TEXT,
                 timings_json TEXT,
                 hops_json TEXT NOT NULL DEFAULT '[]',
                 source TEXT NOT NULL DEFAULT 'unknown',
                 prompt_tokens INTEGER,
                 completion_tokens INTEGER,
                 duration_ms INTEGER,
                 status TEXT NOT NULL DEFAULT 'incomplete',
                 error TEXT
             );
             CREATE INDEX IF NOT EXISTS requests_received_idx
                 ON requests(received_at DESC);
             CREATE INDEX IF NOT EXISTS requests_model_idx
                 ON requests(model, received_at DESC);",
        )
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn parse_event(input: ByteSlice) -> Result<Value, String> {
    serde_json::from_slice(input_bytes(input)?).map_err(|error| error.to_string())
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn optional_text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn optional_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn json_text(value: Option<&Value>, fallback: &str) -> String {
    serde_json::to_string(value.unwrap_or(&Value::Null)).unwrap_or_else(|_| fallback.into())
}

fn optional_json_text(value: Option<&Value>) -> Option<String> {
    value
        .filter(|value| !value.is_null())
        .and_then(|value| serde_json::to_string(value).ok())
}

fn ingest_inner(state: &PluginState, input: ByteSlice) -> Result<(), String> {
    let event = parse_event(input)?;
    let kind = event
        .get("type")
        .or_else(|| event.get("event"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let database = state
        .database
        .lock()
        .map_err(|_| "mutex do banco envenenado")?;
    match kind {
        "begin" => {
            let request = event.get("request").unwrap_or(&event);
            let id = text(request.get("id"));
            if id.is_empty() {
                return Err("evento begin sem id".into());
            }
            database
                .execute(
                    "INSERT INTO requests (
                        id, received_at, route, model, caller_json, entry_node_id,
                        prompt, request_details_json
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(id) DO UPDATE SET
                        received_at = excluded.received_at,
                        route = excluded.route,
                        model = excluded.model,
                        caller_json = excluded.caller_json,
                        entry_node_id = excluded.entry_node_id,
                        prompt = excluded.prompt,
                        request_details_json = excluded.request_details_json",
                    params![
                        id,
                        text(request.get("received_at")),
                        text(request.get("route")),
                        text(request.get("model")),
                        json_text(request.get("caller"), "{}"),
                        text(request.get("entry_node_id")),
                        text(request.get("prompt")),
                        json_text(request.get("request_details"), "null"),
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        "complete" => {
            let id = text(event.get("id"));
            let completion = event.get("completion").unwrap_or(&event);
            if id.is_empty() {
                return Err("evento complete sem id".into());
            }
            database
                .execute(
                    "UPDATE requests SET completed_at = ?, result = ?, telemetry_json = ?,
                        timings_json = ?, hops_json = ?, source = ?, prompt_tokens = ?,
                        completion_tokens = ?, duration_ms = ?, status = ?, error = ?
                     WHERE id = ?",
                    params![
                        text(completion.get("completed_at")),
                        optional_text(completion.get("result")),
                        optional_json_text(completion.get("telemetry")),
                        optional_json_text(completion.get("timings")),
                        json_text(completion.get("hops"), "[]"),
                        text(completion.get("source")),
                        optional_u64(completion.get("prompt_tokens")),
                        optional_u64(completion.get("completion_tokens")),
                        optional_u64(completion.get("duration_ms")),
                        text(completion.get("status")),
                        optional_text(completion.get("error")),
                        id,
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        _ => return Err("evento Enterprise desconhecido".into()),
    }
    Ok(())
}

fn parse_json(raw: Option<String>) -> Value {
    raw.and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

fn filter_clause(query: &Value) -> (String, Vec<SqlValue>) {
    let mut clauses = vec!["1 = 1".to_owned()];
    let mut values = Vec::new();
    if let Some(node_id) = query.get("node_id").and_then(Value::as_str) {
        clauses.push("(entry_node_id = ? OR hops_json LIKE ?)".into());
        values.push(SqlValue::Text(node_id.into()));
        values.push(SqlValue::Text(format!("%\"{node_id}\"%")));
    }
    if let Some(user) = query.get("user").and_then(Value::as_str) {
        clauses.push("json_extract(caller_json, '$.key_label') = ?".into());
        values.push(SqlValue::Text(user.into()));
    }
    if let Some(api_key) = query.get("api_key").and_then(Value::as_str) {
        clauses.push("json_extract(caller_json, '$.key_id') = ?".into());
        values.push(SqlValue::Text(api_key.into()));
    }
    if let Some(model) = query.get("model").and_then(Value::as_str) {
        clauses.push("model = ?".into());
        values.push(SqlValue::Text(model.into()));
    }
    if let Some(prompt) = query.get("prompt").and_then(Value::as_str) {
        clauses.push("prompt LIKE ?".into());
        values.push(SqlValue::Text(format!("%{prompt}%")));
    }
    if let Some(received_from) = query.get("from").and_then(Value::as_str) {
        clauses.push("received_at >= ?".into());
        values.push(SqlValue::Text(received_from.into()));
    }
    if let Some(received_to) = query.get("to").and_then(Value::as_str) {
        clauses.push("received_at <= ?".into());
        values.push(SqlValue::Text(received_to.into()));
    }
    (clauses.join(" AND "), values)
}

fn json_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as f64))
}

fn add_peer(peer_consumption: &mut serde_json::Map<String, Value>, node: &str) {
    let entry = peer_consumption.entry(node.to_owned()).or_insert_with(
        || json!({"requests": 0, "bytes_sent": 0, "bytes_received": 0, "network_ms": 0.0}),
    );
    let requests = entry.get("requests").and_then(Value::as_u64).unwrap_or(0);
    entry["requests"] = Value::from(requests + 1);
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let caller: String = row.get(5)?;
    let caller = parse_json(Some(caller));
    let telemetry: Option<String> = row.get(10)?;
    let timings: Option<String> = row.get(11)?;
    let hops: String = row.get(12)?;
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "received_at": row.get::<_, String>(1)?,
        "completed_at": row.get::<_, Option<String>>(2)?,
        "route": row.get::<_, String>(3)?,
        "model": row.get::<_, String>(4)?,
        "api_key_id": caller.get("key_id").and_then(Value::as_str).unwrap_or(""),
        "api_key_label": caller.get("key_label").and_then(Value::as_str).unwrap_or(""),
        "api_key_node_id": caller.get("key_node_id").cloned().unwrap_or(Value::Null),
        "entry_node_id": row.get::<_, String>(6)?,
        "prompt": row.get::<_, String>(7)?,
        "request_details": parse_json(Some(row.get::<_, String>(8)?)),
        "result": row.get::<_, Option<String>>(9)?,
        "telemetry": parse_json(telemetry),
        "timings": parse_json(timings),
        "hops": parse_json(Some(hops)),
        "source": row.get::<_, String>(13)?,
        "prompt_tokens": row.get::<_, Option<u64>>(14)?,
        "completion_tokens": row.get::<_, Option<u64>>(15)?,
        "duration_ms": row.get::<_, Option<u64>>(16)?,
        "status": row.get::<_, String>(17)?,
        "error": row.get::<_, Option<String>>(18)?,
    }))
}

fn query_inner(state: &PluginState, input: ByteSlice) -> Result<Value, String> {
    let query = parse_event(input)?;
    let operation = query
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("list");
    let (where_sql, values) = filter_clause(&query);
    let database = state
        .database
        .lock()
        .map_err(|_| "mutex do banco envenenado")?;
    match operation {
        "list" => {
            let limit = query
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 100) as i64;
            let offset = query.get("offset").and_then(Value::as_u64).unwrap_or(0) as i64;
            let mut statement = database
                .prepare(&format!(
                    "SELECT id, received_at, completed_at, route, model, caller_json,
                        entry_node_id, prompt, request_details_json, result, telemetry_json,
                        timings_json, hops_json, source, prompt_tokens, completion_tokens,
                        duration_ms, status, error
                     FROM requests WHERE {where_sql}
                     ORDER BY received_at DESC, id DESC LIMIT ? OFFSET ?"
                ))
                .map_err(|error| error.to_string())?;
            let mut entry_values = values.clone();
            entry_values.push(SqlValue::Integer(limit));
            entry_values.push(SqlValue::Integer(offset));
            let rows = statement
                .query_map(
                    rusqlite::params_from_iter(entry_values.iter()),
                    entry_from_row,
                )
                .map_err(|error| error.to_string())?;
            let entries = rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| error.to_string())?;
            let total: u64 = database
                .query_row(
                    &format!("SELECT COUNT(*) FROM requests WHERE {where_sql}"),
                    rusqlite::params_from_iter(values.iter()),
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let errors: u64 = database
                .query_row(
                    &format!("SELECT COUNT(*) FROM requests WHERE {where_sql} AND status IN ('error', 'abandoned')"),
                    rusqlite::params_from_iter(values.iter()),
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let incomplete: u64 = database
                .query_row(
                    &format!("SELECT COUNT(*) FROM requests WHERE {where_sql} AND status IN ('incomplete', 'abandoned')"),
                    rusqlite::params_from_iter(values.iter()),
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let mut by_token = serde_json::Map::new();
            let mut token_statement = database
                .prepare(&format!(
                    "SELECT json_extract(caller_json, '$.key_label'), COUNT(*)
                     FROM requests WHERE {where_sql}
                     GROUP BY json_extract(caller_json, '$.key_label')"
                ))
                .map_err(|error| error.to_string())?;
            for row in token_statement
                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        row.get::<_, u64>(1)?,
                    ))
                })
                .map_err(|error| error.to_string())?
            {
                let (label, count) = row.map_err(|error| error.to_string())?;
                by_token.insert(label, Value::from(count));
            }
            let mut by_node = serde_json::Map::new();
            let mut node_statement = database
                .prepare(&format!(
                    "SELECT entry_node_id, hops_json FROM requests WHERE {where_sql}"
                ))
                .map_err(|error| error.to_string())?;
            for row in node_statement
                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| error.to_string())?
            {
                let (entry_node, hops) = row.map_err(|error| error.to_string())?;
                let mut nodes = vec![entry_node];
                nodes.extend(
                    parse_json(Some(hops))
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                nodes.sort();
                nodes.dedup();
                for node in nodes {
                    let count = by_node.get(&node).and_then(Value::as_u64).unwrap_or(0);
                    by_node.insert(node, Value::from(count + 1));
                }
            }
            Ok(json!({
                "entries": entries,
                "total": total,
                "errors": errors,
                "incomplete": incomplete,
                "by_token": by_token,
                "by_node": by_node,
                "storage": "enterprise.db",
            }))
        }
        "stats" => {
            let mut statement = database
                .prepare(&format!(
                    "SELECT received_at, model, caller_json, entry_node_id,
                        prompt_tokens, completion_tokens, duration_ms, status,
                        timings_json, telemetry_json, hops_json
                     FROM requests WHERE {where_sql}"
                ))
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                        row.get::<_, Option<u64>>(5)?,
                        row.get::<_, Option<u64>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            let mut total = 0_u64;
            let mut input_tokens = 0_u64;
            let mut output_tokens = 0_u64;
            let mut inference_sum = 0.0_f64;
            let mut inference_count = 0_u64;
            let mut ttft_sum = 0.0_f64;
            let mut ttft_count = 0_u64;
            let mut by_token = serde_json::Map::new();
            let mut by_node = serde_json::Map::new();
            let mut by_day = serde_json::Map::new();
            let mut by_model = serde_json::Map::new();
            let mut peer_consumption = serde_json::Map::new();
            let mut terminal_states = serde_json::Map::new();
            let mut cache = json!({"eligible_requests": 0, "hit_requests": 0, "input_tokens": 0, "cached_tokens": 0});
            for row in rows {
                let (
                    received_at,
                    model,
                    caller,
                    entry_node,
                    prompt,
                    completion,
                    duration,
                    status,
                    timings,
                    telemetry,
                    hops,
                ) = row.map_err(|error| error.to_string())?;
                total += 1;
                input_tokens += prompt.unwrap_or(0);
                output_tokens += completion.unwrap_or(0);
                let caller = parse_json(Some(caller));
                let token = caller
                    .get("key_label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let token_count = by_token.get(&token).and_then(Value::as_u64).unwrap_or(0);
                by_token.insert(token, Value::from(token_count + 1));
                let day = received_at
                    .split('T')
                    .next()
                    .unwrap_or(&received_at)
                    .to_owned();
                let day_value = by_day.entry(day).or_insert_with(
                    || json!({"requests": 0, "input_tokens": 0, "output_tokens": 0}),
                );
                day_value["requests"] =
                    Value::from(day_value["requests"].as_u64().unwrap_or(0) + 1);
                day_value["input_tokens"] = Value::from(
                    day_value["input_tokens"].as_u64().unwrap_or(0) + prompt.unwrap_or(0),
                );
                day_value["output_tokens"] = Value::from(
                    day_value["output_tokens"].as_u64().unwrap_or(0) + completion.unwrap_or(0),
                );
                let model_value = by_model.entry(model.clone()).or_insert_with(|| json!({"requests": 0, "average_tokens_per_sec": null, "average_inference_ms": null}));
                model_value["requests"] =
                    Value::from(model_value["requests"].as_u64().unwrap_or(0) + 1);
                if let Some(duration) = duration {
                    inference_sum += duration as f64;
                    inference_count += 1;
                    model_value["_duration_sum"] = Value::from(
                        model_value["_duration_sum"].as_f64().unwrap_or(0.0) + duration as f64,
                    );
                    model_value["_duration_count"] =
                        Value::from(model_value["_duration_count"].as_u64().unwrap_or(0) + 1);
                }
                if status != "incomplete" {
                    let count = terminal_states
                        .get(&status)
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    terminal_states.insert(status, Value::from(count + 1));
                }
                let mut nodes = vec![entry_node];
                nodes.extend(
                    parse_json(Some(hops.clone()))
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                nodes.sort();
                nodes.dedup();
                for node in nodes {
                    let count = by_node.get(&node).and_then(Value::as_u64).unwrap_or(0);
                    by_node.insert(node.clone(), Value::from(count + 1));
                    add_peer(&mut peer_consumption, &node);
                }
                let timings = parse_json(timings);
                if let Some(ttft) = json_number(timings.get("ttft_ms")) {
                    ttft_sum += ttft;
                    ttft_count += 1;
                }
                if let Some(speed) = json_number(timings.get("total_tokens_per_sec")) {
                    if let Some(model) = by_model.get_mut(&model) {
                        model["_speed_sum"] =
                            Value::from(model["_speed_sum"].as_f64().unwrap_or(0.0) + speed);
                        model["_speed_count"] =
                            Value::from(model["_speed_count"].as_u64().unwrap_or(0) + 1);
                    }
                }
                let telemetry = parse_json(telemetry);
                if let Some(input) = json_number(telemetry.get("input_tokens")) {
                    cache["eligible_requests"] =
                        Value::from(cache["eligible_requests"].as_u64().unwrap_or(0) + 1);
                    cache["input_tokens"] =
                        Value::from(cache["input_tokens"].as_u64().unwrap_or(0) + input as u64);
                    let cached = json_number(telemetry.get("cached_tokens")).unwrap_or(0.0) as u64;
                    cache["cached_tokens"] =
                        Value::from(cache["cached_tokens"].as_u64().unwrap_or(0) + cached);
                    if cached > 0 {
                        cache["hit_requests"] =
                            Value::from(cache["hit_requests"].as_u64().unwrap_or(0) + 1);
                    }
                }
            }
            for value in by_model.values_mut() {
                let speed_count = value["_speed_count"].as_u64().unwrap_or(0);
                let duration_count = value["_duration_count"].as_u64().unwrap_or(0);
                value["average_tokens_per_sec"] = (speed_count > 0)
                    .then(|| {
                        Value::from(
                            value["_speed_sum"].as_f64().unwrap_or(0.0) / speed_count as f64,
                        )
                    })
                    .unwrap_or(Value::Null);
                value["average_inference_ms"] = (duration_count > 0)
                    .then(|| {
                        Value::from(
                            value["_duration_sum"].as_f64().unwrap_or(0.0) / duration_count as f64,
                        )
                    })
                    .unwrap_or(Value::Null);
                value
                    .as_object_mut()
                    .unwrap()
                    .retain(|key, _| !key.starts_with('_'));
            }
            Ok(json!({
                "total": total,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "average_inference_ms": (inference_count > 0).then(|| inference_sum / inference_count as f64),
                "average_ttft_ms": (ttft_count > 0).then(|| ttft_sum / ttft_count as f64),
                "by_token": by_token,
                "by_node": by_node,
                "by_day": by_day,
                "by_model": by_model,
                "peer_consumption": peer_consumption,
                "terminal_states": terminal_states,
                "cache": cache,
                "storage": "enterprise.db",
            }))
        }
        _ => Err("operação de consulta Enterprise desconhecida".into()),
    }
}

fn buffer_from_bytes(bytes: Vec<u8>) -> ByteBuffer {
    let mut bytes = bytes.into_boxed_slice();
    let buffer = ByteBuffer {
        ptr: bytes.as_mut_ptr(),
        len: bytes.len(),
    };
    std::mem::forget(bytes);
    buffer
}

unsafe extern "C" fn create(_: HostCallbacks, config: ByteSlice) -> PluginHandle {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        open_database(config).map(|database| {
            Box::into_raw(Box::new(PluginState {
                database: Mutex::new(database),
            }))
            .cast::<c_void>()
        })
    }));
    match result {
        Ok(Ok(handle)) => handle,
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn ingest(handle: PluginHandle, event: ByteSlice) -> i32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() {
            return Err("handle nulo".to_owned());
        }
        ingest_inner(&*handle.cast::<PluginState>(), event)
    }));
    matches!(result, Ok(Ok(()))).then_some(0).unwrap_or(-1)
}

unsafe extern "C" fn query(handle: PluginHandle, request: ByteSlice) -> ByteBuffer {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() {
            return Err("handle nulo".to_owned());
        }
        query_inner(&*handle.cast::<PluginState>(), request)
            .and_then(|value| serde_json::to_vec(&value).map_err(|error| error.to_string()))
    }));
    match result {
        Ok(Ok(bytes)) => buffer_from_bytes(bytes),
        _ => ByteBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        },
    }
}

unsafe extern "C" fn free_buffer(buffer: ByteBuffer) {
    if !buffer.ptr.is_null() {
        drop(Vec::from_raw_parts(buffer.ptr, buffer.len, buffer.len));
    }
}

unsafe extern "C" fn shutdown(handle: PluginHandle) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !handle.is_null() {
            drop(Box::from_raw(handle.cast::<PluginState>()));
        }
    }));
}

static API: PluginApiV1 = PluginApiV1 {
    abi_version: enterprise_protocol::PLUGIN_ABI_VERSION,
    plugin_version: VERSION.as_ptr().cast(),
    capabilities: CAPABILITY_AUDIT,
    create: Some(create),
    ingest: Some(ingest),
    query: Some(query),
    free_buffer: Some(free_buffer),
    shutdown: Some(shutdown),
};

#[no_mangle]
pub unsafe extern "C" fn pom_enterprise_plugin_v1() -> *const PluginApiV1 {
    &API
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config(path: &std::path::Path) -> ByteSlice {
        let value = serde_json::to_vec(&json!({"database_path": path})).expect("config");
        let value = Box::leak(value.into_boxed_slice());
        ByteSlice {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }

    #[test]
    fn plugin_persists_request_events_in_its_own_database() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pom-enterprise-plugin-{stamp}.db"));
        let handle = unsafe { create(HostCallbacks { log: None }, config(&path)) };
        assert!(!handle.is_null());
        let begin = json!({
            "type": "begin",
            "request": {
                "id": "request-1",
                "received_at": "2099-01-01T00:00:00Z",
                "route": "/v1/chat",
                "model": "test",
                "caller": {"key_id": "key-1", "key_label": "operator"},
                "entry_node_id": "node-a",
                "prompt": "hello",
                "request_details": {}
            }
        });
        let begin = serde_json::to_vec(&begin).expect("begin");
        assert_eq!(
            unsafe {
                ingest(
                    handle,
                    ByteSlice {
                        ptr: begin.as_ptr(),
                        len: begin.len(),
                    },
                )
            },
            0
        );
        let complete = serde_json::to_vec(&json!({
            "type": "complete",
            "id": "request-1",
            "completion": {
                "completed_at": "2099-01-01T00:00:01Z",
                "result": "world",
                "telemetry": {"input_tokens": 4, "cached_tokens": 2},
                "timings": {"ttft_ms": 10, "total_tokens_per_sec": 20.0},
                "hops": ["node-a", "node-b"],
                "source": "pipeline:node-a→node-b",
                "prompt_tokens": 4,
                "completion_tokens": 2,
                "duration_ms": 100,
                "status": "ok"
            }
        }))
        .expect("complete");
        assert_eq!(
            unsafe {
                ingest(
                    handle,
                    ByteSlice {
                        ptr: complete.as_ptr(),
                        len: complete.len(),
                    },
                )
            },
            0
        );
        let request = serde_json::to_vec(&json!({"operation":"list"})).expect("query");
        let result = unsafe {
            query(
                handle,
                ByteSlice {
                    ptr: request.as_ptr(),
                    len: request.len(),
                },
            )
        };
        let body = unsafe { std::slice::from_raw_parts(result.ptr, result.len) };
        let body: Value = serde_json::from_slice(body).expect("result");
        unsafe { free_buffer(result) };
        assert_eq!(body["total"], 1);
        assert_eq!(body["storage"], "enterprise.db");
        let stats_request = serde_json::to_vec(&json!({"operation":"stats"})).expect("stats");
        let stats_result = unsafe {
            query(
                handle,
                ByteSlice {
                    ptr: stats_request.as_ptr(),
                    len: stats_request.len(),
                },
            )
        };
        let stats_body = unsafe { std::slice::from_raw_parts(stats_result.ptr, stats_result.len) };
        let stats_body: Value = serde_json::from_slice(stats_body).expect("stats result");
        unsafe { free_buffer(stats_result) };
        assert_eq!(stats_body["input_tokens"], 4);
        assert_eq!(stats_body["cache"]["hit_requests"], 1);
        assert_eq!(stats_body["by_node"]["node-b"], 1);
        let filtered_request = serde_json::to_vec(&json!({
            "operation":"list",
            "prompt":"hello"
        }))
        .expect("filtered query");
        let filtered_result = unsafe {
            query(
                handle,
                ByteSlice {
                    ptr: filtered_request.as_ptr(),
                    len: filtered_request.len(),
                },
            )
        };
        let filtered_body =
            unsafe { std::slice::from_raw_parts(filtered_result.ptr, filtered_result.len) };
        let filtered_body: Value = serde_json::from_slice(filtered_body).expect("filtered result");
        unsafe { free_buffer(filtered_result) };
        assert_eq!(filtered_body["total"], 1);
        unsafe { shutdown(handle) };
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
