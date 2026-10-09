//! Persistent local state for the node.
//!
//! `pom.db` is the only database used by the runtime. This module deliberately
//! creates a fresh schema and contains no compatibility path for the old audit
//! database or deployment JSON files. Optional preservation of those files is
//! an explicit operation provided by `scripts/migrate_pom_db.py`.

use crate::catalog::{Catalog, DiscoveredModel};
use crate::config::{ApiKey, NetworkSettings};
use crate::deployment::Deployment;
use crate::thermal::ThermalPolicy;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub const DATABASE_FILENAME: &str = "pom.db";
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Last geo label observed for a public network peer. This tiny local index is
/// what lets a prefix/suffix search choose candidates before opening HTTP
/// probes; the signed pkarr record remains the source of the label itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PublicNetworkGeoCacheEntry {
    pub node_id: String,
    pub geo_label: String,
    pub fingerprint: String,
    pub observed_at: String,
}

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

/// Mensagem aceita pelo principal do Chat da rede. Eventos de digitação não
/// entram nesta tabela: são presença efêmera e nunca devem crescer o banco.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkChatMessageRecord {
    pub received_seq: u64,
    pub message_id: String,
    pub room_id: String,
    pub sender_node_id: String,
    pub recipient_node_id: Option<String>,
    pub kind: String,
    pub text: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_data: Vec<u8>,
    pub sent_at: String,
    pub received_at: String,
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
        if key.id.trim().is_empty() || key.key_hash.trim().is_empty() {
            anyhow::bail!("API key sem identidade ou hash");
        }
        self.with_write("salvar chave de API", |connection| {
            connection.execute(
                "INSERT INTO api_keys
                    (id, key_hash, key_hint, label, node_id, origin_node_id, active, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    label = excluded.label,
                    node_id = excluded.node_id,
                    origin_node_id = excluded.origin_node_id,
                    key_hint = CASE
                        WHEN excluded.key_hint = '' THEN api_keys.key_hint
                        ELSE excluded.key_hint
                    END,
                    active = CASE
                        WHEN EXISTS (
                            SELECT 1 FROM api_key_revocations
                            WHERE key_id = api_keys.id
                        ) THEN 0
                        ELSE excluded.active
                    END",
                params![
                    &key.id,
                    &key.key_hash,
                    &key.key_hint,
                    &key.label,
                    &key.node_id,
                    if key.origin_node_id.is_empty() {
                        &key.node_id
                    } else {
                        &key.origin_node_id
                    },
                    key.active as i64,
                    &key.created_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn api_keys(&self) -> Result<Vec<ApiKey>> {
        self.with_read("ler chaves de API", |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {API_KEY_COLUMNS} FROM api_keys ORDER BY created_at, id"
            ))?;
            let rows = statement.query_map([], api_key_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("ler API keys do banco local")
        })
    }

    /// Remove every locally stored API credential when the node leaves a
    /// network. The trust boundary is reset together with these rows, so a
    /// later setup cannot accidentally resurrect a network-owned secret from
    /// the database.
    pub fn clear_api_keys(&self) -> Result<()> {
        self.with_write("limpar chaves de API locais", |connection| {
            let tx = connection.unchecked_transaction()?;
            tx.execute("DELETE FROM api_key_revocations", [])?;
            tx.execute("DELETE FROM api_keys", [])?;
            tx.execute(
                "DELETE FROM local_secrets WHERE secret_key = ?",
                params![NODE_API_KEY_SECRET],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The active key whose hash matches a presented token. The token itself
    /// is hashed here and never reaches SQLite.
    pub fn api_key_for_token(&self, token: &str) -> Result<Option<ApiKey>> {
        if token.is_empty() {
            return Ok(None);
        }
        self.api_key_for_hash(&crate::auth::api_key_hash(token))
    }

    pub fn api_key_for_hash(&self, key_hash: &str) -> Result<Option<ApiKey>> {
        self.with_read("consultar chave de API", |connection| {
            connection
                .query_row(
                    &format!(
                        "SELECT {API_KEY_COLUMNS} FROM api_keys WHERE key_hash = ? AND active = 1"
                    ),
                    params![key_hash],
                    api_key_from_row,
                )
                .optional()
                .context("consultar API key no banco local")
        })
    }

    pub fn active_api_key_named(&self, label: &str, node_id: &str) -> Result<Option<ApiKey>> {
        self.with_read("consultar chave nomeada de API", |connection| {
            connection
                .query_row(
                    &format!(
                        "SELECT {API_KEY_COLUMNS} FROM api_keys
                         WHERE label = ? AND node_id = ? AND active = 1
                         ORDER BY created_at DESC, id DESC LIMIT 1"
                    ),
                    params![label, node_id],
                    api_key_from_row,
                )
                .optional()
                .context("consultar chave nomeada de API")
        })
    }

    /// The secret of this node's own key (the one its chat UI and plugins call
    /// `/v1` with), and its tracking id. It lives in `local_secrets`, the same
    /// place as the Hugging Face token: it is this node's client credential,
    /// not a row of the key registry, which holds only hashes.
    pub fn node_api_key(&self) -> Result<Option<(String, String)>> {
        self.with_read("ler chave de API do nó", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_text FROM local_secrets WHERE secret_key = ?",
                    params![NODE_API_KEY_SECRET],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let Some(value) = value else { return Ok(None) };
            let parsed: serde_json::Value =
                serde_json::from_str(&value).context("interpretar chave de API do nó")?;
            Ok(parsed["id"]
                .as_str()
                .zip(parsed["secret"].as_str())
                .map(|(id, secret)| (id.to_string(), secret.to_string())))
        })
    }

    pub fn save_node_api_key(&self, id: &str, secret: &str) -> Result<()> {
        let value = serde_json::json!({ "id": id, "secret": secret }).to_string();
        self.with_write("salvar chave de API do nó", |connection| {
            connection.execute(
                "INSERT INTO local_secrets (secret_key, value_text) VALUES (?, ?)
                 ON CONFLICT(secret_key) DO UPDATE SET value_text = excluded.value_text",
                params![NODE_API_KEY_SECRET, value],
            )?;
            Ok(())
        })
    }

    /// Revoke once, retaining both the inactive key and the first audit event.
    /// `key_or_id` is the tracking id or, for an operator holding it, the
    /// secret itself (matched by hash).
    pub fn revoke_api_key(&self, key_or_id: &str, revoked_by: &str) -> Result<bool> {
        let presented_hash = crate::auth::api_key_hash(key_or_id);
        self.with_write("revogar chave de API", |connection| {
            let tx = connection.unchecked_transaction()?;
            let key = tx
                .query_row(
                    "SELECT id, key_hash FROM api_keys WHERE id = ? OR key_hash = ?",
                    params![key_or_id, presented_hash],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((key_id, key_hash)) = key else {
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
                 SELECT ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE NOT EXISTS (SELECT 1 FROM api_key_revocations WHERE key_id = ?)",
                params![&key_id, &key_hash, revoked_by, &key_id],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Revoke a node's superseded internal keys (`pom-<node_id>`), keeping
    /// only the newest active one per node. The node mints a new internal key
    /// whenever its local secret stops matching, and the old secret is gone at
    /// that point, so every older active row is an orphan nobody can use.
    pub fn revoke_superseded_node_keys(&self, revoked_by: &str) -> Result<usize> {
        let superseded = self.with_read("procurar chaves internas substituídas", |connection| {
            let mut statement = connection.prepare(
                "SELECT older.id FROM api_keys AS older
                 WHERE older.active = 1
                   AND older.label = 'pom-' || older.node_id
                   AND EXISTS (
                       SELECT 1 FROM api_keys AS newer
                       WHERE newer.active = 1
                         AND newer.label = older.label
                         AND newer.node_id = older.node_id
                         AND (newer.created_at > older.created_at
                              OR (newer.created_at = older.created_at AND newer.id > older.id))
                   )",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("procurar chaves internas substituídas")
        })?;
        let mut revoked = 0;
        for id in superseded {
            if self.revoke_api_key(&id, revoked_by)? {
                revoked += 1;
            }
        }
        Ok(revoked)
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

    /// Revoke every active key from an origin. The update and audit inserts
    /// are one transaction, so retries are safe and cannot duplicate history.
    pub fn revoke_api_keys_by_origin(&self, origin: &str, revoked_by: &str) -> Result<usize> {
        self.with_write("revogar chaves da origem", |connection| {
            let tx = connection.unchecked_transaction()?;
            let ids = {
                let mut statement = tx.prepare(
                    "SELECT id, key_hash FROM api_keys
                     WHERE origin_node_id = ? OR (origin_node_id = '' AND node_id = ?)",
                )?;
                let rows = statement.query_map(params![origin, origin], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut changed = 0;
            for (id, key_hash) in ids {
                changed += tx.execute(
                    "UPDATE api_keys SET active = 0 WHERE id = ? AND active = 1",
                    params![&id],
                )?;
                tx.execute(
                    "INSERT INTO api_key_revocations
                        (key_id, key_sha256, revoked_by, revoked_at)
                     SELECT ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE NOT EXISTS (SELECT 1 FROM api_key_revocations WHERE key_id = ?)",
                    params![&id, &key_hash, revoked_by, &id],
                )?;
            }
            tx.commit()?;
            Ok(changed)
        })
    }

    /// Every model -> sidecar link. Authoritative on the principal; elsewhere
    /// the last copy read from it, used when the principal does not answer.
    pub fn model_sidecar_links(&self) -> Result<Vec<crate::catalog::SidecarLink>> {
        self.with_read("ler vínculos de visão e MTP", |connection| {
            let mut statement = connection.prepare(
                "SELECT model_key, kind, mode, file_name, size_bytes, updated_at, updated_by
                 FROM model_sidecar_links ORDER BY model_key, kind",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?;
            let mut links = Vec::new();
            for row in rows {
                let (model_key, kind, mode, file_name, size_bytes, updated_at, updated_by) = row?;
                links.push(crate::catalog::SidecarLink {
                    model_key,
                    kind: serde_json::from_value(serde_json::Value::String(kind))
                        .context("tipo de vínculo inválido")?,
                    mode: serde_json::from_value(serde_json::Value::String(mode))
                        .context("modo de vínculo inválido")?,
                    file_name,
                    size_bytes: size_bytes.and_then(|size| u64::try_from(size).ok()),
                    updated_at,
                    updated_by,
                });
            }
            Ok(links)
        })
    }

    /// Insert or replace one link (one model, one kind).
    pub fn save_model_sidecar_link(&self, link: &crate::catalog::SidecarLink) -> Result<()> {
        link.validate().map_err(anyhow::Error::msg)?;
        let kind = serde_json::to_value(link.kind)?.as_str().unwrap_or_default().to_string();
        let mode = serde_json::to_value(link.mode)?.as_str().unwrap_or_default().to_string();
        self.with_write("salvar vínculo de visão ou MTP", |connection| {
            connection.execute(
                "INSERT INTO model_sidecar_links
                    (model_key, kind, mode, file_name, size_bytes, updated_at, updated_by)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(model_key, kind) DO UPDATE SET
                    mode = excluded.mode,
                    file_name = excluded.file_name,
                    size_bytes = excluded.size_bytes,
                    updated_at = excluded.updated_at,
                    updated_by = excluded.updated_by",
                params![
                    &link.model_key,
                    kind,
                    mode,
                    &link.file_name,
                    link.size_bytes.and_then(|size| i64::try_from(size).ok()),
                    &link.updated_at,
                    &link.updated_by,
                ],
            )?;
            Ok(())
        })
    }

    /// Replace the local copy with the principal's full list.
    pub fn replace_model_sidecar_links(&self, links: &[crate::catalog::SidecarLink]) -> Result<()> {
        self.with_write("espelhar vínculos de visão e MTP", |connection| {
            let tx = connection.unchecked_transaction()?;
            tx.execute("DELETE FROM model_sidecar_links", [])?;
            tx.commit()?;
            Ok(())
        })?;
        for link in links {
            self.save_model_sidecar_link(link)?;
        }
        Ok(())
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

    pub fn network_settings(&self) -> Result<Option<NetworkSettings>> {
        self.with_read("ler detalhes da rede", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'network_settings'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).context("interpretar detalhes da rede"))
                .transpose()
        })
    }

    pub fn save_network_settings(&self, settings: &NetworkSettings) -> Result<()> {
        self.save_setting("network_settings", settings)
    }

    pub fn windows_firewall_setup(&self) -> Result<Option<crate::windows_firewall::SetupRecord>> {
        self.with_read("ler estado do firewall do Windows", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'windows_firewall_setup'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| {
                    serde_json::from_str(&value)
                        .context("interpretar estado do firewall do Windows")
                })
                .transpose()
        })
    }

    pub fn save_windows_firewall_setup(
        &self,
        record: &crate::windows_firewall::SetupRecord,
    ) -> Result<()> {
        self.save_setting("windows_firewall_setup", record)
    }

    pub(crate) fn public_directory_settings(
        &self,
    ) -> Result<Option<crate::public_directory::PublicDirectorySettings>> {
        self.with_read("ler ingressos do diretório público", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'public_directory_settings'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| {
                    serde_json::from_str(&value)
                        .context("interpretar ingressos do diretório público")
                })
                .transpose()
        })
    }

    pub(crate) fn save_public_directory_settings(
        &self,
        settings: &crate::public_directory::PublicDirectorySettings,
    ) -> Result<()> {
        self.save_setting("public_directory_settings", settings)
    }

    pub(crate) fn public_directory_records(
        &self,
    ) -> Result<Vec<crate::public_directory::SignedPublicDirectoryRecord>> {
        self.with_read("ler cache do diretório público", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'public_directory_records'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| {
                    serde_json::from_str(&value).context("interpretar cache do diretório público")
                })
                .transpose()
                .map(|records| records.unwrap_or_default())
        })
    }

    pub(crate) fn save_public_directory_records(
        &self,
        records: &[crate::public_directory::SignedPublicDirectoryRecord],
    ) -> Result<()> {
        self.save_setting("public_directory_records", records)
    }

    pub fn public_network_geo_cache(&self) -> Result<Vec<PublicNetworkGeoCacheEntry>> {
        self.with_read("ler índice geo de redes públicas", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = 'public_network_geo_cache'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            value
                .map(|value| {
                    serde_json::from_str(&value).context("interpretar índice geo de redes públicas")
                })
                .transpose()
                .map(|entries| entries.unwrap_or_default())
        })
    }

    /// Admission requests this node holds as a network's principal.
    pub fn lobby_admissions(&self) -> Result<Vec<crate::network_lobby::AdmissionRecord>> {
        self.read_setting("lobby_admissions")
    }

    pub fn save_lobby_admissions(
        &self,
        records: &[crate::network_lobby::AdmissionRecord],
    ) -> Result<()> {
        self.save_setting("lobby_admissions", &records.to_vec())
    }

    /// Admission requests this node made to other networks, without grants.
    pub fn lobby_requests(&self) -> Result<Vec<serde_json::Value>> {
        self.read_setting("lobby_requests")
    }

    pub fn save_lobby_requests(&self, requests: &[serde_json::Value]) -> Result<()> {
        self.save_setting("lobby_requests", &requests.to_vec())
    }

    /// Trust documents of the networks the operator imported on this machine.
    pub fn custom_networks(&self) -> Result<Vec<String>> {
        self.read_setting("custom_networks")
    }

    pub fn save_custom_networks(&self, documents: &[String]) -> Result<()> {
        self.save_setting("custom_networks", &documents.to_vec())
    }

    /// A JSON list kept in `local_settings`; missing or unreadable is empty.
    fn read_setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Vec<T>> {
        self.with_read("ler configuração local", |connection| {
            let value = connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = ?",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(value
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or_default())
        })
    }

    pub fn save_public_network_geo_cache(
        &self,
        entries: &[PublicNetworkGeoCacheEntry],
    ) -> Result<()> {
        self.save_setting("public_network_geo_cache", &entries.to_vec())
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

    /// Persiste uma ação de texto ou arquivo exatamente uma vez por id.
    pub fn append_network_chat(&self, action: &crate::network_chat::ChatAction) -> Result<bool> {
        action
            .validate()
            .map_err(|error| anyhow::anyhow!("ação de chat inválida: {error}"))?;
        let (
            message_id,
            room_id,
            sender_node_id,
            recipient_node_id,
            kind,
            text,
            file_name,
            mime_type,
            file_data,
            sent_at,
        ) = match action {
            crate::network_chat::ChatAction::Text {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                text,
                sent_at,
            } => (
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                "text",
                Some(text),
                None,
                None,
                Vec::new(),
                sent_at.to_rfc3339(),
            ),
            crate::network_chat::ChatAction::File {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                file_name,
                mime_type,
                data,
                sent_at,
            } => (
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                "file",
                None,
                Some(file_name),
                Some(mime_type),
                data.clone(),
                sent_at.to_rfc3339(),
            ),
            crate::network_chat::ChatAction::Typing { .. } => {
                anyhow::bail!("digitação não é persistente")
            }
            crate::network_chat::ChatAction::PluginNotice { .. } => {
                anyhow::bail!("aviso de plugin não é persistente")
            }
            crate::network_chat::ChatAction::LobbyPresence { .. } => {
                anyhow::bail!("presença do lobby não é persistente")
            }
        };
        self.with_write("salvar mensagem do Chat da rede", |connection| {
            let changed = connection.execute(
                "INSERT OR IGNORE INTO network_chat_messages (
                    message_id, room_id, sender_node_id, recipient_node_id, kind,
                    text, file_name, mime_type, file_data, sent_at, received_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                params![
                    message_id,
                    room_id,
                    sender_node_id,
                    recipient_node_id,
                    kind,
                    text,
                    file_name,
                    mime_type,
                    file_data,
                    sent_at,
                ],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn network_chat_messages(
        &self,
        room_id: &str,
        limit: usize,
    ) -> Result<Vec<NetworkChatMessageRecord>> {
        let limit = limit.clamp(1, 500) as i64;
        self.with_read("ler histórico do Chat da rede", |connection| {
            let mut statement = connection.prepare(
                "SELECT received_seq, message_id, room_id, sender_node_id,
                        recipient_node_id, kind, text, file_name, mime_type,
                        file_data, sent_at, received_at
                 FROM network_chat_messages
                 WHERE room_id = ?
                 ORDER BY received_seq DESC
                 LIMIT ?",
            )?;
            let rows = statement.query_map(params![room_id, limit], |row| {
                Ok(NetworkChatMessageRecord {
                    received_seq: row.get::<_, i64>(0)? as u64,
                    message_id: row.get(1)?,
                    room_id: row.get(2)?,
                    sender_node_id: row.get(3)?,
                    recipient_node_id: row.get(4)?,
                    kind: row.get(5)?,
                    text: row.get(6)?,
                    file_name: row.get(7)?,
                    mime_type: row.get(8)?,
                    file_data: row.get::<_, Option<Vec<u8>>>(9)?.unwrap_or_default(),
                    sent_at: row.get(10)?,
                    received_at: row.get(11)?,
                })
            })?;
            let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            messages.reverse();
            Ok(messages)
        })
    }

    pub fn network_chat_file(&self, message_id: &str) -> Result<Option<(Vec<u8>, String)>> {
        self.with_read("ler arquivo do Chat da rede", |connection| {
            connection
                .query_row(
                    "SELECT file_data, mime_type
                     FROM network_chat_messages
                     WHERE message_id = ? AND kind = 'file'",
                    params![message_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default(),
                            row.get::<_, Option<String>>(1)?
                                .unwrap_or_else(|| "application/octet-stream".to_string()),
                        ))
                    },
                )
                .optional()
                .context("consultar arquivo do Chat da rede")
        })
    }

    fn save_setting<T: serde::Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        self.with_write("salvar configuração local", |connection| {
            connection.execute(
                "INSERT INTO local_settings (setting_key, value_json) VALUES (?, ?)
                 ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json",
                params![key, serde_json::to_string(value)?],
            )?;
            Ok(())
        })
    }

    pub(crate) fn local_setting_json(&self, key: &str) -> Result<Option<String>> {
        self.with_read("ler configuração local", |connection| {
            connection
                .query_row(
                    "SELECT value_json FROM local_settings WHERE setting_key = ?",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("ler configuração local")
        })
    }

    pub(crate) fn local_setting_keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        self.with_read("listar chaves de configuração local", |connection| {
            let mut statement = connection.prepare(
                "SELECT setting_key FROM local_settings
                 WHERE substr(setting_key, 1, length(?1)) = ?1
                 ORDER BY setting_key",
            )?;
            let rows = statement.query_map(params![prefix], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .context("listar chaves de configuração local")
        })
    }

    pub(crate) fn save_local_setting<T: serde::Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<()> {
        self.save_setting(key, value)
    }

    pub(crate) fn delete_local_setting(&self, key: &str) -> Result<bool> {
        self.with_write("remover configuração local", |connection| {
            Ok(connection.execute(
                "DELETE FROM local_settings WHERE setting_key = ?",
                params![key],
            )? > 0)
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
            key_hash: crate::auth::api_key_hash(&format!("secret-{marker}")),
            key_hint: String::new(),
            label: marker.into(),
            node_id: String::new(),
            origin_node_id: String::new(),
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
                "INSERT INTO api_keys (id, key_hash, label, node_id, origin_node_id, active, created_at)
                 VALUES ('held-write', 'held-hash', 'held', '', '', 1, 'held')",
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
            key_hash TEXT NOT NULL UNIQUE,
            key_hint TEXT NOT NULL DEFAULT '',
            label TEXT NOT NULL,
            node_id TEXT NOT NULL,
            origin_node_id TEXT NOT NULL DEFAULT '',
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
        CREATE TABLE IF NOT EXISTS model_sidecar_links (
            model_key TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('mmproj', 'mtp')),
            mode TEXT NOT NULL CHECK (mode IN ('file', 'embedded', 'none')),
            file_name TEXT,
            size_bytes INTEGER,
            updated_at TEXT NOT NULL,
            updated_by TEXT NOT NULL,
            PRIMARY KEY (model_key, kind)
        );
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
        CREATE TABLE IF NOT EXISTS network_chat_messages (
            received_seq INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id TEXT NOT NULL UNIQUE,
            room_id TEXT NOT NULL,
            sender_node_id TEXT NOT NULL,
            recipient_node_id TEXT,
            kind TEXT NOT NULL CHECK (kind IN ('text', 'file')),
            text TEXT,
            file_name TEXT,
            mime_type TEXT,
            file_data BLOB NOT NULL DEFAULT X'',
            sent_at TEXT NOT NULL,
            received_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS network_chat_room_seq_idx
            ON network_chat_messages(room_id, received_seq);
        CREATE INDEX IF NOT EXISTS network_chat_sender_idx
            ON network_chat_messages(sender_node_id, received_seq);
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
        -- Process logs belong to the logs plugin's own database now.
        DROP TABLE IF EXISTS cluster_logs;
        DROP TABLE IF EXISTS cluster_log_counters;
        DROP TABLE IF EXISTS geo_cities;
        DROP TABLE IF EXISTS geo_countries;
        PRAGMA user_version = 1;",
    )?;
    let has_origin_node_id = connection
        .prepare("PRAGMA table_info(api_keys)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "origin_node_id");
    if !has_origin_node_id {
        connection.execute(
            "ALTER TABLE api_keys ADD COLUMN origin_node_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
        connection.execute(
            "UPDATE api_keys SET origin_node_id = node_id WHERE origin_node_id = ''",
            [],
        )?;
    }
    migrate_api_key_secrets(connection)?;
    Ok(())
}

/// Bancos de antes do hash guardavam cada segredo, em texto puro, na coluna
/// `api_keys.key`. Esta migração troca o segredo pelo SHA-256 e pelo trecho
/// visível, e renomeia a coluna para `key_hash`. O tracking id (`id`) não
/// muda: os logs continuam ligados às mesmas chaves, e quem já tem o token
/// continua usando o mesmo, porque a validação compara hashes.
///
/// O texto antigo não pode sobrar no arquivo: `secure_delete` zera as páginas
/// liberadas, e o checkpoint e o `VACUUM` reescrevem WAL e banco sem ele.
fn migrate_api_key_secrets(connection: &Connection) -> Result<()> {
    let columns = connection
        .prepare("PRAGMA table_info(api_keys)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "key") {
        return Ok(());
    }
    connection.execute_batch("PRAGMA secure_delete = ON;")?;
    let tx = connection.unchecked_transaction()?;
    if !columns.iter().any(|column| column == "key_hint") {
        tx.execute(
            "ALTER TABLE api_keys ADD COLUMN key_hint TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    let secrets = {
        let mut statement = tx.prepare("SELECT id, key FROM api_keys")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, secret) in &secrets {
        tx.execute(
            "UPDATE api_keys SET key = ?, key_hint = ? WHERE id = ?",
            params![
                crate::auth::api_key_hash(secret),
                crate::auth::api_key_hint(secret),
                id
            ],
        )?;
    }
    tx.execute("ALTER TABLE api_keys RENAME COLUMN key TO key_hash", [])?;
    tx.commit()?;
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        .context("reescrever o banco sem os segredos antigos")?;
    tracing::info!(
        "API keys migradas para hash: {} chave(s), tracking ids preservados",
        secrets.len()
    );
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

/// Columns of an `ApiKey`, in `api_key_from_row` order.
const API_KEY_COLUMNS: &str =
    "id, key_hash, key_hint, label, node_id, origin_node_id, active, created_at";

/// `local_secrets` row holding this node's own API key (`node_api_key`).
const NODE_API_KEY_SECRET: &str = "node_api_key";

fn api_key_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: row.get(0)?,
        key_hash: row.get(1)?,
        key_hint: row.get(2)?,
        label: row.get(3)?,
        node_id: row.get(4)?,
        origin_node_id: row.get(5)?,
        active: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
    })
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
        Ok((format!("path:{identity}"), "path", identity, "whole"))
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
    use rusqlite::Connection;
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
            key_hash: crate::auth::api_key_hash("sk-secret"),
            key_hint: String::new(),
            label: "operador".into(),
            node_id: "node-a".into(),
            origin_node_id: "node-a".into(),
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
            key_hash: crate::auth::api_key_hash("sk-secret"),
            key_hint: String::new(),
            label: "operador".into(),
            node_id: "node-a".into(),
            origin_node_id: "node-a".into(),
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
    fn revoking_an_origin_is_idempotent_and_does_not_touch_other_origins() {
        let path = temporary_path("revoke-origin");
        let db = PomDb::open(&path).expect("abrir banco novo");
        for (id, origin) in [("a-1", "node-a"), ("a-2", "node-a"), ("b-1", "node-b")] {
            db.insert_api_key(&ApiKey {
                id: id.into(),
                key_hash: crate::auth::api_key_hash(&format!("sk-{id}")),
                key_hint: String::new(),
                label: id.into(),
                node_id: origin.into(),
                origin_node_id: origin.into(),
                active: true,
                created_at: "2026-09-07T00:00:00Z".into(),
            })
            .unwrap();
        }

        assert_eq!(
            db.revoke_api_keys_by_origin("node-a", "principal").unwrap(),
            2
        );
        assert_eq!(
            db.revoke_api_keys_by_origin("node-a", "principal").unwrap(),
            0
        );
        assert!(db.api_key_for_token("sk-a-1").unwrap().is_none());
        assert!(db.api_key_for_token("sk-a-2").unwrap().is_none());
        assert!(db.api_key_for_token("sk-b-1").unwrap().is_some());
        assert_eq!(db.api_key_revocations().unwrap().len(), 2);
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
    fn thermal_policy_rejects_integrity_limits_before_persisting() {
        let path = temporary_path("thermal-limits");
        let db = PomDb::open(&path).unwrap();

        assert!(db
            .save_thermal_policy(crate::thermal::ThermalPolicy {
                limit_c: 69.9,
                hysteresis_c: 6.0,
                cooldown_secs: 30,
            })
            .is_err());
        assert!(db
            .save_thermal_policy(crate::thermal::ThermalPolicy {
                limit_c: 85.0,
                hysteresis_c: 6.0,
                cooldown_secs: 61,
            })
            .is_err());
        assert!(db.thermal_policy().unwrap().is_none());

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
        use crate::deployment::{Deployment, DeploymentKind, DeploymentModel, DeploymentNode};
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
            sidecars: Vec::new(),
            sidecar_roots: Vec::new(),
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
            sidecars: Vec::new(),
            sidecar_roots: Vec::new(),
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
            sidecars: Vec::new(),
            sidecar_roots: Vec::new(),
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
            mmproj_path: None,
            mmproj_candidates: Vec::new(),
            mmproj_reason: None,
            identity_names: Vec::new(),
            size_bytes,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
            architecture: Some("llama".into()),
            embedding_length: None,
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

    #[test]
    fn revokes_superseded_node_keys_and_keeps_the_newest() {
        let path = temporary_path("superseded");
        let db = PomDb::open(&path).expect("abrir banco novo");
        let key = |id: &str, label: &str, node: &str, created: &str| ApiKey {
            id: id.into(),
            key_hash: crate::auth::api_key_hash(id),
            key_hint: String::new(),
            label: label.into(),
            node_id: node.into(),
            origin_node_id: node.into(),
            active: true,
            created_at: created.into(),
        };
        db.insert_api_key(&key("old", "pom-n1", "n1", "2026-01-01"))
            .unwrap();
        db.insert_api_key(&key("new", "pom-n1", "n1", "2026-10-03"))
            .unwrap();
        db.insert_api_key(&key("only", "pom-n2", "n2", "2026-01-01"))
            .unwrap();
        // Chave de produto com dois tokens: não é interna, nada muda.
        db.insert_api_key(&key("prod-a", "produto", "n1", "2026-01-01"))
            .unwrap();
        db.insert_api_key(&key("prod-b", "produto", "n1", "2026-10-03"))
            .unwrap();

        assert_eq!(db.revoke_superseded_node_keys("principal").unwrap(), 1);
        let active: Vec<String> = db
            .api_keys()
            .unwrap()
            .into_iter()
            .filter(|key| key.active)
            .map(|key| key.id)
            .collect();
        assert_eq!(active, vec!["only", "prod-a", "new", "prod-b"]);
        assert_eq!(db.revoke_superseded_node_keys("principal").unwrap(), 0);
        remove_database(&path);
    }

    /// Um banco de antes do hash: o segredo em texto puro na coluna `key`.
    /// Depois de abrir, a chave vale pelo mesmo token, mantém o tracking id, e
    /// o texto não está mais em nenhum byte do arquivo nem do WAL.
    #[test]
    fn migrates_plaintext_api_keys_to_hashes_and_scrubs_the_file() {
        let root = temporary_path("hash-migration");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(DATABASE_FILENAME);
        let secret = "sk-0b5d7e2a-1c3f-4a6b-9d8e-7f6a5b4c3d2e";
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(&format!(
                    "PRAGMA journal_mode = WAL;
                     CREATE TABLE api_keys (
                        id TEXT PRIMARY KEY,
                        key TEXT NOT NULL UNIQUE,
                        label TEXT NOT NULL,
                        node_id TEXT NOT NULL,
                        origin_node_id TEXT NOT NULL DEFAULT '',
                        active INTEGER NOT NULL DEFAULT 1,
                        created_at TEXT NOT NULL
                     );
                     CREATE TABLE api_key_revocations (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        key_id TEXT NOT NULL REFERENCES api_keys(id),
                        key_sha256 TEXT NOT NULL,
                        revoked_by TEXT NOT NULL,
                        revoked_at TEXT NOT NULL
                     );
                     INSERT INTO api_keys VALUES
                        ('produto-x', '{secret}', 'produto', 'n1', 'n1', 1, '2026-01-01');"
                ))
                .unwrap();
        }

        let db = PomDb::open(&path).expect("abrir e migrar");
        let key = db
            .api_key_for_token(secret)
            .unwrap()
            .expect("mesmo token vale");
        assert_eq!(key.id, "produto-x");
        assert_eq!(key.key_hash, crate::auth::api_key_hash(secret));
        assert_eq!(key.key_hint, "sk-0b5d7…3d2e");
        drop(db);
        let reopened = PomDb::open(&path).expect("reabrir já migrado");
        assert_eq!(reopened.api_keys().unwrap().len(), 1);
        drop(reopened);

        for file in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
        ] {
            if let Ok(bytes) = std::fs::read(&file) {
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "segredo em texto puro sobrou em {file:?}"
                );
            }
        }
        remove_database(&path);
        let _ = std::fs::remove_dir_all(root);
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
