//! Pkarr: identity-to-address resolver.
//!
//! Each node publishes under its own key (the same as the `node_id`, the
//! endpoint id and the authority - see `identity.rs`) a signed `_pom` TXT in
//! the Mainline DHT with the **public** direct addresses of its endpoint and
//! the URLs of the relays it is reachable through. Whoever only knows the key
//! (a trust document, a public directory record, `--pkarr-peers`) resolves
//! here where to dial. The record is never an identity: it is a pointer the
//! publisher renews when its addresses change.
//!
//! iroh's own pkarr publisher is deliberately not used: it would sign a
//! record under the same key, and one key holds one record in the DHT - the
//! two would overwrite each other.

use crate::p2p::{EndpointAddr, EndpointId};
use anyhow::Result;
use pkarr::{Client, Keypair, SignedPacket};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Cap on direct addresses in the record. The whole DNS packet has to fit
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
    /// Direct public endpoint addresses (`ip:port`).
    pub addrs: Vec<String>,
    /// URLs of the relays this endpoint is reachable through.
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
    pub endpoint_id: EndpointId,
    pub record: PkarrRecord,
}

// ── Private helper ────────────────────────────────────────────────────────────

/// Compact JSON payload stored in the `_pom` TXT record.
/// Field names are kept short to stay within the 1000-byte DNS packet limit.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct TxtPayload {
    /// Direct endpoint addresses, `ip:port`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    a: Vec<String>,
    /// Relay URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    x: Vec<String>,
    /// HTTP gateway port for the public metadata endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<u16>,
    /// Optional inference server port
    #[serde(skip_serializing_if = "Option::is_none")]
    i: Option<u16>,
    /// Relay capability (true when this node is a public relay)
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g: Option<String>,
}

impl TxtPayload {
    fn from_record(record: &PkarrRecord) -> Self {
        Self {
            a: record
                .addrs
                .iter()
                .take(MAX_RECORD_ADDRS)
                .cloned()
                .collect(),
            x: record
                .relays
                .iter()
                .filter(|url| crate::p2p::relay_url(url).is_some())
                .take(MAX_RECORD_RELAYS)
                .cloned()
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
            addrs: self.a,
            relays: self
                .x
                .into_iter()
                .filter(|url| crate::p2p::relay_url(url).is_some())
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

// ── Identity: endpoint id <-> pkarr key ──────────────────────────────────────

/// The pkarr key (`pk:...`) of an endpoint id. Every node uses the same key
/// for both.
#[cfg(test)]
pub fn uri_from_endpoint_id(id: &EndpointId) -> String {
    format!("pk:{}", id.to_z32())
}

/// The endpoint id of a pkarr key (`pk:...` or bare z-base-32).
pub fn endpoint_id_from_uri(uri: &str) -> Option<EndpointId> {
    let key = pkarr::PublicKey::try_from(uri).ok()?;
    EndpointId::from_bytes(key.as_bytes()).ok()
}

/// Where to dial a resolved peer: its direct addresses and its relays, in one
/// endpoint address. iroh tries them together and keeps what answers.
pub fn dial_addr(peer: &ResolvedPeer) -> EndpointAddr {
    let texts: Vec<String> = peer
        .record
        .addrs
        .iter()
        .chain(peer.record.relays.iter())
        .cloned()
        .collect();
    crate::p2p::endpoint_addr(peer.endpoint_id, &texts)
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
        self.client.publish(&sp).await?;
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
        let endpoint_id = endpoint_id_from_uri(key)?;
        let packet = self
            .client
            .resolve(&pk, pkarr::ResolvePolicy::CacheFirst)
            .await
            .ok()?;
        let record = parse_signed_packet(&packet)?;
        debug!("pkarr: {key} resolved: {record:?}");
        Some(ResolvedPeer {
            endpoint_id,
            record,
        })
    }

    /// Resolve a public candidate from its announced hex public key.
    pub async fn resolve_public_peer(&self, pubkey_hex: &str) -> Option<ResolvedPeer> {
        let uri = Self::public_key_uri_from_hex(pubkey_hex)?;
        self.resolve(&uri).await
    }

    /// Resolves the peer and returns where to dial it, when its record has
    /// anywhere to dial.
    pub async fn resolve_dial_addr(&self, key: &str) -> Option<EndpointAddr> {
        let peer = self.resolve(key).await?;
        let addr = dial_addr(&peer);
        (!addr.is_empty()).then_some(addr)
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

    #[test]
    fn the_payload_carries_addresses_and_relay_urls() {
        let record = PkarrRecord {
            addrs: vec!["203.0.113.7:52777".into()],
            relays: vec!["https://relay.example.org/".into(), "pk:not-a-url".into()],
            gateway_port: Some(8080),
            inference_port: None,
            is_relay: false,
            geo_label: None,
        };
        let json = TxtPayload::from_record(&record).to_bounded_json().unwrap();
        let decoded: TxtPayload = serde_json::from_str(&json).unwrap();
        let back = decoded.into_record();
        assert_eq!(back.addrs, vec!["203.0.113.7:52777"]);
        assert_eq!(back.relays, vec!["https://relay.example.org/"]);
        assert_eq!(back.gateway_port, Some(8080));
    }

    #[test]
    fn an_oversized_record_is_trimmed_to_fit_the_packet() {
        let long = "[2001:db8:ffff:ffff:ffff:ffff:ffff:ffff]:52777".to_string();
        let record = PkarrRecord {
            addrs: vec![long; 20],
            relays: (0..10)
                .map(|i| format!("https://relay-{i}-{}.example.org/", "x".repeat(60)))
                .collect(),
            geo_label: Some("x".repeat(200)),
            ..Default::default()
        };
        let json = TxtPayload::from_record(&record).to_bounded_json().unwrap();
        assert!(json.len() <= MAX_PAYLOAD_BYTES);
        let decoded: TxtPayload = serde_json::from_str(&json).unwrap();
        assert!(!decoded.a.is_empty());
    }

    #[test]
    fn endpoint_id_and_pkarr_uri_name_the_same_key() {
        let keypair = Keypair::random();
        let id = endpoint_id_from_uri(&keypair.to_uri_string()).unwrap();
        assert_eq!(uri_from_endpoint_id(&id), keypair.to_uri_string());
        assert_eq!(id.as_bytes(), keypair.public_key().as_bytes());
    }

    #[test]
    fn the_dial_address_holds_direct_routes_and_relays() {
        let key = Keypair::random();
        let peer = ResolvedPeer {
            endpoint_id: endpoint_id_from_uri(&key.to_uri_string()).unwrap(),
            record: PkarrRecord {
                addrs: vec!["203.0.113.7:52777".into()],
                relays: vec!["https://relay.example.org/".into()],
                ..Default::default()
            },
        };
        let addr = dial_addr(&peer);
        assert_eq!(addr.id, peer.endpoint_id);
        assert_eq!(addr.ip_addrs().count(), 1);
        assert_eq!(addr.relay_urls().count(), 1);
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
        let testnet = mainline::Testnet::builder(5).build().unwrap();
        let client = || {
            let mut builder = Client::builder();
            builder
                .no_default_network()
                .bootstrap(&testnet.bootstrap)
                .dht(|dht| {
                    dht.bind_address = Some(std::net::Ipv4Addr::LOCALHOST);
                    dht
                })
                .dht_request_timeout(std::time::Duration::from_millis(500));
            builder.build().unwrap()
        };
        let keypair = Keypair::random();
        let publisher = PkarrNode::with_client(client(), keypair.clone());
        let resolver = PkarrNode::with_client(client(), Keypair::random());

        publisher
            .publish(&PkarrRecord {
                addrs: vec!["[2001:db8::1]:52777".into()],
                relays: vec!["https://relay.example.org/".into()],
                gateway_port: Some(8080),
                geo_label: Some("com.br.curitiba.public-network".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        let authority = hex::encode(keypair.public_key().as_bytes());
        let resolved = resolver.resolve_public_peer(&authority).await.unwrap();
        assert_eq!(resolved.record.addrs, vec!["[2001:db8::1]:52777"]);
        assert_eq!(resolved.record.gateway_port, Some(8080));
        assert_eq!(
            resolved.record.geo_label.as_deref(),
            Some("com.br.curitiba.public-network")
        );

        let dial = resolver
            .resolve_dial_addr(&keypair.to_uri_string())
            .await
            .unwrap();
        assert_eq!(dial.ip_addrs().count(), 1);
        assert_eq!(dial.relay_urls().count(), 1);
    }
}
