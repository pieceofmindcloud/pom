//! Inference tunnels through a visitor's anonymous relay.
//!
//! A visitor that relays for its network does not learn who uses it
//! (`crate::relay_pass`). For that, nothing that reaches the relay may name a
//! node: every member keeps an **ephemeral endpoint** with a random key,
//! whose relay map holds only the visitors' anonymous relays an admin handed
//! out, each with its pass as the relay's auth token. The relay only ever
//! sees ephemeral ids, on both ends of a call.
//!
//! The ephemeral endpoint carries one protocol, `ANON_TUNNEL_PROTOCOL`: the
//! inference tunnel (`InferTunnelCodec`), after a **binding**. The first
//! stream of a connection proves which node is calling - its key signs
//! keying material exported from this very QUIC connection - inside the
//! encrypted session, where only the two ends see it. The answering node
//! checks the signature and the caller's membership, and stamps the
//! request with that node exactly as the main tunnel does
//! (`TUNNEL_PEER_HEADER`), so everything behind it is unchanged. File bytes
//! never travel this way.
//!
//! An endpoint is replaced every `ROTATE_AFTER`, when unused passes are at
//! hand (a pass stays with the first endpoint that used it), and the old one
//! lives `RETIRE_AFTER` longer for the calls it carries.

use crate::infer_tunnel::{InferTunnelCodec, TunnelRequest, TunnelResponse};
use crate::network::AnonymousRelay;
use crate::p2p::{self, CallError, Codec, EndpointId, RelayUrl, SecretKey};
use futures::future::BoxFuture;
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info};

pub const ANON_TUNNEL_PROTOCOL: &str = "/pom/anon-tunnel/1";

/// The exporter label of the keying material a binding signs.
const BIND_LABEL: &[u8] = b"pom/anon-tunnel-bind/1";
/// Domain of the binding signature, so the node key's signature over it can
/// never stand for anything else that key signs.
const BIND_SIGNATURE_DOMAIN: &[u8] = b"pom/anon-tunnel-bind-signature/1:";

/// How long an ephemeral endpoint serves before a new one takes over.
pub const ROTATE_AFTER: Duration = Duration::from_secs(60 * 60);
/// How long a replaced endpoint lives on for the calls it still carries.
const RETIRE_AFTER: Duration = Duration::from_secs(10 * 60);
/// When a relay's pass in the map is swapped for a newer one: two thirds
/// into its life, as the membership token is renewed.
const PASS_SWAP_AFTER: Duration = Duration::from_secs(crate::membership::RENEW_AFTER_SECS as u64);
/// Bound of one binding frame.
const MAX_BIND_BYTES: usize = 4 * 1024;

/// Where a member answers anonymous tunnels: its ephemeral endpoint and the
/// visitor relay it is homed on. It travels in the member's signed announce,
/// which is what ties the ephemeral id to the node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnonTunnelAddr {
    pub endpoint_id: String,
    pub relay_url: String,
}

/// The first message of an anonymous tunnel connection: who calls, proved
/// for this connection only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub node_id: String,
    /// The node key, hex.
    pub pubkey: String,
    /// The node key's signature over the connection's keying material, hex.
    pub signature: String,
}

/// The connection's own keying material, the same on both ends and on no
/// other connection: what makes a binding impossible to replay elsewhere.
pub fn keying_material(connection: &Connection) -> Option<[u8; 32]> {
    let mut material = [0_u8; 32];
    connection
        .export_keying_material(&mut material, BIND_LABEL, b"")
        .ok()?;
    Some(material)
}

fn signed_bytes(material: &[u8; 32]) -> Vec<u8> {
    let mut bytes = BIND_SIGNATURE_DOMAIN.to_vec();
    bytes.extend_from_slice(material);
    bytes
}

/// The binding `secret` (the node key of `node_id`) gives for a connection
/// with `material`.
pub fn sign_binding(material: &[u8; 32], secret: &SecretKey, node_id: &str) -> Binding {
    Binding {
        node_id: node_id.to_string(),
        pubkey: hex::encode(secret.public().as_bytes()),
        signature: hex::encode(secret.sign(&signed_bytes(material)).to_bytes()),
    }
}

/// The node's key, as an endpoint id, when `binding` proves it for the
/// connection with `material`: the node id is the key's, and the key signed
/// that connection's material.
pub fn check_binding(material: &[u8; 32], binding: &Binding) -> Option<EndpointId> {
    let raw: [u8; 32] = hex::decode(&binding.pubkey).ok()?.try_into().ok()?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&raw).ok()?;
    if crate::identity::NodeId::from_pubkey(&key).0 != binding.node_id {
        return None;
    }
    let signature: [u8; 64] = hex::decode(&binding.signature).ok()?.try_into().ok()?;
    let id = EndpointId::from_bytes(&raw).ok()?;
    id.verify(
        &signed_bytes(material),
        &iroh::Signature::from_bytes(&signature),
    )
    .ok()?;
    Some(id)
}

/// The binding exchange: the caller sends its `Binding`, the answer says
/// whether the connection may carry tunnels.
#[derive(Debug, Clone, Default)]
pub struct BindCodec;

#[async_trait::async_trait]
impl Codec for BindCodec {
    const PROTOCOL: &'static str = ANON_TUNNEL_PROTOCOL;
    type Request = Binding;
    type Response = bool;

    async fn read_request<T>(&mut self, io: &mut T) -> std::io::Result<Binding>
    where
        T: futures::AsyncRead + Unpin + Send,
    {
        read_frame(io).await
    }

    async fn read_response<T>(&mut self, io: &mut T) -> std::io::Result<bool>
    where
        T: futures::AsyncRead + Unpin + Send,
    {
        read_frame(io).await
    }

    async fn write_request<T>(&mut self, io: &mut T, req: Binding) -> std::io::Result<()>
    where
        T: futures::AsyncWrite + Unpin + Send,
    {
        write_frame(io, &req).await
    }

    async fn write_response<T>(&mut self, io: &mut T, res: bool) -> std::io::Result<()>
    where
        T: futures::AsyncWrite + Unpin + Send,
    {
        write_frame(io, &res).await
    }
}

async fn read_frame<T, R>(io: &mut R) -> std::io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: futures::AsyncRead + Unpin + Send,
{
    use futures::AsyncReadExt;
    let mut len = [0_u8; 4];
    io.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_BIND_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "binding exceeds the size bound",
        ));
    }
    let mut data = vec![0_u8; len];
    io.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

async fn write_frame<T, W>(io: &mut W, message: &T) -> std::io::Result<()>
where
    T: Serialize,
    W: futures::AsyncWrite + Unpin + Send,
{
    use futures::AsyncWriteExt;
    let data = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await
}

/// Answers one tunnel request from the node a binding proved.
pub type AnonHandler =
    Arc<dyn Fn(EndpointId, TunnelRequest) -> BoxFuture<'static, TunnelResponse> + Send + Sync>;
/// Whether the node with this id and key (hex) is a member now.
pub type MemberCheck = Arc<dyn Fn(String, String) -> BoxFuture<'static, bool> + Send + Sync>;

/// The answering side: the binding first, then the tunnel requests.
#[derive(Clone)]
struct AnonServe {
    handler: AnonHandler,
    is_member: MemberCheck,
}

impl std::fmt::Debug for AnonServe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AnonServe")
    }
}

impl iroh::protocol::ProtocolHandler for AnonServe {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let Ok((send, recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        let Ok(binding) = BindCodec.read_request(&mut recv.compat()).await else {
            return Ok(());
        };
        let caller = match keying_material(&connection) {
            Some(material) => check_binding(&material, &binding),
            None => None,
        };
        let caller = match caller {
            Some(id) if (self.is_member)(binding.node_id.clone(), binding.pubkey.clone()).await => {
                Some(id)
            }
            _ => None,
        };
        let mut send = send.compat_write();
        let _ = BindCodec.write_response(&mut send, caller.is_some()).await;
        let _ = send.into_inner().finish();
        let Some(caller) = caller else {
            debug!("anon-tunnel: refused a connection whose binding does not hold");
            connection.close(0u32.into(), b"binding refused");
            return Ok(());
        };
        let handler = self.handler.clone();
        p2p::serve_connection(connection, InferTunnelCodec, move |_connection, request| {
            let handler = handler.clone();
            async move { handler(caller, request).await }
        })
        .await;
        Ok(())
    }
}

/// One ephemeral endpoint and what it carries.
struct Generation {
    router: iroh::protocol::Router,
    born: Instant,
    /// The relay URLs in its map, each with when its pass went in.
    relays: Mutex<HashMap<String, Instant>>,
    /// Bound connections to other members' ephemeral endpoints.
    connections: tokio::sync::Mutex<HashMap<EndpointId, Connection>>,
}

impl Generation {
    fn endpoint(&self) -> &iroh::Endpoint {
        self.router.endpoint()
    }
}

/// This node's side of the anonymous tunnels: the ephemeral endpoint, kept
/// in step with the relays an admin hands out (`sync`), and the calls.
pub struct AnonTunnels {
    /// The node key: it signs bindings, and is never the endpoint's key.
    secret: SecretKey,
    node_id: String,
    handler: AnonHandler,
    is_member: MemberCheck,
    current: tokio::sync::RwLock<Option<Arc<Generation>>>,
    retiring: Mutex<Vec<(Arc<Generation>, Instant)>>,
    /// Every pass handed to an endpoint: a relay binds a pass to the first
    /// endpoint that used it, so a pass goes to one endpoint only.
    used_passes: Mutex<HashSet<String>>,
}

impl AnonTunnels {
    pub fn new(
        secret: SecretKey,
        node_id: String,
        handler: AnonHandler,
        is_member: MemberCheck,
    ) -> Self {
        Self {
            secret,
            node_id,
            handler,
            is_member,
            current: tokio::sync::RwLock::new(None),
            retiring: Mutex::new(Vec::new()),
            used_passes: Mutex::new(HashSet::new()),
        }
    }

    /// Brings the ephemeral endpoint in step with `relays`: the first one
    /// is made when a relay is handed out, a newer pass replaces an older
    /// one in the map, a relay no longer handed out leaves it, and after
    /// `ROTATE_AFTER` a new endpoint takes the unused passes.
    pub async fn sync(&self, relays: &[AnonymousRelay]) {
        if let Ok(mut retiring) = self.retiring.lock() {
            retiring.retain(|(_, since)| since.elapsed() < RETIRE_AFTER);
        }
        let fresh: Vec<&AnonymousRelay> = {
            let used = self
                .used_passes
                .lock()
                .map(|used| used.clone())
                .unwrap_or_default();
            relays
                .iter()
                .filter(|relay| !used.contains(&relay.pass))
                .collect()
        };
        let mut current = self.current.write().await;
        let rotate = current
            .as_ref()
            .is_none_or(|generation| generation.born.elapsed() >= ROTATE_AFTER);
        if rotate && !fresh.is_empty() {
            match self.spawn_generation(&fresh).await {
                Ok(generation) => {
                    info!(
                        "anon-tunnel: ephemeral endpoint on {} visitor relay(s)",
                        generation
                            .relays
                            .lock()
                            .map(|relays| relays.len())
                            .unwrap_or(0)
                    );
                    self.mark_used(&fresh);
                    if let Some(old) = current.replace(Arc::new(generation)) {
                        if let Ok(mut retiring) = self.retiring.lock() {
                            retiring.push((old, Instant::now()));
                        }
                    }
                }
                Err(error) => debug!("anon-tunnel: no ephemeral endpoint: {error:#}"),
            }
            return;
        }
        let Some(generation) = current.as_ref() else {
            return;
        };
        let endpoint = generation.endpoint();
        let handed: HashSet<&str> = relays.iter().map(|relay| relay.url.as_str()).collect();
        let (gone, due): (Vec<String>, Vec<&AnonymousRelay>) = {
            let Ok(mut held) = generation.relays.lock() else {
                return;
            };
            let gone: Vec<String> = held
                .keys()
                .filter(|url| !handed.contains(url.as_str()))
                .cloned()
                .collect();
            for url in &gone {
                held.remove(url);
            }
            // A relay new to the map, or one whose pass is getting old.
            let due: Vec<&AnonymousRelay> = fresh
                .iter()
                .copied()
                .filter(|relay| {
                    held.get(&relay.url)
                        .is_none_or(|since| since.elapsed() >= PASS_SWAP_AFTER)
                })
                .collect();
            for relay in &due {
                held.insert(relay.url.clone(), Instant::now());
            }
            (gone, due)
        };
        for url in gone {
            if let Ok(url) = url.parse::<RelayUrl>() {
                endpoint.remove_relay(&url).await;
            }
        }
        for relay in &due {
            if let Some((url, config)) = relay_config(relay) {
                endpoint.insert_relay(url, config).await;
            }
        }
        self.mark_used(&due);
    }

    fn mark_used(&self, relays: &[&AnonymousRelay]) {
        if let Ok(mut used) = self.used_passes.lock() {
            used.extend(relays.iter().map(|relay| relay.pass.clone()));
        }
    }

    async fn spawn_generation(&self, relays: &[&AnonymousRelay]) -> anyhow::Result<Generation> {
        let map = iroh::RelayMap::empty();
        let mut held = HashMap::new();
        for relay in relays {
            if let Some((url, config)) = relay_config(relay) {
                map.insert(url, config);
                held.insert(relay.url.clone(), Instant::now());
            }
        }
        anyhow::ensure!(!held.is_empty(), "no usable relay URL");
        let builder = || {
            iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
                .secret_key(SecretKey::generate())
                .relay_mode(iroh::RelayMode::Custom(map.clone()))
        };
        let endpoint = match builder()
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?
            .bind_addr(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))?
            .bind()
            .await
        {
            Ok(endpoint) => endpoint,
            Err(_) => {
                builder()
                    .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?
                    .bind()
                    .await?
            }
        };
        let router = iroh::protocol::Router::builder(endpoint)
            .accept(
                ANON_TUNNEL_PROTOCOL.as_bytes(),
                AnonServe {
                    handler: self.handler.clone(),
                    is_member: self.is_member.clone(),
                },
            )
            .spawn();
        Ok(Generation {
            router,
            born: Instant::now(),
            relays: Mutex::new(held),
            connections: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Where this node answers anonymous tunnels now, for its announce:
    /// the current ephemeral endpoint and the visitor relay it is homed on.
    pub async fn address(&self) -> Option<AnonTunnelAddr> {
        let current = self.current.read().await;
        let endpoint = current.as_ref()?.endpoint();
        let relay = endpoint.addr().relay_urls().next()?.to_string();
        Some(AnonTunnelAddr {
            endpoint_id: endpoint.id().to_string(),
            relay_url: relay,
        })
    }

    /// One tunnel call to the member answering at `target`, through its
    /// visitor relay, on a bound connection.
    pub async fn call(
        &self,
        target: &AnonTunnelAddr,
        request: TunnelRequest,
    ) -> Result<TunnelResponse, CallError> {
        let generation = self
            .current
            .read()
            .await
            .clone()
            .ok_or_else(|| CallError::BeforeRequest("no ephemeral endpoint".into()))?;
        let id = p2p::parse_endpoint_id(&target.endpoint_id)
            .ok_or_else(|| CallError::BeforeRequest("invalid ephemeral id".into()))?;
        let connection = self.bound_connection(&generation, id, target).await?;
        let result = p2p::call_on(&connection, InferTunnelCodec, request).await;
        if result.is_err() {
            generation.connections.lock().await.remove(&id);
        }
        result
    }

    async fn bound_connection(
        &self,
        generation: &Generation,
        id: EndpointId,
        target: &AnonTunnelAddr,
    ) -> Result<Connection, CallError> {
        let mut connections = generation.connections.lock().await;
        if let Some(connection) = connections.get(&id) {
            if connection.close_reason().is_none() {
                return Ok(connection.clone());
            }
        }
        let relay: RelayUrl = target
            .relay_url
            .parse()
            .map_err(|_| CallError::BeforeRequest("invalid relay URL".into()))?;
        let addr = iroh::EndpointAddr::new(id).with_relay_url(relay);
        let connection = generation
            .endpoint()
            .connect(addr, ANON_TUNNEL_PROTOCOL.as_bytes())
            .await
            .map_err(|error| CallError::BeforeRequest(error.to_string()))?;
        let material = keying_material(&connection)
            .ok_or_else(|| CallError::BeforeRequest("no keying material".into()))?;
        let binding = sign_binding(&material, &self.secret, &self.node_id);
        match p2p::call_on(&connection, BindCodec, binding).await {
            Ok(true) => {}
            Ok(false) => return Err(CallError::BeforeRequest("binding refused".into())),
            Err(CallError::AfterDispatch(error)) | Err(CallError::BeforeRequest(error)) => {
                return Err(CallError::BeforeRequest(error))
            }
        }
        connections.insert(id, connection.clone());
        Ok(connection)
    }
}

/// The relay map entry of an anonymous relay: its pass is the auth token.
fn relay_config(relay: &AnonymousRelay) -> Option<(RelayUrl, Arc<iroh::RelayConfig>)> {
    let url: RelayUrl = relay.url.parse().ok()?;
    let quic = (url.scheme() == "https").then(Default::default);
    let config = iroh::RelayConfig::new(url.clone(), quic).with_auth_token(relay.pass.clone());
    Some((url, Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binding_holds_only_for_its_connection_and_its_node() {
        let secret = SecretKey::from_bytes(&[7; 32]);
        let key = ed25519_dalek::VerifyingKey::from_bytes(secret.public().as_bytes()).unwrap();
        let node_id = crate::identity::NodeId::from_pubkey(&key).0;
        let material = [1_u8; 32];
        let binding = sign_binding(&material, &secret, &node_id);
        assert_eq!(check_binding(&material, &binding), Some(secret.public()));

        // Another connection's material: the binding does not carry over.
        assert_eq!(check_binding(&[2_u8; 32], &binding), None);
        // A node id that is not the key's.
        let mut other = binding.clone();
        other.node_id = "someone-else".into();
        assert_eq!(check_binding(&material, &other), None);
        // Someone else's key signing in the node's name.
        let forger = SecretKey::from_bytes(&[8; 32]);
        let mut forged = sign_binding(&material, &forger, &node_id);
        forged.pubkey = binding.pubkey.clone();
        assert_eq!(check_binding(&material, &forged), None);
    }

    #[tokio::test]
    async fn a_binding_round_trips_and_is_bounded() {
        let binding = sign_binding(&[3; 32], &SecretKey::from_bytes(&[4; 32]), "node");
        let mut wire = Vec::new();
        BindCodec
            .write_request(&mut wire, binding.clone())
            .await
            .unwrap();
        let read = BindCodec
            .read_request(&mut futures::io::Cursor::new(wire))
            .await
            .unwrap();
        assert_eq!(read, binding);

        let mut oversized = ((MAX_BIND_BYTES + 1) as u32).to_be_bytes().to_vec();
        oversized.extend_from_slice(&[0; 8]);
        assert!(BindCodec
            .read_request(&mut futures::io::Cursor::new(oversized))
            .await
            .is_err());
    }
}
