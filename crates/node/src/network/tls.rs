//! TLS between peers: a self-signed certificate bound to the node's identity.
//!
//! The question this module answers is "who am I talking to", and the
//! answer already existed: `node_id` is `hex(SHA256(pubkey))` of the long-lived
//! Ed25519 key (`identity.rs`), and the whole cluster already distributes that hash
//! through the gossip. So the certificate does not need to be signed by anyone - it
//! only needs to **prove possession of the key that yields that node_id**, and that
//! is what the TLS handshake does.
//!
//! Nothing here goes to disk: the certificate is derived from the key on every
//! start. There is nothing to renew, revoke or distribute.

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
/// one is ECDSA P-256, which they know how to verify.
///
/// It is still self-signed, so the browser warns on the first visit from each
/// machine. That is inherent to having no CA; the addresses go into the SAN so the
/// warning is "unknown issuer" and not "wrong host", which is what would make the
/// operator distrust the machine instead of the certificate.
pub fn browser_certificate(sans: &[String]) -> Result<NodeCertificate> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let mut names: Vec<String> = vec!["localhost".into()];
    names.extend(sans.iter().cloned());
    names.sort();
    names.dedup();

    let mut params = CertificateParams::new(names)
        .map_err(|e| anyhow!("invalid browser certificate parameters: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "pom");
    params.distinguished_name = dn;

    // rcgen's default: ECDSA P-256.
    let key_pair = KeyPair::generate().map_err(|e| anyhow!("web certificate key: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("could not self-sign the web certificate: {e}"))?;

    Ok(NodeCertificate {
        cert: cert.der().clone(),
        key: PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow!("invalid web certificate key: {e}"))?,
    })
}

/// Picks the certificate per connection: a peer gets the pinning one, a browser
/// gets the one it knows how to verify.
#[derive(Debug)]
pub struct NodeCertResolver {
    peer: Arc<rustls::sign::CertifiedKey>,
    browser: Arc<rustls::sign::CertifiedKey>,
}

impl NodeCertResolver {
    pub fn new(id: &Identity, sans: &[String]) -> Result<Self> {
        Ok(Self {
            peer: Arc::new(certified(node_certificate(id)?)?),
            browser: Arc::new(certified(browser_certificate(sans)?)?),
        })
    }

    /// The rule, isolated from the trait so it can be tested without a handshake.
    pub fn wants_peer<'a>(alpn: Option<impl Iterator<Item = &'a [u8]>>) -> bool {
        match alpn {
            Some(mut offered) => offered.any(|p| p == PEER_ALPN),
            None => false,
        }
    }
}

fn certified(c: NodeCertificate) -> Result<rustls::sign::CertifiedKey> {
    let key = rustls::crypto::ring::sign::any_supported_type(&c.key)
        .map_err(|e| anyhow!("rustls could not sign with this key: {e}"))?;
    Ok(rustls::sign::CertifiedKey::new(vec![c.cert], key))
}

impl rustls::server::ResolvesServerCert for NodeCertResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if Self::wants_peer(client_hello.alpn()) {
            Some(self.peer.clone())
        } else {
            Some(self.browser.clone())
        }
    }
}

/// Server configuration: presents the node's certificate, does not ask for
/// a client certificate.
///
/// mTLS is left out because the client already authenticates with the connection key
/// in the header, and duplicating the authorization policy in two places is how it
/// starts to diverge.
pub fn server_config(id: &Identity, sans: &[String]) -> Result<Arc<rustls::ServerConfig>> {
    install_crypto_provider();
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(NodeCertResolver::new(id, sans)?));
    // `pom/1` first: the preference is the server's, and a peer that offers
    // both has to settle on ours. `h2` and `http/1.1` exist for the browser,
    // and hyper's automatic builder serves both.
    cfg.alpn_protocols = vec![PEER_ALPN.to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Client configuration that only accepts the peer whose node_id is `expected`.
pub fn client_config(expected: &str) -> Result<rustls::ClientConfig> {
    install_crypto_provider();
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedNodeId::new(expected.to_string())))
        .with_no_client_auth();
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
