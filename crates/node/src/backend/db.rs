//! Persistent local state for the node.
//!
//! `pom.db` is the only database used by the runtime. This module deliberately
//! creates a fresh schema and contains no compatibility path for the old audit
//! database or deployment JSON files. Optional preservation of those files is
//! an explicit operation provided by `scripts/migrate_pom_db.py`.

use crate::catalog::{Catalog, DiscoveredModel};
use crate::config::ApiKey;
use crate::deployment::Deployment;
use crate::thermal::ThermalPolicy;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub const DATABASE_FILENAME: &str = "pom.db";
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Durable transcript accepted by the authenticated UI history API.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSessionRecord {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub titled: bool,
    pub model: Option<String>,
    #[serde(default)]
    pub model_at_start: Option<String>,
    pub messages: Vec<ChatMessageRecord>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageRecord {
    pub id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub request_content: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tokens_per_second: Option<f64>,
    #[serde(default)]
    pub tokens_generated: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub timings: Option<JsonValue>,
    #[serde(default)]
    pub trace: Option<JsonValue>,
    pub at: String,
}

impl ChatSessionRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() || self.title.trim().is_empty() {
            anyhow::bail!("conversa sem identidade ou título");
        }
        if self.updated_at.trim().is_empty() {
            anyhow::bail!("conversa sem data de atualização");
        }
        let mut ids = HashSet::new();
        for message in &self.messages {
            if message.id.trim().is_empty() || !ids.insert(&message.id) {
                anyhow::bail!("mensagem sem identidade ou repetida");
            }
            if !matches!(message.role.as_str(), "user" | "assistant" | "event") {
                anyhow::bail!("papel de mensagem inválido");
            }
            if message.at.trim().is_empty() {
                anyhow::bail!("mensagem sem data");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PomDb {
    path: PathBuf,
    inner: Arc<PomDbInner>,
}

#[derive(Debug)]
struct PomDbInner {
    connection: Mutex<Connection>,
    write_lock: Mutex<()>,
    #[cfg(test)]
    setup_count: std::sync::atomic::AtomicUsize,
}

impl PomDb {
    /// Open the database file, creating its parent directory and schema.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("criar diretório do banco {}", parent.display()))?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("abrir banco local {}", path.display()))?;
        secure_database_permissions(&path)?;
        configure_connection(&connection)?;
        ensure_schema(&connection)?;
        secure_database_permissions(&path)?;
        Ok(Self {
            path,
            inner: Arc::new(PomDbInner {
                connection: Mutex::new(connection),
                write_lock: Mutex::new(()),
                #[cfg(test)]
                setup_count: std::sync::atomic::AtomicUsize::new(1),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.inner
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("mutex da conexão do banco local envenenado"))
    }

    fn with_read<T>(
        &self,
        operation: &str,
        action: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let connection = self.lock_connection()?;
        action(&connection).with_context(|| format!("operação de banco não concluída: {operation}"))
    }

    pub(crate) fn with_write<T>(
        &self,
        operation: &str,
        action: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let _write_lock = self
            .inner
            .write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("mutex de escrita do banco local envenenado"))?;
        let mut connection = self.lock_connection()?;
        action(&mut connection)
            .with_context(|| format!("operação de banco não concluída: {operation}"))
    }

    pub fn save_deployment(&self, deployment: &Deployment) -> Result<()> {
        deployment.validate()?;
        self.with_write("salvar implantação", |connection| {
            let tx = connection.unchecked_transaction()?;
            let payload = serde_json::to_string(deployment).context("serializar implantação")?;
            tx.execute(
                "INSERT INTO deployments (
                    deploy_id, payload_json, kind, model_label, created_at, activated_at,
                    active, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 ON CONFLICT(deploy_id) DO UPDATE SET
                    payload_json = excluded.payload_json,
                    kind = excluded.kind,
                    model_label = excluded.model_label,
                    created_at = excluded.created_at,
                    activated_at = excluded.activated_at,
                    active = 1,
                    updated_at = excluded.updated_at",
                params![
                    &deployment.deploy_id,
                    payload,
                    serde_json::to_string(&deployment.kind)?,
                    &deployment.model.label,
                    &deployment.created_at,
                    &deployment.activated_at,
                ],
            )?;
            tx.execute(
                "DELETE FROM deployment_tombstones WHERE deploy_id = ?",
                params![&deployment.deploy_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn deployment(&self, deploy_id: &str) -> Result<Option<Deployment>> {
        self.with_read("ler implantação", |connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM deployments WHERE deploy_id = ? AND active = 1",
                    params![deploy_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload
                .map(|payload| {
                    let mut deployment: Deployment = serde_json::from_str(&payload)
                        .with_context(|| format!("interpretar implantação {deploy_id}"))?;
                    // Um registro v1 chega sem `versions`; sintetiza a v1 antes
                    // de qualquer leitor ver o registro.
                    deployment.ensure_versions();
                    deployment
                        .validate()
                        .with_context(|| format!("validar implantação {deploy_id}"))?;
                    Ok(deployment)
                })
                .transpose()
        })
    }

    pub fn deployments(&self) -> Result<Vec<Deployment>> {
        self.with_read("ler implantações", |connection| {
            let mut statement = connection.prepare(
                "SELECT deploy_id, payload_json FROM deployments
                 WHERE active = 1 ORDER BY deploy_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut deployments = Vec::new();
            for row in rows {
                let (deploy_id, payload) = row?;
                let mut deployment: Deployment = serde_json::from_str(&payload)
                    .with_context(|| format!("interpretar implantação {deploy_id}"))?;
                deployment.ensure_versions();
                deployment
                    .validate()
                    .with_context(|| format!("validar implantação {deploy_id}"))?;
                deployments.push(deployment);
            }
            Ok(deployments)
        })
    }

    /// Mark a deployment removed without deleting its record or its tombstone.
    pub fn remove_deployment(&self, deploy_id: &str) -> Result<bool> {
        self.with_write("remover implantação", |connection| {
            let tx = connection.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE deployments SET active = 0,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE deploy_id = ? AND active = 1",
                params![deploy_id],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO deployment_tombstones (deploy_id, removed_at)
                 VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                params![deploy_id],
            )?;
            tx.commit()?;
            Ok(changed > 0)
        })
    }

    pub fn insert_api_key(&self, key: &ApiKey) -> Result<()> {
        if key.id.trim().is_empty() || key.key.trim().is_empty() {
            anyhow::bail!("API key sem identidade ou segredo");
        }
        self.with_write("salvar chave de API", |connection| {
            connection.execute(
                "INSERT INTO api_keys (id, key, label, node_id, active, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    label = excluded.label,
                    node_id = excluded.node_id,
                    active = CASE
                        WHEN EXISTS (
                            SELECT 1 FROM api_key_revocations
                            WHERE key_id = api_keys.id
                        ) THEN 0
                        ELSE excluded.active
                    END",
                params![
                    &key.id,
                    &key.key,
                    &key.label,
                    &key.node_id,
                    key.active as i64,
                    &key.created_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn api_keys(&self) -> Result<Vec<ApiKey>> {
        self.with_read("ler chaves de API", |connection| {
            let mut statement = connection.prepare(
                "SELECT id, key, label, node_id, active, created_at
                 FROM api_keys ORDER BY created_at, id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(ApiKey {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    label: row.get(2)?,
                    node_id: row.get(3)?,
                    active: row.get::<_, i64>(4)? != 0,
                    created_at: row.get(5)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler API keys do banco local")
        })
    }

    pub fn api_key_for_token(&self, token: &str) -> Result<Option<ApiKey>> {
        self.with_read("consultar chave de API", |connection| {
            connection
                .query_row(
                    "SELECT id, key, label, node_id, active, created_at
                     FROM api_keys WHERE key = ? AND active = 1",
                    params![token],
                    |row| {
                        Ok(ApiKey {
                            id: row.get(0)?,
                            key: row.get(1)?,
                            label: row.get(2)?,
                            node_id: row.get(3)?,
                            active: row.get::<_, i64>(4)? != 0,
                            created_at: row.get(5)?,
                        })
                    },
                )
                .optional()
                .context("consultar API key no banco local")
        })
    }

    /// Revoke once, retaining both the inactive key and the first audit event.
    pub fn revoke_api_key(&self, key_or_id: &str, revoked_by: &str) -> Result<bool> {
        self.with_write("revogar chave de API", |connection| {
            let tx = connection.unchecked_transaction()?;
            let key = tx
                .query_row(
                    "SELECT id, key FROM api_keys WHERE (key = ? OR id = ?) AND active = 1",
                    params![key_or_id, key_or_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((key_id, secret)) = key else {
                tx.commit()?;
                return Ok(false);
            };
            tx.execute(
                "UPDATE api_keys SET active = 0 WHERE id = ?",
                params![&key_id],
            )?;
            tx.execute(
                "INSERT INTO api_key_revocations
                    (key_id, key_sha256, revoked_by, revoked_at)
                 VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                params![&key_id, secret_digest(&secret), revoked_by],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    pub fn api_key_revocations(&self) -> Result<Vec<(String, String, String)>> {
        self.with_read("ler histórico de revogações", |connection| {
            let mut statement = connection.prepare(
                "SELECT key_id, revoked_by, revoked_at FROM api_key_revocations
                 ORDER BY revoked_at, id",
            )?;
            let rows =
                statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler histórico de revogações")
        })
    }

    pub fn thermal_policy(&self) -> Result<Option<ThermalPolicy>> {
        self.with_read("ler política térmica", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'thermal_policy'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).context("interpretar política térmica"))
                .transpose()
        })
    }

    pub fn save_thermal_policy(&self, policy: ThermalPolicy) -> Result<()> {
        policy.validate().context("validar política térmica")?;
        self.save_setting("thermal_policy", &policy)
    }

    /// Read the stored credential without logging, formatting or returning it
    /// from an HTTP handler. The database file is restricted to its owner.
    pub fn huggingface_token(&self) -> Result<Option<String>> {
        self.with_read("ler credencial do Hugging Face", |connection| {
            connection
                .query_row(
                    "SELECT value_text FROM local_secrets WHERE secret_key = 'huggingface_token'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(|value| value.flatten())
                .context("ler credencial do Hugging Face")
        })
    }

    pub fn save_huggingface_token(&self, token: Option<&str>) -> Result<()> {
        self.with_write("salvar credencial do Hugging Face", |connection| {
            match token.map(str::trim).filter(|token| !token.is_empty()) {
                Some(token) => {
                    connection.execute(
                        "INSERT INTO local_secrets (secret_key, value_text)
                         VALUES ('huggingface_token', ?)
                         ON CONFLICT(secret_key) DO UPDATE SET value_text = excluded.value_text",
                        params![token],
                    )?;
                }
                None => {
                    connection.execute(
                        "DELETE FROM local_secrets WHERE secret_key = 'huggingface_token'",
                        [],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Replace one transcript atomically. Messages are normalized so a
    /// conversation can be read without keeping a large opaque JSON document.
    pub fn save_chat_session(&self, session: &ChatSessionRecord) -> Result<()> {
        session.validate()?;
        self.with_write("salvar conversa", |connection| {
            let tx = connection.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO chat_conversations (
                id, title, titled, model, model_at_start, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                titled = excluded.titled,
                model = excluded.model,
                model_at_start = excluded.model_at_start,
                updated_at = excluded.updated_at",
                params![
                    &session.id,
                    &session.title,
                    session.titled as i64,
                    &session.model,
                    &session.model_at_start,
                    &session.updated_at,
                    &session.updated_at,
                ],
            )?;
            tx.execute(
                "DELETE FROM chat_messages WHERE conversation_id = ?",
                params![&session.id],
            )?;
            for (position, message) in session.messages.iter().enumerate() {
                tx.execute(
                    "INSERT INTO chat_messages (
                    conversation_id, position, message_id, role, content,
                    request_content, error, source, reasoning, tokens_per_second,
                    tokens_generated, duration_ms, timings_json, trace_json, at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        &session.id,
                        position as i64,
                        &message.id,
                        &message.role,
                        &message.content,
                        &message.request_content,
                        &message.error,
                        &message.source,
                        &message.reasoning,
                        message.tokens_per_second,
                        message.tokens_generated.map(|value| value as i64),
                        message.duration_ms.map(|value| value as i64),
                        optional_json_string(message.timings.as_ref())?,
                        optional_json_string(message.trace.as_ref())?,
                        &message.at,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn chat_sessions(&self) -> Result<Vec<ChatSessionRecord>> {
        self.with_read("ler conversas", |connection| {
            let mut statement = connection.prepare(
                "SELECT id, title, titled, model, model_at_start, updated_at
             FROM chat_conversations ORDER BY updated_at DESC, id DESC",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                let (id, title, titled, model, model_at_start, updated_at) = row?;
                sessions.push(ChatSessionRecord {
                    messages: chat_messages(connection, &id)?,
                    id,
                    title,
                    titled,
                    model,
                    model_at_start,
                    updated_at,
                });
            }
            Ok(sessions)
        })
    }

    pub fn delete_chat_session(&self, id: &str) -> Result<bool> {
        self.with_write("excluir conversa", |connection| {
            Ok(connection.execute("DELETE FROM chat_conversations WHERE id = ?", params![id])? > 0)
        })
    }

    pub fn clear_chat_sessions(&self) -> Result<()> {
        self.with_write("limpar conversas", |connection| {
            connection.execute("DELETE FROM chat_conversations", [])?;
            Ok(())
        })
    }

    /// Persiste um lote na ordem em que o principal o aceitou. O contador de
    /// perdas é global ao banco porque uma linha descartada não tem mais
    /// `deploy_id` confiável para uma segunda tentativa.
    pub fn append_cluster_logs(
        &self,
        entries: &[crate::cluster_logs::LogEntry],
        dropped: u64,
    ) -> Result<()> {
        if entries.is_empty() && dropped == 0 {
            return Ok(());
        }
        self.with_write("persistir logs do cluster", |connection| {
            let tx = connection.unchecked_transaction()?;
            for entry in entries {
                tx.execute(
                    "INSERT INTO cluster_logs (
                        node_id, node_name, deploy_id, origin_at, received_at,
                        level, target, message
                     ) VALUES (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?, ?, ?)",
                    params![
                        &entry.node_id,
                        &entry.node_name,
                        &entry.deploy_id,
                        &entry.origin_at,
                        &entry.level,
                        &entry.target,
                        &entry.message,
                    ],
                )?;
            }
            if dropped > 0 {
                tx.execute(
                    "INSERT INTO cluster_log_counters (counter_key, value)
                     VALUES ('dropped', ?)
                     ON CONFLICT(counter_key) DO UPDATE SET
                        value = cluster_log_counters.value + excluded.value",
                    params![dropped as i64],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Lê a janela mais recente e devolve-a em ordem crescente de chegada.
    /// `origin_at` nunca aparece no ORDER BY: relógios remotos são apenas
    /// informação, não uma fonte de ordenação.
    pub fn cluster_logs(
        &self,
        deploy_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::cluster_logs::StoredLogEntry>> {
        let limit = limit.clamp(1, 1000) as i64;
        self.with_read("ler logs do cluster", |connection| {
            let mut rows = Vec::new();
            if let Some(deploy_id) = deploy_id {
                let mut statement = connection.prepare(
                    "SELECT received_seq, received_at, node_id, node_name,
                            deploy_id, origin_at, level, target, message
                     FROM cluster_logs
                     WHERE deploy_id = ?
                     ORDER BY received_seq DESC LIMIT ?",
                )?;
                let mapped = statement.query_map(params![deploy_id, limit], |row| {
                    Ok(crate::cluster_logs::StoredLogEntry {
                        received_seq: row.get::<_, i64>(0)? as u64,
                        received_at: row.get(1)?,
                        node_id: row.get(2)?,
                        node_name: row.get(3)?,
                        deploy_id: row.get(4)?,
                        origin_at: row.get(5)?,
                        level: row.get(6)?,
                        target: row.get(7)?,
                        message: row.get(8)?,
                    })
                })?;
                for row in mapped {
                    rows.push(row?);
                }
            } else {
                let mut statement = connection.prepare(
                    "SELECT received_seq, received_at, node_id, node_name,
                            deploy_id, origin_at, level, target, message
                     FROM cluster_logs
                     ORDER BY received_seq DESC LIMIT ?",
                )?;
                let mapped = statement.query_map(params![limit], |row| {
                    Ok(crate::cluster_logs::StoredLogEntry {
                        received_seq: row.get::<_, i64>(0)? as u64,
                        received_at: row.get(1)?,
                        node_id: row.get(2)?,
                        node_name: row.get(3)?,
                        deploy_id: row.get(4)?,
                        origin_at: row.get(5)?,
                        level: row.get(6)?,
                        target: row.get(7)?,
                        message: row.get(8)?,
                    })
                })?;
                for row in mapped {
                    rows.push(row?);
                }
            }
            rows.reverse();
            Ok(rows)
        })
    }

    pub fn cluster_log_dropped(&self) -> Result<u64> {
        self.with_read("ler perdas dos logs do cluster", |connection| {
            Ok(connection
                .query_row(
                    "SELECT value FROM cluster_log_counters WHERE counter_key = 'dropped'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0) as u64)
        })
    }

    fn save_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.with_write("salvar configuração local", |connection| {
            connection.execute(
                "INSERT INTO local_settings (setting_key, value_json) VALUES (?, ?)
                 ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json",
                params![key, serde_json::to_string(value)?],
            )?;
            Ok(())
        })
    }

    /// Record the current catalog while retaining rows for files that vanished.
    pub fn record_catalog(&self, catalog: &Catalog) -> Result<()> {
        self.with_write("registrar catálogo", |connection| {
            let tx = connection.unchecked_transaction()?;
            let now = chrono::Utc::now().to_rfc3339();
            let mut seen_models = HashSet::new();

            for model in &catalog.models {
                let (model_id, identity_kind, identity, model_kind) = model_identity(model)?;
                seen_models.insert(model_id.clone());
                tx.execute(
                    "INSERT INTO model_history (
                    model_id, identity_kind, identity, label, model_kind, source,
                    source_path, size_bytes, total_layers, quant, first_seen_at,
                    last_seen_at, present
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)
                 ON CONFLICT(model_id) DO UPDATE SET
                    label = excluded.label,
                    model_kind = excluded.model_kind,
                    source = excluded.source,
                    source_path = excluded.source_path,
                    size_bytes = excluded.size_bytes,
                    total_layers = excluded.total_layers,
                    quant = excluded.quant,
                    last_seen_at = excluded.last_seen_at,
                    present = 1",
                    params![
                        &model_id,
                        identity_kind,
                        identity,
                        &model.name,
                        model_kind,
                        model.source.label(),
                        &model.path,
                        model.size_bytes as i64,
                        model.total_layers.map(i64::from),
                        &model.quant,
                        &now,
                        &now,
                    ],
                )?;

                if model.is_shard() {
                    tx.execute(
                        "UPDATE model_slices SET present = 0 WHERE model_id = ?",
                        params![&model_id],
                    )?;
                    let slice_id = slice_identity(model);
                    let range = model.shard_range;
                    tx.execute(
                        "INSERT INTO model_slices (
                        model_id, slice_id, file_path, sha256, start_layer, end_layer,
                        bytes, tp_rank, tp_size, is_first, is_last, first_seen_at,
                        last_seen_at, present
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)
                     ON CONFLICT(model_id, slice_id) DO UPDATE SET
                        file_path = excluded.file_path,
                        sha256 = excluded.sha256,
                        start_layer = excluded.start_layer,
                        end_layer = excluded.end_layer,
                        bytes = excluded.bytes,
                        tp_rank = excluded.tp_rank,
                        tp_size = excluded.tp_size,
                        is_first = excluded.is_first,
                        is_last = excluded.is_last,
                        last_seen_at = excluded.last_seen_at,
                        present = 1",
                        params![
                            &model_id,
                            slice_id,
                            &model.path,
                            &model.sha256,
                            range.map(|r| r.0 as i64),
                            range.map(|r| r.1 as i64),
                            model.size_bytes as i64,
                            model.tp_rank.map(i64::from),
                            model.tp_size.map(i64::from),
                            is_first(model),
                            is_last(model),
                            &now,
                            &now,
                        ],
                    )?;
                }
            }

            let existing = {
                let mut statement = tx.prepare("SELECT model_id FROM model_history")?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for model_id in existing {
                if !seen_models.contains(&model_id) {
                    tx.execute(
                        "UPDATE model_history SET present = 0 WHERE model_id = ?",
                        params![&model_id],
                    )?;
                    tx.execute(
                        "UPDATE model_slices SET present = 0 WHERE model_id = ?",
                        params![&model_id],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn model_history(&self) -> Result<Vec<(String, bool)>> {
        self.with_read("ler histórico de modelos", |connection| {
            let mut statement = connection
                .prepare("SELECT model_id, present FROM model_history ORDER BY model_id")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler histórico de modelos")
        })
    }

    pub fn model_slice_history(
        &self,
        model_id: &str,
    ) -> Result<Vec<(String, bool, Option<u32>, Option<u32>, u64)>> {
        self.with_read("ler histórico de fatias", |connection| {
            let mut statement = connection.prepare(
                "SELECT slice_id, present, start_layer, end_layer, bytes
                 FROM model_slices WHERE model_id = ? ORDER BY start_layer, end_layer, slice_id",
            )?;
            let rows = statement.query_map(params![model_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, Option<i64>>(2)?.map(|value| value as u32),
                    row.get::<_, Option<i64>>(3)?.map(|value| value as u32),
                    row.get::<_, i64>(4)? as u64,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler histórico de fatias")
        })
    }

    #[cfg(test)]
    fn table_names(&self) -> Result<Vec<String>> {
        self.with_read("ler tabelas do banco", |connection| {
            let mut statement = connection.prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')
                 UNION ALL SELECT name FROM sqlite_temp_master WHERE type = 'table'",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler tabelas do banco")
        })
    }

    #[cfg(test)]
    fn journal_mode(&self) -> Result<String> {
        self.with_read("ler modo de journal", |connection| {
            connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .context("ler modo de journal")
        })
    }

    #[cfg(test)]
    fn setup_count(&self) -> usize {
        self.inner
            .setup_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn test_marker_count(&self) -> Result<u64> {
        Ok(self.api_keys()?.len() as u64)
    }

    #[cfg(test)]
    fn insert_test_marker(&self, marker: &str) -> Result<()> {
        let key = ApiKey {
            id: marker.into(),
            key: format!("secret-{marker}"),
            label: marker.into(),
            node_id: String::new(),
            active: true,
            created_at: marker.into(),
        };
        self.insert_api_key(&key)
    }

    #[cfg(test)]
    fn hold_test_write(
        &self,
        started: std::sync::mpsc::Sender<()>,
        duration: std::time::Duration,
    ) -> Result<()> {
        self.with_write("teste de escrita presa", |connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute(
                "INSERT INTO api_keys (id, key, label, node_id, active, created_at)
                 VALUES ('held-write', 'held-secret', 'held', '', 1, 'held')",
                [],
            )?;
            started.send(()).expect("sinalizar escrita presa");
            std::thread::sleep(duration);
            transaction.commit()?;
            Ok(())
        })
    }
}

fn chat_messages(connection: &Connection, conversation_id: &str) -> Result<Vec<ChatMessageRecord>> {
    let mut statement = connection.prepare(
        "SELECT message_id, role, content, request_content, error, source,
                reasoning, tokens_per_second, tokens_generated, duration_ms,
                timings_json, trace_json, at
         FROM chat_messages WHERE conversation_id = ? ORDER BY position",
    )?;
    let rows = statement.query_map(params![conversation_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<String>>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (
            id,
            role,
            content,
            request_content,
            error,
            source,
            reasoning,
            tokens_per_second,
            tokens_generated,
            duration_ms,
            timings_json,
            trace_json,
            at,
        ) = row?;
        messages.push(ChatMessageRecord {
            id,
            role,
            content,
            request_content,
            error,
            source,
            reasoning,
            tokens_per_second,
            tokens_generated: tokens_generated.map(|value| value as u64),
            duration_ms: duration_ms.map(|value| value as u64),
            timings: parse_optional_json(timings_json, "timings")?,
            trace: parse_optional_json(trace_json, "trace")?,
            at,
        });
    }
    Ok(messages)
}

fn optional_json_string(value: Option<&JsonValue>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .context("serializar metadados da conversa")
}

fn parse_optional_json(value: Option<String>, field: &str) -> Result<Option<JsonValue>> {
    value
        .map(|value| {
            serde_json::from_str(&value).with_context(|| format!("interpretar {field} da conversa"))
        })
        .transpose()
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(())
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS deployments (
            deploy_id TEXT PRIMARY KEY,
            payload_json TEXT NOT NULL,
            kind TEXT NOT NULL,
            model_label TEXT NOT NULL,
            created_at TEXT NOT NULL,
            activated_at TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS deployments_active_idx
            ON deployments(active, model_label, activated_at DESC);
        CREATE TABLE IF NOT EXISTS deployment_tombstones (
            deploy_id TEXT PRIMARY KEY,
            removed_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS api_keys (
            id TEXT PRIMARY KEY,
            key TEXT NOT NULL UNIQUE,
            label TEXT NOT NULL,
            node_id TEXT NOT NULL,
            active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS api_keys_active_idx ON api_keys(active);
        CREATE TABLE IF NOT EXISTS api_key_revocations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            key_id TEXT NOT NULL REFERENCES api_keys(id),
            key_sha256 TEXT NOT NULL,
            revoked_by TEXT NOT NULL,
            revoked_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS api_key_revocations_key_idx
            ON api_key_revocations(key_id, revoked_at);
        CREATE TABLE IF NOT EXISTS model_history (
            model_id TEXT PRIMARY KEY,
            identity_kind TEXT NOT NULL,
            identity TEXT NOT NULL,
            label TEXT NOT NULL,
            model_kind TEXT NOT NULL,
            source TEXT NOT NULL,
            source_path TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            total_layers INTEGER,
            quant TEXT,
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            present INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1))
        );
        CREATE INDEX IF NOT EXISTS model_history_present_idx
            ON model_history(present, last_seen_at DESC);
        CREATE TABLE IF NOT EXISTS model_slices (
            model_id TEXT NOT NULL REFERENCES model_history(model_id),
            slice_id TEXT NOT NULL,
            file_path TEXT NOT NULL,
            sha256 TEXT,
            start_layer INTEGER,
            end_layer INTEGER,
            bytes INTEGER NOT NULL,
            tp_rank INTEGER,
            tp_size INTEGER,
            is_first INTEGER NOT NULL DEFAULT 0,
            is_last INTEGER NOT NULL DEFAULT 0,
            first_seen_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL,
            present INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1)),
            PRIMARY KEY(model_id, slice_id)
        );
        CREATE INDEX IF NOT EXISTS model_slices_present_idx
            ON model_slices(model_id, present, start_layer);
        CREATE TABLE IF NOT EXISTS local_settings (
            setting_key TEXT PRIMARY KEY,
            value_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS local_secrets (
            secret_key TEXT PRIMARY KEY,
            value_text TEXT
        );
        CREATE TABLE IF NOT EXISTS chat_conversations (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            titled INTEGER NOT NULL DEFAULT 0 CHECK (titled IN (0, 1)),
            model TEXT,
            model_at_start TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS chat_conversations_updated_idx
            ON chat_conversations(updated_at DESC);
        CREATE TABLE IF NOT EXISTS chat_messages (
            conversation_id TEXT NOT NULL REFERENCES chat_conversations(id) ON DELETE CASCADE,
            position INTEGER NOT NULL,
            message_id TEXT NOT NULL,
            role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'event')),
            content TEXT NOT NULL,
            request_content TEXT,
            error TEXT,
            source TEXT,
            reasoning TEXT,
            tokens_per_second REAL,
            tokens_generated INTEGER,
            duration_ms INTEGER,
            timings_json TEXT,
            trace_json TEXT,
            at TEXT NOT NULL,
            PRIMARY KEY(conversation_id, position),
            UNIQUE(conversation_id, message_id)
        );
        CREATE INDEX IF NOT EXISTS chat_messages_id_idx
            ON chat_messages(conversation_id, message_id);
        CREATE TABLE IF NOT EXISTS audit_requests (
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
        CREATE TABLE IF NOT EXISTS cluster_logs (
            received_seq INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            node_name TEXT NOT NULL,
            deploy_id TEXT NOT NULL,
            origin_at TEXT NOT NULL,
            received_at TEXT NOT NULL,
            level TEXT NOT NULL,
            target TEXT NOT NULL,
            message TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS cluster_logs_deploy_seq_idx
            ON cluster_logs(deploy_id, received_seq);
        CREATE TABLE IF NOT EXISTS cluster_log_counters (
            counter_key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );
        PRAGMA user_version = 1;",
    )?;
    Ok(())
}

fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .with_context(|| format!("ler permissões do banco {}", path.display()))?
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("restringir permissões do banco {}", path.display()))?;
    }
    Ok(())
}

fn restrict_sidecar_permissions(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.exists() {
            restrict_permissions(&sidecar)?;
        }
    }
    Ok(())
}

/// Keep the database and SQLite WAL sidecars private after `PomDb::open` has
/// created and configured the shared connection.
pub(crate) fn secure_database_permissions(path: &Path) -> Result<()> {
    if path == Path::new(":memory:") {
        return Ok(());
    }
    restrict_permissions(path)?;
    restrict_sidecar_permissions(path)
}

fn secret_digest(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

fn model_identity(model: &DiscoveredModel) -> Result<(String, &'static str, String, &'static str)> {
    if model.is_shard() {
        let identity_kind = if model.pom_id.as_deref().is_some_and(|id| !id.is_empty()) {
            "pom_id"
        } else if model
            .shard_group
            .as_deref()
            .is_some_and(|group| !group.is_empty())
        {
            "shard_group"
        } else {
            "path"
        };
        let identity = match identity_kind {
            "pom_id" => model.pom_id.clone().unwrap_or_default(),
            "shard_group" => model.shard_group.clone().unwrap_or_default(),
            _ => model.path.clone(),
        };
        Ok((
            format!("{identity_kind}:{identity}"),
            identity_kind,
            identity,
            "slice",
        ))
    } else {
        // The catalog is called while a model library may still be warming.
        // A whole-model content hash is an explicit identity operation, not
        // metadata: doing it here made every catalog snapshot read all weights
        // before `metadata_complete` could be published. The local history is
        // keyed by the configured path; deployment still proves content with
        // its separate identity route before copying or launching.
        let identity = model.path.clone();
        Ok((
            format!("path:{identity}"),
            "path",
            identity,
            "whole",
        ))
    }
}

fn slice_identity(model: &DiscoveredModel) -> String {
    format!(
        "{}:{}-{}:tp{}of{}",
        model.sha256.as_deref().unwrap_or(&model.path),
        model.shard_range.map(|range| range.0).unwrap_or_default(),
        model.shard_range.map(|range| range.1).unwrap_or_default(),
        model
            .tp_rank
            .map(|rank| rank.to_string())
            .unwrap_or_default(),
        model
            .tp_size
            .map(|size| size.to_string())
            .unwrap_or_default()
    )
}

fn is_first(model: &DiscoveredModel) -> bool {
    model.shard_range.is_some_and(|range| range.0 == 0)
}

fn is_last(model: &DiscoveredModel) -> bool {
    model
        .shard_range
        .zip(model.total_layers)
        .is_some_and(|(range, total)| range.1 + 1 == total)
}

#[cfg(test)]
mod tests {
    use super::{PomDb, DATABASE_FILENAME};
    use crate::config::ApiKey;
    use std::path::{Path, PathBuf};

    #[test]
    fn creates_missing_database_with_the_complete_schema() {
        let root = temporary_path("schema");
        let path = root.join("volume").join(DATABASE_FILENAME);
        let db = PomDb::open(&path).expect("abrir banco novo");

        assert!(path.is_file());
        assert_eq!(db.path(), path);
        let tables = db.table_names().expect("ler schema");
        for table in [
            "deployments",
            "api_keys",
            "api_key_revocations",
            "model_history",
            "model_slices",
            "local_settings",
            "local_secrets",
            "chat_conversations",
            "chat_messages",
            "audit_requests",
            "audit_request_hops",
            "audit_prompt_fts",
            "cluster_logs",
            "cluster_log_counters",
        ] {
            assert!(
                tables.contains(&table.to_string()),
                "tabela ausente: {table}"
            );
        }
        assert_eq!(db.journal_mode().expect("ler journal"), "wal");
        remove_database(&path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cluster_logs_keep_deploy_id_and_database_arrival_order() {
        let path = temporary_path("cluster-logs");
        let db = PomDb::open(&path).expect("abrir banco novo");
        let first = crate::cluster_logs::LogEntry {
            node_id: "node-b".into(),
            node_name: "ksia".into(),
            deploy_id: "deploy-1".into(),
            origin_at: "2099-01-01T00:00:00Z".into(),
            level: "INFO".into(),
            target: "test".into(),
            message: "primeiro recebido".into(),
        };
        let second = crate::cluster_logs::LogEntry {
            origin_at: "2000-01-01T00:00:00Z".into(),
            message: "segundo recebido".into(),
            ..first.clone()
        };

        db.append_cluster_logs(&[first, second], 3)
            .expect("persistir logs");
        let rows = db
            .cluster_logs(Some("deploy-1"), 100)
            .expect("ler logs");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message, "primeiro recebido");
        assert_eq!(rows[1].message, "segundo recebido");
        assert_eq!(rows[0].received_seq + 1, rows[1].received_seq);
        assert_eq!(rows[0].deploy_id, "deploy-1");
        assert_eq!(db.cluster_log_dropped().unwrap(), 3);
        remove_database(&path);
    }

    #[test]
    fn concurrent_connections_can_write_without_losing_rows() {
        let path = temporary_path("concurrent");
        let db = PomDb::open(&path).expect("abrir banco novo");
        let mut workers = Vec::new();
        for index in 0..8 {
            let db = db.clone();
            workers.push(std::thread::spawn(move || {
                db.insert_test_marker(&format!("marker-{index}"))
                    .expect("gravar marcador");
            }));
        }
        for worker in workers {
            worker.join().expect("thread de escrita");
        }
        assert_eq!(db.test_marker_count().expect("contar marcadores"), 8);
        remove_database(&path);
    }

    #[test]
    fn sustained_parallel_writes_do_not_hit_sqlite_busy() {
        let path = temporary_path("concurrent-stress");
        let db = PomDb::open(&path).expect("abrir banco novo");
        let mut workers = Vec::new();
        for worker_id in 0..32 {
            let db = db.clone();
            workers.push(std::thread::spawn(move || {
                let mut errors = Vec::new();
                for sequence in 0..500 {
                    if let Err(error) =
                        db.insert_test_marker(&format!("stress-{worker_id}-{sequence}"))
                    {
                        errors.push(error.to_string());
                    }
                }
                errors
            }));
        }

        let errors = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("thread de escrita"))
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "falhas de escrita: {errors:?}");
        assert_eq!(db.test_marker_count().expect("contar marcadores"), 16_000);
        remove_database(&path);
    }

    #[test]
    fn a_write_waits_for_another_write_instead_of_being_lost_to_busy() {
        let path = temporary_path("serialized-write");
        let db = PomDb::open(&path).expect("abrir banco novo");
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let holder_db = db.clone();
        let holder = std::thread::spawn(move || {
            holder_db.hold_test_write(started_sender, std::time::Duration::from_secs(6))
        });
        started_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a escrita presa iniciou");

        let reader_db = db.clone();
        let reader = std::thread::spawn(move || reader_db.deployments());
        let writer_db = db.clone();
        let writer = std::thread::spawn(move || writer_db.insert_test_marker("after-held"));
        let reader_result = reader.join().expect("thread da leitura");
        let writer_result = writer.join().expect("thread da segunda escrita");
        holder
            .join()
            .expect("thread da escrita presa")
            .expect("commit da escrita presa");
        reader_result.expect("a leitura não pode falhar");
        writer_result.expect("a segunda escrita não pode ser perdida");
        assert_eq!(db.test_marker_count().expect("contar marcadores"), 2);
        remove_database(&path);
    }

    #[test]
    fn configures_connection_and_schema_only_once() {
        let path = temporary_path("setup-once");
        let db = PomDb::open(&path).expect("abrir banco novo");

        for index in 0..20 {
            db.insert_test_marker(&format!("setup-{index}"))
                .expect("gravar marcador");
        }

        assert_eq!(db.setup_count(), 1);
        remove_database(&path);
    }

    #[test]
    fn revoked_key_is_inactive_and_revocation_history_survives_reopen() {
        let path = temporary_path("revocation");
        let db = PomDb::open(&path).expect("abrir banco novo");
        db.insert_api_key(&ApiKey {
            id: "key-1".into(),
            key: "sk-secret".into(),
            label: "operador".into(),
            node_id: "node-a".into(),
            active: true,
            created_at: "2026-09-02T10:00:00Z".into(),
        })
        .unwrap();
        assert!(db.api_key_for_token("sk-secret").unwrap().is_some());
        assert!(db.revoke_api_key("sk-secret", "node-a").unwrap());
        assert!(db.api_key_for_token("sk-secret").unwrap().is_none());
        drop(db);

        let reopened = PomDb::open(&path).expect("reabrir banco");
        assert!(reopened.api_key_for_token("sk-secret").unwrap().is_none());
        let key = ApiKey {
            id: "key-1".into(),
            key: "sk-secret".into(),
            label: "operador".into(),
            node_id: "node-a".into(),
            active: true,
            created_at: "2026-09-02T10:00:00Z".into(),
        };
        reopened.insert_api_key(&key).unwrap();
        assert!(reopened.api_key_for_token("sk-secret").unwrap().is_none());
        let history = reopened.api_key_revocations().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1, "node-a");
        assert!(!history[0].2.is_empty());
        remove_database(&path);
    }

    #[test]
    fn thermal_preferences_and_huggingface_token_survive_reopen() {
        let path = temporary_path("preferences");
        let policy = crate::thermal::ThermalPolicy {
            limit_c: 79.0,
            hysteresis_c: 6.0,
            cooldown_secs: 45,
        };
        let db = PomDb::open(&path).unwrap();
        db.save_thermal_policy(policy).unwrap();
        db.save_huggingface_token(Some("hf_test_secret")).unwrap();
        drop(db);

        let reopened = PomDb::open(&path).unwrap();
        assert_eq!(reopened.thermal_policy().unwrap(), Some(policy));
        assert_eq!(
            reopened.huggingface_token().unwrap().as_deref(),
            Some("hf_test_secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
                if sidecar.exists() {
                    assert_eq!(
                        std::fs::metadata(sidecar).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                }
            }
        }
        remove_database(&path);
    }

    #[test]
    fn chat_history_is_normalized_and_survives_reopen() {
        let path = temporary_path("chat-history");
        let session = super::ChatSessionRecord {
            id: "conversation-1".into(),
            title: "Teste persistente".into(),
            titled: true,
            model: Some("model.gguf".into()),
            model_at_start: Some("model.gguf".into()),
            updated_at: "2026-09-02T12:00:00Z".into(),
            messages: vec![super::ChatMessageRecord {
                id: "message-1".into(),
                role: "user".into(),
                content: "Olá".into(),
                request_content: None,
                error: None,
                source: None,
                reasoning: None,
                tokens_per_second: None,
                tokens_generated: None,
                duration_ms: None,
                timings: Some(serde_json::json!({"totalMs": 12})),
                trace: Some(serde_json::json!({"model": "model.gguf"})),
                at: "2026-09-02T11:59:00Z".into(),
            }],
        };
        let db = PomDb::open(&path).unwrap();
        db.save_chat_session(&session).unwrap();
        assert_eq!(db.chat_sessions().unwrap(), vec![session.clone()]);
        drop(db);

        let reopened = PomDb::open(&path).unwrap();
        assert_eq!(reopened.chat_sessions().unwrap(), vec![session]);
        assert!(reopened.delete_chat_session("conversation-1").unwrap());
        assert!(reopened.chat_sessions().unwrap().is_empty());
        remove_database(&path);
    }

    #[test]
    fn a_deployment_keeps_its_versions_across_save_and_reopen() {
        use crate::deployment::{
            Deployment, DeploymentKind, DeploymentModel, DeploymentNode,
        };
        let path = temporary_path("deployment-versions");

        let no = DeploymentNode {
            node_id: "n1".into(),
            hostname: "ksia".into(),
            start_layer: 0,
            end_layer: 31,
            peer_index: 0,
            peer_count: 1,
            primary: true,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: crate::webui::RuntimeParams::default(),
        };
        let mut deployment = Deployment::new(
            "d-4f2a91c3".into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: "qwen3:8b".into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            vec![no.clone()],
        );
        let mut params = crate::webui::RuntimeParams::default();
        params.n_ctx = 16_384;
        deployment.upsert_version(params, vec![no], "coordenador");
        assert_eq!(deployment.versions.len(), 2);
        assert_eq!(deployment.active_version, 2);

        let db = PomDb::open(&path).expect("abrir banco novo");
        db.save_deployment(&deployment).expect("salvar implantação");
        drop(db);

        let reopened = PomDb::open(&path).expect("reabrir banco");
        let lido = reopened
            .deployment("d-4f2a91c3")
            .expect("ler implantação")
            .expect("implantação existe");
        assert_eq!(lido, deployment);
        assert_eq!(lido.versions.len(), 2);
        assert_eq!(lido.active_version, 2);
        assert_eq!(lido.versions[1].params.n_ctx, 16_384);

        remove_database(&path);
    }

    #[test]
    fn model_and_slice_history_remain_after_files_disappear() {
        let path = temporary_path("model-history");
        let whole_path = temporary_path("whole-gguf");
        let slice_path = temporary_path("slice-gguf");
        std::fs::write(&whole_path, b"whole-model").unwrap();
        std::fs::write(&slice_path, vec![7_u8; 4_096]).unwrap();
        let db = PomDb::open(&path).unwrap();
        let catalog = crate::catalog::Catalog {
            models: vec![
                model(
                    "whole.gguf",
                    &whole_path,
                    crate::catalog::ModelSource::Folder,
                    None,
                    None,
                    11,
                ),
                model(
                    "qwen · L0-15",
                    &slice_path,
                    crate::catalog::ModelSource::Local,
                    Some((0, 15)),
                    Some("pom-1"),
                    4_096,
                ),
            ],
            shard_sets: Vec::new(),
            scanned: Vec::new(),
            metadata_complete: true,
        };
        db.record_catalog(&catalog).unwrap();

        let present = db.model_history().unwrap();
        assert_eq!(present.len(), 2);
        assert!(present.iter().all(|(_, present)| *present));
        let slices = db.model_slice_history("pom_id:pom-1").unwrap();
        assert_eq!(slices.len(), 1);
        assert!(slices[0].1);
        assert_eq!(slices[0].2, Some(0));
        assert_eq!(slices[0].3, Some(15));
        assert_eq!(slices[0].4, 4_096);

        std::fs::remove_file(&whole_path).unwrap();
        std::fs::remove_file(&slice_path).unwrap();
        db.record_catalog(&crate::catalog::Catalog {
            models: Vec::new(),
            shard_sets: Vec::new(),
            scanned: Vec::new(),
            metadata_complete: true,
        })
        .unwrap();
        assert!(db
            .model_history()
            .unwrap()
            .iter()
            .all(|(_, present)| !present));
        assert!(!db.model_slice_history("pom_id:pom-1").unwrap()[0].1);
        remove_database(&path);
    }

    #[test]
    fn recording_a_whole_model_uses_its_path_without_hashing_the_weights() {
        let path = temporary_path("model-history-path-identity");
        let whole_path = temporary_path("whole-identity-gguf");
        std::fs::write(&whole_path, vec![7_u8; 4_096]).unwrap();
        let db = PomDb::open(&path).unwrap();
        let catalog = crate::catalog::Catalog {
            models: vec![model(
                "whole.gguf",
                &whole_path,
                crate::catalog::ModelSource::Folder,
                None,
                None,
                4_096,
            )],
            shard_sets: Vec::new(),
            scanned: Vec::new(),
            metadata_complete: false,
        };

        db.record_catalog(&catalog).unwrap();
        let identity = db
            .with_read("ler identidade de teste", |connection| {
                Ok(connection.query_row(
                    "SELECT identity_kind, identity FROM model_history",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?)
            })
            .unwrap();

        assert_eq!(identity.0, "path");
        assert_eq!(identity.1, whole_path.display().to_string());
        remove_database(&path);
        std::fs::remove_file(&whole_path).unwrap();
    }

    fn model(
        name: &str,
        path: &Path,
        source: crate::catalog::ModelSource,
        shard_range: Option<(u32, u32)>,
        pom_id: Option<&str>,
        size_bytes: u64,
    ) -> crate::catalog::DiscoveredModel {
        crate::catalog::DiscoveredModel {
            id: name.into(),
            name: name.into(),
            source,
            source_label: source.label(),
            path: path.display().to_string(),
            size_bytes,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
            architecture: Some("llama".into()),
            is_moe: false,
            total_layers: shard_range.map(|_| 32),
            quant: Some("Q4_K_M".into()),
            shard_range,
            shard_group: pom_id.map(str::to_string),
            sha256: None,
            model_label: pom_id.map(|_| "qwen".into()),
            pom_id: pom_id.map(str::to_string),
            tensor_parallel: false,
            tp_rank: None,
            tp_size: None,
            capabilities: None,
        }
    }

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pom-db-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn remove_database(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
