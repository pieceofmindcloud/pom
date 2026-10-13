//! IPC supervisor of a plugin, through a helper process.
//!
//! This module does not load a dynamic library. It starts the `pom-plugin-host`
//! executable pointed at the binary of ONE plugin, sends framed JSON frames and
//! breaks the circuit when the helper process fails or does not answer within
//! the timeout. Each instance is its own process: nothing here is global, so
//! `crate::plugins::PluginManager` can keep one instance per active plugin_code,
//! concurrent, without stepping on each other.

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::time::{timeout, Duration};

const MAX_FRAME_BYTES: usize = enterprise_protocol::MAX_IPC_FRAME_BYTES;
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
/// The initial "ready" handshake can compete with a heavy boot (a large model
/// loading, a machine still recovering from an OOM) and take much longer than
/// an ordinary IPC call. A short timeout here does not really fail the plugin -
/// it only drops the connection and, without `kill_on_drop`, left the
/// `pom-plugin-host` process orphaned and running with no `PluginRuntime`
/// pointing at it: audit events silently fell into the legacy fallback while
/// the process stayed alive in the OS.
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
    /// The host and everything it starts; dropping it ends the tree (Windows).
    _tree: Option<crate::process_tree::ProcessTree>,
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
            .map_err(|_| anyhow::anyhow!("plugin host IPC identifiers exhausted"))?;
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
            return Err(error).with_context(|| format!("send IPC request {method}"));
        }

        let response = match timeout(request_timeout, &mut response_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => anyhow::bail!("plugin host IPC response channel was closed"),
            Err(_) => match response_rx.try_recv() {
                Ok(response) => response,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    anyhow::bail!("plugin host IPC response channel was closed")
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    anyhow::bail!("timeout do plugin host")
                }
            },
        }
        .map_err(anyhow::Error::msg)?;

        if response.id != id {
            anyhow::bail!("IPC response incompatible with request {id}");
        }
        if !response.ok {
            anyhow::bail!(
                "plugin host refused {}: {}",
                method,
                response.error.unwrap_or_else(|| "unknown error".into())
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
                    let error = format!("reading the plugin host IPC channel failed: {error:#}");
                    if expected_close.load(Ordering::Acquire) {
                        tracing::debug!(%error, "plugin host IPC response channel ended during shutdown");
                    } else {
                        tracing::warn!(%error, "plugin host IPC response channel ended");
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
                    "plugin host IPC response arrived without a pending request"
                );
            }
        }
    }
}

impl PluginHost {
    pub(crate) async fn start(plugin_code: &str, plugin: &Path, database: &Path) -> Result<Self> {
        let host = host_executable()?;
        let mut command = Command::new(&host);
        command
            .arg(plugin)
            .arg(plugin_code)
            .env("POM_PLUGIN_DB", database)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Into POM's log, line by line. Inherited, the host's stderr (and
            // that of the node/npm processes plugins start) went nowhere on
            // Windows, where pom.exe has no console, and a plugin that failed
            // there left no trace of why.
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // `pom.exe` is a GUI program with no console, so Windows opened a
        // console window for the host and left it on screen. CREATE_NO_WINDOW
        // gives the host a console without a window, which the processes it
        // starts (node, npm, code-server) inherit too.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command
            .spawn()
            .with_context(|| format!("start pom-plugin-host in {}", host.display()))?;
        #[cfg(windows)]
        let tree = child
            .raw_handle()
            .and_then(crate::process_tree::ProcessTree::adopt);
        if let Some(stderr) = child.stderr.take() {
            let code = plugin_code.to_owned();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        tracing::info!(plugin_code = %code, "plugin: {line}");
                    }
                }
            });
        }
        #[cfg(not(windows))]
        let tree = Some(crate::process_tree::ProcessTree);
        let stdin = match child.stdin.take().context("capture the plugin host stdin") {
            Ok(stdin) => stdin,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let stdout = match child
            .stdout
            .take()
            .context("capture the plugin host stdout")
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
                _tree: tree,
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
                anyhow::bail!("plugin host did not become ready: response {ready}")
            }
            Ok(Err(error)) => {
                supervisor.kill().await;
                Err(error).context("plugin host ready handshake failed")
            }
            Err(_) => {
                supervisor.kill().await;
                anyhow::bail!(
                    "plugin host ready handshake did not answer in {:?}",
                    READY_TIMEOUT
                )
            }
        }
    }

    /// Kills the helper process without going through the `shutdown` protocol -
    /// used when the initial handshake fails and there is no healthy IPC
    /// connection to negotiate a clean shutdown.
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
            child.kill().await.context("shut down the plugin host")?;
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
    let bytes = serde_json::to_vec(value).context("serialize IPC frame")?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        anyhow::bail!("IPC frame outside the limit");
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        anyhow::bail!("IPC frame outside the limit");
    }
    let mut bytes = vec![0_u8; size];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("parse IPC response")
}

fn host_executable() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("POM_PLUGIN_HOST") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        anyhow::bail!("POM_PLUGIN_HOST does not point to a file");
    }
    let current = std::env::current_exe().context("locate the pom executable")?;
    let sibling = current
        .parent()
        .context("locate the directory of the pom executable")?
        .join(if cfg!(windows) {
            "pom-plugin-host.exe"
        } else {
            "pom-plugin-host"
        });
    if sibling.is_file() {
        return Ok(sibling);
    }
    anyhow::bail!("pom-plugin-host not found next to {}", current.display())
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
