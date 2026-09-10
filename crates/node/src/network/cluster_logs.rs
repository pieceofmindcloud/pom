//! Processo de captura e transporte dos logs de uma implantação.
//!
//! Este módulo é deliberadamente independente do túnel de inferência. A camada
//! de `tracing` só usa `try_send` numa fila limitada; o escritor e o envio por
//! libp2p vivem em tasks próprias e podem perder linhas quando estão ocupados.

use crate::db::ClusterLogsPruneResult;
use crate::network::Network;
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::prelude::*;
use libp2p::{request_response, PeerId, StreamProtocol};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
/// Retention keeps the last seven days, capped at 500,000 rows. A burst of
/// logs therefore cannot grow the database beyond the lower of the time and
/// count windows.
pub const LOG_RETENTION_MAX_ROWS: u64 = 500_000;
pub const LOG_RETENTION_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LOG_RETENTION_BATCH_SIZE: usize = 256;
const LOG_RETENTION_MAX_BATCHES_PER_CYCLE: usize = 128;
const LOG_RETENTION_PAUSE: Duration = Duration::from_millis(50);
const LOG_RETENTION_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_LOG_PAYLOAD_BYTES: usize = 256 * 1024;
const LOG_QUERY_DEFAULT_LIMIT: usize = 500;
const LOG_QUERY_MAX_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClusterLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Default for ClusterLogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl ClusterLogLevel {
    fn rank(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
        }
    }

    fn from_rank(rank: u8) -> Self {
        match rank {
            0 => Self::Trace,
            1 => Self::Debug,
            3 => Self::Warn,
            4 => Self::Error,
            _ => Self::Info,
        }
    }

    fn allows(self, level: &tracing::Level) -> bool {
        let rank = match *level {
            tracing::Level::TRACE => 0,
            tracing::Level::DEBUG => 1,
            tracing::Level::INFO => 2,
            tracing::Level::WARN => 3,
            tracing::Level::ERROR => 4,
        };
        rank >= self.rank()
    }
}

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
    min_level: AtomicU8,
}

impl LogCapture {
    pub fn new(capacity: usize) -> (Arc<Self>, mpsc::Receiver<LogEntry>) {
        let (queue, receiver) = LogQueue::with_capacity(capacity);
        (
            Arc::new(Self {
                queue,
                context: ArcSwap::from_pointee(LogContext::default()),
                enabled: AtomicBool::new(false),
                min_level: AtomicU8::new(ClusterLogLevel::default().rank()),
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

    pub fn set_min_level(&self, level: ClusterLogLevel) {
        self.min_level.store(level.rank(), Ordering::Release);
    }

    pub fn min_level(&self) -> ClusterLogLevel {
        ClusterLogLevel::from_rank(self.min_level.load(Ordering::Acquire))
    }

    fn record(&self, event: &Event<'_>) {
        if !self.enabled() || !self.min_level().allows(event.metadata().level()) {
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

        if self.principal.load(Ordering::Acquire) {
            let service = self.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(LOG_RETENTION_INTERVAL);
                loop {
                    ticker.tick().await;
                    let cutoff = (chrono::Utc::now()
                        - chrono::Duration::from_std(LOG_RETENTION_AGE).unwrap_or_default())
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    if let Err(error) = run_retention_cycle(
                        service.database.clone(),
                        &cutoff,
                        LOG_RETENTION_MAX_ROWS,
                        LOG_RETENTION_BATCH_SIZE,
                        LOG_RETENTION_MAX_BATCHES_PER_CYCLE,
                        LOG_RETENTION_PAUSE,
                    )
                    .await
                    {
                        tracing::warn!("poda dos logs do cluster falhou: {error:#}");
                    }
                }
            });
        }
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

async fn run_retention_cycle(
    database: Arc<crate::db::PomDb>,
    older_than: &str,
    max_rows: u64,
    batch_size: usize,
    max_batches: usize,
    pause: Duration,
) -> Result<usize> {
    let mut deleted_total = 0;
    for batch_index in 0..max_batches {
        let database = database.clone();
        let older_than = older_than.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            database.try_prune_cluster_logs(&older_than, max_rows, batch_size)
        })
        .await??;
        let ClusterLogsPruneResult::Deleted {
            count,
            space_reclaimed,
        } = result
        else {
            break;
        };
        deleted_total += count;
        if !space_reclaimed {
            break;
        }
        if batch_index + 1 < max_batches && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }
    Ok(deleted_total)
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
    fn captura_descarta_debug_por_padrao_sem_filtrar_o_subscriber() {
        use std::sync::atomic::AtomicUsize;
        use tracing_subscriber::layer::Context;
        use tracing_subscriber::prelude::*;

        struct Counter(Arc<AtomicUsize>);
        impl<S> Layer<S> for Counter
        where
            S: Subscriber,
        {
            fn on_event(&self, _event: &Event<'_>, _ctx: Context<'_, S>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let (capture, mut receiver) = LogCapture::new(8);
        capture.set_enabled(true);
        let observed = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(Counter(observed.clone()))
            .with(CaptureLayer::new(capture));

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "cluster_logs_test", "nao persistir trace");
            tracing::debug!(target: "cluster_logs_test", "nao persistir");
            tracing::info!(target: "cluster_logs_test", "persistir");
            tracing::warn!(target: "cluster_logs_test", "persistir warn");
            tracing::error!(target: "cluster_logs_test", "persistir error");
        });

        assert_eq!(observed.load(Ordering::Relaxed), 5);
        assert_eq!(receiver.try_recv().unwrap().message, "persistir");
        assert_eq!(receiver.try_recv().unwrap().message, "persistir warn");
        assert_eq!(receiver.try_recv().unwrap().message, "persistir error");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn captura_aceita_debug_quando_o_limiar_e_configurado() {
        use tracing_subscriber::prelude::*;

        let (capture, mut receiver) = LogCapture::new(8);
        capture.set_min_level(ClusterLogLevel::Debug);
        capture.set_enabled(true);
        let subscriber = tracing_subscriber::registry().with(CaptureLayer::new(capture));

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "cluster_logs_test", "trace nao configuravel");
            tracing::debug!(target: "cluster_logs_test", "debug configuravel");
        });

        assert_eq!(receiver.try_recv().unwrap().message, "debug configuravel");
        assert!(receiver.try_recv().is_err());
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

    #[tokio::test]
    async fn ciclo_de_retencao_consumes_apenas_lotes_do_seu_orcamento() {
        let path = std::env::temp_dir().join(format!(
            "pom-cluster-logs-cycle-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let database = Arc::new(crate::db::PomDb::open(&path).expect("abrir banco"));
        for index in 0..7 {
            database
                .with_write("preparar log do ciclo", |connection| {
                    connection.execute(
                        "INSERT INTO cluster_logs (
                            node_id, node_name, deploy_id, origin_at, received_at,
                            level, target, message
                         ) VALUES ('node-a', 'test', 'deploy-1', ?, ?, 'INFO', 'test', ?)",
                        rusqlite::params![
                            "2099-01-01T00:00:00Z",
                            "2099-01-01T00:00:00Z",
                            format!("mensagem-{index}")
                        ],
                    )?;
                    Ok(())
                })
                .expect("preparar log");
        }

        let deleted = run_retention_cycle(
            database.clone(),
            "2026-09-03T00:00:00Z",
            3,
            2,
            2,
            Duration::ZERO,
        )
        .await
        .expect("executar ciclo");

        assert_eq!(deleted, 4);
        assert_eq!(database.cluster_logs(None, 100).unwrap().len(), 3);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[tokio::test]
    async fn ciclo_de_retencao_poda_linhas_aceitas_pelo_writer_do_principal() {
        let path = std::env::temp_dir().join(format!(
            "pom-cluster-logs-writer-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let database = Arc::new(crate::db::PomDb::open(&path).expect("abrir banco"));
        let (capture, receiver) = LogCapture::new(8);
        let service = ClusterLogs::new(capture, receiver, database.clone());
        service.set_principal(true);
        assert!(service.try_ingest_remote(LogBatch {
            source_node_id: "node-a".into(),
            entries: vec![entry("aceito pelo writer")],
            dropped: 0,
        }));

        for _ in 0..20 {
            if database.cluster_logs(None, 100).unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(database.cluster_logs(None, 100).unwrap().len(), 1);
        database
            .with_write("envelhecer log do teste", |connection| {
                connection.execute(
                    "UPDATE cluster_logs SET received_at = '2026-01-01T00:00:00Z'",
                    [],
                )?;
                Ok(())
            })
            .expect("envelhecer log");

        let deleted = run_retention_cycle(
            database.clone(),
            "2026-09-03T00:00:00Z",
            500_000,
            256,
            1,
            Duration::ZERO,
        )
        .await
        .expect("executar poda");

        assert_eq!(deleted, 1);
        assert!(database.cluster_logs(None, 100).unwrap().is_empty());
        drop(service);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
