//! Federated discovery records for public networks.
//!
//! Replicas are deliberately untrusted transport. A publisher signs the
//! complete public-network descriptor and pays a small proof-of-work cost.
//! Every node can query several replicas, merge the signed records locally,
//! and publish the records it knows to the same set. This gives the federation
//! eventual convergence without making a POM-operated service a trust root.

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const PUBLIC_DIRECTORY_VERSION: u32 = 3;
pub(crate) const PUBLIC_DIRECTORY_POW_BITS: u8 = 16;
pub(crate) const PUBLIC_DIRECTORY_MAX_RECORD_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub(crate) const PUBLIC_DIRECTORY_CLOCK_SKEW_SECS: i64 = 5 * 60;
pub(crate) const PUBLIC_DIRECTORY_MAX_RECORDS: usize = 1024;
pub(crate) const PUBLIC_DIRECTORY_MAX_REPLICAS: usize = 16;
pub(crate) const PUBLIC_DIRECTORY_RECORD_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
pub(crate) const PUBLIC_DIRECTORY_ENV: &str = "POM_PUBLIC_DIRECTORY";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PublicDirectorySettings {
    pub replicas: Vec<String>,
}

impl PublicDirectorySettings {
    pub(crate) fn from_urls<I, S>(urls: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut replicas = Vec::new();
        let mut seen = HashSet::new();
        for raw in urls {
            let raw = raw.as_ref().trim();
            if raw.is_empty() {
                continue;
            }
            let parsed = url::Url::parse(raw)
                .with_context(|| format!("ingresso do diretório inválido: {raw}"))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                bail!("ingresso do diretório deve ser uma URL HTTP(S) sem credenciais ou query");
            }
            let normalized = raw.trim_end_matches('/').to_string();
            if seen.insert(normalized.clone()) {
                replicas.push(normalized);
            }
        }
        if replicas.len() > PUBLIC_DIRECTORY_MAX_REPLICAS {
            bail!(
                "o diretório público aceita no máximo {} ingressos",
                PUBLIC_DIRECTORY_MAX_REPLICAS
            );
        }
        Ok(Self { replicas })
    }

    pub(crate) fn from_csv(value: &str) -> Result<Self> {
        Self::from_urls(value.split(',').map(str::trim))
    }

    pub(crate) fn effective_with_env(self) -> Result<Self> {
        let Some(value) = std::env::var_os(PUBLIC_DIRECTORY_ENV) else {
            return Ok(self);
        };
        let env = Self::from_csv(&value.to_string_lossy())?;
        let mut urls = self.replicas;
        urls.extend(env.replicas);
        Self::from_urls(urls.drain(..))
    }
}

/// The signed part of a directory record. `discovered_via` is intentionally
/// absent: it is a local UI annotation and must not affect the network
/// identity or its signature.
///
/// There is no address here. The network is found by the publisher's key
/// (`publisher_pubkey`, the same as the principal's and the pkarr record's)
/// and by the relay keys in `relay_hints`; nobody's IP is ever listed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PublicDirectoryNetwork {
    pub cluster_id: String,
    pub name: String,
    pub fingerprint: String,
    pub trust_document: String,
    pub organization: String,
    pub description: String,
    pub rules: String,
    pub channels: String,
    pub country_code: String,
    pub country_name: String,
    pub city: String,
    pub geo_label: String,
    pub access_policy: crate::config::NetworkAccessPolicy,
    pub location: Option<crate::config::NetworkLocation>,
    #[serde(default)]
    pub relay_hints: Vec<String>,
    #[serde(default)]
    pub directory_replicas: Vec<String>,
}

/// Cap on relays in a public record.
pub(crate) const PUBLIC_DIRECTORY_MAX_RELAY_HINTS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SignedPublicDirectoryRecord {
    pub version: u32,
    pub publisher_pubkey: String,
    pub sequence: u64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub network: PublicDirectoryNetwork,
    pub pow_nonce: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    Inserted,
    Replaced,
    Duplicate,
    Stale,
}

impl SignedPublicDirectoryRecord {
    pub(crate) fn sign(
        signing_key: &SigningKey,
        sequence: u64,
        issued_at: i64,
        network: PublicDirectoryNetwork,
    ) -> Result<Self> {
        let mut record = Self {
            version: PUBLIC_DIRECTORY_VERSION,
            publisher_pubkey: hex::encode(signing_key.verifying_key().as_bytes()),
            sequence,
            issued_at,
            expires_at: issued_at + PUBLIC_DIRECTORY_MAX_RECORD_TTL_SECS,
            network,
            pow_nonce: 0,
            signature: String::new(),
        };
        record.validate_unsigned()?;
        while !record.has_valid_pow() {
            record.pow_nonce = record
                .pow_nonce
                .checked_add(1)
                .context("prova de trabalho esgotou o nonce")?;
        }
        record.signature = hex::encode(signing_key.sign(&record.signing_bytes()).to_bytes());
        Ok(record)
    }

    pub(crate) fn validate(&self, now: i64) -> Result<()> {
        self.validate_unsigned()?;
        if self.issued_at > now + PUBLIC_DIRECTORY_CLOCK_SKEW_SECS {
            bail!("registro do diretório está no futuro");
        }
        if self.expires_at <= now {
            bail!("registro do diretório expirou");
        }
        if now.saturating_sub(self.issued_at) > PUBLIC_DIRECTORY_RECORD_MAX_AGE_SECS {
            bail!("registro do diretório é antigo demais");
        }
        if !self.has_valid_pow() {
            bail!("prova de trabalho do diretório inválida");
        }
        let signature_bytes: [u8; 64] = hex::decode(&self.signature)
            .context("assinatura do diretório não é hexadecimal")?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("assinatura do diretório tem tamanho inválido"))?;
        let signature = Signature::from_bytes(&signature_bytes);
        let public_key_bytes: [u8; 32] = hex::decode(&self.publisher_pubkey)
            .context("publicador do diretório não é hexadecimal")?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("publicador do diretório tem tamanho inválido"))?;
        let public_key = VerifyingKey::from_bytes(&public_key_bytes)
            .context("publicador do diretório é inválido")?;
        public_key
            .verify(&self.signing_bytes(), &signature)
            .context("assinatura do diretório não confere")?;
        Ok(())
    }

    fn validate_unsigned(&self) -> Result<()> {
        if self.version != PUBLIC_DIRECTORY_VERSION {
            bail!("versão do diretório público não suportada");
        }
        if self.publisher_pubkey.len() != 64
            || self.sequence == 0
            || self.issued_at <= 0
            || self.expires_at <= self.issued_at
            || self.expires_at - self.issued_at > PUBLIC_DIRECTORY_MAX_RECORD_TTL_SECS
        {
            bail!("metadados temporais do diretório são inválidos");
        }
        validate_network(&self.network)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Unsigned<'a> {
            version: u32,
            publisher_pubkey: &'a str,
            sequence: u64,
            issued_at: i64,
            expires_at: i64,
            network: &'a PublicDirectoryNetwork,
            pow_nonce: u64,
        }
        serde_json::to_vec(&Unsigned {
            version: self.version,
            publisher_pubkey: &self.publisher_pubkey,
            sequence: self.sequence,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            network: &self.network,
            pow_nonce: self.pow_nonce,
        })
        .expect("registro do diretório só contém tipos serializáveis")
    }

    fn has_valid_pow(&self) -> bool {
        has_leading_zero_bits(
            &Sha256::digest(self.signing_bytes()),
            PUBLIC_DIRECTORY_POW_BITS,
        )
    }

    pub(crate) fn record_hash(&self) -> String {
        hex::encode(Sha256::digest(self.signing_bytes()))
    }
}

fn validate_network(network: &PublicDirectoryNetwork) -> Result<()> {
    for (label, value) in [
        ("cluster_id", &network.cluster_id),
        ("name", &network.name),
        ("fingerprint", &network.fingerprint),
        ("trust_document", &network.trust_document),
    ] {
        if value.trim().is_empty() {
            bail!("campo público obrigatório ausente: {label}");
        }
        if value.len() > 64 * 1024 {
            bail!("campo público grande demais: {label}");
        }
    }
    if network.directory_replicas.len() > PUBLIC_DIRECTORY_MAX_REPLICAS {
        bail!("o registro anuncia ingressos demais");
    }
    PublicDirectorySettings::from_urls(network.directory_replicas.iter())?;
    if network.relay_hints.len() > PUBLIC_DIRECTORY_MAX_RELAY_HINTS {
        bail!("the record announces too many relays");
    }
    for relay in &network.relay_hints {
        if !relay.starts_with("pk:") || crate::pkarr_node::peer_id_from_uri(relay).is_none() {
            bail!("a public record relay must be a pk: key, never an address");
        }
    }
    Ok(())
}

fn has_leading_zero_bits(digest: &[u8], bits: u8) -> bool {
    let full_bytes = usize::from(bits / 8);
    let remaining_bits = bits % 8;
    digest.iter().take(full_bytes).all(|byte| *byte == 0)
        && (remaining_bits == 0
            || digest
                .get(full_bytes)
                .is_some_and(|byte| *byte >> (8 - remaining_bits) == 0))
}

pub(crate) fn merge_record(
    records: &mut Vec<SignedPublicDirectoryRecord>,
    incoming: SignedPublicDirectoryRecord,
    now: i64,
) -> Result<MergeOutcome> {
    incoming.validate(now)?;
    prune_records(records, now);
    if let Some(index) = records.iter().position(|record| {
        record.publisher_pubkey == incoming.publisher_pubkey && record.sequence == incoming.sequence
    }) {
        if records[index].record_hash() == incoming.record_hash() {
            return Ok(MergeOutcome::Duplicate);
        }
        if incoming.record_hash() < records[index].record_hash() {
            records[index] = incoming;
            return Ok(MergeOutcome::Replaced);
        }
        return Ok(MergeOutcome::Stale);
    }
    if records.iter().any(|record| {
        record.publisher_pubkey == incoming.publisher_pubkey && record.sequence > incoming.sequence
    }) {
        return Ok(MergeOutcome::Stale);
    }
    records.retain(|record| {
        record.publisher_pubkey != incoming.publisher_pubkey || record.sequence >= incoming.sequence
    });
    if records.len() >= PUBLIC_DIRECTORY_MAX_RECORDS {
        bail!("o cache do diretório atingiu o limite de registros");
    }
    records.push(incoming);
    Ok(MergeOutcome::Inserted)
}

pub(crate) fn prune_records(records: &mut Vec<SignedPublicDirectoryRecord>, now: i64) {
    records.retain(|record| {
        record.expires_at > now
            && now.saturating_sub(record.issued_at) <= PUBLIC_DIRECTORY_RECORD_MAX_AGE_SECS
    });
}

pub(crate) fn current_time_secs() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("relógio local anterior ao Unix epoch")?
        .as_secs()
        .try_into()
        .context("relógio local fora do intervalo suportado")?)
}

pub(crate) fn next_sequence() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("relógio local anterior ao Unix epoch")?
        .as_millis()
        .try_into()
        .context("sequência do diretório fora do intervalo suportado")?)
}

#[cfg(test)]
impl PublicDirectoryNetwork {
    fn test_fixture() -> Self {
        Self {
            cluster_id: "cluster-test".to_string(),
            name: "Rede pública de teste".to_string(),
            fingerprint: "a".repeat(64),
            trust_document: "{}".to_string(),
            organization: "POM".to_string(),
            description: String::new(),
            rules: String::new(),
            channels: String::new(),
            country_code: "BR".to_string(),
            country_name: "Brasil".to_string(),
            city: "Curitiba".to_string(),
            geo_label: "br.curitiba".to_string(),
            access_policy: crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
            location: None,
            relay_hints: Vec::new(),
            directory_replicas: vec!["https://directory.example".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn signed_record_requires_valid_signature_and_proof_of_work() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let network = PublicDirectoryNetwork::test_fixture();
        let record = SignedPublicDirectoryRecord::sign(&signing_key, 1, 1_700_000_000, network)
            .expect("fixture should be signable");

        record
            .validate(1_700_000_001)
            .expect("record should validate");
        let mut tampered = record.clone();
        tampered.network.name.push_str(" adulterada");
        assert!(tampered.validate(1_700_000_001).is_err());
    }

    /// A public record names relays by key. An address does not pass.
    #[test]
    fn relay_hints_must_be_keys_not_addresses() {
        let mut network = PublicDirectoryNetwork::test_fixture();
        network.relay_hints =
            vec!["pk:yg4gxe7z1r7mr6orids9fh95y7gxhdsxjqi6nngsxxtakqaxr5no".to_string()];
        assert!(validate_network(&network).is_ok());
        network.relay_hints = vec!["198.51.100.1:42777".to_string()];
        assert!(validate_network(&network).is_err());
    }

    #[test]
    fn merge_keeps_the_highest_sequence_and_rejects_stale_records() {
        let signing_key = SigningKey::from_bytes(&[8; 32]);
        let older = SignedPublicDirectoryRecord::sign(
            &signing_key,
            1,
            1_700_000_000,
            PublicDirectoryNetwork::test_fixture(),
        )
        .expect("older fixture should be signable");
        let newer = SignedPublicDirectoryRecord::sign(
            &signing_key,
            2,
            1_700_000_001,
            PublicDirectoryNetwork::test_fixture(),
        )
        .expect("newer fixture should be signable");
        let mut records = vec![newer.clone()];

        assert_eq!(
            merge_record(&mut records, older, 1_700_000_002).expect("merge should work"),
            MergeOutcome::Stale
        );
        assert_eq!(records, vec![newer]);
    }

    #[test]
    fn merge_removes_older_versions_of_the_same_publisher() {
        let signing_key = SigningKey::from_bytes(&[9; 32]);
        let older = SignedPublicDirectoryRecord::sign(
            &signing_key,
            1,
            1_700_000_000,
            PublicDirectoryNetwork::test_fixture(),
        )
        .expect("older fixture should be signable");
        let newer = SignedPublicDirectoryRecord::sign(
            &signing_key,
            2,
            1_700_000_001,
            PublicDirectoryNetwork::test_fixture(),
        )
        .expect("newer fixture should be signable");
        let mut records = vec![older];

        assert_eq!(
            merge_record(&mut records, newer.clone(), 1_700_000_002)
                .expect("new version should merge"),
            MergeOutcome::Inserted
        );
        assert_eq!(records, vec![newer]);
    }

    #[test]
    fn bootstrap_urls_are_normalized_and_bounded() {
        let settings = PublicDirectorySettings::from_urls([
            " https://directory-a.example/ ",
            "https://directory-a.example",
            "http://directory-b.example/base",
        ])
        .expect("bootstrap URLs should be accepted");

        assert_eq!(
            settings.replicas,
            vec![
                "https://directory-a.example",
                "http://directory-b.example/base",
            ]
        );
    }
}
