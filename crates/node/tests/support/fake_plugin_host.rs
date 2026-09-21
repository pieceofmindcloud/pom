//! Host IPC mínimo, só para o teste de regressão em `plugins.rs`. Fala o
//! mesmo protocolo de frame (u32 big-endian + JSON) que `plugin_host.rs`
//! usa contra o `pom-plugin-host` real, mas sem nenhuma lógica de plugin: só
//! responde `ready`, registra cada `ingest` numa linha do arquivo apontado
//! por `POM_FAKE_HOST_LOG` e sai em `shutdown`.

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
        serde_json::from_slice(&buffer).expect("frame JSON válido"),
    ))
}

fn write_frame<W: Write>(writer: &mut W, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).expect("serializar frame");
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn main() {
    let log_path = std::env::var("POM_FAKE_HOST_LOG").expect("POM_FAKE_HOST_LOG ausente");
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

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
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": true, "result": {"status": "ready"}, "error": null}),
                )
                .expect("responder ready");
            }
            "ingest" => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .expect("abrir log do fake host");
                writeln!(
                    file,
                    "{}",
                    request.get("payload").cloned().unwrap_or(Value::Null)
                )
                .expect("gravar evento");
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": true, "result": {}, "error": null}),
                )
                .expect("responder ingest");
            }
            "shutdown" => {
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": true, "result": {}, "error": null}),
                )
                .expect("responder shutdown");
                break;
            }
            _ => {
                write_frame(
                    &mut writer,
                    &json!({"id": id, "ok": false, "result": null, "error": "método desconhecido"}),
                )
                .expect("responder erro");
            }
        }
    }
}
