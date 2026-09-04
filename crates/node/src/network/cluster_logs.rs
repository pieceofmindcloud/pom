//! Processo de captura e transporte dos logs de uma implantação.
//!
//! Este módulo é deliberadamente independente do túnel de inferência. A camada
//! de `tracing` só usa `try_send` numa fila limitada; o escritor e o envio por
//! libp2p vivem em tasks próprias e podem perder linhas quando estão ocupados.

use crate::network::Network;
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::prelude::*;
use libp2p::{request_response, PeerId, StreamProtocol};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock as TokioRwLock};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

pub const CLUSTER_LOGS_PROTOCOL: &str = "/pom/cluster-logs/1.0.0";
pub const LOG_CAPTURE_QUEUE_CAPACITY: usize = 1024;
pub const LOG_TRANSPORT_QUEUE_CAPACITY: usize = 32;
pub const LOG_BATCH_MAX_ENTRIES: usize = 64;
pub const LOG_BATCH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_LOG_PAYLOAD_BYTES: usize = 256 * 1024;
const LOG_QUERY_DEFAULT_LIMIT: usize = 500;
const LOG_QUERY_MAX_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub node_id: String,
    pub node_name: String,
    pub deploy_id: String,
    pub origin_at: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogBatch {
    pub source_node_id: String,
    pub entries: Vec<LogEntry>,
    pub dropped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogResponse {
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredLogEntry {
    pub received_seq: u64,
    pub received_at: String,
    pub node_id: String,
    pub node_name: String,
    pub deploy_id: String,
    pub origin_at: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogList {
    pub entries: Vec<StoredLogEntry>,
    pub dropped: u64,
}

#[derive(Debug, Clone)]
pub struct LogQueue {
    sender: mpsc::Sender<LogEntry>,
    dropped: Arc<AtomicU64>,
}

impl LogQueue {
    pub fn with_capacity(capacity: usize) -> (Self, mpsc::Receiver<LogEntry>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }

    pub fn try_push(&self, entry: LogEntry) -> bool {
        match self.sender.try_send(entry) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
struct LogContext {
    node_id: String,
    node_name: String,
    deploy_id: String,
}

/// Handle used by both the tracing layer and the runtime/policy tasks.
pub struct LogCapture {
    queue: LogQueue,
    context: ArcSwap<LogContext>,
    enabled: AtomicBool,
}

impl LogCapture {
    pub fn new(capacity: usize) -> (Arc<Self>, mpsc::Receiver<LogEntry>) {
        let (queue, receiver) = LogQueue::with_capacity(capacity);
        (
            Arc::new(Self {
                queue,
                context: ArcSwap::from_pointee(LogContext::default()),
                enabled: AtomicBool::new(false),
            }),
            receiver,
        )
    }

    pub fn set_identity(&self, node_id: impl Into<String>, node_name: impl Into<String>) {
        let node_id = node_id.into();
        let node_name = node_name.into();
        self.context.rcu(|current| {
            let mut next = (**current).clone();
            next.node_id = node_id.clone();
            next.node_name = node_name.clone();
            Arc::new(next)
        });
    }

    pub fn set_deploy_id(&self, deploy_id: impl Into<String>) {
        let deploy_id = deploy_id.into();
        self.context.rcu(|current| {
            let mut next = (**current).clone();
            next.deploy_id = deploy_id.clone();
            Arc::new(next)
        });
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn record(&self, event: &Event<'_>) {
        if !self.enabled() {
            return;
        }
        let context = self.context.load();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let message = visitor.render();
        let _ = self.queue.try_push(LogEntry {
            node_id: context.node_id.clone(),
            node_name: context.node_name.clone(),
            deploy_id: context.deploy_id.clone(),
            origin_at: chrono::Utc::now().to_rfc3339(),
            level: event.metadata().level().to_string(),
            target: event.metadata().target().to_string(),
            message,
        });
    }

    fn take_dropped(&self) -> u64 {
        self.queue.take_dropped()
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl EventVisitor {
    fn render(self) -> String {
        match (self.message, self.fields.is_empty()) {
            (Some(message), true) => message,
            (Some(message), false) => format!("{message} {}", self.fields.join(" ")),
            (None, false) => self.fields.join(" "),
            (None, true) => String::new(),
        }
    }
}

impl tracing::field::Visit for EventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered.trim_matches('"').to_string());
        } else {
            self.fields.push(format!("{}={rendered}", field.name()));
        }
    }
}

pub struct CaptureLayer {
    capture: Arc<LogCapture>,
}

impl CaptureLayer {
    pub fn new(capture: Arc<LogCapture>) -> Self {
        Self { capture }
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.capture.record(event);
    }
}

#[derive(Debug, Clone)]
pub struct LogOutbound {
    pub peer_id: PeerId,
    pub addr_candidates: Vec<String>,
    pub request: LogBatch,
}

#[derive(Debug, Clone, Default)]
pub struct ClusterLogsCodec;

async fn read_msg<T, R>(io: &mut R) -> std::io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0_u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_LOG_PAYLOAD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cluster log frame exceeds the size bound",
        ));
    }
    let mut data = vec![0_u8; len];
    io.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

async fn write_msg<T, W>(io: &mut W, message: &T) -> std::io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin + Send,
{
    let data = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    if data.len() > MAX_LOG_PAYLOAD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cluster log frame exceeds the size bound",
        ));
    }
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await
}

#[async_trait]
impl request_response::Codec for ClusterLogsCodec {
    type Protocol = StreamProtocol;
    type Request = LogBatch;
    type Response = LogResponse;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &request).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &response).await
    }
}

pub fn cluster_logs_behaviour() -> request_response::Behaviour<ClusterLogsCodec> {
    request_response::Behaviour::new(
        [(
            StreamProtocol::new(CLUSTER_LOGS_PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default().with_request_timeout(Duration::from_secs(3)),
    )
}

enum PersistCommand {
    Batch(LogBatch),
}

/// Cluster log state. Its writer queue is independent from the capture queue
/// and from the libp2p swarm queue.
pub struct ClusterLogs {
    capture: Arc<LogCapture>,
    database: Arc<crate::db::PomDb>,
    writer: std::sync::mpsc::SyncSender<PersistCommand>,
    dropped: AtomicU64,
    principal: AtomicBool,
    started: AtomicBool,
    local_receiver: Mutex<Option<mpsc::Receiver<LogEntry>>>,
    live: broadcast::Sender<StoredLogEntry>,
}

impl ClusterLogs {
    pub fn new(
        capture: Arc<LogCapture>,
        local_receiver: mpsc::Receiver<LogEntry>,
        database: Arc<crate::db::PomDb>,
    ) -> Arc<Self> {
        let (writer, receiver) = std::sync::mpsc::sync_channel(256);
        let writer_database = database.clone();
        std::thread::Builder::new()
            .name("pom-cluster-log-writer".into())
            .spawn(move || {
                while let Ok(PersistCommand::Batch(batch)) = receiver.recv() {
                    if let Err(error) =
                        writer_database.append_cluster_logs(&batch.entries, batch.dropped)
                    {
                        eprintln!("cluster log persistence failed: {error:#}");
                    }
                }
            })
            .ok();
        let (live, _) = broadcast::channel(256);
        Arc::new(Self {
            capture,
            database,
            writer,
            dropped: AtomicU64::new(0),
            principal: AtomicBool::new(false),
            started: AtomicBool::new(false),
            local_receiver: Mutex::new(Some(local_receiver)),
            live,
        })
    }

    pub fn capture(&self) -> Arc<LogCapture> {
        self.capture.clone()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.capture.set_enabled(enabled);
    }

    pub fn enabled(&self) -> bool {
        self.capture.enabled()
    }

    pub fn set_identity(&self, node_id: impl Into<String>, node_name: impl Into<String>) {
        self.capture.set_identity(node_id, node_name);
    }

    pub fn set_principal(&self, principal: bool) {
        self.principal.store(principal, Ordering::Release);
    }

    pub fn try_ingest_remote(&self, batch: LogBatch) -> bool {
        if !self.principal.load(Ordering::Acquire) || batch.entries.len() > LOG_BATCH_MAX_ENTRIES {
            self.dropped.fetch_add(
                batch.entries.len() as u64 + batch.dropped,
                Ordering::Relaxed,
            );
            return false;
        }
        self.try_ingest(batch)
    }

    fn try_ingest(&self, batch: LogBatch) -> bool {
        for entry in &batch.entries {
            let _ = self.live.send(StoredLogEntry {
                received_seq: 0,
                received_at: String::new(),
                node_id: entry.node_id.clone(),
                node_name: entry.node_name.clone(),
                deploy_id: entry.deploy_id.clone(),
                origin_at: entry.origin_at.clone(),
                level: entry.level.clone(),
                target: entry.target.clone(),
                message: entry.message.clone(),
            });
        }
        match self.writer.try_send(PersistCommand::Batch(batch.clone())) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(
                    batch.entries.len() as u64 + batch.dropped,
                    Ordering::Relaxed,
                );
                false
            }
        }
    }

    pub fn start(
        self: &Arc<Self>,
        network: Arc<Network>,
        assignment: Arc<TokioRwLock<Option<crate::model::LayerAssignment>>>,
        outbound: mpsc::Sender<LogOutbound>,
    ) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(mut receiver) = self
            .local_receiver
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        else {
            return;
        };
        let service = self.clone();
        tokio::spawn(async move {
            let mut pending = Vec::with_capacity(LOG_BATCH_MAX_ENTRIES);
            let mut ticker = tokio::time::interval(LOG_BATCH_INTERVAL);
            loop {
                tokio::select! {
                    entry = receiver.recv() => {
                        let Some(entry) = entry else { break };
                        pending.push(entry);
                        if pending.len() >= LOG_BATCH_MAX_ENTRIES {
                            flush_local(&service, &network, &mut pending, &outbound).await;
                        }
                    }
                    _ = ticker.tick() => {
                        flush_local(&service, &network, &mut pending, &outbound).await;
                    }
                }
            }
        });

        let service = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            loop {
                ticker.tick().await;
                let deploy_id = assignment
                    .read()
                    .await
                    .as_ref()
                    .map(|assignment| assignment.deploy_id.clone())
                    .unwrap_or_default();
                service.capture.set_deploy_id(deploy_id);
            }
        });
    }

    pub fn try_send_dropped(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
    }

    pub async fn list(&self, deploy_id: Option<&str>, limit: Option<usize>) -> Result<LogList> {
        let database = self.database.clone();
        let deploy_id = deploy_id.map(str::to_owned);
        let limit = limit
            .unwrap_or(LOG_QUERY_DEFAULT_LIMIT)
            .clamp(1, LOG_QUERY_MAX_LIMIT);
        let entries =
            tokio::task::spawn_blocking(move || database.cluster_logs(deploy_id.as_deref(), limit))
                .await??;
        let dropped = self
            .database
            .cluster_log_dropped()?
            .saturating_add(self.dropped.load(Ordering::Relaxed))
            .saturating_add(self.capture.queue.dropped());
        Ok(LogList { entries, dropped })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoredLogEntry> {
        self.live.subscribe()
    }
}

async fn flush_local(
    service: &ClusterLogs,
    network: &Network,
    pending: &mut Vec<LogEntry>,
    outbound: &mpsc::Sender<LogOutbound>,
) {
    let dropped = service.capture.take_dropped();
    if pending.is_empty() && dropped == 0 {
        return;
    }
    let entries = std::mem::take(pending);
    let batch = LogBatch {
        source_node_id: network.node_id.clone(),
        entries,
        dropped,
    };
    if service.principal.load(Ordering::Acquire) {
        service.try_ingest(batch);
        return;
    }
    let Some((peer_id, addr_candidates)) = network.principal_peer().await else {
        service.try_send_dropped(batch.entries.len() as u64 + batch.dropped);
        return;
    };
    if outbound
        .try_send(LogOutbound {
            peer_id,
            addr_candidates,
            request: batch.clone(),
        })
        .is_err()
    {
        service.try_send_dropped(batch.entries.len() as u64 + batch.dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            node_id: "node-a".into(),
            node_name: "salem".into(),
            deploy_id: "deploy-1".into(),
            origin_at: "2026-09-04T12:00:00Z".into(),
            level: "INFO".into(),
            target: "node::test".into(),
            message: message.into(),
        }
    }

    #[test]
    fn fila_limitada_descarta_sem_bloquear_e_conta_perdas() {
        let (queue, mut receiver) = LogQueue::with_capacity(1);

        assert!(queue.try_push(entry("primeira")));
        assert!(!queue.try_push(entry("segunda")));
        assert_eq!(queue.dropped(), 1);
        assert_eq!(receiver.try_recv().unwrap().message, "primeira");
    }

    #[test]
    fn emissao_de_tracing_cheia_descarta_e_conta_sem_espera() {
        use tracing_subscriber::prelude::*;

        let (capture, mut receiver) = LogCapture::new(1);
        capture.set_identity("node-a", "salem");
        capture.set_enabled(true);
        let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(capture.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "cluster_logs_test", "primeira");
            tracing::info!(target: "cluster_logs_test", "segunda");
        });

        assert_eq!(capture.queue.dropped(), 1);
        assert_eq!(receiver.try_recv().unwrap().message, "primeira");
    }

    #[test]
    fn microbenchmark_de_enfileiramento_e_lote_por_linha() {
        const ITERATIONS: usize = 50_000;
        let template = entry("benchmark");
        let (queue, mut receiver) = LogQueue::with_capacity(ITERATIONS);

        let enqueue_started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            assert!(std::hint::black_box(queue.try_push(template.clone())));
        }
        let enqueue_elapsed = enqueue_started.elapsed();

        let batching_started = std::time::Instant::now();
        let mut pending = Vec::with_capacity(LOG_BATCH_MAX_ENTRIES);
        let mut batches = 0;
        let mut received = 0;
        while let Ok(log) = receiver.try_recv() {
            pending.push(std::hint::black_box(log));
            received += 1;
            if pending.len() == LOG_BATCH_MAX_ENTRIES {
                std::hint::black_box(&pending);
                pending.clear();
                batches += 1;
            }
        }
        if !pending.is_empty() {
            std::hint::black_box(&pending);
            batches += 1;
        }
        let batching_elapsed = batching_started.elapsed();

        assert_eq!(received, ITERATIONS);
        assert_eq!(queue.dropped(), 0);
        assert_eq!(batches, ITERATIONS.div_ceil(LOG_BATCH_MAX_ENTRIES));
        eprintln!(
            "cluster_logs_microbenchmark entries={ITERATIONS} batches={batches} enqueue_ns_per_line={} batch_ns_per_line={}",
            enqueue_elapsed.as_nanos() / ITERATIONS as u128,
            batching_elapsed.as_nanos() / ITERATIONS as u128,
        );
    }

    #[test]
    fn lote_preserva_a_ordem_de_chegada_e_o_deploy_id() {
        let first = entry("chegou primeiro");
        let second = entry("chegou depois");
        let batch = LogBatch {
            source_node_id: "node-a".into(),
            entries: vec![first.clone(), second.clone()],
            dropped: 0,
        };

        assert_eq!(batch.entries, vec![first, second]);
        assert_eq!(batch.entries[0].deploy_id, "deploy-1");
    }
}
