//! Canal global de conversa sobre o enxame libp2p.
//!
//! Os tipos e limites ficam neste módulo para que o protocolo, o gateway e a
//! interface compartilhem a mesma fronteira. O worker do swarm é adicionado
//! depois da validação destes contratos; nada aqui depende do túnel de
//! inferência.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::prelude::*;
use libp2p::{request_response, StreamProtocol};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

pub const NETWORK_CHAT_PROTOCOL: &str = "/pom/network-chat/1.0.0";
pub const NETWORK_CHAT_ROOM: &str = "network";

#[derive(Debug, Clone, Copy)]
pub struct NetworkChatLimits;

impl NetworkChatLimits {
    pub const FRAME_BYTES: usize = 12 * 1024 * 1024;
    pub const TEXT_BYTES: usize = 8 * 1024;
    pub const FILE_BYTES: usize = 8 * 1024 * 1024;
    pub const NAME_BYTES: usize = 255;
    pub const ROOM_BYTES: usize = 128;
    pub const NODE_ID_BYTES: usize = 128;
    pub const NOTICE_TITLE_BYTES: usize = 200;
    pub const NOTICE_BODY_BYTES: usize = 2 * 1024;
    pub const PLUGIN_CODE_BYTES: usize = 64;
}

/// Quantos avisos de plugin ficam na memória de cada nó e por quanto tempo.
/// Avisos não são histórico de conversa: não vão para o `pom.db`.
const NOTICE_CAPACITY: usize = 50;
const NOTICE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const NOTICE_LEVELS: [&str; 4] = ["info", "success", "warning", "error"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatAction {
    Text {
        message_id: String,
        room_id: String,
        sender_node_id: String,
        recipient_node_id: Option<String>,
        text: String,
        sent_at: DateTime<Utc>,
    },
    File {
        message_id: String,
        room_id: String,
        sender_node_id: String,
        recipient_node_id: Option<String>,
        file_name: String,
        mime_type: String,
        data: Vec<u8>,
        sent_at: DateTime<Utc>,
    },
    Typing {
        message_id: String,
        room_id: String,
        sender_node_id: String,
        recipient_node_id: Option<String>,
        is_typing: bool,
        sent_at: DateTime<Utc>,
    },
    /// Aviso enviado por um plugin aos usuários da rede (`pom-plugin-events/v1`,
    /// comando `notification.notify` com `scope: "network"`). Viaja pelo mesmo
    /// caminho autenticado do Chat, mas fica só na memória de cada nó.
    PluginNotice {
        message_id: String,
        room_id: String,
        sender_node_id: String,
        recipient_node_id: Option<String>,
        plugin_code: String,
        title: String,
        body: String,
        level: String,
        sent_at: DateTime<Utc>,
    },
    /// A candidate is present in the lobby (`network_lobby.rs`). Only the
    /// principal originates it, from a lobby request it authenticated by the
    /// transport; `sender_node_id` is the candidate. Kept in memory only.
    LobbyPresence {
        message_id: String,
        room_id: String,
        sender_node_id: String,
        recipient_node_id: Option<String>,
        card: crate::network_lobby::LobbyCard,
        sent_at: DateTime<Utc>,
    },
}

impl ChatAction {
    pub fn text_message(
        room_id: impl Into<String>,
        sender_node_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::Text {
            message_id: uuid::Uuid::new_v4().simple().to_string(),
            room_id: room_id.into(),
            sender_node_id: sender_node_id.into(),
            recipient_node_id: None,
            text: text.into(),
            sent_at: Utc::now(),
        }
    }

    pub fn sender_node_id(&self) -> &str {
        match self {
            Self::Text { sender_node_id, .. }
            | Self::File { sender_node_id, .. }
            | Self::Typing { sender_node_id, .. }
            | Self::PluginNotice { sender_node_id, .. }
            | Self::LobbyPresence { sender_node_id, .. } => sender_node_id,
        }
    }

    pub fn room_id(&self) -> &str {
        match self {
            Self::Text { room_id, .. }
            | Self::File { room_id, .. }
            | Self::Typing { room_id, .. }
            | Self::PluginNotice { room_id, .. }
            | Self::LobbyPresence { room_id, .. } => room_id,
        }
    }

    pub fn message_id(&self) -> &str {
        match self {
            Self::Text { message_id, .. }
            | Self::File { message_id, .. }
            | Self::Typing { message_id, .. }
            | Self::PluginNotice { message_id, .. }
            | Self::LobbyPresence { message_id, .. } => message_id,
        }
    }

    pub fn recipient_node_id(&self) -> Option<&str> {
        match self {
            Self::Text {
                recipient_node_id, ..
            }
            | Self::File {
                recipient_node_id, ..
            }
            | Self::Typing {
                recipient_node_id, ..
            }
            | Self::PluginNotice {
                recipient_node_id, ..
            }
            | Self::LobbyPresence {
                recipient_node_id, ..
            } => recipient_node_id.as_deref(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::File { .. } => "file",
            Self::Typing { .. } => "typing",
            Self::PluginNotice { .. } => "plugin_notice",
            Self::LobbyPresence { .. } => "lobby_presence",
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::PluginNotice { body, .. } => Some(body),
            Self::File { .. } | Self::Typing { .. } | Self::LobbyPresence { .. } => None,
        }
    }

    pub fn is_typing(&self) -> bool {
        matches!(
            self,
            Self::Typing {
                is_typing: true,
                ..
            }
        )
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.message_id().is_empty()
            || self.message_id().len() > NetworkChatLimits::NODE_ID_BYTES
        {
            return Err("message_id_invalid");
        }
        if self.room_id().is_empty() || self.room_id().len() > NetworkChatLimits::ROOM_BYTES {
            return Err("room_too_large");
        }
        if self.sender_node_id().is_empty()
            || self.sender_node_id().len() > NetworkChatLimits::NODE_ID_BYTES
        {
            return Err("sender_node_id_invalid");
        }
        match self {
            Self::Text { text, .. } if text.len() > NetworkChatLimits::TEXT_BYTES => {
                Err("text_too_large")
            }
            Self::File { file_name, .. } if file_name.is_empty() => Err("file_name_empty"),
            Self::File {
                file_name, data, ..
            } if file_name.len() > NetworkChatLimits::NAME_BYTES => Err("file_name_too_large"),
            Self::File { data, .. } if data.len() > NetworkChatLimits::FILE_BYTES => {
                Err("file_too_large")
            }
            Self::PluginNotice {
                plugin_code,
                title,
                body,
                level,
                ..
            } => {
                let code_ok = !plugin_code.is_empty()
                    && plugin_code.len() <= NetworkChatLimits::PLUGIN_CODE_BYTES
                    && plugin_code.as_bytes()[0].is_ascii_lowercase()
                    && plugin_code
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
                if !code_ok {
                    Err("plugin_code_invalid")
                } else if title.trim().is_empty()
                    || title.len() > NetworkChatLimits::NOTICE_TITLE_BYTES
                {
                    Err("notice_title_invalid")
                } else if body.len() > NetworkChatLimits::NOTICE_BODY_BYTES {
                    Err("notice_body_too_large")
                } else if !NOTICE_LEVELS.contains(&level.as_str()) {
                    Err("notice_level_invalid")
                } else {
                    Ok(())
                }
            }
            Self::LobbyPresence { room_id, .. }
                if room_id != crate::network_lobby::LOBBY_ROOM =>
            {
                Err("lobby_room_invalid")
            }
            Self::Text { .. }
            | Self::File { .. }
            | Self::Typing { .. }
            | Self::LobbyPresence { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatRequest {
    pub action: ChatAction,
    /// Credencial do cliente. O principal valida este campo no frame original;
    /// uma retransmissão o deixa vazio para não espalhar segredo pela rede.
    pub api_key: String,
    #[serde(default)]
    pub relayed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatResponse {
    pub accepted: bool,
    pub error: Option<String>,
}

impl ChatResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            error: None,
        }
    }

    pub fn rejected(error: impl Into<String>) -> Self {
        Self {
            accepted: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug)]
pub struct ChatOutbound {
    pub request: ChatRequest,
    pub reply: oneshot::Sender<ChatResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatEvent {
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
    /// Só em `plugin_notice`: o plugin que emitiu o aviso.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_code: Option<String>,
    /// Só em `plugin_notice`: título do aviso.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Só em `plugin_notice`: `info`, `success`, `warning` ou `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

impl ChatEvent {
    pub fn from_action(action: &ChatAction) -> Self {
        match action {
            ChatAction::Text {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                text,
                sent_at,
            } => Self {
                message_id: message_id.clone(),
                room_id: room_id.clone(),
                sender_node_id: sender_node_id.clone(),
                recipient_node_id: recipient_node_id.clone(),
                kind: "text".into(),
                text: Some(text.clone()),
                file_name: None,
                mime_type: None,
                file_data: Vec::new(),
                sent_at: sent_at.to_rfc3339(),
                plugin_code: None,
                title: None,
                level: None,
            },
            ChatAction::File {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                file_name,
                mime_type,
                data,
                sent_at,
            } => Self {
                message_id: message_id.clone(),
                room_id: room_id.clone(),
                sender_node_id: sender_node_id.clone(),
                recipient_node_id: recipient_node_id.clone(),
                kind: "file".into(),
                text: None,
                file_name: Some(file_name.clone()),
                mime_type: Some(mime_type.clone()),
                file_data: data.clone(),
                sent_at: sent_at.to_rfc3339(),
                plugin_code: None,
                title: None,
                level: None,
            },
            ChatAction::Typing {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                is_typing,
                sent_at,
            } => Self {
                message_id: message_id.clone(),
                room_id: room_id.clone(),
                sender_node_id: sender_node_id.clone(),
                recipient_node_id: recipient_node_id.clone(),
                kind: if *is_typing {
                    "typing"
                } else {
                    "typing_stopped"
                }
                .into(),
                text: None,
                file_name: None,
                mime_type: None,
                file_data: Vec::new(),
                sent_at: sent_at.to_rfc3339(),
                plugin_code: None,
                title: None,
                level: None,
            },
            ChatAction::PluginNotice {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                plugin_code,
                title,
                body,
                level,
                sent_at,
            } => Self {
                message_id: message_id.clone(),
                room_id: room_id.clone(),
                sender_node_id: sender_node_id.clone(),
                recipient_node_id: recipient_node_id.clone(),
                kind: "plugin_notice".into(),
                text: Some(body.clone()),
                file_name: None,
                mime_type: None,
                file_data: Vec::new(),
                sent_at: sent_at.to_rfc3339(),
                plugin_code: Some(plugin_code.clone()),
                title: Some(title.clone()),
                level: Some(level.clone()),
            },
            ChatAction::LobbyPresence {
                message_id,
                room_id,
                sender_node_id,
                recipient_node_id,
                sent_at,
                ..
            } => Self {
                message_id: message_id.clone(),
                room_id: room_id.clone(),
                sender_node_id: sender_node_id.clone(),
                recipient_node_id: recipient_node_id.clone(),
                kind: "lobby_presence".into(),
                text: None,
                file_name: None,
                mime_type: None,
                file_data: Vec::new(),
                sent_at: sent_at.to_rfc3339(),
                plugin_code: None,
                title: None,
                level: None,
            },
        }
    }
}

impl From<crate::db::NetworkChatMessageRecord> for ChatEvent {
    fn from(message: crate::db::NetworkChatMessageRecord) -> Self {
        Self {
            message_id: message.message_id,
            room_id: message.room_id,
            sender_node_id: message.sender_node_id,
            recipient_node_id: message.recipient_node_id,
            kind: message.kind,
            text: message.text,
            file_name: message.file_name,
            mime_type: message.mime_type,
            file_data: message.file_data,
            sent_at: message.sent_at,
            plugin_code: None,
            title: None,
            level: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RateWindow {
    started: Instant,
    count: u16,
    typing_started: Instant,
    typing_count: u16,
}

#[derive(Debug)]
enum PersistCommand {
    Action(ChatAction),
}

/// Estado do canal. O envio para o swarm é sempre `try_send`; o serviço não
/// conhece nem compartilha a fila de frames de inferência.
pub struct NetworkChat {
    database: Arc<crate::db::PomDb>,
    auth: Arc<RwLock<crate::config::Config>>,
    outbound: mpsc::Sender<ChatOutbound>,
    dropped: AtomicU64,
    enabled: std::sync::atomic::AtomicBool,
    live: broadcast::Sender<ChatEvent>,
    writer: std::sync::mpsc::SyncSender<PersistCommand>,
    rate: Mutex<HashMap<String, RateWindow>>,
    typing: Mutex<HashMap<String, (ChatEvent, Instant)>>,
    notices: Mutex<std::collections::VecDeque<(ChatEvent, Instant)>>,
    /// Candidates present in the lobby: fed by lobby requests on the
    /// principal and by relayed `LobbyPresence` frames on every other member.
    lobby_guests: crate::network_lobby::LobbyGuests,
    /// What this network publishes about itself to non-members, on the
    /// principal (`network_lobby::PublicStats`).
    public_stats: crate::network_lobby::PublicStats,
    /// What this node answers about public networks to whoever asks by key
    /// (`network_lobby::DirectoryBoard`).
    directory_board: crate::network_lobby::DirectoryBoard,
    /// Admission requests this node holds as principal. Loaded from the
    /// database and written back on every change.
    admissions: Mutex<Vec<crate::network_lobby::AdmissionRecord>>,
    /// Nodes that left recently, kept off the member list for a while.
    departed: Mutex<HashMap<String, Instant>>,
    /// Departures heard on the lobby connection, waiting for the gateway to
    /// rotate the connection key (the swarm loop cannot push it itself).
    rotations: Mutex<Vec<String>>,
    rotation_wanted: tokio::sync::Notify,
}

impl NetworkChat {
    pub fn new(
        database: Arc<crate::db::PomDb>,
        auth: Arc<RwLock<crate::config::Config>>,
        enabled: bool,
    ) -> (Arc<Self>, mpsc::Receiver<ChatOutbound>) {
        let (outbound, receiver) = mpsc::channel(32);
        let (writer, persist_rx) = std::sync::mpsc::sync_channel(256);
        let writer_database = database.clone();
        std::thread::Builder::new()
            .name("pom-network-chat-writer".into())
            .spawn(move || {
                while let Ok(PersistCommand::Action(action)) = persist_rx.recv() {
                    if let Err(error) = writer_database.append_network_chat(&action) {
                        eprintln!("network chat persistence failed: {error:#}");
                    }
                }
            })
            .ok();
        let (live, _) = broadcast::channel(256);
        (
            Arc::new(Self {
                database,
                auth,
                outbound,
                dropped: AtomicU64::new(0),
                enabled: std::sync::atomic::AtomicBool::new(enabled),
                live,
                writer,
                rate: Mutex::new(HashMap::new()),
                typing: Mutex::new(HashMap::new()),
                notices: Mutex::new(std::collections::VecDeque::new()),
                lobby_guests: Default::default(),
                public_stats: Default::default(),
                directory_board: Default::default(),
                admissions: Mutex::new(Vec::new()),
                departed: Mutex::new(HashMap::new()),
                rotations: Mutex::new(Vec::new()),
                rotation_wanted: tokio::sync::Notify::new(),
            }),
            receiver,
        )
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub async fn node_is_blocked(&self, node_id: &str) -> bool {
        self.auth
            .read()
            .await
            .blocked_nodes
            .iter()
            .any(|blocked| blocked == node_id)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn lobby_guests(&self) -> &crate::network_lobby::LobbyGuests {
        &self.lobby_guests
    }

    /// Whether the network currently accepts new members with `role`.
    pub async fn accepts_role(&self, role: crate::config::EnrollmentRole) -> bool {
        self.auth
            .read()
            .await
            .network
            .accepted_enrollments
            .0
            .contains(&role)
    }

    /// The connection key, relay keys and current settings that go into an
    /// admission grant.
    pub async fn grant_secrets(&self) -> (String, Vec<String>, crate::config::NetworkSettings) {
        let config = self.auth.read().await;
        let network = self
            .database
            .network_settings()
            .ok()
            .flatten()
            .unwrap_or_else(|| config.network.clone());
        (
            config.connection_key.clone(),
            config.relay_peers.clone(),
            network,
        )
    }

    /// Reads the persisted admission requests. Called once at startup.
    pub fn load_admissions(&self) {
        if let Ok(records) = self.database.lobby_admissions() {
            *self.admissions.lock().expect("admissions lock") = records;
        }
    }

    fn store_admissions(&self, records: &[crate::network_lobby::AdmissionRecord]) {
        if let Err(error) = self.database.save_lobby_admissions(records) {
            eprintln!("lobby admissions persistence failed: {error:#}");
        }
    }

    pub fn admissions(&self) -> Vec<crate::network_lobby::AdmissionRecord> {
        self.admissions.lock().expect("admissions lock").clone()
    }

    pub fn admission_of(&self, node_id: &str) -> Option<crate::network_lobby::AdmissionRecord> {
        self.admissions
            .lock()
            .expect("admissions lock")
            .iter()
            .find(|record| record.node_id == node_id)
            .cloned()
    }

    /// Records a candidate's request (`network_lobby::apply_admission_request`).
    pub fn request_admission(
        &self,
        node_id: &str,
        card: crate::network_lobby::LobbyCard,
        role: crate::config::EnrollmentRole,
    ) -> std::result::Result<crate::network_lobby::AdmissionStatus, &'static str> {
        let mut records = self.admissions.lock().expect("admissions lock");
        let status = crate::network_lobby::apply_admission_request(
            &mut records,
            node_id,
            card,
            role,
            chrono::Utc::now(),
        )?;
        self.store_admissions(&records);
        Ok(status)
    }

    /// Puts an admitted peer on the member list, when it is not there yet.
    /// The approved admission records are the list of nodes that may claim
    /// the network's key after a rotation; a member that joined before
    /// admission requests existed has no record, and without one it could
    /// not get back in. A node that just left is not put back.
    pub fn record_member(
        &self,
        node_id: &str,
        hostname: &str,
        role: crate::config::EnrollmentRole,
    ) -> bool {
        {
            let mut departed = self.departed.lock().expect("departed lock");
            departed.retain(|_, at| at.elapsed() < crate::network_lobby::DEPARTED_HOLD);
            if departed.contains_key(node_id) {
                return false;
            }
        }
        let mut records = self.admissions.lock().expect("admissions lock");
        if records.iter().any(|record| record.node_id == node_id) {
            return false;
        }
        if records.len() >= crate::network_lobby::ADMISSION_RECORDS {
            return false;
        }
        let now = chrono::Utc::now().to_rfc3339();
        records.push(crate::network_lobby::AdmissionRecord {
            node_id: node_id.to_string(),
            card: crate::network_lobby::LobbyCard {
                hostname: hostname.to_string(),
                ..Default::default()
            }
            .sanitized(),
            role,
            status: crate::network_lobby::AdmissionStatus::Approved,
            requested_at: now.clone(),
            decided_at: Some(now),
        });
        self.store_admissions(&records);
        true
    }

    /// A member left: off the member list, and not put back by the upkeep
    /// while it is still fading out of the peer table.
    pub fn member_departed(&self, node_id: &str) {
        self.departed
            .lock()
            .expect("departed lock")
            .insert(node_id.to_string(), Instant::now());
        self.decide_admission(node_id, None);
    }

    /// Asks the gateway to rotate the connection key because `node_id` left.
    pub fn request_rotation(&self, node_id: &str) {
        self.rotations
            .lock()
            .expect("rotations lock")
            .push(node_id.to_string());
        self.rotation_wanted.notify_one();
    }

    /// Waits for departures that need a rotation and hands them over.
    pub async fn rotations_wanted(&self) -> Vec<String> {
        loop {
            let pending = std::mem::take(&mut *self.rotations.lock().expect("rotations lock"));
            if !pending.is_empty() {
                return pending;
            }
            self.rotation_wanted.notified().await;
        }
    }

    /// An admin's decision. `None` forgets the request altogether. Returns
    /// whether a request of `node_id` existed.
    pub fn decide_admission(
        &self,
        node_id: &str,
        decision: Option<crate::network_lobby::AdmissionStatus>,
    ) -> bool {
        let mut records = self.admissions.lock().expect("admissions lock");
        let Some(index) = records.iter().position(|record| record.node_id == node_id) else {
            return false;
        };
        match decision {
            Some(status) => {
                records[index].status = status;
                records[index].decided_at = Some(chrono::Utc::now().to_rfc3339());
            }
            None => {
                records.remove(index);
            }
        }
        self.store_admissions(&records);
        true
    }

    pub fn directory_board(&self) -> &crate::network_lobby::DirectoryBoard {
        &self.directory_board
    }

    pub fn public_stats(&self) -> &crate::network_lobby::PublicStats {
        &self.public_stats
    }

    /// Whether this network publishes its models, node count and VRAM to
    /// non-members: a public network with the operator's switch set. It does
    /// not depend on the network Chat being on.
    pub async fn public_stats_enabled(&self) -> bool {
        let config = self.auth.read().await;
        config.network.public_stats
            && config.network.visibility == crate::config::NetworkVisibility::Public
    }

    /// Whether this network has a lobby for non-members: discovery not off.
    /// There is no switch, because the lobby is the only way in. A hidden
    /// (private) network answers too: nobody lists it, and whoever was handed
    /// its trust document reaches the principal by key. It does not depend on
    /// the network Chat: with the Chat off the lobby still takes admission
    /// requests, and only its conversation is closed (`enabled`).
    pub async fn lobby_enabled(&self) -> bool {
        self.auth.read().await.network.visibility != crate::config::NetworkVisibility::Local
    }

    /// Whether an admission request is approved on arrival, with no admin
    /// deciding it (`NetworkAccessPolicy::AutoApprove`).
    pub async fn auto_approves(&self) -> bool {
        self.auth.read().await.network.access_policy
            == crate::config::NetworkAccessPolicy::AutoApprove
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.live.subscribe()
    }

    pub fn typing_snapshot(&self) -> Vec<ChatEvent> {
        let now = Instant::now();
        let mut typing = self.typing.lock().expect("network chat typing lock");
        typing.retain(|_, (_, seen)| now.duration_since(*seen) < Duration::from_secs(3));
        typing.values().map(|(event, _)| event.clone()).collect()
    }

    /// Avisos de plugin recebidos nas últimas 24 h, do mais antigo ao mais novo.
    pub fn notice_snapshot(&self) -> Vec<ChatEvent> {
        let now = Instant::now();
        let mut notices = self.notices.lock().expect("network chat notices lock");
        notices.retain(|(_, seen)| now.duration_since(*seen) < NOTICE_TTL);
        notices.iter().map(|(event, _)| event.clone()).collect()
    }

    pub async fn submit(&self, request: ChatRequest) -> Result<ChatResponse> {
        let (reply, receiver) = oneshot::channel();
        self.outbound
            .try_send(ChatOutbound { request, reply })
            .map_err(|_| {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                anyhow::anyhow!("fila do Chat da rede está cheia")
            })?;
        tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .context("tempo esgotado aguardando o principal")?
            .context("principal encerrou o envio do Chat da rede")
    }

    pub fn history(&self, room_id: &str, limit: usize) -> Result<Vec<ChatEvent>> {
        Ok(self
            .database
            .network_chat_messages(room_id, limit)?
            .into_iter()
            .map(ChatEvent::from)
            .collect())
    }

    /// Valida a ação original no principal. O `transport_node_id` já foi
    /// amarrado pelo PeerId no discovery, então a API key não pode ser usada
    /// por outro nó apenas trocando o campo JSON.
    pub async fn validate_original(
        &self,
        action: &ChatAction,
        api_key: &str,
        transport_node_id: &str,
    ) -> Result<()> {
        if !self.enabled() {
            anyhow::bail!("network_chat_disabled")
        }
        action.validate().map_err(|error| anyhow::anyhow!(error))?;
        // Presence is the principal's statement about a transport it
        // authenticated; a member's API key never vouches for one.
        if matches!(action, ChatAction::LobbyPresence { .. }) {
            anyhow::bail!("lobby_presence_not_allowed")
        }
        if api_key.is_empty() {
            anyhow::bail!("api_key_missing")
        }
        if action.sender_node_id() != transport_node_id {
            anyhow::bail!("sender_node_id_mismatch")
        }
        let key = self
            .database
            .api_key_for_token(api_key)?
            .ok_or_else(|| anyhow::anyhow!("api_key_invalid"))?;
        if !key.active || key.node_id != action.sender_node_id() {
            anyhow::bail!("api_key_invalid")
        }
        let blocked = self.auth.read().await.blocked_nodes.clone();
        let origin = if key.origin_node_id.is_empty() {
            &key.node_id
        } else {
            &key.origin_node_id
        };
        if blocked
            .iter()
            .any(|node| node == origin || node == &key.node_id)
        {
            anyhow::bail!("node_blocked")
        }
        if !self.rate_allows(
            action.sender_node_id(),
            matches!(action, ChatAction::Typing { .. }),
        ) {
            anyhow::bail!("rate_limited")
        }
        Ok(())
    }

    pub fn accept(&self, action: ChatAction) -> Result<bool> {
        if !self.enabled() {
            return Ok(false);
        }
        action.validate().map_err(|error| anyhow::anyhow!(error))?;
        if matches!(action, ChatAction::Typing { .. }) {
            let event = ChatEvent::from_action(&action);
            let mut typing = self.typing.lock().expect("network chat typing lock");
            if action.is_typing() {
                typing.insert(
                    action.sender_node_id().to_string(),
                    (event.clone(), Instant::now()),
                );
            } else {
                typing.remove(action.sender_node_id());
            }
            let _ = self.live.send(event);
            return Ok(true);
        }
        if let ChatAction::LobbyPresence {
            sender_node_id,
            card,
            ..
        } = &action
        {
            self.lobby_guests
                .remember(sender_node_id, card.clone().sanitized());
            return Ok(true);
        }
        if matches!(action, ChatAction::PluginNotice { .. }) {
            let event = ChatEvent::from_action(&action);
            let mut notices = self.notices.lock().expect("network chat notices lock");
            if notices
                .iter()
                .any(|(known, _)| known.message_id == event.message_id)
            {
                return Ok(true);
            }
            notices.push_back((event.clone(), Instant::now()));
            while notices.len() > NOTICE_CAPACITY {
                notices.pop_front();
            }
            let _ = self.live.send(event);
            return Ok(true);
        }
        self.writer
            .try_send(PersistCommand::Action(action.clone()))
            .map_err(|_| {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                anyhow::anyhow!("fila de persistência do Chat da rede está cheia")
            })?;
        let _ = self.live.send(ChatEvent::from_action(&action));
        Ok(true)
    }

    fn rate_allows(&self, node_id: &str, typing: bool) -> bool {
        let now = Instant::now();
        let mut windows = self.rate.lock().expect("network chat rate lock");
        let window = windows.entry(node_id.to_string()).or_insert(RateWindow {
            started: now,
            count: 0,
            typing_started: now,
            typing_count: 0,
        });
        if now.duration_since(window.started) >= Duration::from_secs(10) {
            window.started = now;
            window.count = 0;
        }
        if now.duration_since(window.typing_started) >= Duration::from_secs(1) {
            window.typing_started = now;
            window.typing_count = 0;
        }
        if typing {
            if window.typing_count >= 8 {
                return false;
            }
            window.typing_count += 1;
            true
        } else {
            if window.count >= 20 {
                return false;
            }
            window.count += 1;
            true
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NetworkChatCodec;

async fn read_msg<T, R>(io: &mut R) -> std::io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0_u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > NetworkChatLimits::FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "network chat frame exceeds the size bound",
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
    if data.len() > NetworkChatLimits::FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "network chat frame exceeds the size bound",
        ));
    }
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await
}

#[async_trait]
impl request_response::Codec for NetworkChatCodec {
    type Protocol = StreamProtocol;
    type Request = ChatRequest;
    type Response = ChatResponse;

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

pub fn network_chat_behaviour() -> request_response::Behaviour<NetworkChatCodec> {
    request_response::Behaviour::new(
        [(
            StreamProtocol::new(NETWORK_CHAT_PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default().with_request_timeout(Duration::from_secs(5)),
    )
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct ChatQueue {
    sender: mpsc::Sender<ChatAction>,
    dropped: std::sync::Arc<AtomicU64>,
}

#[cfg(test)]
impl ChatQueue {
    pub fn with_capacity(capacity: usize) -> (Self, mpsc::Receiver<ChatAction>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                dropped: std::sync::Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }

    pub fn try_push(&self, action: ChatAction) -> bool {
        match self.sender.try_send(action) {
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
}
