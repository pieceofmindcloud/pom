//! TLS between peers: a self-signed certificate bound to the node's identity.
//!
//! The question this module answers is "who am I talking to", and the
//! answer already existed: `node_id` is `hex(SHA256(pubkey))` of the long-lived
//! Ed25519 key (`identity.rs`), and the whole cluster already distributes that hash
//! through the gossip. So the certificate does not need to be signed by anyone - it
//! only needs to **prove possession of the key that yields that node_id**, and that
//! is what the TLS handshake does.
//!
//! Nothing here goes to disk: the peer certificate is derived from the key on
//! every start. There is nothing to renew, revoke or distribute. The browser's
//! certificate is the exception, and lives in `web_ca.rs`.
//!
//! The same certificate is the client certificate of every peer connection
//! (mutual TLS), so the server knows which node is calling as well. What
//! that node may do is decided from its key per request, against the trust
//! chain and the membership tokens (`auth::validate_internal_node_headers`).

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::identity::Identity;

/// Certificate and key this node presents, both in memory.
pub struct NodeCertificate {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

/// Derives the self-signed certificate from the node's identity.
///
/// The certificate's key **is** the `.pom-node.key`: `rcgen` accepts the PKCS#8
/// that `ed25519-dalek` exports, and rustls signs TLS 1.3 with Ed25519. Generating
/// a new key here would defeat the purpose - the pair presented has to be the
/// pair that defines the node_id.
pub fn node_certificate(id: &Identity) -> Result<NodeCertificate> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let pkcs8 = id
        .signing_key
        .to_pkcs8_der()
        .context("the node key did not export as PKCS#8")?;
    let key_pair = KeyPair::try_from(pkcs8.as_bytes())
        .map_err(|e| anyhow!("the node key did not work as a TLS key pair: {e}"))?;

    let node_id = id.node_id.0.clone();
    // The SAN is a synthetic DNS name with the node_id. It is never resolved: it exists
    // only because a certificate without a SAN is refused by strict libraries, and
    // because it puts the identity in a field readable by `openssl x509 -text` when
    // debugging.
    let mut params = CertificateParams::new(vec![format!("{node_id}.{}", crate::brand::slug())])
        .map_err(|e| anyhow!("invalid certificate parameters: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_id);
    params.distinguished_name = dn;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("could not self-sign the certificate: {e}"))?;

    Ok(NodeCertificate {
        cert: cert.der().clone(),
        key: PrivateKeyDer::Pkcs8(key_pair.serialize_der().into()),
    })
}

/// The node_id a certificate presents: SHA256 of the SPKI's pubkey.
///
/// Same computation as `NodeId::from_pubkey`, over the same 32 bytes. The test
/// `the_certificate_fingerprint_is_the_same_as_from_pubkey` ties the two: if they
/// ever diverge, the pinning starts refusing everyone.
pub fn node_id_of_cert(der: &[u8]) -> Result<String> {
    let (_, parsed) =
        x509_parser::parse_x509_certificate(der).context("peer certificate did not parse")?;
    let bits = parsed.public_key().subject_public_key.data.as_ref();
    if bits.len() != 32 {
        return Err(anyhow!(
            "certificate key has {} bytes, expected 32 (Ed25519)",
            bits.len()
        ));
    }
    Ok(hex::encode(Sha256::digest(bits)))
}

/// Verifier that only accepts the node whose node_id was pinned.
#[derive(Debug)]
pub struct PinnedNodeId {
    expected: String,
}

impl PinnedNodeId {
    pub fn new(expected: String) -> Self {
        Self { expected }
    }

    /// The whole rule, isolated from the trait so it can be tested without a handshake.
    pub fn matches(&self, der: &[u8]) -> Result<()> {
        let found = node_id_of_cert(der)?;
        if found == self.expected {
            Ok(())
        } else {
            Err(anyhow!(
                "certificate belongs to node {}, expected {}",
                &found[..12.min(found.len())],
                &self.expected[..12.min(self.expected.len())]
            ))
        }
    }
}

impl ServerCertVerifier for PinnedNodeId {
    /// Validity, chain and name are **ignored on purpose**.
    ///
    /// This looks like a hole when read out of context, so: the trust
    /// here does not come from an authority or an expiry, it comes from key possession,
    /// which the handshake proves in `verify_tls13_signature`. Checking validity would
    /// only produce operational failures from a skewed clock, and checking the name would
    /// only produce failures from dialing an IP instead of the CN - neither would say
    /// anything about who we are talking to.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        self.matches(end_entity.as_ref())
            .map(|()| ServerCertVerified::assertion())
            .map_err(|e| rustls::Error::General(e.to_string()))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Installs the rustls crypto provider once per process.
///
/// rustls 0.23 requires an explicit default provider when more than one is
/// compiled in, and the call is idempotent by design - an error here means "there
/// already is one", which is exactly what we want.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// ALPN that only this project's nodes offer.
///
/// It is the discriminator that decides **which of the two certificates** the server
/// presents, and it has to be reliable: SNI does not work (a peer dials an IP, and an
/// IP carries no SNI) and the client's algorithm list varies between browsers. The
/// ALPN is entirely under our control - our client offers `pom/1`,
/// no browser does.
pub const PEER_ALPN: &[u8] = b"pom/1";

/// Certificate a **browser** accepts, for the web interface.
///
/// It cannot be the same as the pinning one: that one is Ed25519, because its key is
/// the node's identity, and a browser does not verify an Ed25519 certificate - measured
/// with macOS's curl/LibreSSL, which answers `sslv3 alert handshake failure`. This
/// one is ECDSA P-256, signed by this machine's local CA with a persisted key, so it
/// survives restarts and a system that trusts the CA shows no warning. The addresses
/// go into the SAN so the warning, where the CA is not trusted, is "unknown issuer"
/// and not "wrong host". See `web_ca.rs`.
pub fn browser_certificate(sans: &[String]) -> Result<NodeCertificate> {
    crate::web_ca::browser_certificate(sans)
}

/// Whether a ClientHello comes from a peer: it offers `PEER_ALPN`, which no
/// browser does. It picks the server configuration per connection
/// (`serve::handle_conn`): a peer gets the pinning certificate and is asked
/// for its own, a browser gets the one it can verify and is asked for none.
pub fn wants_peer<'a>(alpn: Option<impl Iterator<Item = &'a [u8]>>) -> bool {
    match alpn {
        Some(mut offered) => offered.any(|p| p == PEER_ALPN),
        None => false,
    }
}

/// Client certificate verifier of the peer configuration: any node
/// certificate, proven by the handshake signature. Who the node is, and
/// whether it is a member, is decided per request from the certificate's key
/// (`serve::TransportPeer`, `auth::validate_internal_node_headers`), not
/// here: the chain and the tokens change while a connection lives.
#[derive(Debug)]
struct AnyNodeClient;

/// The client verifier peers are served with, for the QUIC endpoint too.
pub(crate) fn any_node_client() -> Arc<dyn rustls::server::danger::ClientCertVerifier> {
    Arc::new(AnyNodeClient)
}

impl rustls::server::danger::ClientCertVerifier for AnyNodeClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    /// Optional, so a peer config still serves a client without a node
    /// key; such a request carries no identity and is refused wherever one
    /// is needed.
    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        node_id_of_cert(end_entity.as_ref())
            .map(|_| rustls::server::danger::ClientCertVerified::assertion())
            .map_err(|e| rustls::Error::General(e.to_string()))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// The two server configurations of a node: one for peers, one for browsers.
#[derive(Clone)]
pub struct ServerConfigs {
    /// Presents the node certificate and asks the peer for its own (mutual TLS).
    pub peer: Arc<rustls::ServerConfig>,
    /// Presents the browser certificate and asks for nothing.
    pub browser: Arc<rustls::ServerConfig>,
}

pub fn server_configs(id: &Identity, sans: &[String]) -> Result<ServerConfigs> {
    install_crypto_provider();
    let node = node_certificate(id)?;
    let mut peer = rustls::ServerConfig::builder()
        .with_client_cert_verifier(any_node_client())
        .with_single_cert(vec![node.cert], node.key)
        .map_err(|e| anyhow!("node certificate rejected by rustls: {e}"))?;
    peer.alpn_protocols = vec![PEER_ALPN.to_vec(), b"http/1.1".to_vec()];
    let web = browser_certificate(sans)?;
    let mut browser = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![web.cert], web.key)
        .map_err(|e| anyhow!("web certificate rejected by rustls: {e}"))?;
    // `h2` and `http/1.1` exist for the browser, and hyper's automatic
    // builder serves both.
    browser.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(ServerConfigs {
        peer: Arc::new(peer),
        browser: Arc::new(browser),
    })
}

/// This node's certificate, presented as the client certificate on every
/// peer connection it opens. Set once at startup (`set_client_identity`);
/// a test bench that never sets it connects without one.
static CLIENT_IDENTITY: std::sync::OnceLock<NodeCertificate> = std::sync::OnceLock::new();

pub fn set_client_identity(id: &Identity) -> Result<()> {
    let _ = CLIENT_IDENTITY.set(node_certificate(id)?);
    Ok(())
}

/// Client configuration that only accepts the peer whose node_id is
/// `expected`, and proves this node's own identity to it.
pub fn client_config(expected: &str) -> Result<rustls::ClientConfig> {
    client_config_as(expected, CLIENT_IDENTITY.get())
}

/// `client_config` presenting `own` as the client certificate, or none.
pub(crate) fn client_config_as(
    expected: &str,
    own: Option<&NodeCertificate>,
) -> Result<rustls::ClientConfig> {
    install_crypto_provider();
    let builder = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedNodeId::new(expected.to_string())));
    let mut cfg = match own {
        Some(own) => builder
            .with_client_auth_cert(vec![own.cert.clone()], own.key.clone_key())
            .map_err(|e| anyhow!("node certificate rejected as a client certificate: {e}"))?,
        None => builder.with_no_client_auth(),
    };
    // `pom/1` is what makes the server pick the pinning certificate; without it
    // the browser certificate would come, which is ECDSA, and this verifier - which only
    // accepts Ed25519 - would refuse it. Fail-closed, but by mistake. `http/1.1`
    // stays on the list because it is the protocol the peers actually speak.
    cfg.alpn_protocols = vec![PEER_ALPN.to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

/// TLS 1.3 configuration of HTTP/3, with the same identity pinning as HTTP
/// over TCP. QUIC uses ALPN `h3` and never takes part in the `tls_required` key.
pub fn quic_client_config(expected: &str) -> Result<rustls::ClientConfig> {
    let mut cfg = client_config(expected)?;
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, NodeId};

    /// The certificate has to carry the node's identity, not a new key:
    /// that is what makes the pinning hold without distributing any file.
    #[test]
    fn the_certificate_carries_the_identity_node_id() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), id.node_id.0);
    }

    /// Two different identities cannot produce the same fingerprint.
    #[test]
    fn a_certificate_from_another_key_has_another_node_id() {
        let a = node_certificate(&Identity::generate()).unwrap();
        let b = node_certificate(&Identity::generate()).unwrap();
        assert_ne!(
            node_id_of_cert(a.cert.as_ref()).unwrap(),
            node_id_of_cert(b.cert.as_ref()).unwrap()
        );
    }

    /// The verifier accepts the certificate of the node it expected.
    #[test]
    fn verifier_accepts_the_expected_node_id() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let v = PinnedNodeId::new(id.node_id.0.clone());
        assert!(v.matches(cert.cert.as_ref()).is_ok());
    }

    /// And refuses anyone else's - that is the attack of someone getting in the middle
    /// with their own self-signed certificate, which is the only attack the pinning
    /// exists to prevent.
    #[test]
    fn verifier_refuses_a_certificate_from_another_node() {
        let expected = Identity::generate();
        let impostor = node_certificate(&Identity::generate()).unwrap();
        let v = PinnedNodeId::new(expected.node_id.0.clone());
        assert!(v.matches(impostor.cert.as_ref()).is_err());
    }

    /// The certificate's node_id is, literally, the same hash the cluster already
    /// uses to name the machine. If they ever diverge, the pinning starts
    /// refusing everyone, and this test is what warns about it.
    #[test]
    fn the_certificate_fingerprint_is_the_same_as_from_pubkey() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let expected = NodeId::from_pubkey(&id.signing_key.verifying_key());
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), expected.0);
    }
}
