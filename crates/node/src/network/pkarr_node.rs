//! Pkarr-based peer discovery and publishing for pom nodes.
//!
//! Each node publishes its layer assignment and addresses as a TXT record
//! under the `_pom` DNS name in a signed pkarr packet keyed by its
//! Ed25519 keypair.  Peers can be resolved by looking up their public key.

use anyhow::Result;
use pkarr::{Client, Keypair, SignedPacket};
use serde::{Deserialize, Serialize};
use std::net::UdpSocket;
use std::path::Path;
use tracing::{info, warn};

// ── Public structs ────────────────────────────────────────────────────────────

/// A pkarr node that can publish and resolve peer information on the DHT.
pub struct PkarrNode {
    client: Client,
    keypair: Keypair,
}

/// A peer that has been resolved from the DHT.
///
/// Hoje só `multiaddr` e `is_relay` são lidos: o pkarr serve para *discar*, e
/// quem preenche a tabela de peers (modelo, faixa de camadas, porta de
/// inferência) continua sendo o `Announce` UDP. O resto do payload é decodado
/// e guardado porque é o que o registro TXT carrega - descartar no parse seria
/// perder a informação que falta para o peer que só chega por DHT.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub multiaddr: String,
    pub gateway_port: Option<u16>,
    pub inference_port: Option<u16>,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub is_relay: bool,
    /// Canonical geo discovery label carried as pkarr record metadata.
    pub geo_label: Option<String>,
}

// ── Private helper ────────────────────────────────────────────────────────────

/// Compact JSON payload stored in the `_pom` TXT record.
/// Field names are kept short to stay within the 1000-byte DNS packet limit.
#[derive(Debug, Serialize, Deserialize)]
struct TxtPayload {
    /// Multiaddr / address string (e.g. "192.168.1.5:42777")
    a: String,
    /// HTTP gateway port for the public metadata endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<u16>,
    /// Optional inference server port
    #[serde(skip_serializing_if = "Option::is_none")]
    i: Option<u16>,
    /// Model name
    m: String,
    /// Start layer (inclusive)
    s: u32,
    /// End layer (inclusive)
    e: u32,
    /// Total layers in the full model
    t: u32,
    /// Relay capability (true if this node accepts relay connections)
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<bool>,
    /// Canonical geo discovery label. This is record metadata, never the pkarr key.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    g: Option<String>,
}

// ── PkarrNode impl ────────────────────────────────────────────────────────────

impl PkarrNode {
    /// Load an existing keypair from `key_path`, or generate a fresh one and
    /// persist it there.
    pub fn load_or_create(key_path: &Path) -> Result<Self> {
        let keypair = match Keypair::from_secret_key_file(key_path) {
            Ok(kp) => {
                info!("Loaded existing pkarr keypair from {}", key_path.display());
                kp
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let kp = Keypair::random();
                kp.write_secret_key_file(key_path)?;
                info!("Generated new pkarr identity → {}", key_path.display());
                kp
            }
            Err(e) => return Err(e.into()),
        };

        let client = Client::builder().build()?;

        Ok(Self { client, keypair })
    }

    /// Returns the `pk:` URI string for this node's public key.
    pub fn public_key_uri(&self) -> String {
        self.keypair.to_uri_string()
    }

    /// Derive the pkarr URI from the public authority key carried by an
    /// announce. The authority uses the same Ed25519 key as the pkarr record,
    /// so a public candidate does not need a second discovery credential.
    pub fn public_key_uri_from_hex(public_key_hex: &str) -> Option<String> {
        let bytes: [u8; 32] = hex::decode(public_key_hex).ok()?.try_into().ok()?;
        let public_key = pkarr::PublicKey::try_from(&bytes).ok()?;
        Some(public_key.to_uri_string())
    }

    /// Resolve a public candidate's pkarr record from its announced authority
    /// key. This is deliberately independent of network admission: the record
    /// is public metadata, while membership still goes through `admit_peer`.
    pub async fn resolve_public_peer(&self, authority_pubkey_hex: &str) -> Option<ResolvedPeer> {
        let uri = Self::public_key_uri_from_hex(authority_pubkey_hex)?;
        resolve_one(&self.client, &uri).await
    }

    /// Extract the host from the address published in a pkarr record. The
    /// record carries the public P2P port; the caller supplies the advertised
    /// gateway port for the public metadata endpoint.
    pub fn host_from_multiaddr(addr: &str) -> Option<String> {
        for protocol in addr.parse::<libp2p::Multiaddr>().ok()?.iter() {
            match protocol {
                libp2p::multiaddr::Protocol::Ip4(ip) => return Some(ip.to_string()),
                libp2p::multiaddr::Protocol::Ip6(ip) => return Some(format!("[{ip}]")),
                _ => {}
            }
        }
        None
    }

    /// Raw 32-byte Ed25519 seed backing this identity.
    ///
    /// Lets the libp2p swarm derive a *stable* `PeerId` from the same
    /// already-persisted identity instead of a fresh keypair every restart —
    /// see `Discovery::new`. `pkarr::Keypair` and `libp2p::identity::Keypair`
    /// both wrap plain `ed25519_dalek::SigningKey` seeds, so this is a
    /// direct, lossless handoff, not a conversion.
    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.keypair.secret_key()
    }

    /// Publish this node's layer assignment and address to the DHT.
    pub async fn publish(
        &self,
        addr: &str,
        gateway_port: Option<u16>,
        inference_port: Option<u16>,
        model: &str,
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
        is_relay: bool,
        geo_label: Option<&str>,
    ) -> Result<()> {
        let payload = TxtPayload {
            a: addr.to_string(),
            h: gateway_port,
            i: inference_port,
            m: model.to_string(),
            s: start_layer,
            e: end_layer,
            t: total_layers,
            r: if is_relay { Some(true) } else { None },
            g: geo_label
                .filter(|label| !label.trim().is_empty())
                .map(str::to_owned),
        };

        let json = serde_json::to_string(&payload)?;

        let sp = SignedPacket::builder()
            .txt(
                crate::brand::dns_record().as_str().try_into()?,
                json.as_str().try_into()?,
                300,
            )
            .sign(&self.keypair)?;

        self.client.publish(&sp, None).await?;

        info!("Published pkarr record: {}", json);

        Ok(())
    }

    /// Resolve a list of peer public keys concurrently and return any successfully decoded peers.
    pub async fn resolve_peers(&self, pubkeys: &[String]) -> Vec<ResolvedPeer> {
        let futs: Vec<_> = pubkeys
            .iter()
            .map(|k| resolve_one(&self.client, k))
            .collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .flatten()
            .collect()
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Parse a [SignedPacket] into a [ResolvedPeer] by reading the first valid
/// `_pom` TXT record.
fn parse_signed_packet(sp: &SignedPacket) -> Option<ResolvedPeer> {
    for rr in sp.resource_records(&crate::brand::dns_record()) {
        if let pkarr::dns::rdata::RData::TXT(txt) = &rr.rdata {
            let raw: Result<String, _> = txt.clone().try_into();
            if let Ok(json) = raw {
                if let Ok(payload) = serde_json::from_str::<TxtPayload>(&json) {
                    return Some(ResolvedPeer {
                        multiaddr: payload.a,
                        gateway_port: payload.h,
                        inference_port: payload.i,
                        model: payload.m,
                        start_layer: payload.s,
                        end_layer: payload.e,
                        total_layers: payload.t,
                        is_relay: payload.r.unwrap_or(false),
                        geo_label: payload.g,
                    });
                }
            }
        }
    }
    None
}

/// Resolve a single pkarr public key to a [ResolvedPeer], returning `None` on
/// any failure (parse error, DHT miss, missing record).
async fn resolve_one(client: &pkarr::Client, key_str: &str) -> Option<ResolvedPeer> {
    let pk: pkarr::PublicKey = match key_str.try_into() {
        Ok(pk) => pk,
        Err(e) => {
            warn!("Invalid pkarr key '{}': {}", key_str, e);
            return None;
        }
    };
    match client.resolve(&pk).await {
        Some(sp) => {
            if let Some(peer) = parse_signed_packet(&sp) {
                info!("Resolved pkarr peer {}: {}", key_str, peer.multiaddr);
                Some(peer)
            } else {
                warn!("No _pom TXT record for {}", key_str);
                None
            }
        }
        None => {
            warn!("Could not resolve pkarr key {}", key_str);
            None
        }
    }
}

/// Detect the local machine's outbound IP address by connecting a UDP socket
/// to a public address (no data is sent).
pub fn detect_local_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_txt_payload() {
        let payload = TxtPayload {
            a: "/ip4/1.2.3.4/tcp/52777".to_string(),
            h: None,
            i: Some(8181),
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: None,
            g: None,
        };

        let json = serde_json::to_string(&payload).expect("serialize");
        let decoded: TxtPayload = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.a, "/ip4/1.2.3.4/tcp/52777");
        assert_eq!(decoded.i, Some(8181));
        assert_eq!(decoded.m, "llama3:8b");
        assert_eq!(decoded.s, 0);
        assert_eq!(decoded.e, 15);
        assert_eq!(decoded.t, 32);
    }

    #[test]
    fn roundtrip_no_inference_port() {
        let payload = TxtPayload {
            a: "10.0.0.2:42777".to_string(),
            h: None,
            i: None,
            m: "gemma3".to_string(),
            s: 16,
            e: 31,
            t: 32,
            r: None,
            g: None,
        };

        let json = serde_json::to_string(&payload).expect("serialize");

        // `skip_serializing_if = "Option::is_none"` means `"i"` must be absent
        assert!(
            !json.contains("\"i\""),
            "JSON should not contain '\"i\"' when inference_port is None, got: {}",
            json
        );

        let decoded: TxtPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.i, None);
        assert_eq!(decoded.m, "gemma3");
    }

    /// `Discovery::new` derives the libp2p keypair straight from this seed
    /// (`libp2p::identity::Keypair::ed25519_from_bytes`) so the swarm's
    /// `PeerId` is stable across restarts instead of a fresh one every time.
    /// That only holds if the same seed always yields the same `PeerId`.
    #[test]
    fn the_same_pkarr_seed_always_derives_the_same_libp2p_peer_id() {
        let dir = std::env::temp_dir().join(format!("pom-pkarr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join(".pom.key");

        let node = PkarrNode::load_or_create(&key_path).expect("create pkarr node");
        let seed = node.secret_key_bytes();

        let mut seed_a = seed;
        let peer_a = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed_a[..])
            .expect("derive keypair")
            .public()
            .to_peer_id();

        let mut seed_b = seed;
        let peer_b = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed_b[..])
            .expect("derive keypair")
            .public()
            .to_peer_id();

        assert_eq!(peer_a, peer_b, "same seed must yield the same PeerId");

        // Reload from disk (simulates a process restart) — must still match.
        let reloaded = PkarrNode::load_or_create(&key_path).expect("reload pkarr node");
        let mut seed_c = reloaded.secret_key_bytes();
        let peer_c = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed_c[..])
            .expect("derive keypair")
            .public()
            .to_peer_id();
        assert_eq!(
            peer_a, peer_c,
            "PeerId must survive a reload of the persisted key"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detect_local_ip_returns_something() {
        let ip = detect_local_ip();
        assert!(ip.is_some(), "should detect local IP");
        let ip = ip.unwrap();
        assert!(!ip.is_empty());
        assert!(ip.contains('.'), "IP should be IPv4: {}", ip);
    }

    #[test]
    fn roundtrip_relay_field() {
        let with_relay = TxtPayload {
            a: "1.2.3.4:42777".to_string(),
            h: None,
            i: None,
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: Some(true),
            g: Some("com.br.curitiba.node-a".to_string()),
        };
        let json = serde_json::to_string(&with_relay).expect("serialize");
        assert!(
            json.contains("\"r\":true"),
            "relay flag should be in JSON: {}",
            json
        );
        let decoded: TxtPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.r, Some(true));
        assert_eq!(decoded.g.as_deref(), Some("com.br.curitiba.node-a"));

        let no_relay = TxtPayload {
            a: "1.2.3.4:42777".to_string(),
            h: None,
            i: None,
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: None,
            g: None,
        };
        let json2 = serde_json::to_string(&no_relay).expect("serialize");
        assert!(
            !json2.contains("\"r\""),
            "relay flag should be absent: {}",
            json2
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
        let mut builder = Client::builder();
        builder
            .no_default_network()
            .bootstrap(&testnet.bootstrap)
            .dht(|dht| dht.bind_address(std::net::Ipv4Addr::LOCALHOST))
            .request_timeout(std::time::Duration::from_millis(500));
        let publisher = builder.build().unwrap();

        let mut resolver_builder = Client::builder();
        resolver_builder
            .no_default_network()
            .bootstrap(&testnet.bootstrap)
            .dht(|dht| dht.bind_address(std::net::Ipv4Addr::LOCALHOST))
            .request_timeout(std::time::Duration::from_millis(500));
        let resolver = resolver_builder.build().unwrap();
        let keypair = Keypair::random();
        let publisher = PkarrNode {
            client: publisher,
            keypair: keypair.clone(),
        };
        let resolver = PkarrNode {
            client: resolver,
            keypair: Keypair::random(),
        };

        publisher
            .publish(
                "/ip6/2001:db8::1/tcp/52777",
                Some(8080),
                None,
                "",
                0,
                0,
                0,
                false,
                Some("com.br.curitiba.public-network"),
            )
            .await
            .unwrap();

        let authority = hex::encode(keypair.public_key().as_bytes());
        let resolved = resolver.resolve_public_peer(&authority).await.unwrap();
        assert_eq!(resolved.multiaddr, "/ip6/2001:db8::1/tcp/52777");
        assert_eq!(resolved.gateway_port, Some(8080));
        assert_eq!(
            resolved.geo_label.as_deref(),
            Some("com.br.curitiba.public-network")
        );
    }
}
