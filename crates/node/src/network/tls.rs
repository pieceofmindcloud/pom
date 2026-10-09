//! TLS entre peers: certificado auto assinado amarrado à identidade do nó.
//!
//! A pergunta que este módulo responde é "com quem eu estou falando", e a
//! resposta já existia: `node_id` é `hex(SHA256(pubkey))` da chave Ed25519 de
//! longa duração (`identity.rs`), e o cluster inteiro já distribui esse hash
//! pelo gossip. Então o certificado não precisa ser assinado por ninguém - ele
//! precisa apenas **provar posse da chave que dá naquele node_id**, e é isso
//! que o handshake TLS faz.
//!
//! Nada aqui vai para o disco: o certificado é derivado da chave a cada
//! arranque. Não há o que renovar, revogar ou distribuir.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::identity::Identity;

/// Certificado e chave que este nó apresenta, ambos em memória.
pub struct NodeCertificate {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

/// Deriva o certificado auto assinado da identidade do nó.
///
/// A chave do certificado **é** a `.pom-node.key`: o `rcgen` aceita o PKCS#8
/// que o `ed25519-dalek` exporta, e o rustls assina TLS 1.3 com Ed25519. Gerar
/// uma chave nova aqui destruiria o propósito - o par apresentado precisa ser o
/// par que define o node_id.
pub fn node_certificate(id: &Identity) -> Result<NodeCertificate> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let pkcs8 = id
        .signing_key
        .to_pkcs8_der()
        .context("a chave do nó não exportou como PKCS#8")?;
    let key_pair = KeyPair::try_from(pkcs8.as_bytes())
        .map_err(|e| anyhow!("a chave do nó não serviu como par de chaves TLS: {e}"))?;

    let node_id = id.node_id.0.clone();
    // O SAN é um nome DNS sintético com o node_id. Nunca é resolvido: existe só
    // porque um certificado sem SAN é recusado por bibliotecas estritas, e
    // porque põe a identidade num campo legível por `openssl x509 -text` na
    // hora de depurar.
    let mut params = CertificateParams::new(vec![format!("{node_id}.{}", crate::brand::slug())])
        .map_err(|e| anyhow!("parâmetros do certificado inválidos: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_id);
    params.distinguished_name = dn;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("não deu para auto assinar o certificado: {e}"))?;

    Ok(NodeCertificate {
        cert: cert.der().clone(),
        key: PrivateKeyDer::Pkcs8(key_pair.serialize_der().into()),
    })
}

/// O node_id que um certificado apresenta: SHA256 da pubkey do SPKI.
///
/// Mesma conta de `NodeId::from_pubkey`, sobre os mesmos 32 bytes. O teste
/// `the_certificate_fingerprint_is_the_same_as_from_pubkey` prende as duas: se um
/// dia divergirem, o pinning passa a recusar todo mundo.
pub fn node_id_of_cert(der: &[u8]) -> Result<String> {
    let (_, parsed) =
        x509_parser::parse_x509_certificate(der).context("certificado do peer não parseou")?;
    let bits = parsed.public_key().subject_public_key.data.as_ref();
    if bits.len() != 32 {
        return Err(anyhow!(
            "chave do certificado tem {} bytes, esperava 32 (Ed25519)",
            bits.len()
        ));
    }
    Ok(hex::encode(Sha256::digest(bits)))
}

/// Verifier que só aceita o nó cujo node_id foi fixado.
#[derive(Debug)]
pub struct PinnedNodeId {
    expected: String,
}

impl PinnedNodeId {
    pub fn new(expected: String) -> Self {
        Self { expected }
    }

    /// A regra inteira, isolada do trait para poder ser testada sem handshake.
    pub fn matches(&self, der: &[u8]) -> Result<()> {
        let found = node_id_of_cert(der)?;
        if found == self.expected {
            Ok(())
        } else {
            Err(anyhow!(
                "certificado é do nó {}, esperava {}",
                &found[..12.min(found.len())],
                &self.expected[..12.min(self.expected.len())]
            ))
        }
    }
}

impl ServerCertVerifier for PinnedNodeId {
    /// Validade, cadeia e nome são **ignorados de propósito**.
    ///
    /// Isso parece um buraco quando lido fora de contexto, então: a confiança
    /// aqui não vem de autoridade nem de prazo, vem de posse de chave, que o
    /// handshake prova em `verify_tls13_signature`. Checar validade só
    /// produziria falha operacional por relógio torto, e checar nome só
    /// produziria falha por discar um IP em vez do CN - nenhuma das duas diria
    /// nada sobre com quem estamos falando.
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

/// Instala o provedor de cripto do rustls uma vez por processo.
///
/// O rustls 0.23 exige um provedor default explícito quando mais de um está
/// compilado, e a chamada é idempotente por design - erro aqui significa "já
/// tem um", que é exatamente o que queremos.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// ALPN que só os nós deste projeto oferecem.
///
/// É o discriminador que decide **qual dos dois certificados** o servidor
/// apresenta, e ele precisa ser confiável: SNI não serve (peer disca IP, e IP
/// não leva SNI) e a lista de algoritmos do cliente varia entre navegadores. O
/// ALPN está inteiramente sob nosso controle - nosso cliente oferece `pom/1`,
/// navegador nenhum oferece.
pub const PEER_ALPN: &[u8] = b"pom/1";

/// Certificado que um **navegador** aceita, para a interface web.
///
/// Não pode ser o mesmo do pinning: aquele é Ed25519, porque a chave dele é a
/// identidade do nó, e navegador não verifica certificado Ed25519 - medido com
/// o curl/LibreSSL do macOS, que responde `sslv3 alert handshake failure`. Este
/// aqui é ECDSA P-256, que eles sabem verificar.
///
/// Continua auto assinado, então o navegador avisa na primeira visita de cada
/// máquina. Isso é inerente a não ter uma CA; os endereços entram no SAN para o
/// aviso ser "emissor desconhecido" e não "host errado", que é o que faria o
/// operador desconfiar da máquina em vez do certificado.
pub fn browser_certificate(sans: &[String]) -> Result<NodeCertificate> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let mut names: Vec<String> = vec!["localhost".into()];
    names.extend(sans.iter().cloned());
    names.sort();
    names.dedup();

    let mut params = CertificateParams::new(names)
        .map_err(|e| anyhow!("parâmetros do certificado de navegador inválidos: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "pom");
    params.distinguished_name = dn;

    // Padrão do rcgen: ECDSA P-256.
    let key_pair = KeyPair::generate().map_err(|e| anyhow!("chave do certificado web: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| anyhow!("não deu para auto assinar o certificado web: {e}"))?;

    Ok(NodeCertificate {
        cert: cert.der().clone(),
        key: PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow!("chave do certificado web inválida: {e}"))?,
    })
}

/// Escolhe o certificado por conexão: peer recebe o do pinning, navegador
/// recebe o que ele sabe verificar.
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

    /// A regra, isolada do trait para ter teste sem handshake.
    pub fn wants_peer<'a>(alpn: Option<impl Iterator<Item = &'a [u8]>>) -> bool {
        match alpn {
            Some(mut offered) => offered.any(|p| p == PEER_ALPN),
            None => false,
        }
    }
}

fn certified(c: NodeCertificate) -> Result<rustls::sign::CertifiedKey> {
    let key = rustls::crypto::ring::sign::any_supported_type(&c.key)
        .map_err(|e| anyhow!("rustls não soube assinar com esta chave: {e}"))?;
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

/// Configuração de servidor: apresenta o certificado do nó, não pede
/// certificado do cliente.
///
/// mTLS fica de fora porque o cliente já se autentica pela chave de conexão no
/// header, e duplicar a política de autorização em dois lugares é como ela
/// passa a divergir.
pub fn server_config(id: &Identity, sans: &[String]) -> Result<Arc<rustls::ServerConfig>> {
    install_crypto_provider();
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(NodeCertResolver::new(id, sans)?));
    // `pom/1` primeiro: a preferência é do servidor, e um peer que ofereça os
    // dois tem que fechar no nosso. `h2` e `http/1.1` existem para o navegador,
    // e o builder automático do hyper serve os dois.
    cfg.alpn_protocols = vec![PEER_ALPN.to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Configuração de cliente que só aceita o peer de node_id `expected`.
pub fn client_config(expected: &str) -> Result<rustls::ClientConfig> {
    install_crypto_provider();
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedNodeId::new(expected.to_string())))
        .with_no_client_auth();
    // `pom/1` é o que faz o servidor escolher o certificado do pinning; sem ele
    // viria o certificado de navegador, que é ECDSA, e este verifier - que só
    // aceita Ed25519 - recusaria. Falha fechada, mas por engano. O `http/1.1`
    // continua na lista porque é o protocolo que os pares de fato falam.
    cfg.alpn_protocols = vec![PEER_ALPN.to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

/// Configuração TLS 1.3 do HTTP/3, com o mesmo pinning de identidade do HTTP
/// sobre TCP. QUIC usa ALPN `h3` e nunca participa da chave `tls_required`.
pub fn quic_client_config(expected: &str) -> Result<rustls::ClientConfig> {
    let mut cfg = client_config(expected)?;
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, NodeId};

    /// O certificado tem que carregar a identidade do nó, não uma chave nova:
    /// é isso que faz o pinning valer sem distribuir arquivo nenhum.
    #[test]
    fn the_certificate_carries_the_identity_node_id() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), id.node_id.0);
    }

    /// Duas identidades diferentes não podem produzir o mesmo fingerprint.
    #[test]
    fn a_certificate_from_another_key_has_another_node_id() {
        let a = node_certificate(&Identity::generate()).unwrap();
        let b = node_certificate(&Identity::generate()).unwrap();
        assert_ne!(
            node_id_of_cert(a.cert.as_ref()).unwrap(),
            node_id_of_cert(b.cert.as_ref()).unwrap()
        );
    }

    /// O verifier aceita o certificado do nó que ele esperava.
    #[test]
    fn verifier_accepts_the_expected_node_id() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let v = PinnedNodeId::new(id.node_id.0.clone());
        assert!(v.matches(cert.cert.as_ref()).is_ok());
    }

    /// E recusa o de qualquer outro - é o ataque de quem se põe no meio com o
    /// próprio auto assinado, que é o único ataque que o pinning existe para
    /// impedir.
    #[test]
    fn verifier_refuses_a_certificate_from_another_node() {
        let expected = Identity::generate();
        let impostor = node_certificate(&Identity::generate()).unwrap();
        let v = PinnedNodeId::new(expected.node_id.0.clone());
        assert!(v.matches(impostor.cert.as_ref()).is_err());
    }

    /// O node_id do certificado é, literalmente, o mesmo hash que o cluster já
    /// usa para nomear a máquina. Se um dia divergirem, o pinning passa a
    /// recusar todo mundo, e este teste é o que avisa.
    #[test]
    fn the_certificate_fingerprint_is_the_same_as_from_pubkey() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let expected = NodeId::from_pubkey(&id.signing_key.verifying_key());
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), expected.0);
    }
}
