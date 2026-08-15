//! Pkarr-based peer discovery and publishing for rdma-p2p nodes.
//!
//! Each node publishes its layer assignment and addresses as a TXT record
//! under the `_rdmap2p` DNS name in a signed pkarr packet keyed by its
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
    pub inference_port: Option<u16>,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub is_relay: bool,
}

// ── Private helper ────────────────────────────────────────────────────────────

/// Compact JSON payload stored in the `_rdmap2p` TXT record.
/// Field names are kept short to stay within the 1000-byte DNS packet limit.
#[derive(Debug, Serialize, Deserialize)]
struct TxtPayload {
    /// Multiaddr / address string (e.g. "192.168.1.5:42777")
    a: String,
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

    /// Publish this node's layer assignment and address to the DHT.
    pub async fn publish(
        &self,
        addr: &str,
        inference_port: Option<u16>,
        model: &str,
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
        is_relay: bool,
    ) -> Result<()> {
        let payload = TxtPayload {
            a: addr.to_string(),
            i: inference_port,
            m: model.to_string(),
            s: start_layer,
            e: end_layer,
            t: total_layers,
            r: if is_relay { Some(true) } else { None },
        };

        let json = serde_json::to_string(&payload)?;

        let sp = SignedPacket::builder()
            .txt("_rdmap2p".try_into()?, json.as_str().try_into()?, 300)
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
/// `_rdmap2p` TXT record.
fn parse_signed_packet(sp: &SignedPacket) -> Option<ResolvedPeer> {
    for rr in sp.resource_records("_rdmap2p") {
        if let pkarr::dns::rdata::RData::TXT(txt) = &rr.rdata {
            let raw: Result<String, _> = txt.clone().try_into();
            if let Ok(json) = raw {
                if let Ok(payload) = serde_json::from_str::<TxtPayload>(&json) {
                    return Some(ResolvedPeer {
                        multiaddr: payload.a,
                        inference_port: payload.i,
                        model: payload.m,
                        start_layer: payload.s,
                        end_layer: payload.e,
                        total_layers: payload.t,
                        is_relay: payload.r.unwrap_or(false),
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
                warn!("No _rdmap2p TXT record for {}", key_str);
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
            i: Some(8181),
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: None,
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
            i: None,
            m: "gemma3".to_string(),
            s: 16,
            e: 31,
            t: 32,
            r: None,
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
            i: None,
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: Some(true),
        };
        let json = serde_json::to_string(&with_relay).expect("serialize");
        assert!(
            json.contains("\"r\":true"),
            "relay flag should be in JSON: {}",
            json
        );
        let decoded: TxtPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.r, Some(true));

        let no_relay = TxtPayload {
            a: "1.2.3.4:42777".to_string(),
            i: None,
            m: "llama3:8b".to_string(),
            s: 0,
            e: 15,
            t: 32,
            r: None,
        };
        let json2 = serde_json::to_string(&no_relay).expect("serialize");
        assert!(
            !json2.contains("\"r\""),
            "relay flag should be absent: {}",
            json2
        );
    }
}
