//! LAN bootstrap for a node that has not saved a POM configuration yet.
//!
//! The trust document is public discovery material. The connection key is
//! never present in a probe, offer, or proof. It is encrypted only after the
//! node proves possession of the ephemeral X25519 secret that it announced in
//! the probe.

use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, sync::Mutex, time::Instant};
use x25519_dalek::{PublicKey, StaticSecret};

pub const PROTOCOL_VERSION: u8 = 1;
pub const SESSION_TTL: Duration = Duration::from_secs(20);
/// Trust documents are transferred as authenticated fragments instead of a
/// single UDP datagram. Keep a bounded upper limit so a forged offer cannot
/// make the node retain unbounded state while still allowing real authority
/// chains with history and replay records.
pub const MAX_TRUST_DOCUMENT_BYTES: usize = 512 * 1024;
const MAX_SECRET_CHUNK_BYTES: usize = 640;
const MAX_SECRET_PAYLOAD_BYTES: usize = MAX_TRUST_DOCUMENT_BYTES + 1024;
const MAX_SECRET_CHUNKS: usize =
    (MAX_SECRET_PAYLOAD_BYTES + MAX_SECRET_CHUNK_BYTES - 1) / MAX_SECRET_CHUNK_BYTES;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LocalAutoDiscoveryMessage {
    Probe {
        version: u8,
        request_id: String,
        node_ephemeral_public: String,
        node_nonce: String,
    },
    Offer {
        version: u8,
        request_id: String,
        admin_ephemeral_public: String,
        admin_nonce: String,
        trust_document_bytes: u32,
        trust_chunk_count: u16,
        trust_fingerprint: String,
    },
    Proof {
        version: u8,
        request_id: String,
        node_ephemeral_public: String,
        node_nonce: String,
        admin_ephemeral_public: String,
        admin_nonce: String,
        proof: String,
    },
    Secret {
        version: u8,
        request_id: String,
        chunk_index: u16,
        chunk_count: u16,
        nonce: String,
        ciphertext: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalDiscoveryStatus {
    pub enabled: bool,
    pub active: bool,
    pub searching: bool,
    pub found: bool,
    pub port: u16,
    pub peers: Vec<LocalDiscoveryPeer>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalDiscoveryPeer {
    pub request_id: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalDiscoveryResult {
    pub request_id: String,
    pub trust_document: String,
    pub trust_fingerprint: String,
    pub connection_key: String,
}

struct AdminMaterial {
    enabled: bool,
    trust_document: String,
    trust_fingerprint: String,
    connection_key: String,
    secret_payload: Vec<u8>,
}

struct AdminSession {
    node_ephemeral_public: PublicKey,
    node_nonce: [u8; 16],
    admin_secret: StaticSecret,
    admin_ephemeral_public: PublicKey,
    admin_nonce: [u8; 16],
    expires_at: Instant,
    source: SocketAddr,
}

struct SearchSession {
    request_id: String,
    node_secret: StaticSecret,
    node_ephemeral_public: PublicKey,
    node_nonce: [u8; 16],
    admin_ephemeral_public: Option<PublicKey>,
    admin_nonce: Option<[u8; 16]>,
    offered_trust_fingerprint: Option<String>,
    offered_trust_document_bytes: Option<usize>,
    offered_secret_chunk_count: Option<u16>,
    secret_nonce: Option<[u8; 12]>,
    secret_chunks: HashMap<u16, Vec<u8>>,
    result: Option<LocalDiscoveryResult>,
    expires_at: Instant,
}

struct Inner {
    admin: Option<AdminMaterial>,
    admin_sessions: HashMap<String, AdminSession>,
    peers: Vec<LocalDiscoveryPeer>,
    search: Option<SearchSession>,
    session_ttl: Duration,
    #[cfg(test)]
    outbound_for_test: Vec<LocalAutoDiscoveryMessage>,
}

/// State machine shared by setup mode and the configured runtime network.
/// Setup and Network own the socket lifecycle; this type owns only handshake
/// state and uses its socket to emit responses.
pub struct LocalDiscoveryRuntime {
    socket: Arc<UdpSocket>,
    broadcast_addr: SocketAddr,
    state: Mutex<Inner>,
}

impl LocalDiscoveryRuntime {
    pub fn new(socket: Arc<UdpSocket>, broadcast_addr: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            socket,
            broadcast_addr,
            state: Mutex::new(Inner {
                admin: None,
                admin_sessions: HashMap::new(),
                peers: Vec::new(),
                search: None,
                session_ttl: SESSION_TTL,
                #[cfg(test)]
                outbound_for_test: Vec::new(),
            }),
        })
    }

    pub async fn set_admin_material(
        &self,
        enabled: bool,
        trust_document: impl Into<String>,
        connection_key: impl Into<String>,
    ) -> Result<()> {
        let trust_document = trust_document.into();
        let connection_key = connection_key.into();
        if trust_document.len() > MAX_TRUST_DOCUMENT_BYTES {
            bail!("o documento de confiança excede o limite da transferência local");
        }
        if !connection_key.is_empty() && trust_document.contains(&connection_key) {
            bail!("a chave de conexão não pode aparecer no documento de confiança");
        }
        let secret_payload = serde_json::to_vec(&SecretPayload {
            trust_document: trust_document.clone(),
            connection_key: connection_key.clone(),
        })?;
        if secret_payload.len() > MAX_SECRET_PAYLOAD_BYTES {
            bail!("o material da descoberta local excede o limite da transferência");
        }
        let _ = secret_chunk_count(secret_payload.len())?;
        let mut state = self.state.lock().await;
        state.admin = Some(AdminMaterial {
            enabled,
            trust_fingerprint: trust_fingerprint(&trust_document),
            trust_document,
            connection_key,
            secret_payload,
        });
        state.admin_sessions.clear();
        state.peers.clear();
        if !enabled {
            state.search = None;
        }
        Ok(())
    }

    pub async fn disable(&self) {
        let mut state = self.state.lock().await;
        if let Some(admin) = state.admin.as_mut() {
            admin.enabled = false;
            admin.connection_key.clear();
            admin.trust_document.clear();
            admin.trust_fingerprint.clear();
            admin.secret_payload.clear();
        }
        state.admin_sessions.clear();
        state.peers.clear();
        state.search = None;
    }

    pub async fn status(&self) -> LocalDiscoveryStatus {
        let state = self.state.lock().await;
        let enabled = state.admin.as_ref().is_some_and(|admin| admin.enabled);
        let searching = state
            .search
            .as_ref()
            .is_some_and(|search| search.result.is_none() && Instant::now() < search.expires_at);
        let found = state
            .search
            .as_ref()
            .is_some_and(|search| search.result.is_some());
        LocalDiscoveryStatus {
            enabled,
            active: enabled,
            searching,
            found,
            port: self
                .socket
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(0),
            peers: state.peers.clone(),
        }
    }

    pub async fn peek_result(&self) -> Option<LocalDiscoveryResult> {
        let state = self.state.lock().await;
        state
            .search
            .as_ref()
            .and_then(|search| search.result.clone())
    }

    pub async fn begin_search(&self) -> Result<String> {
        let (request_id, message) = {
            let mut state = self.state.lock().await;
            let mut request_id_bytes = [0u8; 16];
            OsRng.fill_bytes(&mut request_id_bytes);
            let request_id = encode(&request_id_bytes);
            let node_secret = StaticSecret::random_from_rng(OsRng);
            let node_ephemeral_public = PublicKey::from(&node_secret);
            let mut node_nonce = [0u8; 16];
            OsRng.fill_bytes(&mut node_nonce);
            let message = LocalAutoDiscoveryMessage::Probe {
                version: PROTOCOL_VERSION,
                request_id: request_id.clone(),
                node_ephemeral_public: encode(node_ephemeral_public.as_bytes()),
                node_nonce: encode(&node_nonce),
            };
            state.search = Some(SearchSession {
                request_id: request_id.clone(),
                node_secret,
                node_ephemeral_public,
                node_nonce,
                admin_ephemeral_public: None,
                admin_nonce: None,
                offered_trust_fingerprint: None,
                offered_trust_document_bytes: None,
                offered_secret_chunk_count: None,
                secret_nonce: None,
                secret_chunks: HashMap::new(),
                result: None,
                expires_at: Instant::now() + state.session_ttl,
            });
            #[cfg(test)]
            state.outbound_for_test.push(message.clone());
            (request_id, message)
        };
        self.send(self.broadcast_addr, message.clone()).await;
        Ok(request_id)
    }

    /// Process one envelope received by Network or setup mode.
    pub async fn handle(&self, message: LocalAutoDiscoveryMessage, source: SocketAddr) {
        let responses = self.process(message, source).await;
        for (destination, response) in responses {
            self.send(destination, response).await;
        }
    }

    pub async fn run_receive_loop(self: Arc<Self>) {
        let mut buffer = vec![0u8; 65_535];
        loop {
            let Ok((length, source)) = self.socket.recv_from(&mut buffer).await else {
                break;
            };
            if let Ok(crate::network::P2PMessage::LocalAutoDiscovery { message }) =
                serde_json::from_slice(&buffer[..length])
            {
                self.handle(message, source).await;
            }
        }
    }

    async fn process(
        &self,
        message: LocalAutoDiscoveryMessage,
        source: SocketAddr,
    ) -> Vec<(SocketAddr, LocalAutoDiscoveryMessage)> {
        let now = Instant::now();
        let mut state = self.state.lock().await;
        state
            .admin_sessions
            .retain(|_, session| session.expires_at > now);
        if let Some(search) = state.search.as_ref() {
            if search.expires_at <= now && search.result.is_none() {
                state.search = None;
            }
        }
        let mut responses = Vec::new();
        match message {
            LocalAutoDiscoveryMessage::Probe {
                version,
                request_id,
                node_ephemeral_public,
                node_nonce,
            } if version == PROTOCOL_VERSION => {
                let Some(admin) = state.admin.as_ref() else {
                    return responses;
                };
                if !admin.enabled || admin.trust_document.is_empty() {
                    return responses;
                }
                let trust_document = admin.trust_document.clone();
                let trust_fingerprint = admin.trust_fingerprint.clone();
                let Ok(trust_document_bytes) = u32::try_from(trust_document.len()) else {
                    return responses;
                };
                let Ok(trust_chunk_count) = secret_chunk_count(admin.secret_payload.len()) else {
                    return responses;
                };
                let session_ttl = state.session_ttl;
                let (Some(node_ephemeral_public), Some(node_nonce)) = (
                    decode_array::<32>(&node_ephemeral_public),
                    decode_array::<16>(&node_nonce),
                ) else {
                    return responses;
                };
                let node_ephemeral_public = PublicKey::from(node_ephemeral_public);
                let admin_secret = StaticSecret::random_from_rng(OsRng);
                let admin_ephemeral_public = PublicKey::from(&admin_secret);
                let mut admin_nonce = [0u8; 16];
                OsRng.fill_bytes(&mut admin_nonce);
                state.admin_sessions.insert(
                    request_id.clone(),
                    AdminSession {
                        node_ephemeral_public,
                        node_nonce,
                        admin_secret,
                        admin_ephemeral_public,
                        admin_nonce,
                        expires_at: now + session_ttl,
                        source,
                    },
                );
                responses.push((
                    source,
                    LocalAutoDiscoveryMessage::Offer {
                        version,
                        request_id,
                        admin_ephemeral_public: encode(admin_ephemeral_public.as_bytes()),
                        admin_nonce: encode(&admin_nonce),
                        trust_document_bytes,
                        trust_chunk_count,
                        trust_fingerprint,
                    },
                ));
            }
            LocalAutoDiscoveryMessage::Offer {
                version,
                request_id,
                admin_ephemeral_public,
                admin_nonce,
                trust_document_bytes,
                trust_chunk_count,
                trust_fingerprint: offered_fingerprint,
            } if version == PROTOCOL_VERSION => {
                let Some(search) = state.search.as_mut() else {
                    return responses;
                };
                if search.request_id != request_id || search.expires_at <= now {
                    return responses;
                }
                if trust_document_bytes == 0
                    || trust_document_bytes as usize > MAX_TRUST_DOCUMENT_BYTES
                    || trust_chunk_count == 0
                    || trust_chunk_count as usize > MAX_SECRET_CHUNKS
                    || hex::decode(&offered_fingerprint)
                        .map_or(true, |fingerprint| fingerprint.len() != 32)
                {
                    return responses;
                }
                let (Some(admin_ephemeral_public), Some(admin_nonce)) = (
                    decode_array::<32>(&admin_ephemeral_public),
                    decode_array::<16>(&admin_nonce),
                ) else {
                    return responses;
                };
                let admin_ephemeral_public = PublicKey::from(admin_ephemeral_public);
                let transcript = transcript(
                    &request_id,
                    &encode(search.node_ephemeral_public.as_bytes()),
                    &encode(&search.node_nonce),
                    &encode(admin_ephemeral_public.as_bytes()),
                    &encode(&admin_nonce),
                );
                let key = shared_key(&search.node_secret, &admin_ephemeral_public);
                let proof = proof_for(&key, &transcript);
                search.admin_ephemeral_public = Some(admin_ephemeral_public);
                search.admin_nonce = Some(admin_nonce);
                search.offered_trust_fingerprint = Some(offered_fingerprint);
                search.offered_trust_document_bytes = Some(trust_document_bytes as usize);
                search.offered_secret_chunk_count = Some(trust_chunk_count);
                search.secret_nonce = None;
                search.secret_chunks.clear();
                responses.push((
                    source,
                    LocalAutoDiscoveryMessage::Proof {
                        version,
                        request_id,
                        node_ephemeral_public: encode(search.node_ephemeral_public.as_bytes()),
                        node_nonce: encode(&search.node_nonce),
                        admin_ephemeral_public: encode(admin_ephemeral_public.as_bytes()),
                        admin_nonce: encode(&admin_nonce),
                        proof,
                    },
                ));
            }
            LocalAutoDiscoveryMessage::Proof {
                version,
                request_id,
                node_ephemeral_public,
                node_nonce,
                admin_ephemeral_public,
                admin_nonce,
                proof,
            } if version == PROTOCOL_VERSION => {
                let Some(session) = state.admin_sessions.get(&request_id) else {
                    return responses;
                };
                if session.source.ip() != source.ip()
                    || encode(session.node_ephemeral_public.as_bytes()) != node_ephemeral_public
                    || encode(&session.node_nonce) != node_nonce
                    || encode(session.admin_ephemeral_public.as_bytes()) != admin_ephemeral_public
                    || encode(&session.admin_nonce) != admin_nonce
                {
                    return responses;
                }
                let key = shared_key(&session.admin_secret, &session.node_ephemeral_public);
                let transcript = transcript(
                    &request_id,
                    &node_ephemeral_public,
                    &node_nonce,
                    &admin_ephemeral_public,
                    &admin_nonce,
                );
                if proof != proof_for(&key, &transcript) {
                    return responses;
                }
                state.admin_sessions.remove(&request_id);
                let Some(admin) = state.admin.as_ref() else {
                    return responses;
                };
                if !admin.enabled {
                    return responses;
                }
                let secret_payload = admin.secret_payload.clone();
                let mut nonce = [0u8; 12];
                OsRng.fill_bytes(&mut nonce);
                let cipher = ChaCha20Poly1305::new((&key).into());
                let Ok(ciphertext) = cipher.encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &secret_payload,
                        aad: &transcript,
                    },
                ) else {
                    return responses;
                };
                let Ok(chunk_count) = secret_chunk_count(secret_payload.len()) else {
                    return responses;
                };
                if ciphertext.chunks(MAX_SECRET_CHUNK_BYTES).len() != chunk_count as usize {
                    return responses;
                }
                let peer = LocalDiscoveryPeer {
                    request_id: request_id.clone(),
                    address: source.to_string(),
                };
                state.peers.retain(|known| known.address != peer.address);
                if state.peers.len() >= 32 {
                    state.peers.remove(0);
                }
                state.peers.push(peer);
                for (chunk_index, chunk) in ciphertext.chunks(MAX_SECRET_CHUNK_BYTES).enumerate() {
                    let Ok(chunk_index) = u16::try_from(chunk_index) else {
                        return responses;
                    };
                    responses.push((
                        source,
                        LocalAutoDiscoveryMessage::Secret {
                            version,
                            request_id: request_id.clone(),
                            chunk_index,
                            chunk_count,
                            nonce: encode(&nonce),
                            ciphertext: encode(chunk),
                        },
                    ));
                }
            }
            LocalAutoDiscoveryMessage::Secret {
                version,
                request_id,
                chunk_index,
                chunk_count,
                nonce,
                ciphertext,
            } if version == PROTOCOL_VERSION => {
                let Some(search) = state.search.as_mut() else {
                    return responses;
                };
                if search.request_id != request_id || search.expires_at <= now {
                    return responses;
                }
                if search.offered_secret_chunk_count != Some(chunk_count)
                    || chunk_count == 0
                    || chunk_count as usize > MAX_SECRET_CHUNKS
                    || chunk_index >= chunk_count
                {
                    return responses;
                }
                let Some(nonce) = decode_array::<12>(&nonce) else {
                    return responses;
                };
                let Some(ciphertext) = decode(&ciphertext) else {
                    return responses;
                };
                if ciphertext.is_empty() || ciphertext.len() > MAX_SECRET_CHUNK_BYTES {
                    return responses;
                }
                if let Some(expected_nonce) = search.secret_nonce {
                    if expected_nonce != nonce {
                        return responses;
                    }
                } else {
                    search.secret_nonce = Some(nonce);
                }
                search.secret_chunks.insert(chunk_index, ciphertext);
                if search.secret_chunks.len() != chunk_count as usize {
                    return responses;
                }
                let mut assembled_ciphertext = Vec::new();
                for index in 0..chunk_count {
                    let Some(chunk) = search.secret_chunks.get(&index) else {
                        return responses;
                    };
                    assembled_ciphertext.extend_from_slice(chunk);
                }
                if assembled_ciphertext.len() > MAX_SECRET_PAYLOAD_BYTES + 16 {
                    return responses;
                }
                let (Some(admin_ephemeral_public), Some(admin_nonce), Some(nonce)) = (
                    search.admin_ephemeral_public,
                    search.admin_nonce,
                    search.secret_nonce,
                ) else {
                    return responses;
                };
                let key = shared_key(&search.node_secret, &admin_ephemeral_public);
                let transcript = transcript(
                    &request_id,
                    &encode(search.node_ephemeral_public.as_bytes()),
                    &encode(&search.node_nonce),
                    &encode(admin_ephemeral_public.as_bytes()),
                    &encode(&admin_nonce),
                );
                let cipher = ChaCha20Poly1305::new((&key).into());
                let Ok(plaintext) = cipher.decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &assembled_ciphertext,
                        aad: &transcript,
                    },
                ) else {
                    return responses;
                };
                let Ok(payload) = serde_json::from_slice::<SecretPayload>(&plaintext) else {
                    return responses;
                };
                if payload.trust_document.len() > MAX_TRUST_DOCUMENT_BYTES
                    || search.offered_trust_document_bytes != Some(payload.trust_document.len())
                {
                    return responses;
                }
                let trust_fingerprint = trust_fingerprint(&payload.trust_document);
                if search.offered_trust_fingerprint.as_deref() != Some(trust_fingerprint.as_str()) {
                    return responses;
                }
                search.result = Some(LocalDiscoveryResult {
                    request_id,
                    trust_document: payload.trust_document,
                    trust_fingerprint,
                    connection_key: payload.connection_key,
                });
            }
            _ => {}
        }
        responses
    }

    async fn send(&self, destination: SocketAddr, message: LocalAutoDiscoveryMessage) {
        let envelope = crate::network::P2PMessage::LocalAutoDiscovery { message };
        if let Ok(bytes) = serde_json::to_vec(&envelope) {
            if bytes.len() <= crate::network::MAX_DATAGRAM_PAYLOAD {
                let _ = self.socket.send_to(&bytes, destination).await;
            } else {
                tracing::warn!(
                    bytes = bytes.len(),
                    "discarding oversized local discovery datagram"
                );
            }
        }
    }

    #[cfg(test)]
    pub fn test_memory() -> Arc<Self> {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("test UDP socket");
        socket
            .set_nonblocking(true)
            .expect("nonblocking test socket");
        let socket = UdpSocket::from_std(socket).expect("tokio test socket");
        Self::new(Arc::new(socket), SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    #[cfg(test)]
    pub async fn handle_for_test(&self, message: LocalAutoDiscoveryMessage, source: SocketAddr) {
        let responses = self.process(message, source).await;
        self.state
            .lock()
            .await
            .outbound_for_test
            .extend(responses.into_iter().map(|(_, message)| message));
    }

    #[cfg(test)]
    pub async fn take_outbound_for_test(&self) -> Option<LocalAutoDiscoveryMessage> {
        let mut state = self.state.lock().await;
        (!state.outbound_for_test.is_empty()).then(|| state.outbound_for_test.remove(0))
    }

    #[cfg(test)]
    pub async fn set_session_ttl(&self, ttl: Duration) {
        self.state.lock().await.session_ttl = ttl;
    }

    #[cfg(test)]
    pub async fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    #[cfg(test)]
    pub async fn port(&self) -> u16 {
        self.local_addr().await.port()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SecretPayload {
    trust_document: String,
    connection_key: String,
}

fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok()
}

fn decode_array<const N: usize>(value: &str) -> Option<[u8; N]> {
    decode(value)?.try_into().ok()
}

fn trust_fingerprint(document: &str) -> String {
    if let Ok(parsed) = crate::admin_trust::parse_transported(document) {
        return crate::admin_trust::anchor_fingerprint(&parsed);
    }
    hex::encode(Sha256::digest(document.as_bytes()))
}

fn transcript(
    request_id: &str,
    node_ephemeral_public: &str,
    node_nonce: &str,
    admin_ephemeral_public: &str,
    admin_nonce: &str,
) -> Vec<u8> {
    format!(
        "pom-local-discovery-v{PROTOCOL_VERSION}|{request_id}|{node_ephemeral_public}|{node_nonce}|{admin_ephemeral_public}|{admin_nonce}"
    )
    .into_bytes()
}

fn shared_key(secret: &StaticSecret, peer: &PublicKey) -> [u8; 32] {
    Sha256::digest(secret.diffie_hellman(peer).as_bytes()).into()
}

fn proof_for(key: &[u8; 32], transcript: &[u8]) -> String {
    let mut message = Vec::with_capacity(key.len() + transcript.len());
    message.extend_from_slice(key);
    message.extend_from_slice(transcript);
    hex::encode(Sha256::digest(message))
}

fn secret_chunk_count(payload_len: usize) -> Result<u16> {
    let ciphertext_len = payload_len.checked_add(16).ok_or_else(|| {
        anyhow::anyhow!("o material da descoberta local excede o limite da transferência")
    })?;
    let chunk_count = (ciphertext_len + MAX_SECRET_CHUNK_BYTES - 1) / MAX_SECRET_CHUNK_BYTES;
    if chunk_count == 0 || chunk_count > MAX_SECRET_CHUNKS {
        bail!("o material da descoberta local excede o limite da transferência");
    }
    Ok(u16::try_from(chunk_count)?)
}

#[cfg(test)]
mod tests {
    use super::trust_fingerprint;

    #[test]
    fn fallback_fingerprint_is_stable() {
        assert_eq!(trust_fingerprint("trust"), trust_fingerprint("trust"));
        assert_ne!(trust_fingerprint("trust"), trust_fingerprint("other"));
    }
}
