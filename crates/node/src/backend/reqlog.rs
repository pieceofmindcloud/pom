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
use rusqlite::{params, params_from_iter, types::Value, Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use tracing::{error, warn};

const WRITER_QUEUE_CAPACITY: usize = 4096;
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const STALE_AFTER_MINUTES: i64 = 15;

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

enum Command {
    Begin(AuditRequest),
    Complete {
        id: String,
        completion: AuditCompletion,
    },
}

/// Handle shared by gateway tasks and the authenticated query route.
#[derive(Clone)]
pub struct AuditLog {
    path: PathBuf,
    sender: mpsc::SyncSender<Command>,
}

impl AuditLog {
    /// Start the dedicated writer. Startup failures are logged and deliberately
    /// do not prevent the gateway from serving inference.
    pub fn new(path: PathBuf) -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        let writer_path = path.clone();
        if let Err(e) = thread::Builder::new()
            .name("pom-audit-writer".into())
            .spawn(move || run_writer(writer_path, receiver))
        {
            error!(error = %e, "não foi possível iniciar o escritor da auditoria");
        }
        Arc::new(Self { path, sender })
    }

    /// Queue the initial row without waiting for SQLite or the filesystem.
    pub fn begin(&self, request: AuditRequest) {
        if let Err(e) = self.sender.try_send(Command::Begin(request)) {
            error!(error = %e, "não foi possível enfileirar o início da auditoria");
        }
    }

    /// Queue completion data for the same request id.
    pub fn complete(&self, id: String, completion: AuditCompletion) {
        if let Err(e) = self.sender.try_send(Command::Complete { id, completion }) {
            error!(error = %e, "não foi possível enfileirar a conclusão da auditoria");
        }
    }

    pub async fn list(&self, query: AuditQuery) -> Result<AuditList> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let connection = Connection::open(&path)
                .with_context(|| format!("abrir banco de auditoria em {}", path.display()))?;
            let mut db = AuditDb::open_for_query(connection)?;
            db.list(&query)
        })
        .await
        .context("aguardar consulta do banco de auditoria")?
    }
}

fn run_writer(path: PathBuf, receiver: mpsc::Receiver<Command>) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(path = %parent.display(), error = %e, "não foi possível criar a pasta da auditoria");
            return;
        }
    }
    let connection = match Connection::open(&path) {
        Ok(connection) => connection,
        Err(e) => {
            error!(path = %path.display(), error = %e, "não foi possível abrir o banco de auditoria");
            return;
        }
    };
    let mut db = match AuditDb::open(connection) {
        Ok(db) => db,
        Err(e) => {
            error!(path = %path.display(), error = %e, "não foi possível inicializar o banco de auditoria");
            return;
        }
    };
    while let Ok(command) = receiver.recv() {
        let result = match command {
            Command::Begin(request) => db.begin(&request),
            Command::Complete { id, completion } => db.complete(&id, &completion),
        };
        if let Err(e) = result {
            error!(error = %e, "falha ao gravar o registro de auditoria");
        }
    }
}

pub struct AuditDb {
    connection: Connection,
}

impl AuditDb {
    pub fn open(connection: Connection) -> Result<Self> {
        configure_connection(&connection)?;
        ensure_schema(&connection)?;
        let mut db = Self { connection };
        db.mark_in_progress_incomplete("servidor reiniciou antes do fim")?;
        Ok(db)
    }

    fn open_for_query(connection: Connection) -> Result<Self> {
        configure_connection(&connection)?;
        ensure_schema(&connection)?;
        Ok(Self { connection })
    }

    pub fn begin(&mut self, request: &AuditRequest) -> Result<()> {
        let transaction = self.connection.transaction()?;
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
        let transaction = self.connection.transaction()?;
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
        self.connection.execute(
            "UPDATE audit_requests
             SET status = 'incomplete', completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 error = COALESCE(error, ?)
             WHERE status = 'in_progress'",
            params![reason],
        )?;
        Ok(())
    }

    fn mark_stale_in_progress(&mut self) -> Result<()> {
        self.connection.execute(
            "UPDATE audit_requests
             SET status = 'incomplete', completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 error = COALESCE(error, 'não concluiu em 15 minutos')
             WHERE status = 'in_progress'
               AND julianday(received_at) < julianday('now', ?)",
            params![format!("-{STALE_AFTER_MINUTES} minutes")],
        )?;
        Ok(())
    }

    pub fn list(&mut self, query: &AuditQuery) -> Result<AuditList> {
        self.mark_stale_in_progress()?;
        let (where_sql, values) = where_clause(query);

        let (total, errors, incomplete) = self.connection.query_row(
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
        let mut token_stmt = self.connection.prepare(&format!(
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
        let mut node_stmt = self.connection.prepare(&format!(
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
        let mut stmt = self.connection.prepare(&format!(
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
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
