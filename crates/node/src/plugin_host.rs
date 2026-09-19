//! Supervisor IPC de um plugin, via processo auxiliar.
//!
//! Este módulo não carrega biblioteca dinâmica. Ele inicia o executável
//! `pom-plugin-host` apontado para o binário de UM plugin, envia frames JSON
//! enquadrados e encerra o circuito quando o processo auxiliar falha ou não
//! responde dentro do timeout. Cada instância é um processo próprio: nada
//! aqui é global, então `crate::plugins::PluginManager` pode manter uma
//! instância por plugin_code ativo, concorrentes, sem se pisarem.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::time::{timeout, Duration};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
/// O handshake inicial de "ready" pode competir com um boot pesado (modelo
/// grande carregando, máquina ainda se recuperando de um OOM) e demorar bem
/// mais que uma chamada IPC comum. Um timeout curto aqui não falha o plugin
/// de verdade - só descarta a conexão e, sem `kill_on_drop`, deixava o
/// processo `pom-plugin-host` órfão rodando sem que o `PluginRuntime`
/// apontasse para ele: os eventos de auditoria caíam silenciosamente no
/// fallback legado enquanto o processo continuava vivo no SO.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IpcRequest {
    id: u64,
    method: String,
    payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IpcResponse {
    id: u64,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

struct HostProcess {
    child: Mutex<Child>,
    ipc: Arc<IpcConnection>,
    lifecycle: RwLock<()>,
}

#[derive(Clone)]
pub(crate) struct PluginHost {
    process: Arc<HostProcess>,
}

struct IpcConnection {
    writer: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Arc<StdMutex<PendingResponses>>,
    next_id: AtomicU64,
    expected_close: Arc<AtomicBool>,
}

type PendingResponses = HashMap<u64, oneshot::Sender<std::result::Result<IpcResponse, String>>>;

struct PendingRequest {
    id: u64,
    pending: Arc<StdMutex<PendingResponses>>,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        lock_pending(&self.pending).remove(&self.id);
    }
}

fn lock_pending(
    pending: &StdMutex<PendingResponses>,
) -> std::sync::MutexGuard<'_, PendingResponses> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl IpcConnection {
    fn new<R, W>(reader: R, writer: W) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending = Arc::new(StdMutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);
        let expected_close = Arc::new(AtomicBool::new(false));
        let expected_close_reader = Arc::clone(&expected_close);
        tokio::spawn(async move {
            Self::read_responses(reader, pending_reader, expected_close_reader).await;
        });

        Arc::new(Self {
            writer: Mutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            expected_close,
        })
    }

    async fn request(
        &self,
        method: &str,
        payload: Value,
        request_timeout: Duration,
    ) -> Result<Value> {
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("esgotados os identificadores do IPC do plugin host"))?;
        let (response_tx, mut response_rx) = oneshot::channel();
        lock_pending(&self.pending).insert(id, response_tx);
        let _pending_request = PendingRequest {
            id,
            pending: Arc::clone(&self.pending),
        };

        let request = IpcRequest {
            id,
            method: method.into(),
            payload,
        };
        if let Err(error) = send_frame(&mut *self.writer.lock().await, &request).await {
            return Err(error).with_context(|| format!("enviar pedido IPC {method}"));
        }

        let response = match timeout(request_timeout, &mut response_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => anyhow::bail!("canal de respostas IPC do plugin host foi encerrado"),
            Err(_) => match response_rx.try_recv() {
                Ok(response) => response,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    anyhow::bail!("canal de respostas IPC do plugin host foi encerrado")
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    anyhow::bail!("timeout do plugin host")
                }
            },
        }
        .map_err(anyhow::Error::msg)?;

        if response.id != id {
            anyhow::bail!("resposta IPC incompatível com o pedido {id}");
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

    async fn read_responses<R>(
        mut reader: R,
        pending: Arc<StdMutex<PendingResponses>>,
        expected_close: Arc<AtomicBool>,
    ) where
        R: AsyncRead + Unpin,
    {
        loop {
            let response: IpcResponse = match read_frame(&mut reader).await {
                Ok(response) => response,
                Err(error) => {
                    let error = format!("leitura do canal IPC do plugin host falhou: {error:#}");
                    if expected_close.load(Ordering::Acquire) {
                        tracing::debug!(%error, "canal de respostas IPC do plugin host terminou durante encerramento");
                    } else {
                        tracing::warn!(%error, "canal de respostas IPC do plugin host terminou");
                    }
                    for (_, sender) in lock_pending(&pending).drain() {
                        let _ = sender.send(Err(error.clone()));
                    }
                    return;
                }
            };

            let sender = lock_pending(&pending).remove(&response.id);
            if let Some(sender) = sender {
                let _ = sender.send(Ok(response));
            } else {
                tracing::warn!(
                    request_id = response.id,
                    "resposta IPC do plugin host chegou sem pedido pendente"
                );
            }
        }
    }
}

impl PluginHost {
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
            process: Arc::new(HostProcess {
                child: Mutex::new(child),
                ipc: IpcConnection::new(stdout, stdin),
                lifecycle: RwLock::new(()),
            }),
        };
        match timeout(
            READY_TIMEOUT,
            supervisor.request_with_timeout("ready", Value::Null, READY_TIMEOUT),
        )
        .await
        {
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
        let _lifecycle = self.process.lifecycle.write().await;
        self.process
            .ipc
            .expected_close
            .store(true, Ordering::Release);
        let mut child = self.process.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    pub(crate) async fn ingest(&self, event: Value) -> Result<()> {
        self.request("ingest", event).await.map(|_| ())
    }

    pub(crate) async fn query(&self, query: Value) -> Result<Value> {
        self.request("query", query).await
    }

    pub(crate) async fn stop(&self) -> Result<()> {
        let _lifecycle = self.process.lifecycle.write().await;
        self.process
            .ipc
            .expected_close
            .store(true, Ordering::Release);
        let _ = self
            .process
            .ipc
            .request("shutdown", Value::Null, IPC_TIMEOUT)
            .await;
        let mut child = self.process.child.lock().await;
        let _ = timeout(IPC_TIMEOUT, child.wait()).await;
        if child.try_wait()?.is_none() {
            child.kill().await.context("encerrar plugin host")?;
        }
        Ok(())
    }

    async fn request(&self, method: &str, payload: Value) -> Result<Value> {
        self.request_with_timeout(method, payload, IPC_TIMEOUT)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        payload: Value,
        request_timeout: Duration,
    ) -> Result<Value> {
        let _lifecycle = self.process.lifecycle.read().await;
        self.process
            .ipc
            .request(method, payload, request_timeout)
            .await
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
    use tokio::io::duplex;

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

    #[tokio::test]
    async fn late_response_after_timeout_does_not_consume_the_next_response() {
        let (host_stream, mut plugin_stream) = duplex(4096);
        let (host_reader, host_writer) = tokio::io::split(host_stream);
        let connection = IpcConnection::new(host_reader, host_writer);

        let first_connection = Arc::clone(&connection);
        let first_request = tokio::spawn(async move {
            first_connection
                .request("slow_query", Value::Null, Duration::from_millis(20))
                .await
        });
        let first_request_frame: IpcRequest = read_frame(&mut plugin_stream)
            .await
            .expect("read first request");
        let first_error = first_request
            .await
            .expect("join first request")
            .expect_err("first request should time out");
        assert!(first_error.to_string().contains("timeout"));

        let second_connection = Arc::clone(&connection);
        let second_request = tokio::spawn(async move {
            second_connection
                .request("query", Value::Null, Duration::from_secs(1))
                .await
        });
        let second_request_frame: IpcRequest = read_frame(&mut plugin_stream)
            .await
            .expect("read second request");

        send_frame(
            &mut plugin_stream,
            &IpcResponse {
                id: first_request_frame.id,
                ok: true,
                result: Some(serde_json::json!({"request": first_request_frame.id})),
                error: None,
            },
        )
        .await
        .expect("send late first response");
        send_frame(
            &mut plugin_stream,
            &IpcResponse {
                id: second_request_frame.id,
                ok: true,
                result: Some(serde_json::json!({"request": second_request_frame.id})),
                error: None,
            },
        )
        .await
        .expect("send second response");

        let result = second_request
            .await
            .expect("join second request")
            .expect("second request must receive its own response");
        assert_eq!(
            result,
            serde_json::json!({"request": second_request_frame.id})
        );
    }

    #[tokio::test]
    async fn concurrent_responses_are_correlated_when_they_arrive_out_of_order() {
        let (host_stream, mut plugin_stream) = duplex(4096);
        let (host_reader, host_writer) = tokio::io::split(host_stream);
        let connection = IpcConnection::new(host_reader, host_writer);

        let first_connection = Arc::clone(&connection);
        let first = tokio::spawn(async move {
            first_connection
                .request("first", Value::Null, Duration::from_secs(1))
                .await
        });
        let first_request: IpcRequest = read_frame(&mut plugin_stream)
            .await
            .expect("read first request");

        let second_connection = Arc::clone(&connection);
        let second = tokio::spawn(async move {
            second_connection
                .request("second", Value::Null, Duration::from_secs(1))
                .await
        });
        let second_request: IpcRequest = read_frame(&mut plugin_stream)
            .await
            .expect("read second request");
        assert_ne!(first_request.id, second_request.id);

        for request in [&second_request, &first_request] {
            send_frame(
                &mut plugin_stream,
                &IpcResponse {
                    id: request.id,
                    ok: true,
                    result: Some(serde_json::json!({"request": request.id})),
                    error: None,
                },
            )
            .await
            .expect("send out-of-order response");
        }

        assert_eq!(
            first.await.expect("join first").expect("first response"),
            serde_json::json!({"request": first_request.id})
        );
        assert_eq!(
            second.await.expect("join second").expect("second response"),
            serde_json::json!({"request": second_request.id})
        );
    }
}
