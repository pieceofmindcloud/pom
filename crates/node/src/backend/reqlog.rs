//! Persistent request audit trail.
//!
//! The audit record is created before a generation starts and completed by the
//! task that observes its terminal event. SQLite is durable, while a dedicated
//! bounded writer keeps disk I/O out of the inference path.

use crate::auth::ConnectionAuth;
use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::Instant;
use tracing::{error, warn};

const WRITER_QUEUE_CAPACITY: usize = 4096;
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const STALE_AFTER_MINUTES: i64 = 15;

/// Monotonic request activity shared by the gateway and the local inference
/// server. The idle policy reads this atomically from the existing monitor;
/// it never starts a timer or does I/O in the inference path.
#[derive(Debug, Default)]
pub struct RequestActivity {
    last_ms: AtomicU64,
}

impl RequestActivity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn mark_now(&self) {
        self.mark_at(monotonic_ms());
    }

    pub fn mark_at(&self, timestamp_ms: u64) {
        self.last_ms.store(timestamp_ms, Ordering::Relaxed);
    }

    pub fn is_idle(&self, idle_minutes: u32) -> bool {
        self.is_idle_at(monotonic_ms(), idle_minutes)
    }

    pub fn is_idle_at(&self, now_ms: u64, idle_minutes: u32) -> bool {
        now_ms.saturating_sub(self.last_ms.load(Ordering::Relaxed))
            >= u64::from(idle_minutes).saturating_mul(60_000)
    }
}

fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Identity of the credential, without its secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub key_id: String,
    pub key_label: String,
    pub key_node_id: Option<String>,
}

impl Caller {
    pub fn operator() -> Self {
        Self {
            key_id: "connection-key".into(),
            key_label: "chave de conexão".into(),
            key_node_id: None,
        }
    }

    pub fn anonymous() -> Self {
        Self {
            key_id: "anonymous".into(),
            key_label: "sem token".into(),
            key_node_id: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditRequest {
    pub id: String,
    pub received_at: String,
    pub route: String,
    pub model: String,
    pub caller: Caller,
    pub entry_node_id: String,
    pub prompt: String,
    pub request_details: JsonValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStatus {
    Ok,
    Partial,
    Error,
    Abandoned,
    Incomplete,
}

impl AuditStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Error => "error",
            Self::Abandoned => "abandoned",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditCompletion {
    pub completed_at: String,
    pub result: Option<String>,
    pub telemetry: Option<JsonValue>,
    pub timings: Option<crate::gateway::GenerationTimings>,
    pub source: String,
    pub hops: Vec<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub duration_ms: Option<u64>,
    pub status: AuditStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub received_at: String,
    pub completed_at: Option<String>,
    pub route: String,
    pub model: String,
    pub api_key_id: String,
    pub api_key_label: String,
    pub api_key_node_id: Option<String>,
    pub entry_node_id: String,
    pub prompt: String,
    pub request_details: JsonValue,
    pub result: Option<String>,
    pub telemetry: JsonValue,
    pub timings: Option<crate::gateway::GenerationTimings>,
    pub hops: Vec<String>,
    pub source: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub duration_ms: Option<u64>,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditQuery {
    pub node_id: Option<String>,
    pub api_key: Option<String>,
    pub prompt: Option<String>,
    #[serde(rename = "from")]
    pub received_from: Option<String>,
    #[serde(rename = "to")]
    pub received_to: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditList {
    pub entries: Vec<Entry>,
    pub total: u64,
    pub errors: u64,
    pub incomplete: u64,
    pub by_token: HashMap<String, u64>,
    pub by_node: HashMap<String, u64>,
    pub storage: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageStats {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelStats {
    pub requests: u64,
    pub average_tokens_per_sec: Option<f64>,
    pub average_inference_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerStats {
    pub requests: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub network_ms: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStats {
    pub eligible_requests: u64,
    pub hit_requests: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditStats {
    pub total: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub average_inference_ms: Option<f64>,
    pub average_ttft_ms: Option<f64>,
    pub by_token: BTreeMap<String, u64>,
    pub by_node: BTreeMap<String, u64>,
    pub by_day: BTreeMap<String, UsageStats>,
    pub by_model: BTreeMap<String, ModelStats>,
    pub peer_consumption: BTreeMap<String, PeerStats>,
    pub terminal_states: BTreeMap<String, u64>,
    pub cache: CacheStats,
    pub storage: &'static str,
}

enum Command {
    Begin(AuditRequest),
    Complete {
        id: String,
        completion: AuditCompletion,
    },
    #[cfg(test)]
    Flush(mpsc::Sender<()>),
}

/// Handle shared by gateway tasks and the authenticated query route.
#[derive(Clone)]
pub struct AuditLog {
    database: Arc<crate::db::PomDb>,
    sender: mpsc::SyncSender<Command>,
}

impl AuditLog {
    /// Start the dedicated writer. Startup failures are logged and deliberately
    /// do not prevent the gateway from serving inference.
    pub fn new(database: Arc<crate::db::PomDb>) -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        let writer_database = database.clone();
        if let Err(e) = thread::Builder::new()
            .name("pom-audit-writer".into())
            .spawn(move || run_writer(writer_database, receiver))
        {
            error!(error = %e, "não foi possível iniciar o escritor da auditoria");
        }
        Arc::new(Self { database, sender })
    }

    /// Queue the initial row without waiting for SQLite or the filesystem.
    pub fn begin(&self, request: AuditRequest) {
        let request_id = request.id.clone();
        if let Err(e) = self.sender.try_send(Command::Begin(request)) {
            error!(
                operation = "iniciar auditoria",
                request_id = %request_id,
                error = %e,
                "operação de auditoria perdida antes da gravação"
            );
        }
    }

    /// Queue completion data for the same request id.
    pub fn complete(&self, id: String, completion: AuditCompletion) {
        let request_id = id.clone();
        if let Err(e) = self.sender.try_send(Command::Complete { id, completion }) {
            error!(
                operation = "concluir auditoria",
                request_id = %request_id,
                error = %e,
                "operação de auditoria perdida antes da gravação"
            );
        }
    }

    pub async fn list(&self, query: AuditQuery) -> Result<AuditList> {
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            database.with_write("consultar auditoria", |connection| {
                AuditDb::from_shared(connection).list(&query)
            })
        })
        .await
        .context("aguardar consulta do banco de auditoria")?
    }

    /// Read the completed trace for one request without putting SQLite work in
    /// the HTTP or inference executor.
    pub async fn telemetry(&self, id: &str) -> Result<Option<JsonValue>> {
        let database = self.database.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            database.with_write("consultar telemetria", |connection| {
                AuditDb::from_shared(connection).telemetry(&id)
            })
        })
        .await
        .context("aguardar consulta da telemetria")?
    }

    /// Read recent completed traces for the dashboard. The caller filters them
    /// against the requested topology and can skip a trace for another model.
    pub async fn recent_telemetries(&self, limit: usize) -> Result<Vec<JsonValue>> {
        let database = self.database.clone();
        let limit = limit.clamp(1, 100);
        tokio::task::spawn_blocking(move || {
            database.with_write("consultar última telemetria", |connection| {
                AuditDb::from_shared(connection).recent_telemetries(limit)
            })
        })
        .await
        .context("aguardar consulta da última telemetria")?
    }

    pub async fn stats(&self, query: AuditQuery) -> Result<AuditStats> {
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            database.with_write("agregar auditoria", |connection| {
                AuditDb::from_shared(connection).stats(&query)
            })
        })
        .await
        .context("aguardar agregação do banco de auditoria")?
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(Command::Flush(sender))
            .expect("enfileirar barreira da auditoria");
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("aguardar escritor da auditoria");
    }
}

fn run_writer(database: Arc<crate::db::PomDb>, receiver: mpsc::Receiver<Command>) {
    if let Err(error) = database.with_write("recuperar auditorias incompletas", |connection| {
        AuditDb::from_shared(connection)
            .mark_in_progress_incomplete("servidor reiniciou antes do fim")
    }) {
        error!(
            operation = "recuperar auditorias incompletas",
            error = %error,
            "não foi possível inicializar o escritor da auditoria"
        );
        return;
    }
    while let Ok(command) = receiver.recv() {
        let (operation, request_id, result) = match command {
            Command::Begin(request) => {
                let request_id = request.id.clone();
                let result = database.with_write("iniciar auditoria", |connection| {
                    AuditDb::from_shared(connection).begin(&request)
                });
                ("iniciar auditoria", request_id, result)
            }
            Command::Complete { id, completion } => {
                let result = database.with_write("concluir auditoria", |connection| {
                    AuditDb::from_shared(connection).complete(&id, &completion)
                });
                ("concluir auditoria", id, result)
            }
            #[cfg(test)]
            Command::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        if let Err(e) = result {
            error!(
                operation,
                request_id = %request_id,
                error = %e,
                "falha ao gravar o registro de auditoria; operação perdida"
            );
        }
    }
}

pub(crate) trait ConnectionAccess {
    fn connection(&self) -> &Connection;
    fn connection_mut(&mut self) -> &mut Connection;
}

impl ConnectionAccess for Connection {
    fn connection(&self) -> &Connection {
        self
    }

    fn connection_mut(&mut self) -> &mut Connection {
        self
    }
}

impl<'a> ConnectionAccess for &'a mut Connection {
    fn connection(&self) -> &Connection {
        self
    }

    fn connection_mut(&mut self) -> &mut Connection {
        self
    }
}

pub struct AuditDb<C = Connection> {
    connection: C,
}

#[cfg(test)]
impl AuditDb<Connection> {
    pub fn open(connection: Connection) -> Result<Self> {
        configure_connection(&connection)?;
        ensure_schema(&connection)?;
        let mut db = Self { connection };
        db.mark_in_progress_incomplete("servidor reiniciou antes do fim")?;
        Ok(db)
    }
}

impl<'a> AuditDb<&'a mut Connection> {
    fn from_shared(connection: &'a mut Connection) -> Self {
        Self { connection }
    }
}

impl<C: ConnectionAccess> AuditDb<C> {
    fn connection(&self) -> &Connection {
        self.connection.connection()
    }

    fn connection_mut(&mut self) -> &mut Connection {
        self.connection.connection_mut()
    }

    pub fn begin(&mut self, request: &AuditRequest) -> Result<()> {
        let transaction = self.connection_mut().transaction()?;
        transaction.execute(
            "INSERT INTO audit_requests (
                id, received_at, route, model, api_key_id, api_key_label,
                api_key_node_id, entry_node_id, prompt, request_details, status
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'in_progress')",
            params![
                &request.id,
                &request.received_at,
                &request.route,
                &request.model,
                &request.caller.key_id,
                &request.caller.key_label,
                &request.caller.key_node_id,
                &request.entry_node_id,
                &request.prompt,
                serde_json::to_string(&request.request_details)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO audit_prompt_fts (request_id, prompt) VALUES (?, ?)",
            params![&request.id, &request.prompt],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn complete(&mut self, id: &str, completion: &AuditCompletion) -> Result<()> {
        let transaction = self.connection_mut().transaction()?;
        let updated = transaction.execute(
            "UPDATE audit_requests SET
                completed_at = ?, result = ?, telemetry = ?, timings = ?, source = ?, hops = ?,
                prompt_tokens = ?, completion_tokens = ?, duration_ms = ?,
                status = ?, error = ?
             WHERE id = ?",
            params![
                &completion.completed_at,
                &completion.result,
                completion
                    .telemetry
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                completion
                    .timings
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                &completion.source,
                serde_json::to_string(&completion.hops)?,
                &completion.prompt_tokens,
                &completion.completion_tokens,
                &completion.duration_ms,
                completion.status.as_str(),
                &completion.error,
                id,
            ],
        )?;
        if updated == 0 {
            anyhow::bail!("registro de auditoria {id} não existe");
        }
        transaction.execute(
            "DELETE FROM audit_request_hops WHERE request_id = ?",
            params![id],
        )?;
        for (position, node_id) in completion.hops.iter().enumerate() {
            transaction.execute(
                "INSERT INTO audit_request_hops (request_id, position, node_id) VALUES (?, ?, ?)",
                params![id, position as i64, node_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_in_progress_incomplete(&mut self, reason: &str) -> Result<()> {
        self.connection_mut().execute(
            "UPDATE audit_requests
             SET status = 'incomplete', completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 error = COALESCE(error, ?)
             WHERE status = 'in_progress'",
            params![reason],
        )?;
        Ok(())
    }

    fn mark_stale_in_progress(&mut self) -> Result<()> {
        self.connection_mut().execute(
            "UPDATE audit_requests
             SET status = 'incomplete', completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 error = COALESCE(error, 'não concluiu em 15 minutos')
             WHERE status = 'in_progress'
               AND julianday(received_at) < julianday('now', ?)",
            params![format!("-{STALE_AFTER_MINUTES} minutes")],
        )?;
        Ok(())
    }

    fn telemetry(&mut self, id: &str) -> Result<Option<JsonValue>> {
        let raw: Option<String> = self
            .connection()
            .query_row(
                "SELECT telemetry FROM audit_requests WHERE id = ? AND status IN ('ok', 'partial', 'incomplete')",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|value| serde_json::from_str(&value).ok()))
    }

    fn recent_telemetries(&mut self, limit: usize) -> Result<Vec<JsonValue>> {
        let mut statement = self.connection().prepare(
            "SELECT telemetry
             FROM audit_requests
             WHERE telemetry IS NOT NULL AND status IN ('ok', 'partial', 'incomplete')
             ORDER BY completed_at DESC, id DESC
             LIMIT ?",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| row.get::<_, String>(0))?;
        let mut traces = Vec::new();
        for row in rows {
            if let Ok(trace) = serde_json::from_str(&row?) {
                traces.push(trace);
            }
        }
        Ok(traces)
    }

    pub fn list(&mut self, query: &AuditQuery) -> Result<AuditList> {
        self.mark_stale_in_progress()?;
        let (where_sql, values) = where_clause(query);

        let (total, errors, incomplete) = self.connection().query_row(
            &format!(
                "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN status IN ('error', 'abandoned') THEN 1 ELSE 0 END), 0),
                            COALESCE(SUM(CASE WHEN status IN ('in_progress', 'incomplete') THEN 1 ELSE 0 END), 0)
                     FROM audit_requests WHERE {where_sql}"
            ),
            params_from_iter(values.clone()),
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?;

        let mut by_token = HashMap::new();
        let mut token_stmt = self.connection().prepare(&format!(
            "SELECT api_key_label, COUNT(*) FROM audit_requests WHERE {where_sql}
             GROUP BY api_key_label ORDER BY COUNT(*) DESC, api_key_label"
        ))?;
        for row in token_stmt.query_map(params_from_iter(values.clone()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })? {
            let (label, count) = row?;
            by_token.insert(label, count);
        }

        let mut by_node = HashMap::new();
        let mut node_stmt = self.connection().prepare(&format!(
            "SELECT entry_node_id, hops FROM audit_requests WHERE {where_sql}"
        ))?;
        for row in node_stmt.query_map(params_from_iter(values.clone()), |row| {
            let entry: String = row.get(0)?;
            let hops: String = row.get(1)?;
            Ok((entry, hops))
        })? {
            let (entry, hops) = row?;
            let mut nodes = vec![entry];
            if let Ok(hops) = serde_json::from_str::<Vec<String>>(&hops) {
                nodes.extend(hops);
            }
            nodes.sort();
            nodes.dedup();
            for node in nodes {
                *by_node.entry(node).or_insert(0) += 1;
            }
        }

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0);
        let mut entry_values = values;
        entry_values.push(Value::Integer(limit as i64));
        entry_values.push(Value::Integer(offset as i64));
        let mut stmt = self.connection().prepare(&format!(
            "SELECT id, received_at, completed_at, route, model, api_key_id,
                    api_key_label, api_key_node_id, entry_node_id, prompt,
                    request_details, result, telemetry, timings, hops, source,
                    prompt_tokens, completion_tokens, duration_ms, status, error
             FROM audit_requests WHERE {where_sql}
             ORDER BY received_at DESC, id DESC LIMIT ? OFFSET ?"
        ))?;
        let mut entries = Vec::new();
        for row in stmt.query_map(params_from_iter(entry_values), entry_from_row)? {
            entries.push(row?);
        }

        Ok(AuditList {
            entries,
            total,
            errors,
            incomplete,
            by_token,
            by_node,
            storage: "sqlite",
        })
    }

    pub fn stats(&mut self, query: &AuditQuery) -> Result<AuditStats> {
        self.mark_stale_in_progress()?;
        let (where_sql, values) = where_clause(query);
        let mut stmt = self.connection().prepare(&format!(
            "SELECT received_at, model, api_key_label, entry_node_id, prompt_tokens,
                    completion_tokens, duration_ms, status, timings, telemetry, hops
             FROM audit_requests WHERE {where_sql}"
        ))?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
            ))
        })?;

        let mut stats = AuditStats {
            storage: "sqlite",
            ..Default::default()
        };
        let mut inference_total = 0.0;
        let mut inference_count = 0_u64;
        let mut ttft_total = 0.0;
        let mut ttft_count = 0_u64;
        let mut model_accumulators: HashMap<String, (u64, f64, u64, f64, u64)> = HashMap::new();

        for row in rows {
            let (
                received_at,
                model,
                api_key_label,
                entry_node_id,
                prompt_tokens,
                completion_tokens,
                duration_ms,
                status,
                timings_raw,
                telemetry_raw,
                hops_raw,
            ) = row?;
            stats.total += 1;
            let input_tokens = prompt_tokens.unwrap_or_default().max(0) as u64;
            let output_tokens = completion_tokens.unwrap_or_default().max(0) as u64;
            stats.input_tokens += input_tokens;
            stats.output_tokens += output_tokens;
            *stats.by_token.entry(api_key_label).or_default() += 1;
            let day = received_at
                .split('T')
                .next()
                .unwrap_or(received_at.as_str())
                .to_string();
            let day_stats = stats.by_day.entry(day).or_default();
            day_stats.requests += 1;
            day_stats.input_tokens += input_tokens;
            day_stats.output_tokens += output_tokens;

            if let Some(duration_ms) = duration_ms {
                let duration_ms = duration_ms.max(0) as f64;
                inference_total += duration_ms;
                inference_count += 1;
                let model_stats = model_accumulators.entry(model.clone()).or_default();
                model_stats.2 += 1;
                model_stats.3 += duration_ms;
            }
            let model_stats = model_accumulators.entry(model).or_default();
            model_stats.0 += 1;
            if let Some(timings) = timings_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<crate::gateway::GenerationTimings>(raw).ok())
            {
                if timings.total_tokens_per_sec.is_finite() {
                    model_stats.1 += timings.total_tokens_per_sec;
                    model_stats.4 += 1;
                }
                ttft_total += timings.ttft_ms as f64;
                ttft_count += 1;
            }

            if status != "in_progress" {
                *stats.terminal_states.entry(status).or_default() += 1;
            }

            let mut request_nodes = HashSet::new();
            request_nodes.insert(entry_node_id);
            if let Ok(hops) = serde_json::from_str::<Vec<String>>(&hops_raw) {
                request_nodes.extend(hops);
            }
            for node_id in &request_nodes {
                *stats.by_node.entry(node_id.clone()).or_default() += 1;
                stats
                    .peer_consumption
                    .entry(node_id.clone())
                    .or_default()
                    .requests += 1;
            }

            if let Some(telemetry) = telemetry_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<JsonValue>(raw).ok())
            {
                if let Some(input_tokens) = json_u64(telemetry.get("input_tokens"))
                    .or_else(|| prompt_tokens.map(|tokens| tokens.max(0) as u64))
                {
                    stats.cache.eligible_requests += 1;
                    stats.cache.input_tokens += input_tokens;
                    let cached_tokens =
                        json_u64(telemetry.get("cached_tokens")).unwrap_or_default();
                    stats.cache.cached_tokens += cached_tokens;
                    if cached_tokens > 0 {
                        stats.cache.hit_requests += 1;
                    }
                }
                if let Some(telemetry_hops) = telemetry.get("hops").and_then(JsonValue::as_array) {
                    for hop in telemetry_hops {
                        let Some(node_id) = hop.get("node_id").and_then(JsonValue::as_str) else {
                            continue;
                        };
                        if !request_nodes.contains(node_id) {
                            stats
                                .peer_consumption
                                .entry(node_id.to_string())
                                .or_default()
                                .requests += 1;
                        }
                        let peer = stats
                            .peer_consumption
                            .entry(node_id.to_string())
                            .or_default();
                        for phase in ["prefill", "decode"] {
                            let Some(phase) = hop.get(phase) else {
                                continue;
                            };
                            peer.bytes_sent +=
                                json_u64(phase.get("bytes_sent")).unwrap_or_default();
                            peer.bytes_received +=
                                json_u64(phase.get("bytes_received")).unwrap_or_default();
                            peer.network_ms +=
                                json_f64(phase.get("network_ms")).unwrap_or_default();
                        }
                    }
                }
            }
        }

        if inference_count > 0 {
            stats.average_inference_ms = Some(inference_total / inference_count as f64);
        }
        if ttft_count > 0 {
            stats.average_ttft_ms = Some(ttft_total / ttft_count as f64);
        }
        for (model, (requests, tokens_per_sec, _duration_count, duration_total, speed_count)) in
            model_accumulators
        {
            stats.by_model.insert(
                model,
                ModelStats {
                    requests,
                    average_tokens_per_sec: (speed_count > 0)
                        .then_some(tokens_per_sec / speed_count as f64),
                    average_inference_ms: (_duration_count > 0)
                        .then_some(duration_total / _duration_count as f64),
                },
            );
        }
        Ok(stats)
    }
}

fn json_u64(value: Option<&JsonValue>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_f64().map(|number| number as u64))
    })
}

fn json_f64(value: Option<&JsonValue>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_u64().map(|number| number as f64))
    })
}

#[cfg(test)]
fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

#[cfg(test)]
fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_requests (
            id TEXT PRIMARY KEY,
            received_at TEXT NOT NULL,
            completed_at TEXT,
            route TEXT NOT NULL,
            model TEXT NOT NULL,
            api_key_id TEXT NOT NULL,
            api_key_label TEXT NOT NULL,
            api_key_node_id TEXT,
            entry_node_id TEXT NOT NULL,
            prompt TEXT NOT NULL,
            request_details TEXT NOT NULL DEFAULT '{}',
            result TEXT,
            telemetry TEXT,
            timings TEXT,
            hops TEXT NOT NULL DEFAULT '[]',
            source TEXT NOT NULL DEFAULT 'none',
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            duration_ms INTEGER,
            status TEXT NOT NULL,
            error TEXT
        );
        CREATE INDEX IF NOT EXISTS audit_requests_received_idx
            ON audit_requests(received_at DESC);
        CREATE INDEX IF NOT EXISTS audit_requests_api_key_idx
            ON audit_requests(api_key_id, received_at DESC);
        CREATE INDEX IF NOT EXISTS audit_requests_node_idx
            ON audit_requests(entry_node_id, received_at DESC);
        CREATE INDEX IF NOT EXISTS audit_requests_status_idx
            ON audit_requests(status, received_at DESC);
        CREATE TABLE IF NOT EXISTS audit_request_hops (
            request_id TEXT NOT NULL REFERENCES audit_requests(id) ON DELETE CASCADE,
            position INTEGER NOT NULL,
            node_id TEXT NOT NULL,
            PRIMARY KEY(request_id, position)
        );
        CREATE INDEX IF NOT EXISTS audit_request_hops_node_idx
            ON audit_request_hops(node_id, request_id);
        CREATE VIRTUAL TABLE IF NOT EXISTS audit_prompt_fts USING fts5(
            request_id UNINDEXED,
            prompt,
            tokenize = 'unicode61'
        );
        PRAGMA user_version = 1;",
    )?;
    let timings_exists = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('audit_requests') WHERE name = 'timings'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !timings_exists {
        connection.execute("ALTER TABLE audit_requests ADD COLUMN timings TEXT", [])?;
    }
    Ok(())
}

fn where_clause(query: &AuditQuery) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(node_id) = query.node_id.as_deref().filter(|value| !value.is_empty()) {
        clauses.push(
            "(entry_node_id = ? OR id IN
              (SELECT request_id FROM audit_request_hops WHERE node_id = ?))",
        );
        values.push(Value::Text(node_id.into()));
        values.push(Value::Text(node_id.into()));
    }
    if let Some(api_key) = query.api_key.as_deref().filter(|value| !value.is_empty()) {
        clauses.push("(api_key_id LIKE ? OR api_key_label LIKE ? OR api_key_node_id LIKE ?)");
        let pattern = format!("%{api_key}%");
        values.push(Value::Text(pattern.clone()));
        values.push(Value::Text(pattern.clone()));
        values.push(Value::Text(pattern));
    }
    if let Some(prompt) = query
        .prompt
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let terms = prompt
            .split_whitespace()
            .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        if terms.is_empty() {
            clauses.push("0");
        } else {
            clauses.push(
                "id IN (SELECT request_id FROM audit_prompt_fts WHERE audit_prompt_fts MATCH ?)",
            );
            values.push(Value::Text(terms.join(" AND ")));
        }
    }
    if let Some(received_from) = query
        .received_from
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        clauses.push("received_at >= ?");
        values.push(Value::Text(received_from.into()));
    }
    if let Some(received_to) = query
        .received_to
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        clauses.push("received_at <= ?");
        values.push(Value::Text(received_to.into()));
    }
    if clauses.is_empty() {
        ("1".into(), values)
    } else {
        (clauses.join(" AND "), values)
    }
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let parse_json = |column: usize| -> rusqlite::Result<JsonValue> {
        let raw: Option<String> = row.get(column)?;
        Ok(raw
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or(JsonValue::Null))
    };
    let parse_hops = |column: usize| -> rusqlite::Result<Vec<String>> {
        let raw: String = row.get(column)?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    };
    let parse_timings =
        |column: usize| -> rusqlite::Result<Option<crate::gateway::GenerationTimings>> {
            let raw: Option<String> = row.get(column)?;
            raw.as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)
        };
    Ok(Entry {
        id: row.get(0)?,
        received_at: row.get(1)?,
        completed_at: row.get(2)?,
        route: row.get(3)?,
        model: row.get(4)?,
        api_key_id: row.get(5)?,
        api_key_label: row.get(6)?,
        api_key_node_id: row.get(7)?,
        entry_node_id: row.get(8)?,
        prompt: row.get(9)?,
        request_details: parse_json(10)?,
        result: row.get(11)?,
        telemetry: parse_json(12)?,
        timings: parse_timings(13)?,
        hops: parse_hops(14)?,
        source: row.get(15)?,
        prompt_tokens: row.get::<_, Option<i64>>(16)?.map(|value| value as u32),
        completion_tokens: row.get::<_, Option<i64>>(17)?.map(|value| value as u32),
        duration_ms: row.get::<_, Option<i64>>(18)?.map(|value| value as u64),
        status: row.get(19)?,
        error: row.get(20)?,
    })
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(rename = "from", default)]
    pub received_from: Option<String>,
    #[serde(rename = "to", default)]
    pub received_to: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Authenticated because every response contains user prompts and request
/// bodies, not merely operational counters.
pub async fn list_requests(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    let query = AuditQuery {
        node_id: query.node_id,
        api_key: query.api_key,
        prompt: query.prompt,
        received_from: query.received_from,
        received_to: query.received_to,
        limit: query.limit,
        offset: query.offset,
    };
    match state.audit.list(query).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => {
            warn!(error = %e, "consulta do banco de auditoria falhou");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível consultar a auditoria" })),
            )
                .into_response()
        }
    }
}

/// Authenticated aggregate endpoint used by the Logs statistics tab. It keeps
/// the expensive scan over the SQLite audit trail off the browser.
pub async fn stats_requests(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    let query = AuditQuery {
        node_id: query.node_id,
        api_key: query.api_key,
        prompt: query.prompt,
        received_from: query.received_from,
        received_to: query.received_to,
        limit: None,
        offset: None,
    };
    match state.audit.stats(query).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(e) => {
            warn!(error = %e, "agregação do banco de auditoria falhou");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível agregar a auditoria" })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_activity_marks_and_compares_idle_without_a_timer() {
        let activity = RequestActivity::new();
        activity.mark_at(1_000);

        assert!(!activity.is_idle_at(60_999, 1));
        assert!(activity.is_idle_at(61_000, 1));
        activity.mark_at(61_001);
        assert!(!activity.is_idle_at(61_001, 1));
    }

    fn request(id: &str, key_id: &str, prompt: &str) -> AuditRequest {
        AuditRequest {
            id: id.into(),
            received_at: chrono::Utc::now().to_rfc3339(),
            route: "/v1/chat/completions".into(),
            model: "qwen".into(),
            caller: Caller {
                key_id: key_id.into(),
                key_label: "financeiro".into(),
                key_node_id: Some("node-a".into()),
            },
            entry_node_id: "node-gateway".into(),
            prompt: prompt.into(),
            request_details: json!({ "stream": false, "max_tokens": 64 }),
        }
    }

    fn request_at(id: &str, key_id: &str, prompt: &str, received_at: &str) -> AuditRequest {
        let mut request = request(id, key_id, prompt);
        request.received_at = received_at.into();
        request
    }

    fn completion(status: AuditStatus) -> AuditCompletion {
        AuditCompletion {
            completed_at: "2026-08-26T12:00:01Z".into(),
            result: Some("resposta exata".into()),
            telemetry: Some(json!({ "tokens_per_sec": 12.5 })),
            timings: Some(crate::gateway::timing_metrics(1000, 250, 200, 10, 3)),
            source: "pipeline:node-a→node-b".into(),
            hops: vec!["node-a".into(), "node-b".into()],
            prompt_tokens: Some(5),
            completion_tokens: Some(3),
            duration_ms: Some(1000),
            status,
            error: None,
        }
    }

    #[test]
    fn creates_then_completes_the_same_audit_record() {
        let mut db = AuditDb::open(Connection::open_in_memory().unwrap()).unwrap();
        db.begin(&request("req-1", "key-finance", "prompt exato e sensivel"))
            .unwrap();

        let before = db.list(&AuditQuery::default()).unwrap();
        assert_eq!(before.entries.len(), 1);
        assert_eq!(before.entries[0].status, "in_progress");
        assert_eq!(before.entries[0].prompt, "prompt exato e sensivel");
        assert!(before.entries[0].completed_at.is_none());

        db.complete("req-1", &completion(AuditStatus::Ok)).unwrap();

        let after = db.list(&AuditQuery::default()).unwrap();
        assert_eq!(after.entries[0].id, "req-1");
        assert_eq!(after.entries[0].status, "ok");
        assert_eq!(after.entries[0].result.as_deref(), Some("resposta exata"));
        assert_eq!(after.entries[0].telemetry["tokens_per_sec"], 12.5);
        assert_eq!(after.entries[0].timings.as_ref().unwrap().ttft_ms, 250);
        assert_eq!(
            after.entries[0]
                .timings
                .as_ref()
                .unwrap()
                .generation_tokens_per_sec,
            Some(4.0)
        );
        assert_eq!(after.entries[0].hops, ["node-a", "node-b"]);
        assert_eq!(
            after.entries[0].completed_at.as_deref(),
            Some("2026-08-26T12:00:01Z")
        );
        assert_eq!(
            db.telemetry("req-1").unwrap(),
            Some(json!({ "tokens_per_sec": 12.5 }))
        );
        assert_eq!(db.telemetry("does-not-exist").unwrap(), None);
    }

    #[test]
    fn distinguishes_in_progress_incomplete_and_completed_records() {
        let mut db = AuditDb::open(Connection::open_in_memory().unwrap()).unwrap();
        db.begin(&request("req-running", "key-a", "a")).unwrap();
        db.begin(&request("req-restarted", "key-b", "b")).unwrap();
        db.begin(&request("req-ok", "key-c", "c")).unwrap();
        db.complete("req-ok", &completion(AuditStatus::Ok)).unwrap();
        db.mark_in_progress_incomplete("servidor reiniciou antes do fim")
            .unwrap();

        let rows = db.list(&AuditQuery::default()).unwrap().entries;
        let status = rows
            .into_iter()
            .map(|row| (row.id, (row.status, row.error)))
            .collect::<HashMap<_, _>>();
        assert_eq!(status["req-running"].0, "incomplete");
        assert_eq!(
            status["req-running"].1.as_deref(),
            Some("servidor reiniciou antes do fim")
        );
        assert_eq!(status["req-restarted"].0, "incomplete");
        assert_eq!(status["req-ok"].0, "ok");
    }

    #[test]
    fn searches_by_key_identity_and_prompt_fts_without_secret_values() {
        let mut db = AuditDb::open(Connection::open_in_memory().unwrap()).unwrap();
        db.begin(&request(
            "req-finance",
            "key-finance",
            "relatorio trimestral confidencial",
        ))
        .unwrap();
        db.begin(&request(
            "req-support",
            "key-support",
            "status do atendimento",
        ))
        .unwrap();

        let by_key = db
            .list(&AuditQuery {
                api_key: Some("key-finance".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_key.total, 1);
        assert_eq!(by_key.entries[0].id, "req-finance");
        assert!(!serde_json::to_string(&by_key.entries)
            .unwrap()
            .contains("sk-secret"));

        let by_prompt = db
            .list(&AuditQuery {
                prompt: Some("trimestral confidencial".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_prompt.total, 1);
        assert_eq!(by_prompt.entries[0].id, "req-finance");
    }

    #[test]
    fn aggregates_usage_by_token_node_day_model_peer_cache_and_status() {
        let mut db = AuditDb::open(Connection::open_in_memory().unwrap()).unwrap();
        db.begin(&request_at(
            "req-one",
            "key-finance",
            "one",
            "2026-08-26T12:00:00Z",
        ))
        .unwrap();
        let mut first = completion(AuditStatus::Ok);
        first.prompt_tokens = Some(10);
        first.completion_tokens = Some(4);
        first.timings = Some(crate::gateway::timing_metrics(2000, 300, 1000, 10, 4));
        first.hops = vec!["node-a".into(), "node-b".into()];
        first.telemetry = Some(json!({
            "input_tokens": 10,
            "cached_tokens": 4,
            "hops": [{
                "node_id": "node-a",
                "prefill": { "bytes_sent": 100, "bytes_received": 20, "network_ms": 7 },
                "decode": { "bytes_sent": 30, "bytes_received": 40, "network_ms": 3 }
            }]
        }));
        db.complete("req-one", &first).unwrap();

        let mut second_request =
            request_at("req-two", "key-support", "two", "2026-08-27T12:00:00Z");
        second_request.caller.key_label = "suporte".into();
        db.begin(&second_request).unwrap();
        let mut second = completion(AuditStatus::Partial);
        second.prompt_tokens = Some(5);
        second.completion_tokens = Some(2);
        second.timings = Some(crate::gateway::timing_metrics(2000, 200, 500, 5, 2));
        second.hops = vec!["node-b".into()];
        second.telemetry = Some(json!({
            "input_tokens": 5,
            "cached_tokens": 0,
            "hops": [{
                "node_id": "node-b",
                "prefill": { "bytes_sent": 50, "bytes_received": 10, "network_ms": 2 },
                "decode": { "bytes_sent": 15, "bytes_received": 5, "network_ms": 1 }
            }]
        }));
        db.complete("req-two", &second).unwrap();

        let stats = db.stats(&AuditQuery::default()).unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.input_tokens, 15);
        assert_eq!(stats.output_tokens, 6);
        assert_eq!(stats.by_token["financeiro"], 1);
        assert_eq!(stats.by_node["node-b"], 2);
        assert_eq!(stats.by_day["2026-08-26"].input_tokens, 10);
        assert_eq!(stats.by_model["qwen"].requests, 2);
        assert_eq!(stats.by_model["qwen"].average_tokens_per_sec, Some(1.5));
        assert_eq!(stats.peer_consumption["node-a"].bytes_sent, 130);
        assert_eq!(stats.peer_consumption["node-b"].bytes_sent, 65);
        assert_eq!(stats.terminal_states["ok"], 1);
        assert_eq!(stats.terminal_states["partial"], 1);
        assert_eq!(stats.cache.eligible_requests, 2);
        assert_eq!(stats.cache.hit_requests, 1);
        assert_eq!(stats.cache.cached_tokens, 4);
        assert_eq!(stats.average_ttft_ms, Some(250.0));
    }

    #[test]
    fn shared_audit_writer_keeps_all_parallel_records() {
        let path = std::env::temp_dir().join(format!(
            "pom-audit-concurrent-{}-{}.db",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
        let audit = AuditLog::new(database.clone());
        let mut workers = Vec::new();
        for worker_id in 0..8 {
            let audit = audit.clone();
            workers.push(std::thread::spawn(move || {
                for sequence in 0..25 {
                    audit.begin(request(
                        &format!("audit-{worker_id}-{sequence}"),
                        "key-concurrent",
                        "prompt concorrente",
                    ));
                }
            }));
        }
        for worker in workers {
            worker.join().expect("thread de auditoria");
        }
        audit.flush();

        let entries = database
            .with_write("verificar auditoria concorrente", |connection| {
                AuditDb::from_shared(connection).list(&AuditQuery::default())
            })
            .unwrap();
        assert_eq!(entries.total, 200);
        assert_eq!(entries.entries.len(), 200);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
