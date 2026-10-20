//! The certificate a browser receives, issued by this machine's own CA.
//!
//! The peer certificate (`tls.rs`) is Ed25519 and pinned by `node_id`; no browser
//! verifies it. The browser gets a second one, ECDSA P-256, and until now it was
//! self-signed with a key made on every start: each restart was a new certificate
//! and the operator had to accept the warning again. Two things change that:
//!
//! - **The web key is persisted** (`.pom-web.key`). The certificate is issued again
//!   on every start, because the addresses in its SAN move, but its key does not.
//!   That stable key is what the native windows pin (`is_local_web_certificate`):
//!   they load the interface over `https://127.0.0.1` and accept exactly this
//!   key, whether or not the CA below is trusted.
//! - **The certificate is signed by a local CA** (`.pom-ca.key`, its public half
//!   exported as `.pom-ca.crt`), made once per machine. Once the operating system
//!   trusts it (`trust_store.rs`, or the `.crt` downloaded from the interface),
//!   browsers, curl and the harnesses that read the system store accept
//!   `https://<this machine>` with no warning, restart after restart.
//!
//! The CA can sign only for this machine's names: its name constraints permit
//! `localhost` and the host names it was created with, so its key cannot vouch
//! for anyone else's domain. IP addresses are left unconstrained (a constraint on
//! one name form leaves the others free), because a node's addresses move. A DNS
//! name outside the constraints never goes into the web certificate, since a
//! browser refuses the whole certificate over one name the CA may not sign.
//!
//! The files sit beside the node key (`identity::key_path`), with the CA and web
//! keys readable by the owner only. Deleting the CA file makes a new CA, which
//! has to be trusted again.

use anyhow::{anyhow, Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose, NameConstraints, SerialNumber,
};
use rustls::pki_types::PrivateKeyDer;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::tls::NodeCertificate;

const CA_VALID_DAYS: i64 = 3650;
/// Apple refuses a server certificate valid for more than 825 days, even under
/// a CA the user added; 397 is what every browser accepts. The certificate is
/// issued again on every start, so it never gets near the end.
const WEB_VALID_DAYS: i64 = 397;

/// The CA's private key and certificate, in one file: `.pom-ca.key`.
pub fn ca_filename() -> String {
    format!(".{}-ca.key", crate::brand::slug())
}

/// The CA certificate alone, the file the interface offers and the
/// operating system installs: `.pom-ca.crt`.
pub fn ca_cert_filename() -> String {
    format!(".{}-ca.crt", crate::brand::slug())
}

/// The web certificate's key: `.pom-web.key`.
pub fn web_key_filename() -> String {
    format!(".{}-web.key", crate::brand::slug())
}

/// Folder of the files: the node key's own.
pub fn dir() -> PathBuf {
    #[cfg(test)]
    {
        // Route tests build TLS servers; they must not write into the project root.
        let dir = std::env::temp_dir().join(format!("pom-web-ca-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
    #[cfg(not(test))]
    {
        crate::identity::key_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(crate::config::project_root)
    }
}

pub fn ca_cert_path() -> PathBuf {
    dir().join(ca_cert_filename())
}

/// This machine's CA.
pub struct LocalCa {
    key: KeyPair,
    issuer: rcgen::Certificate,
    cert_pem: String,
    cert_der: Vec<u8>,
    permitted_dns: Vec<String>,
}

impl LocalCa {
    /// Loads the CA of `dir()`, creating it on first use, and keeps its public
    /// `.crt` beside it.
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_in(&dir(), &host_names())
    }

    /// `names` are the DNS names the CA may sign for when it is created here;
    /// an existing CA keeps the ones it was made with.
    pub fn load_or_create_in(dir: &Path, names: &[String]) -> Result<Self> {
        let path = dir.join(ca_filename());
        if !path.exists() {
            let (key, cert_pem) = create_ca(names)?;
            let contents = format!("{}{}", key.serialize_pem(), cert_pem);
            // A second process may be creating it at the same time; whoever
            // writes first wins and the other reads that one.
            write_new(&path, contents.as_bytes(), true)?;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let ca = Self::parse(&text).with_context(|| format!("{} is not a valid CA", path.display()))?;
        let public = dir.join(ca_cert_filename());
        if std::fs::read_to_string(&public).ok().as_deref() != Some(ca.cert_pem.as_str()) {
            write_replace(&public, ca.cert_pem.as_bytes())?;
        }
        Ok(ca)
    }

    fn parse(text: &str) -> Result<Self> {
        let marker = "-----BEGIN CERTIFICATE-----";
        let at = text.find(marker).ok_or_else(|| anyhow!("no certificate in the CA file"))?;
        let (key_pem, cert_pem) = text.split_at(at);
        let key = KeyPair::from_pem(key_pem).map_err(|e| anyhow!("CA key: {e}"))?;
        let params = CertificateParams::from_ca_cert_pem(cert_pem)
            .map_err(|e| anyhow!("CA certificate: {e}"))?;
        let permitted_dns = params
            .name_constraints
            .as_ref()
            .map(|constraints| {
                constraints
                    .permitted_subtrees
                    .iter()
                    .filter_map(|subtree| match subtree {
                        GeneralSubtree::DnsName(name) => Some(name.to_ascii_lowercase()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Only the issuer's name and key identifier go into what it signs, and
        // both are the ones of the certificate on disk.
        let issuer = params
            .self_signed(&key)
            .map_err(|e| anyhow!("CA certificate: {e}"))?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .map_err(|e| anyhow!("CA certificate PEM: {e}"))?;
        Ok(Self {
            key,
            issuer,
            cert_pem: cert_pem.to_string(),
            cert_der: pem.contents,
            permitted_dns,
        })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// SHA-256 of the certificate, as browsers and `openssl x509 -fingerprint`
    /// print it: uppercase hex pairs separated by colons.
    pub fn fingerprint_sha256(&self) -> String {
        colon_hex(&Sha256::digest(&self.cert_der))
    }

    /// Whether the CA may sign for a DNS name: the name itself or one under it.
    pub fn permits(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        self.permitted_dns
            .iter()
            .any(|base| name == *base || name.ends_with(&format!(".{base}")))
    }

    /// The certificate the browser receives: the persisted web key, signed by
    /// this CA, for `sans` (IP addresses as they are, DNS names the CA permits).
    pub fn issue_web_certificate(&self, web_key: &KeyPair, sans: &[String]) -> Result<NodeCertificate> {
        let mut names: Vec<String> = vec!["localhost".into()];
        names.extend(sans.iter().cloned());
        names.retain(|name| name.parse::<std::net::IpAddr>().is_ok() || self.permits(name));
        names.sort();
        names.dedup();

        let mut params = CertificateParams::new(names)
            .map_err(|e| anyhow!("invalid web certificate parameters: {e}"))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, crate::brand::slug());
        params.distinguished_name = dn;
        let (y, m, d) = ymd(-1);
        params.not_before = rcgen::date_time_ymd(y, m, d);
        let (y, m, d) = ymd(WEB_VALID_DAYS);
        params.not_after = rcgen::date_time_ymd(y, m, d);
        params.serial_number = Some(random_serial());
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(web_key, &self.issuer, &self.key)
            .map_err(|e| anyhow!("could not sign the web certificate: {e}"))?;
        Ok(NodeCertificate {
            cert: cert.der().clone(),
            key: PrivateKeyDer::try_from(web_key.serialize_der())
                .map_err(|e| anyhow!("invalid web certificate key: {e}"))?,
        })
    }
}

/// The web certificate's key, persisted in `dir`.
pub fn load_or_create_web_key_in(dir: &Path) -> Result<KeyPair> {
    let path = dir.join(web_key_filename());
    if !path.exists() {
        // rcgen's default: ECDSA P-256, which every browser verifies.
        let key = KeyPair::generate().map_err(|e| anyhow!("web certificate key: {e}"))?;
        write_new(&path, key.serialize_pem().as_bytes(), true)?;
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    KeyPair::from_pem(&text).map_err(|e| anyhow!("{} is not a valid key: {e}", path.display()))
}

/// The browser certificate of this machine: the CA and the web key of `dir()`.
pub fn browser_certificate(sans: &[String]) -> Result<NodeCertificate> {
    let dir = dir();
    let ca = LocalCa::load_or_create_in(&dir, &host_names())?;
    let key = load_or_create_web_key_in(&dir)?;
    ca.issue_web_certificate(&key, sans)
}

/// Whether `der` is a certificate for this machine's persisted web key.
///
/// This is the check the native windows make before accepting the interface's
/// certificate when the system does not trust the CA. It compares the public
/// key, not the certificate, because the certificate is issued again on every
/// start and the key is not. Any other certificate, a self-signed one from an
/// impostor on the loopback port included, is refused.
pub fn is_local_web_certificate(der: &[u8]) -> bool {
    is_web_certificate_in(&dir(), der)
}

/// The same check for a certificate in PEM, as WebView2 hands it over.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn is_local_web_certificate_pem(pem: &str) -> bool {
    x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map(|(_, pem)| is_local_web_certificate(&pem.contents))
        .unwrap_or(false)
}

/// Whether a URL the native window loads is this machine's own interface:
/// the certificate exception is never granted to another host.
#[cfg_attr(not(any(windows, target_os = "linux", target_os = "macos")), allow(dead_code))]
pub fn local_host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?.to_string()
    } else {
        authority.rsplit_once(':').map_or(authority, |(host, _)| host).to_string()
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1").then_some(host)
}

fn is_web_certificate_in(dir: &Path, der: &[u8]) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(web_key_filename())) else {
        return false;
    };
    let Ok(key) = KeyPair::from_pem(&text) else {
        return false;
    };
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return false;
    };
    cert.public_key().raw == key.public_key_der().as_slice()
}

/// The DNS names a new CA may sign for: `localhost`, this machine's host name
/// and the names in `<SLUG>_TLS_NAMES` (comma separated), for a node reached by
/// a name its container does not know (`ksia` for a node in Docker, say).
pub fn host_names() -> Vec<String> {
    let mut names = vec!["localhost".to_string()];
    if let Ok(output) = std::process::Command::new("hostname").output() {
        names.push(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    names.extend(extra_names());
    normalize_names(names)
}

/// The names in `<SLUG>_TLS_NAMES`, which also go into the web certificate.
pub fn extra_names() -> Vec<String> {
    crate::brand::env("TLS_NAMES")
        .map(|value| {
            value
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_names(names: Vec<String>) -> Vec<String> {
    let mut names: Vec<String> = names
        .into_iter()
        .map(|name| name.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|name| !name.is_empty() && name.parse::<std::net::IpAddr>().is_err())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn create_ca(names: &[String]) -> Result<(KeyPair, String)> {
    let key = KeyPair::generate().map_err(|e| anyhow!("CA key: {e}"))?;
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    let tag = hex::encode(rand::random::<[u8; 4]>());
    let host = names.iter().find(|name| *name != "localhost").cloned().unwrap_or_default();
    dn.push(DnType::OrganizationName, crate::brand::name());
    dn.push(
        DnType::CommonName,
        format!("{} local CA {host} {tag}", crate::brand::name()).replace("  ", " "),
    );
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut permitted = normalize_names(names.to_vec());
    if !permitted.iter().any(|name| name == "localhost") {
        permitted.push("localhost".into());
    }
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: permitted.into_iter().map(GeneralSubtree::DnsName).collect(),
        excluded_subtrees: vec![],
    });
    let (y, m, d) = ymd(-1);
    params.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(CA_VALID_DAYS);
    params.not_after = rcgen::date_time_ymd(y, m, d);
    params.serial_number = Some(random_serial());
    let cert = params.self_signed(&key).map_err(|e| anyhow!("could not sign the CA: {e}"))?;
    Ok((key, cert.pem()))
}

/// `days` from today, as the year, month and day `rcgen::date_time_ymd` takes.
fn ymd(days: i64) -> (i32, u8, u8) {
    use chrono::Datelike;
    let at = chrono::Utc::now() + chrono::Duration::days(days);
    (at.year(), at.month() as u8, at.day() as u8)
}

/// A fresh serial for every certificate: Firefox refuses two certificates of
/// the same issuer with the same serial.
fn random_serial() -> SerialNumber {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[0] &= 0x7f;
    SerialNumber::from_slice(&bytes)
}

fn colon_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Writes `path` only if it does not exist yet. Two processes starting together
/// (the window and an already running node) must agree on one key, so the file
/// is written aside and linked into place, which fails when it is already there.
fn write_new(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents).with_context(|| format!("could not write {}", tmp.display()))?;
    if private {
        restrict(&tmp);
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) if !path.exists() => {
            // A file system without hard links: plain write, last one wins.
            std::fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))?;
            if private {
                restrict(path);
            }
            Ok(())
        }
        Err(_) => Ok(()),
    }
}

fn write_replace(path: &Path, contents: &[u8]) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))
}

/// The owner only, like the node key. On Windows the ACL inherited from the
/// user's folder already keeps other accounts out (see `identity.rs`).
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["localhost".into(), "machine-a".into()]
    }

    #[test]
    fn the_ca_and_the_web_key_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        let again = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        assert_eq!(ca.cert_der(), again.cert_der());

        let key = load_or_create_web_key_in(dir.path()).unwrap();
        let again = load_or_create_web_key_in(dir.path()).unwrap();
        assert_eq!(key.public_key_der(), again.public_key_der());

        let public = std::fs::read_to_string(dir.path().join(ca_cert_filename())).unwrap();
        assert_eq!(public, ca.cert_pem());
        assert!(!public.contains("PRIVATE KEY"));
    }

    #[test]
    fn the_web_certificate_chains_to_the_ca_and_keeps_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        let key = load_or_create_web_key_in(dir.path()).unwrap();
        let sans = vec!["127.0.0.1".to_string(), "machine-a".to_string()];
        let first = ca.issue_web_certificate(&key, &sans).unwrap();
        let second = ca.issue_web_certificate(&key, &sans).unwrap();
        assert_ne!(first.cert.as_ref(), second.cert.as_ref(), "a new serial every time");
        assert!(is_web_certificate_in(dir.path(), first.cert.as_ref()));
        assert!(is_web_certificate_in(dir.path(), second.cert.as_ref()));

        let (_, leaf) = x509_parser::parse_x509_certificate(first.cert.as_ref()).unwrap();
        let (_, root) = x509_parser::parse_x509_certificate(ca.cert_der()).unwrap();
        assert_eq!(leaf.issuer(), root.subject());
        leaf.verify_signature(Some(root.public_key())).unwrap();
    }

    #[test]
    fn a_name_the_ca_may_not_sign_stays_out_of_the_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        assert!(ca.permits("machine-a"));
        assert!(ca.permits("api.machine-a"));
        assert!(ca.permits("LOCALHOST"));
        assert!(!ca.permits("example.com"));
        assert!(!ca.permits("evil-machine-a"));

        let key = load_or_create_web_key_in(dir.path()).unwrap();
        let sans = vec!["example.com".to_string(), "10.0.0.7".to_string()];
        let cert = ca.issue_web_certificate(&key, &sans).unwrap();
        let (_, leaf) = x509_parser::parse_x509_certificate(cert.cert.as_ref()).unwrap();
        let san = leaf.subject_alternative_name().unwrap().unwrap().value;
        let rendered = format!("{:?}", san.general_names);
        assert!(!rendered.contains("example.com"));
        assert!(rendered.contains("localhost"));
    }

    #[test]
    fn another_key_is_not_this_machines_web_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        load_or_create_web_key_in(dir.path()).unwrap();
        let impostor = KeyPair::generate().unwrap();
        let cert = ca
            .issue_web_certificate(&impostor, &["127.0.0.1".to_string()])
            .unwrap();
        assert!(!is_web_certificate_in(dir.path(), cert.cert.as_ref()));
        assert!(!is_web_certificate_in(dir.path(), b"not a certificate"));
    }

    #[test]
    fn only_this_machines_own_address_gets_the_exception() {
        assert_eq!(local_host_of("https://127.0.0.1:8080/admin-ui/chat").as_deref(), Some("127.0.0.1"));
        assert_eq!(local_host_of("https://localhost:8080/").as_deref(), Some("localhost"));
        assert_eq!(local_host_of("https://[::1]:8080/x").as_deref(), Some("::1"));
        assert_eq!(local_host_of("https://192.168.0.2:8080/"), None);
        assert_eq!(local_host_of("https://127.0.0.1.evil.example/"), None);
        assert_eq!(local_host_of("http://127.0.0.1:8080/"), None);
    }

    /// What a browser or a harness does: trust the CA and verify the server's
    /// certificate for the name it dialed, name constraints included.
    #[test]
    fn a_client_that_trusts_the_ca_accepts_this_machine_and_nothing_else() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        crate::tls::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        let key = load_or_create_web_key_in(dir.path()).unwrap();
        let sans = vec!["127.0.0.1".to_string(), "machine-a".to_string(), "example.com".to_string()];
        let leaf = ca.issue_web_certificate(&key, &sans).unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(ca.cert_der().to_vec())).unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder(std::sync::Arc::new(roots))
            .build()
            .unwrap();
        let verify = |name: &str| {
            verifier.verify_server_cert(
                &leaf.cert,
                &[],
                &ServerName::try_from(name.to_string()).unwrap(),
                &[],
                UnixTime::now(),
            )
        };
        verify("127.0.0.1").unwrap();
        verify("localhost").unwrap();
        verify("machine-a").unwrap();
        assert!(verify("example.com").is_err());
    }

    #[test]
    fn the_ca_is_a_constrained_root() {
        let dir = tempfile::tempdir().unwrap();
        let ca = LocalCa::load_or_create_in(dir.path(), &names()).unwrap();
        let (_, root) = x509_parser::parse_x509_certificate(ca.cert_der()).unwrap();
        assert!(root.is_ca());
        let constraints = root.name_constraints().unwrap().unwrap().value;
        assert!(constraints.permitted_subtrees.is_some());
        assert_eq!(ca.fingerprint_sha256().len(), 32 * 3 - 1);
    }
}
