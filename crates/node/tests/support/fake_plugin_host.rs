//! Minimal IPC host, only for the regression test in `plugins.rs`. It speaks the
//! same frame protocol (u32 big-endian + JSON) that `plugin_host.rs`
//! uses against the real `pom-plugin-host`, but with no plugin logic at all: it only
//! answers `ready`, records each `ingest` and each `query` on a line of the
//! file pointed to by `POM_FAKE_HOST_LOG` and exits on `shutdown`. Of the
//! queries, it accepts `host.configure` and answers `ui.upstream` with the JSON of
//! `POM_FAKE_HOST_UPSTREAM`, when set.

use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut len_bytes = [0u8; 4];
    if let Err(error) = reader.read_exact(&mut len_bytes) {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(error);
    }
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut buffer = vec![0u8; len];
    reader.read_exact(&mut buffer)?;
    Ok(Some(
        serde_json::from_slice(&buffer).expect("valid JSON frame"),
    ))
}

fn write_frame<W: Write>(writer: &mut W, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).expect("serialize frame");
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn main() {
    let log_path = std::env::var("POM_FAKE_HOST_LOG").expect("POM_FAKE_HOST_LOG missing");
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let database_path = std::env::var("POM_PLUGIN_DB").unwrap_or_default();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open the fake host log");
    writeln!(log, "POM_PLUGIN_DB={database_path}").expect("write the plugin database path");

    loop {
        let Some(request) = read_frame(&mut reader).expect("ler frame IPC") else {
            break;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        match method.as_str() {
            "ready" => {
                let rejection = std::env::var("POM_FAKE_HOST_READY_ERROR").ok();
                let reply = match rejection {
                    Some(error) => json!({"id": id, "ok": false, "result": null, "error": error}),
                    None => {
                        json!({"id": id, "ok": true, "result": {"status": "ready"}, "error": null})
                    }
                };
                write_frame(&mut writer, &reply).expect("respond ready");
            }
            "ingest" => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .expect("open the fake host log");
                writeln!(
                    file,
                    "{}",
                    request.get("payload").cloned().unwrap_or(Value::Null)
                )
                .expect("write event");
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": true, "result": {}, "error": null}),
                )
                .expect("respond ingest");
            }
            "query" => {
                let payload = request.get("payload").cloned().unwrap_or(Value::Null);
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .expect("open the fake host log");
                writeln!(file, "query {payload}").expect("write query");
                let operation = payload
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let upstream = std::env::var("POM_FAKE_HOST_UPSTREAM")
                    .ok()
                    .and_then(|value| serde_json::from_str::<Value>(&value).ok());
                let reply = match (operation, upstream) {
                    ("host.configure", _) => {
                        json!({"id": id, "ok": true, "result": {"status": "configured"}, "error": null})
                    }
                    ("ui.upstream", Some(upstream)) => {
                        json!({"id": id, "ok": true, "result": upstream, "error": null})
                    }
                    // Plugin RPC: echo what the node forwarded.
                    (op, _) if op.starts_with("rpc.") => {
                        json!({"id": id, "ok": true, "result": {
                            "operation": op,
                            "params": payload.get("params").cloned().unwrap_or(Value::Null),
                            "profile": payload.get("profile").cloned().unwrap_or(Value::Null),
                            "node_id": payload.get("node_id").cloned().unwrap_or(Value::Null),
                        }, "error": null})
                    }
                    _ => {
                        json!({"id": id, "ok": false, "result": null, "error": "unknown operation"})
                    }
                };
                write_frame(&mut writer, &reply).expect("respond query");
            }
            "shutdown" => {
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": true, "result": {}, "error": null}),
                )
                .expect("respond shutdown");
                break;
            }
            _ => {
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": false, "result": null, "error": "unknown method"}),
                )
                .expect("respond error");
            }
        }
    }
}
