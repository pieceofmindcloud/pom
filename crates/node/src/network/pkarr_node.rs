//! Pkarr: identity-to-address resolver.
//!
//! Each node publishes under its own key (the same as the `node_id`, the
//! `PeerId` and the authority - see `identity.rs`) a signed `_pom` TXT in the
//! Mainline DHT with the **public** multiaddresses the swarm listens on now and
//! the keys of the relays where it holds a circuit reservation. Whoever only
//! knows the key (a trust document, a public directory record,
//! `--relay-peers`) resolves here where to dial. The record is never an
//! identity: it is a pointer the publisher renews when its addresses change.

use anyhow::Result;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use pkarr::{Client, Keypair, SignedPacket};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Cap on direct multiaddresses in the record. The whole DNS packet has to fit
/// in ~1000 bytes.
pub const MAX_RECORD_ADDRS: usize = 4;
/// Cap on relays in the record.
pub const MAX_RECORD_RELAYS: usize = 3;
/// Budget for the TXT JSON, below the packet limit.
const MAX_PAYLOAD_BYTES: usize = 900;

// ── Public structs ────────────────────────────────────────────────────────────

/// A pkarr node that can publish and resolve peer information on the DHT.
pub struct PkarrNode {
    client: Client,
    keypair: Keypair,
}

/// What a node publishes about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PkarrRecord {
    /// Direct public multiaddresses, without the `/p2p/<self>` suffix.
    pub multiaddrs: Vec<String>,
    /// `pk:` keys of the relays where this node holds a circuit reservation.
    pub relays: Vec<String>,
    pub gateway_port: Option<u16>,
    pub inference_port: Option<u16>,
    pub is_relay: bool,
    /// Canonical geo discovery label. Record metadata, never the key.
    pub geo_label: Option<String>,
}

/// A record resolved from the DHT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPeer {
    pub peer_id: PeerId,
    pub record: PkarrRecord,
}

// ── Private helper ────────────────────────────────────────────────────────────

/// Compact JSON payload stored in the `_pom` TXT record.
/// Field names are kept short to stay within the 1000-byte DNS packet limit.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct TxtPayload {
    /// Direct multiaddresses, without `/p2p/<self>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    a: Vec<String>,
    /// Relays holding a reservation, in z-base-32 (without the `pk:` prefix).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    x: Vec<String>,
    /// HTTP gateway port for the public metadata endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<u16>,
    /// Optional inference server port
    #[serde(skip_serializing_if = "Option::is_none")]
    i: Option<u16>,
    /// Relay capability (true if this node accepts relay reservations)
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g: Option<String>,
}

impl TxtPayload {
    fn from_record(record: &PkarrRecord) -> Self {
        Self {
            a: record
                .multiaddrs
                .iter()
                .map(|addr| strip_peer_suffix(addr))
                .take(MAX_RECORD_ADDRS)
                .collect(),
            x: record
                .relays
                .iter()
                .filter_map(|key| pkarr::PublicKey::try_from(key.as_str()).ok())
                .map(|key| key.to_z32())
                .take(MAX_RECORD_RELAYS)
                .collect(),
            h: record.gateway_port,
            i: record.inference_port,
            r: record.is_relay.then_some(true),
            g: record
                .geo_label
                .as_deref()
                .filter(|label| !label.trim().is_empty())
                .map(str::to_owned),
        }
    }

    fn into_record(self) -> PkarrRecord {
        PkarrRecord {
            multiaddrs: self.a,
            relays: self
                .x
                .iter()
                .filter_map(|key| pkarr::PublicKey::try_from(key.as_str()).ok())
                .map(|key| key.to_uri_string())
                .collect(),
            gateway_port: self.h,
            inference_port: self.i,
            is_relay: self.r.unwrap_or(false),
            geo_label: self.g,
        }
    }

    /// Serializes, trimming addresses and relays from the end until it fits
    /// the budget.
    fn to_bounded_json(mut self) -> Result<String> {
        loop {
            let json = serde_json::to_string(&self)?;
            if json.len() <= MAX_PAYLOAD_BYTES {
                return Ok(json);
            }
            if self.a.len() > 1 {
                self.a.pop();
            } else if !self.x.is_empty() {
                self.x.pop();
            } else if self.g.take().is_none() {
                anyhow::bail!("pkarr record does not fit in the DNS packet");
            }
        }
    }
}

/// Strips the trailing `/p2p/<id>`: the resolver knows the `PeerId` from the
/// key.
fn strip_peer_suffix(addr: &str) -> String {
    let Ok(mut multiaddr) = addr.parse::<Multiaddr>() else {
        return addr.to_string();
    };
    if matches!(multiaddr.iter().last(), Some(Protocol::P2p(_))) {
        multiaddr.pop();
    }
    multiaddr.to_string()
}

// ── Identity: PeerId <-> pkarr key ───────────────────────────────────────────

/// The pkarr key (`pk:...`) of an Ed25519 `PeerId`. Every node uses the same
/// key for both, so a relay's `PeerId` already says where to resolve it.
pub fn uri_from_peer_id(peer_id: &PeerId) -> Option<String> {
    let multihash = peer_id.as_ref();
    // Code 0x00 = identity: the digest is the protobuf-encoded public key.
    if multihash.code() != 0x00 {
        return None;
    }
    let public = libp2p::identity::PublicKey::try_decode_protobuf(multihash.digest()).ok()?;
    let ed25519 = public.try_into_ed25519().ok()?;
    let key = pkarr::PublicKey::try_from(&ed25519.to_bytes()).ok()?;
    Some(key.to_uri_string())
}

/// The libp2p `PeerId` of a pkarr key.
pub fn peer_id_from_uri(uri: &str) -> Option<PeerId> {
    let key = pkarr::PublicKey::try_from(uri).ok()?;
    let public = libp2p::identity::ed25519::PublicKey::try_from_bytes(key.as_bytes()).ok()?;
    Some(libp2p::identity::PublicKey::from(public).to_peer_id())
}

/// Dialable addresses of a resolved peer: the direct ones, each with
/// `/p2p/<peer>`, followed by the circuits through each resolved relay
/// (`<relay>/p2p/<relay>/p2p-circuit/p2p/<peer>`). The order is the ladder's:
/// direct first, relay after; DCuTR tries to punch the NAT over the circuit on
/// its own.
pub fn dial_addrs(peer: &ResolvedPeer, relays: &[ResolvedPeer]) -> Vec<Multiaddr> {
    let mut out = Vec::new();
    for addr in &peer.record.multiaddrs {
        let Ok(mut multiaddr) = addr.parse::<Multiaddr>() else {
            continue;
        };
        if !matches!(multiaddr.iter().last(), Some(Protocol::P2p(_))) {
            multiaddr.push(Protocol::P2p(peer.peer_id));
        }
        out.push(multiaddr);
    }
    for relay in relays {
        if relay.peer_id == peer.peer_id {
            continue;
        }
        for addr in &relay.record.multiaddrs {
            let Ok(mut multiaddr) = addr.parse::<Multiaddr>() else {
                continue;
            };
            if !matches!(multiaddr.iter().last(), Some(Protocol::P2p(_))) {
                multiaddr.push(Protocol::P2p(relay.peer_id));
            }
            multiaddr.push(Protocol::P2pCircuit);
            multiaddr.push(Protocol::P2p(peer.peer_id));
            out.push(multiaddr);
        }
    }
    out
}

// ── PkarrNode impl ────────────────────────────────────────────────────────────

impl PkarrNode {
    /// pkarr client over the node identity. There is no separate key file:
    /// the key is the one in `.pom-node.key`.
    pub fn from_signing_key(signing_key: &ed25519_dalek::SigningKey) -> Result<Self> {
        let keypair = Keypair::from_secret_key(&signing_key.to_bytes());
        let client = Client::builder().build()?;
        Ok(Self { client, keypair })
    }

    #[cfg(test)]
    pub(crate) fn with_client(client: Client, keypair: Keypair) -> Self {
        Self { client, keypair }
    }

    /// Returns the `pk:` URI string for this node's public key.
    pub fn public_key_uri(&self) -> String {
        self.keypair.to_uri_string()
    }

    /// Derive the pkarr URI from the hex public key carried by an announce or
    /// a trust principal.
    pub fn public_key_uri_from_hex(public_key_hex: &str) -> Option<String> {
        let bytes: [u8; 32] = hex::decode(public_key_hex).ok()?.try_into().ok()?;
        let public_key = pkarr::PublicKey::try_from(&bytes).ok()?;
        Some(public_key.to_uri_string())
    }

    /// Publishes this node's record to the DHT.
    pub async fn publish(&self, record: &PkarrRecord) -> Result<()> {
        let json = TxtPayload::from_record(record).to_bounded_json()?;
        let sp = SignedPacket::builder()
            .txt(
                crate::brand::dns_record().as_str().try_into()?,
                json.as_str().try_into()?,
                300,
            )
            .sign(&self.keypair)?;
        self.client.publish(&sp, None).await?;
        info!("pkarr: record published: {json}");
        Ok(())
    }

    /// Resolves a key (`pk:...` or z-base-32). Any failure becomes `None`.
    pub async fn resolve(&self, key: &str) -> Option<ResolvedPeer> {
        let pk: pkarr::PublicKey = match key.try_into() {
            Ok(pk) => pk,
            Err(error) => {
                debug!("pkarr: invalid key '{key}': {error}");
                return None;
            }
        };
        let peer_id = peer_id_from_uri(key)?;
        let packet = self.client.resolve(&pk).await?;
        let record = parse_signed_packet(&packet)?;
        debug!("pkarr: {key} resolved: {record:?}");
        Some(ResolvedPeer { peer_id, record })
    }

    /// Resolve several keys concurrently and return the ones that answered.
    pub async fn resolve_peers(&self, keys: &[String]) -> Vec<ResolvedPeer> {
        futures::future::join_all(keys.iter().map(|key| self.resolve(key)))
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// Resolve a public candidate from its announced hex public key.
    pub async fn resolve_public_peer(&self, pubkey_hex: &str) -> Option<ResolvedPeer> {
        let uri = Self::public_key_uri_from_hex(pubkey_hex)?;
        self.resolve(&uri).await
    }

    /// Resolves the peer and the relays it published, and returns where to
    /// dial.
    pub async fn resolve_dial_addrs(&self, key: &str) -> Vec<Multiaddr> {
        let Some(peer) = self.resolve(key).await else {
            return Vec::new();
        };
        let relays = self.resolve_peers(&peer.record.relays).await;
        dial_addrs(&peer, &relays)
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Parse a [SignedPacket] by reading the first valid `_pom` TXT record.
fn parse_signed_packet(sp: &SignedPacket) -> Option<PkarrRecord> {
    for rr in sp.resource_records(&crate::brand::dns_record()) {
        if let pkarr::dns::rdata::RData::TXT(txt) = &rr.rdata {
            let raw: Result<String, _> = txt.clone().try_into();
            if let Ok(json) = raw {
                if let Ok(payload) = serde_json::from_str::<TxtPayload>(&json) {
                    return Some(payload.into_record());
                }
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair_peer_id(keypair: &Keypair) -> PeerId {
        peer_id_from_uri(&keypair.to_uri_string()).unwrap()
    }

    #[test]
    fn the_payload_carries_addresses_and_relays_without_the_own_peer_suffix() {
        let relay = Keypair::random();
        let me = Keypair::random();
        let record = PkarrRecord {
            multiaddrs: vec![format!(
                "/ip4/203.0.113.7/udp/52777/quic-v1/p2p/{}",
                keypair_peer_id(&me)
            )],
            relays: vec![relay.to_uri_string()],
            gateway_port: Some(8080),
            inference_port: None,
            is_relay: false,
            geo_label: None,
        };
        let json = TxtPayload::from_record(&record).to_bounded_json().unwrap();
        assert!(!json.contains("/p2p/"), "{json}");
        let decoded: TxtPayload = serde_json::from_str(&json).unwrap();
        let back = decoded.into_record();
        assert_eq!(back.multiaddrs, vec!["/ip4/203.0.113.7/udp/52777/quic-v1"]);
        assert_eq!(back.relays, vec![relay.to_uri_string()]);
        assert_eq!(back.gateway_port, Some(8080));
    }

    #[test]
    fn an_oversized_record_is_trimmed_to_fit_the_packet() {
        let long = "/ip6/2001:db8:ffff:ffff:ffff:ffff:ffff:ffff/udp/52777/quic-v1".to_string();
        let record = PkarrRecord {
            multiaddrs: vec![long; 20],
            relays: (0..10).map(|_| Keypair::random().to_uri_string()).collect(),
            geo_label: Some("x".repeat(200)),
            ..Default::default()
        };
        let json = TxtPayload::from_record(&record).to_bounded_json().unwrap();
        assert!(json.len() <= MAX_PAYLOAD_BYTES);
        let decoded: TxtPayload = serde_json::from_str(&json).unwrap();
        assert!(!decoded.a.is_empty());
    }

    #[test]
    fn peer_id_and_pkarr_uri_name_the_same_key() {
        let keypair = Keypair::random();
        let peer_id = peer_id_from_uri(&keypair.to_uri_string()).unwrap();
        assert_eq!(uri_from_peer_id(&peer_id).unwrap(), keypair.to_uri_string());
    }

    #[test]
    fn dial_addrs_put_direct_routes_before_relay_circuits() {
        let peer_key = Keypair::random();
        let relay_key = Keypair::random();
        let peer = ResolvedPeer {
            peer_id: keypair_peer_id(&peer_key),
            record: PkarrRecord {
                multiaddrs: vec!["/ip4/203.0.113.7/tcp/52777".into()],
                relays: vec![relay_key.to_uri_string()],
                ..Default::default()
            },
        };
        let relay = ResolvedPeer {
            peer_id: keypair_peer_id(&relay_key),
            record: PkarrRecord {
                multiaddrs: vec!["/ip4/198.51.100.1/tcp/52777".into()],
                is_relay: true,
                ..Default::default()
            },
        };
        let addrs = dial_addrs(&peer, &[relay.clone()]);
        assert_eq!(
            addrs[0].to_string(),
            format!("/ip4/203.0.113.7/tcp/52777/p2p/{}", peer.peer_id)
        );
        assert_eq!(
            addrs[1].to_string(),
            format!(
                "/ip4/198.51.100.1/tcp/52777/p2p/{}/p2p-circuit/p2p/{}",
                relay.peer_id, peer.peer_id
            )
        );
    }

    #[test]
    fn authority_public_key_derives_the_pkarr_uri() {
        let keypair = Keypair::random();
        let authority = hex::encode(keypair.public_key().as_bytes());
        let derived = PkarrNode::public_key_uri_from_hex(&authority).unwrap();

        assert_eq!(derived, keypair.to_uri_string());
    }

    #[tokio::test]
    async fn a_public_record_published_by_one_client_is_resolved_by_another() {
        let testnet = pkarr::mainline::Testnet::builder(5).build().unwrap();
        let client = || {
            let mut builder = Client::builder();
            builder
                .no_default_network()
                .bootstrap(&testnet.bootstrap)
                .dht(|dht| dht.bind_address(std::net::Ipv4Addr::LOCALHOST))
                .request_timeout(std::time::Duration::from_millis(500));
            builder.build().unwrap()
        };
        let keypair = Keypair::random();
        let relay_keypair = Keypair::random();
        let publisher = PkarrNode::with_client(client(), keypair.clone());
        let relay = PkarrNode::with_client(client(), relay_keypair.clone());
        let resolver = PkarrNode::with_client(client(), Keypair::random());

        relay
            .publish(&PkarrRecord {
                multiaddrs: vec!["/ip4/198.51.100.1/udp/52777/quic-v1".into()],
                is_relay: true,
                ..Default::default()
            })
            .await
            .unwrap();
        publisher
            .publish(&PkarrRecord {
                multiaddrs: vec!["/ip6/2001:db8::1/tcp/52777".into()],
                relays: vec![relay_keypair.to_uri_string()],
                gateway_port: Some(8080),
                geo_label: Some("com.br.curitiba.public-network".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        let authority = hex::encode(keypair.public_key().as_bytes());
        let resolved = resolver.resolve_public_peer(&authority).await.unwrap();
        assert_eq!(
            resolved.record.multiaddrs,
            vec!["/ip6/2001:db8::1/tcp/52777"]
        );
        assert_eq!(resolved.record.gateway_port, Some(8080));
        assert_eq!(
            resolved.record.geo_label.as_deref(),
            Some("com.br.curitiba.public-network")
        );

        let dial = resolver.resolve_dial_addrs(&keypair.to_uri_string()).await;
        assert_eq!(dial.len(), 2, "{dial:?}");
        assert!(dial[1].to_string().contains("/p2p-circuit/"));
    }
}
