//! Supervisor IPC do plugin Enterprise.
//!
//! Este módulo não carrega biblioteca dinâmica. Ele inicia o executável
//! `pom-plugin-host`, envia frames JSON enquadrados e encerra o circuito quando
//! o processo auxiliar falha ou não responde dentro do timeout.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
/// O handshake inicial de "ready" pode competir com um boot pesado (modelo
/// grande carregando, máquina ainda se recuperando de um OOM) e demorar bem
/// mais que uma chamada IPC comum. Um timeout curto aqui não falha o plugin
/// de verdade - só descarta a conexão e, sem `kill_on_drop`, deixava o
/// processo `pom-plugin-host` órfão rodando sem que `EnterpriseState.host`
/// apontasse para ele: os eventos de auditoria caíam silenciosamente no
/// fallback legado enquanto o processo continuava vivo no SO.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IpcRequest {
    id: u64,
    method: String,
    payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct IpcResponse {
    id: u64,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

struct HostProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
}

#[derive(Clone)]
pub(crate) struct EnterpriseHost {
    process: Arc<Mutex<HostProcess>>,
}

impl EnterpriseHost {
    pub(crate) async fn start(plugin: &Path, database: &Path) -> Result<Self> {
        let host = host_executable()?;
        let mut child = Command::new(&host)
            .arg(plugin)
            .env("POM_ENTERPRISE_DB", database)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("iniciar pom-plugin-host em {}", host.display()))?;
        let stdin = match child.stdin.take().context("capturar stdin do plugin host") {
            Ok(stdin) => stdin,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let stdout = match child
            .stdout
            .take()
            .context("capturar stdout do plugin host")
        {
            Ok(stdout) => stdout,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let supervisor = Self {
            process: Arc::new(Mutex::new(HostProcess {
                child,
                stdin,
                stdout,
                next_id: 1,
            })),
        };
        match timeout(READY_TIMEOUT, supervisor.request("ready", Value::Null)).await {
            Ok(Ok(ready)) if ready.get("status").and_then(Value::as_str) == Some("ready") => {
                Ok(supervisor)
            }
            Ok(Ok(ready)) => {
                supervisor.kill().await;
                anyhow::bail!("plugin host não ficou pronto: resposta {ready}")
            }
            Ok(Err(error)) => {
                supervisor.kill().await;
                Err(error).context("handshake ready do plugin host falhou")
            }
            Err(_) => {
                supervisor.kill().await;
                anyhow::bail!(
                    "handshake ready do plugin host não respondeu em {:?}",
                    READY_TIMEOUT
                )
            }
        }
    }

    /// Mata o processo auxiliar sem passar pelo protocolo `shutdown` - usado
    /// quando o handshake inicial falha e não há conexão IPC íntegra para
    /// negociar um encerramento limpo.
    async fn kill(&self) {
        let mut process = self.process.lock().await;
        let _ = process.child.start_kill();
        let _ = process.child.wait().await;
    }

    pub(crate) async fn ingest(&self, event: Value) -> Result<()> {
        self.request("ingest", event).await.map(|_| ())
    }

    pub(crate) async fn query(&self, query: Value) -> Result<Value> {
        self.request("query", query).await
    }

    pub(crate) async fn stop(&self) -> Result<()> {
        let mut process = self.process.lock().await;
        let id = process.next_id;
        let _ = send_frame(
            &mut process.stdin,
            &IpcRequest {
                id,
                method: "shutdown".into(),
                payload: Value::Null,
            },
        )
        .await;
        process.next_id = process.next_id.saturating_add(1);
        let _ = timeout(IPC_TIMEOUT, process.child.wait()).await;
        if process.child.try_wait()?.is_none() {
            process.child.kill().await.context("encerrar plugin host")?;
        }
        Ok(())
    }

    async fn request(&self, method: &str, payload: Value) -> Result<Value> {
        let mut process = self.process.lock().await;
        let id = process.next_id;
        process.next_id = process.next_id.saturating_add(1);
        send_frame(
            &mut process.stdin,
            &IpcRequest {
                id,
                method: method.into(),
                payload,
            },
        )
        .await?;
        let response: IpcResponse = timeout(IPC_TIMEOUT, read_frame(&mut process.stdout))
            .await
            .context("timeout do plugin host")??;
        if response.id != id {
            anyhow::bail!("resposta IPC fora de ordem");
        }
        if !response.ok {
            anyhow::bail!(
                "plugin host recusou {}: {}",
                method,
                response.error.unwrap_or_else(|| "erro desconhecido".into())
            );
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

async fn send_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("serializar frame IPC")?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        anyhow::bail!("frame IPC fora do limite");
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        anyhow::bail!("frame IPC fora do limite");
    }
    let mut bytes = vec![0_u8; size];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("interpretar resposta IPC")
}

fn host_executable() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("POM_PLUGIN_HOST") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        anyhow::bail!("POM_PLUGIN_HOST não aponta para um arquivo");
    }
    let current = std::env::current_exe().context("localizar executável pom")?;
    let sibling = current
        .parent()
        .context("localizar diretório do executável pom")?
        .join(if cfg!(windows) {
            "pom-plugin-host.exe"
        } else {
            "pom-plugin-host"
        });
    if sibling.is_file() {
        return Ok(sibling);
    }
    anyhow::bail!(
        "pom-plugin-host não encontrado ao lado de {}",
        current.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn frames_are_bounded_and_round_trip_over_async_streams() {
        let request = IpcRequest {
            id: 4,
            method: "ping".into(),
            payload: serde_json::json!({"ok": true}),
        };
        let mut bytes = Vec::new();
        send_frame(&mut bytes, &request).await.expect("write");
        let mut cursor = Cursor::new(bytes);
        let decoded: IpcRequest = read_frame(&mut cursor).await.expect("read");
        assert_eq!(decoded.id, 4);
        assert_eq!(decoded.method, "ping");
    }
}
