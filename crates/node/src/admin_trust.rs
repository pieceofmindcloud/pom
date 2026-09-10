//! The signed, durable authority chain for administrator capabilities.
//!
//! `Config.profile` is deliberately not consulted as a root of trust.  This
//! module is the only place that turns a libp2p Ed25519 identity into an
//! administrator capability.  The file is intentionally separate from
//! `pom.json`: editing the product profile cannot manufacture a grant.

use crate::profile::NodeProfile;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub(crate) const TRUST_VERSION: u32 = 1;
pub(crate) const CONTROL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AdminLevel {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AuthorityPrincipal {
    pub(crate) node_id: String,
    pub(crate) peer_id: String,
    pub(crate) pubkey: String,
}

/// Envelope used on the control plane when a network does not have a shared
/// secret. It authenticates the operation metadata and the exact JSON body
/// hash. Inference frames deliberately do not use this envelope: their
/// transport is authenticated separately and signing every tensor frame would
/// put control-plane policy in the hot path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ControlEnvelope {
    pub(crate) version: u32,
    pub(crate) command_id: String,
    pub(crate) nonce: String,
    pub(crate) cluster_id: String,
    pub(crate) authority_epoch: u64,
    pub(crate) issuer: AuthorityPrincipal,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body_hash: String,
    pub(crate) signature: String,
}

impl AuthorityPrincipal {
    pub(crate) fn from_signing_key(
        node_id: impl Into<String>,
        peer_id: impl Into<String>,
        signing_key: &SigningKey,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            peer_id: peer_id.into(),
            pubkey: hex::encode(signing_key.verifying_key().as_bytes()),
        }
    }

    fn verifying_key(&self) -> Result<VerifyingKey> {
        let bytes: [u8; 32] = hex::decode(&self.pubkey)
            .context("chave pública de autoridade não é hexadecimal")?
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("chave pública de autoridade tem tamanho inválido"))?;
        VerifyingKey::from_bytes(&bytes).context("chave pública de autoridade inválida")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TrustAnchor {
    pub(crate) cluster_id: String,
    pub(crate) principal: AuthorityPrincipal,
    pub(crate) fingerprint: String,
}

impl TrustAnchor {
    pub(crate) fn manual(
        cluster_id: impl Into<String>,
        principal: AuthorityPrincipal,
        fingerprint: impl Into<String>,
    ) -> Result<Self> {
        let anchor = Self {
            cluster_id: cluster_id.into(),
            principal,
            fingerprint: fingerprint.into(),
        };
        if anchor.validates() {
            Ok(anchor)
        } else {
            bail!("a impressão digital manual não corresponde à chave ancorada")
        }
    }

    pub(crate) fn for_principal(
        cluster_id: impl Into<String>,
        principal: AuthorityPrincipal,
    ) -> Self {
        let fingerprint = fingerprint_of(&principal.pubkey);
        Self {
            cluster_id: cluster_id.into(),
            principal,
            fingerprint,
        }
    }

    fn validates(&self) -> bool {
        self.fingerprint == fingerprint_of(&self.principal.pubkey)
            && self.cluster_id != ""
            && self.principal.verifying_key().is_ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AdminGrant {
    pub(crate) version: u32,
    pub(crate) grant_id: String,
    pub(crate) cluster_id: String,
    pub(crate) issuer: AuthorityPrincipal,
    pub(crate) subject: AuthorityPrincipal,
    pub(crate) profile: NodeProfile,
    pub(crate) level: AdminLevel,
    pub(crate) authority_epoch: u64,
    pub(crate) promotion_seq: u64,
    pub(crate) previous_grant_hash: Option<String>,
    pub(crate) command_id: String,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AdminTransferOffer {
    pub(crate) version: u32,
    pub(crate) offer_id: String,
    pub(crate) cluster_id: String,
    pub(crate) issuer: AuthorityPrincipal,
    pub(crate) successor: AuthorityPrincipal,
    pub(crate) successor_grant_hash: String,
    pub(crate) authority_epoch: u64,
    pub(crate) promotion_seq: u64,
    pub(crate) command_id: String,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AdminTransferAcceptance {
    pub(crate) version: u32,
    pub(crate) offer_id: String,
    pub(crate) cluster_id: String,
    pub(crate) successor: AuthorityPrincipal,
    pub(crate) authority_epoch: u64,
    pub(crate) command_id: String,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NodePromotionCommand {
    pub(crate) version: u32,
    pub(crate) command_id: String,
    pub(crate) cluster_id: String,
    pub(crate) issuer: AuthorityPrincipal,
    pub(crate) subject: AuthorityPrincipal,
    pub(crate) authority_epoch: u64,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NodePromotionRequest {
    pub(crate) version: u32,
    pub(crate) command_id: String,
    pub(crate) cluster_id: String,
    pub(crate) subject: AuthorityPrincipal,
    pub(crate) authority_epoch: u64,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CompletedTransfer {
    offer: AdminTransferOffer,
    acceptance: AdminTransferAcceptance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReplayRecord {
    nonce: String,
    command_id: String,
    offer_id: Option<String>,
    authority_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AdminTrustDocument {
    pub(crate) version: u32,
    pub(crate) anchor: TrustAnchor,
    pub(crate) authority_epoch: u64,
    pub(crate) promotion_seq: u64,
    pub(crate) primary: AuthorityPrincipal,
    pub(crate) primary_history: Vec<AuthorityPrincipal>,
    pub(crate) grants: Vec<AdminGrant>,
    #[serde(default)]
    pub(crate) demotions: Vec<AdminDemotion>,
    pub(crate) pending_transfer: Option<AdminTransferOffer>,
    pub(crate) completed_transfers: Vec<CompletedTransfer>,
    pub(crate) replay: Vec<ReplayRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AdminDemotion {
    pub(crate) version: u32,
    pub(crate) demotion_id: String,
    pub(crate) cluster_id: String,
    pub(crate) issuer: AuthorityPrincipal,
    pub(crate) subject: AuthorityPrincipal,
    pub(crate) target_grant_hash: String,
    pub(crate) authority_epoch: u64,
    pub(crate) promotion_seq: u64,
    pub(crate) command_id: String,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectiveAuthority {
    Primary,
    Secondary,
}

#[derive(Clone)]
pub(crate) struct AdminTrust {
    state: Arc<Mutex<AdminTrustDocument>>,
    path: Option<PathBuf>,
}

impl AdminTrust {
    /// Um nó sem âncora e sem arquivo, para as bancadas de teste. O binário
    /// usa `load_or_empty`, que guarda o caminho.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::empty_at(None)
    }

    /// O mesmo estado sem âncora, mas ciente de onde a âncora vai ficar.
    ///
    /// Um nó por ancorar precisa lembrar o caminho: é ali que a instalação
    /// pelo operador vai escrever, e sem isso `adopt_anchor` mudaria a
    /// autoridade em memória sem deixar nada no disco.
    fn empty_at(path: Option<PathBuf>) -> Self {
        let signing_key = SigningKey::from_bytes(&[0; 32]);
        let principal = AuthorityPrincipal::from_signing_key("", "", &signing_key);
        let anchor = TrustAnchor::for_principal("uninitialized", principal.clone());
        Self {
            state: Arc::new(Mutex::new(AdminTrustDocument {
                version: TRUST_VERSION,
                anchor,
                authority_epoch: 0,
                promotion_seq: 0,
                primary: principal.clone(),
                primary_history: vec![principal],
                grants: Vec::new(),
                demotions: Vec::new(),
                pending_transfer: None,
                completed_transfers: Vec::new(),
                replay: Vec::new(),
            })),
            path,
        }
    }

    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("não foi possível ler {}", path.display()))?;
        let document = parse_transported(&content)
            .with_context(|| format!("concessão administrativa inválida em {}", path.display()))?;
        Ok(Self {
            state: Arc::new(Mutex::new(document)),
            path: Some(path),
        })
    }

    pub(crate) fn load_or_empty(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        match Self::load(&path) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!("cadeia de confiança administrativa indisponível: {error:#}");
                Self::empty_at(Some(path))
            }
        }
    }

    /// Se este nó já tem raiz de confiança. É o mesmo julgamento que decide a
    /// capacidade efetiva de admin, e é o que a interface mostra como
    /// "ancorado" ou "por ancorar".
    pub(crate) fn is_anchored(&self) -> bool {
        validate_document(&self.document()).is_ok()
    }

    /// Identificador do enxame que esta máquina aceitou fora de banda.
    ///
    /// O valor serve apenas para selecionar a cadeia local. A admissão ainda
    /// exige a tupla completa do principal em `admits_participant`.
    pub(crate) fn cluster_id(&self) -> Option<String> {
        let document = self.document();
        validate_document(&document)
            .is_ok()
            .then(|| document.anchor.cluster_id)
    }

    /// Confere uma prova de participação contra a cadeia já validada.
    ///
    /// No formato atual, a cadeia só tem concessões de autoridade primária e
    /// secundária. Elas são também as concessões de participação disponíveis
    /// para a descoberta: o `profile` anunciado nunca substitui esta prova.
    pub(crate) fn admits_participant(
        &self,
        cluster_id: &str,
        subject: &AuthorityPrincipal,
    ) -> bool {
        let document = self.document();
        if validate_document(&document).is_err() || document.anchor.cluster_id != cluster_id {
            return false;
        }
        if subject == &document.primary {
            return true;
        }
        document.grants.iter().any(|grant| {
            grant.subject == *subject
                && grant.level == AdminLevel::Secondary
                && !document.demotions.iter().any(|demotion| {
                    demotion.subject == *subject
                        && demotion.target_grant_hash == grant_hash(grant).unwrap_or_default()
                })
        })
    }

    /// Se já existe arquivo de confiança no disco, mesmo ilegível. Um arquivo
    /// ilegível não é âncora, mas também não é coisa que a interface apague:
    /// a recusa manda o operador para a reancoragem manual na máquina.
    pub(crate) fn anchor_file_exists(&self) -> bool {
        self.path.as_deref().is_some_and(Path::exists)
    }

    /// Adota, num nó ainda sem raiz, a cadeia assinada que o operador
    /// transportou, depois de ele conferir a impressão digital à mão.
    ///
    /// A assinatura só prova que a cadeia é coerente consigo mesma; o arquivo
    /// não se auto-valida. Quem o valida é `typed_fingerprint`, o valor que o
    /// operador trouxe pelo canal dele. Trocar uma âncora existente não é
    /// gesto de interface: é reancoragem manual (`pom admin-anchor
    /// --replace`), por isso um nó já ancorado recusa aqui.
    pub(crate) fn adopt_anchor(
        &self,
        document: AdminTrustDocument,
        typed_fingerprint: &str,
    ) -> Result<()> {
        if self.anchor_file_exists() {
            bail!(
                "este nó já tem um arquivo de confiança administrativa; trocar a raiz é reancoragem manual na máquina"
            );
        }
        let mut current = self.state.lock().expect("admin trust lock poisoned");
        if validate_document(&current).is_ok() {
            bail!("este nó já está ancorado; trocar a raiz é reancoragem manual na máquina");
        }
        validate_document(&document)?;
        let expected = anchor_fingerprint(&document);
        if !fingerprint_matches(typed_fingerprint, &expected) {
            bail!("a impressão digital conferida não corresponde à âncora desta cadeia");
        }
        persist_document(&document, self.path.as_deref())?;
        *current = document;
        Ok(())
    }

    /// Installs a manually transported, already signed chain at the local
    /// trust path. The caller must obtain the document out of band and must
    /// separately compare the anchor fingerprint with the operator's record.
    pub(crate) fn install_document(
        path: impl AsRef<Path>,
        document: AdminTrustDocument,
    ) -> Result<Self> {
        validate_document(&document)?;
        let trust = Self {
            state: Arc::new(Mutex::new(document)),
            path: Some(path.as_ref().to_path_buf()),
        };
        trust.save()?;
        Ok(trust)
    }

    #[cfg(test)]
    pub(crate) fn in_memory(document: AdminTrustDocument) -> Result<Self> {
        validate_document(&document)?;
        Ok(Self {
            state: Arc::new(Mutex::new(document)),
            path: None,
        })
    }

    pub(crate) fn document(&self) -> AdminTrustDocument {
        self.state
            .lock()
            .expect("admin trust lock poisoned")
            .clone()
    }

    /// Consume a signed control command before its handler mutates anything.
    /// The nonce and command id are durable so retrying a captured envelope
    /// cannot replay a control action after a process restart.
    pub(crate) fn consume_control_replay(&self, envelope: &ControlEnvelope) -> Result<()> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.replay.iter().any(|record| {
            record.nonce == envelope.nonce || record.command_id == envelope.command_id
        }) {
            bail!("comando de controle já foi processado");
        }
        document.replay.push(ReplayRecord {
            nonce: envelope.nonce.clone(),
            command_id: envelope.command_id.clone(),
            offer_id: None,
            authority_epoch: envelope.authority_epoch,
        });
        const MAX_CONTROL_REPLAYS: usize = 4096;
        if document.replay.len() > MAX_CONTROL_REPLAYS {
            let excess = document.replay.len() - MAX_CONTROL_REPLAYS;
            document.replay.drain(..excess);
        }
        persist_document(&document, self.path.as_deref())
    }

    pub(crate) fn bootstrap_primary_with_anchor(
        path: impl AsRef<Path>,
        anchor: TrustAnchor,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        let anchor = TrustAnchor::manual(
            anchor.cluster_id.clone(),
            anchor.principal.clone(),
            anchor.fingerprint.clone(),
        )?;
        let principal = anchor.principal.clone();
        if principal.verifying_key()? != signing_key.verifying_key() {
            bail!("a chave de bootstrap não corresponde ao principal ancorado");
        }
        let mut grant = AdminGrant {
            version: TRUST_VERSION,
            grant_id: Uuid::new_v4().simple().to_string(),
            cluster_id: anchor.cluster_id.clone(),
            issuer: principal.clone(),
            subject: principal.clone(),
            profile: NodeProfile::Admin,
            level: AdminLevel::Primary,
            authority_epoch: 0,
            promotion_seq: 0,
            previous_grant_hash: None,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        grant.signature = sign_bytes(signing_key, &grant_message(&grant));
        let document = AdminTrustDocument {
            version: TRUST_VERSION,
            anchor,
            authority_epoch: 0,
            promotion_seq: 0,
            primary: principal.clone(),
            primary_history: vec![principal],
            grants: vec![grant],
            demotions: Vec::new(),
            pending_transfer: None,
            completed_transfers: Vec::new(),
            replay: Vec::new(),
        };
        validate_document(&document)?;
        let trust = Self {
            state: Arc::new(Mutex::new(document)),
            path: Some(path.as_ref().to_path_buf()),
        };
        trust.save()?;
        Ok(trust)
    }

    #[cfg(test)]
    pub(crate) fn bootstrap_primary(
        path: impl AsRef<Path>,
        cluster_id: impl Into<String>,
        principal: AuthorityPrincipal,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        let anchor = TrustAnchor::for_principal(cluster_id, principal);
        Self::bootstrap_primary_with_anchor(path, anchor, signing_key)
    }

    #[cfg(test)]
    pub(crate) fn bootstrap_primary_in_memory(
        cluster_id: impl Into<String>,
        principal: AuthorityPrincipal,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        if principal.verifying_key()? != signing_key.verifying_key() {
            bail!("a chave de bootstrap não corresponde ao principal ancorado");
        }
        let anchor = TrustAnchor::for_principal(cluster_id, principal.clone());
        let mut grant = AdminGrant {
            version: TRUST_VERSION,
            grant_id: Uuid::new_v4().simple().to_string(),
            cluster_id: anchor.cluster_id.clone(),
            issuer: principal.clone(),
            subject: principal.clone(),
            profile: NodeProfile::Admin,
            level: AdminLevel::Primary,
            authority_epoch: 0,
            promotion_seq: 0,
            previous_grant_hash: None,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        grant.signature = sign_bytes(signing_key, &grant_message(&grant));
        let document = AdminTrustDocument {
            version: TRUST_VERSION,
            anchor,
            authority_epoch: 0,
            promotion_seq: 0,
            primary: principal.clone(),
            primary_history: vec![principal],
            grants: vec![grant],
            demotions: Vec::new(),
            pending_transfer: None,
            completed_transfers: Vec::new(),
            replay: Vec::new(),
        };
        Self::in_memory(document)
    }

    pub(crate) fn effective_authority(
        &self,
        subject: &AuthorityPrincipal,
        profile: NodeProfile,
    ) -> Option<EffectiveAuthority> {
        if profile != NodeProfile::Admin {
            return None;
        }
        let document = self.document();
        if validate_document(&document).is_err() {
            return None;
        }
        if subject == &document.primary {
            return Some(EffectiveAuthority::Primary);
        }
        document
            .grants
            .iter()
            .any(|grant| {
                grant.subject == *subject
                    && grant.level == AdminLevel::Secondary
                    && !document.demotions.iter().any(|demotion| {
                        demotion.subject == *subject
                            && demotion.target_grant_hash == grant_hash(grant).unwrap_or_default()
                    })
            })
            .then_some(EffectiveAuthority::Secondary)
    }

    pub(crate) fn current_primary(&self) -> AuthorityPrincipal {
        self.document().primary
    }

    pub(crate) fn pending_transfer(&self) -> Option<AdminTransferOffer> {
        self.document().pending_transfer
    }

    pub(crate) fn latest_secondary(&self) -> Option<AuthorityPrincipal> {
        let document = self.document();
        document
            .grants
            .iter()
            .rev()
            .find(|grant| {
                grant.level == AdminLevel::Secondary
                    && grant.subject != document.primary
                    && !document.demotions.iter().any(|demotion| {
                        demotion.subject == grant.subject
                            && demotion.target_grant_hash == grant_hash(grant).unwrap_or_default()
                    })
            })
            .map(|grant| grant.subject.clone())
    }

    pub(crate) fn signed_secondary_grant(
        &self,
        signing_key: &SigningKey,
        subject: AuthorityPrincipal,
        target_node_id: &str,
    ) -> Result<AdminGrant> {
        let document = self.document();
        validate_document(&document)?;
        if document.primary.node_id != target_node_id {
            bail!("somente o principal vigente pode emitir concessões");
        }
        if document.primary.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
            bail!("a chave emissora não é a do principal vigente");
        }
        let previous_grant_hash = document.grants.last().map(grant_hash).transpose()?;
        let mut grant = AdminGrant {
            version: TRUST_VERSION,
            grant_id: Uuid::new_v4().simple().to_string(),
            cluster_id: document.anchor.cluster_id,
            issuer: document.primary,
            subject,
            profile: NodeProfile::Admin,
            level: AdminLevel::Secondary,
            authority_epoch: document.authority_epoch,
            promotion_seq: document.promotion_seq.saturating_add(1),
            previous_grant_hash,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        grant.signature = sign_bytes(signing_key, &grant_message(&grant));
        Ok(grant)
    }

    /// Record a grant on the issuer before it is delivered to the promoted
    /// node. The target later validates this same signed record, so a node
    /// that only edits its own profile cannot create an admin capability.
    pub(crate) fn record_secondary_grant(
        &self,
        grant: AdminGrant,
        signing_key: &SigningKey,
    ) -> Result<()> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.pending_transfer.is_some() {
            bail!("não é possível promover enquanto há transferência aberta");
        }
        if grant.issuer != document.primary
            || grant.issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes())
            || grant.level != AdminLevel::Secondary
            || grant.profile != NodeProfile::Admin
        {
            bail!("concessão não emitida pelo principal vigente");
        }
        verify_grant_signature(&grant)?;
        if grant.authority_epoch != document.authority_epoch
            || grant.previous_grant_hash != document.grants.last().map(grant_hash).transpose()?
            || grant.promotion_seq != document.promotion_seq.saturating_add(1)
        {
            bail!("concessão fora da cadeia vigente");
        }
        consume_replay(&mut document, &grant.nonce, &grant.command_id, None)?;
        document.promotion_seq = grant.promotion_seq;
        document.grants.push(grant);
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn apply_secondary_grant(
        &self,
        grant: AdminGrant,
        local_subject: &AuthorityPrincipal,
        target_profile: NodeProfile,
    ) -> Result<()> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.pending_transfer.is_some() {
            bail!("não é possível aplicar concessão enquanto há transferência aberta");
        }
        if target_profile != NodeProfile::Node {
            bail!("somente um nó pode receber uma concessão de admin");
        }
        if grant.subject != *local_subject {
            bail!("a concessão não é destinada a este peer");
        }
        if grant.issuer != document.primary {
            bail!("somente o principal vigente pode promover um admin");
        }
        if grant.level != AdminLevel::Secondary
            || grant.authority_epoch != document.authority_epoch
            || grant.previous_grant_hash != document.grants.last().map(grant_hash).transpose()?
            || grant.promotion_seq != document.promotion_seq.saturating_add(1)
        {
            bail!("concessão fora da cadeia vigente");
        }
        verify_grant_signature(&grant)?;
        consume_replay(&mut document, &grant.nonce, &grant.command_id, None)?;
        document.promotion_seq = grant.promotion_seq;
        document.grants.push(grant);
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(())
    }

    pub(crate) fn demote_secondary(
        &self,
        signing_key: &SigningKey,
        subject: AuthorityPrincipal,
    ) -> Result<AdminDemotion> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.pending_transfer.is_some() {
            bail!("não é possível despromover enquanto há transferência aberta");
        }
        if document.primary.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
            bail!("somente o principal vigente pode despromover um secundário");
        }
        let grant = document
            .grants
            .iter()
            .rev()
            .find(|grant| {
                grant.subject == subject
                    && grant.subject != document.primary
                    && grant.level == AdminLevel::Secondary
            })
            .ok_or_else(|| anyhow::anyhow!("o alvo não é um admin secundário"))?;
        let target_grant_hash = grant_hash(grant)?;
        if document.demotions.iter().any(|demotion| {
            demotion.subject == subject && demotion.target_grant_hash == target_grant_hash
        }) {
            bail!("o admin secundário já foi despromovido");
        }
        let mut demotion = AdminDemotion {
            version: TRUST_VERSION,
            demotion_id: Uuid::new_v4().simple().to_string(),
            cluster_id: document.anchor.cluster_id.clone(),
            issuer: document.primary.clone(),
            subject,
            target_grant_hash,
            authority_epoch: document.authority_epoch,
            promotion_seq: document.promotion_seq,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        demotion.signature = sign_bytes(signing_key, &demotion_message(&demotion));
        consume_replay(&mut document, &demotion.nonce, &demotion.command_id, None)?;
        document.demotions.push(demotion.clone());
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(demotion)
    }

    pub(crate) fn node_promotion_request(
        &self,
        signing_key: &SigningKey,
        subject: AuthorityPrincipal,
    ) -> Result<NodePromotionRequest> {
        let document = self.document();
        validate_document(&document)?;
        if subject.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
            bail!("o pedido precisa ser assinado pelo nó que o solicita");
        }
        let mut request = NodePromotionRequest {
            version: TRUST_VERSION,
            command_id: Uuid::new_v4().simple().to_string(),
            cluster_id: document.anchor.cluster_id,
            subject,
            authority_epoch: document.authority_epoch,
            nonce: random_nonce(),
            signature: String::new(),
        };
        request.signature = sign_bytes(signing_key, &node_promotion_request_message(&request));
        Ok(request)
    }

    pub(crate) fn authorize_node_promotion_request(
        &self,
        signing_key: &SigningKey,
        request: NodePromotionRequest,
    ) -> Result<NodePromotionCommand> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if request.cluster_id != document.anchor.cluster_id
            || request.authority_epoch != document.authority_epoch
        {
            bail!("pedido de entrada fora da época vigente");
        }
        verify_node_promotion_request_signature(&request)?;
        let issuer = document
            .primary_history
            .iter()
            .find(|principal| {
                principal.pubkey == hex::encode(signing_key.verifying_key().as_bytes())
                    && (document.primary == **principal
                        || document.grants.iter().any(|grant| {
                            grant.subject == **principal
                                && grant.level == AdminLevel::Secondary
                                && !document.demotions.iter().any(|demotion| {
                                    demotion.target_grant_hash
                                        == grant_hash(grant).unwrap_or_default()
                                })
                        }))
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("a chave emissora não é de um admin vigente"))?;
        if document.pending_transfer.is_some() {
            bail!("não é possível autorizar nó durante transferência aberta");
        }
        consume_replay(&mut document, &request.nonce, &request.command_id, None)?;
        let mut command = NodePromotionCommand {
            version: TRUST_VERSION,
            command_id: Uuid::new_v4().simple().to_string(),
            cluster_id: document.anchor.cluster_id.clone(),
            issuer,
            subject: request.subject,
            authority_epoch: document.authority_epoch,
            nonce: random_nonce(),
            signature: String::new(),
        };
        command.signature = sign_bytes(signing_key, &node_promotion_message(&command));
        consume_replay(&mut document, &command.nonce, &command.command_id, None)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(command)
    }

    pub(crate) fn apply_node_promotion(
        &self,
        command: NodePromotionCommand,
        local_subject: &AuthorityPrincipal,
        target_profile: NodeProfile,
    ) -> Result<()> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if target_profile != NodeProfile::Infer {
            bail!("somente um nó infer pode pedir entrada no processamento");
        }
        if command.subject != *local_subject
            || command.cluster_id != document.anchor.cluster_id
            || command.authority_epoch != document.authority_epoch
        {
            bail!("o pedido de entrada não corresponde a este nó e época");
        }
        let issuer_is_current = command.issuer == document.primary
            || document.grants.iter().any(|grant| {
                grant.subject == command.issuer
                    && grant.level == AdminLevel::Secondary
                    && !document.demotions.iter().any(|demotion| {
                        demotion.target_grant_hash == grant_hash(grant).unwrap_or_default()
                    })
            });
        if !issuer_is_current {
            bail!("o pedido não foi autorizado por um admin vigente");
        }
        verify_node_promotion_signature(&command)?;
        consume_replay(&mut document, &command.nonce, &command.command_id, None)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(())
    }

    pub(crate) fn reconcile(&self, incoming: AdminTrustDocument) -> Result<()> {
        validate_document(&incoming)?;
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        if document.anchor != incoming.anchor {
            bail!("a cadeia recebida não corresponde à âncora manual deste nó");
        }
        if incoming.authority_epoch < document.authority_epoch
            || incoming.promotion_seq < document.promotion_seq
        {
            bail!("a cadeia recebida é anterior ao estado local");
        }
        persist_document(&incoming, self.path.as_deref())?;
        *document = incoming;
        Ok(())
    }

    pub(crate) fn begin_transfer(
        &self,
        signing_key: &SigningKey,
        successor: AuthorityPrincipal,
        successor_is_active: bool,
    ) -> Result<AdminTransferOffer> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.primary.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
            bail!("a chave emissora não é a do principal vigente");
        }
        if document.pending_transfer.is_some() {
            bail!("já existe uma oferta de transferência aberta");
        }
        if !successor_is_active {
            bail!("o sucessor não está ativo");
        }
        if successor == document.primary {
            bail!("o sucessor já é o principal");
        }
        let grant = document
            .grants
            .iter()
            .rev()
            .find(|grant| grant.subject == successor && grant.level == AdminLevel::Secondary)
            .ok_or_else(|| anyhow::anyhow!("o sucessor não é o último secundário promovido"))?;
        let mut offer = AdminTransferOffer {
            version: TRUST_VERSION,
            offer_id: Uuid::new_v4().simple().to_string(),
            cluster_id: document.anchor.cluster_id.clone(),
            issuer: document.primary.clone(),
            successor,
            successor_grant_hash: grant_hash(grant)?,
            authority_epoch: document.authority_epoch,
            promotion_seq: document.promotion_seq,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        offer.signature = sign_bytes(signing_key, &offer_message(&offer));
        consume_replay(
            &mut document,
            &offer.nonce,
            &offer.command_id,
            Some(&offer.offer_id),
        )?;
        document.pending_transfer = Some(offer.clone());
        persist_document(&document, self.path.as_deref())?;
        Ok(offer)
    }

    pub(crate) fn accept_transfer(
        &self,
        offer: AdminTransferOffer,
        signing_key: &SigningKey,
        successor: AuthorityPrincipal,
    ) -> Result<AdminTransferAcceptance> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        let pending = document
            .pending_transfer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("não existe oferta de transferência correspondente"))?;
        if pending != &offer
            || offer.successor != successor
            || offer.authority_epoch != document.authority_epoch
        {
            bail!("oferta de transferência não corresponde ao estado vigente");
        }
        verify_offer_signature(&offer)?;
        if successor.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
            bail!("a aceitação não prova a chave do sucessor");
        }
        let mut acceptance = AdminTransferAcceptance {
            version: TRUST_VERSION,
            offer_id: offer.offer_id.clone(),
            cluster_id: offer.cluster_id.clone(),
            successor: successor.clone(),
            authority_epoch: offer.authority_epoch,
            command_id: Uuid::new_v4().simple().to_string(),
            nonce: random_nonce(),
            signature: String::new(),
        };
        acceptance.signature = sign_bytes(signing_key, &acceptance_message(&acceptance));
        consume_replay(
            &mut document,
            &acceptance.nonce,
            &acceptance.command_id,
            Some(&offer.offer_id),
        )?;
        document.authority_epoch = document.authority_epoch.saturating_add(1);
        document.primary = successor.clone();
        document.primary_history.push(successor);
        document.pending_transfer = None;
        document
            .completed_transfers
            .push(CompletedTransfer { offer, acceptance });
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(document
            .completed_transfers
            .last()
            .expect("transferência recém concluída")
            .acceptance
            .clone())
    }

    pub(crate) fn complete_transfer(
        &self,
        offer: AdminTransferOffer,
        acceptance: AdminTransferAcceptance,
    ) -> Result<()> {
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        validate_document(&document)?;
        if document.pending_transfer.as_ref() != Some(&offer)
            || acceptance.offer_id != offer.offer_id
            || acceptance.successor != offer.successor
        {
            bail!("aceitação de transferência não corresponde à oferta aberta");
        }
        verify_offer_signature(&offer)?;
        verify_acceptance_signature(&acceptance)?;
        consume_replay(
            &mut document,
            &acceptance.nonce,
            &acceptance.command_id,
            Some(&acceptance.offer_id),
        )?;
        document.authority_epoch = document.authority_epoch.saturating_add(1);
        document.primary = offer.successor.clone();
        document.primary_history.push(offer.successor.clone());
        document.pending_transfer = None;
        document
            .completed_transfers
            .push(CompletedTransfer { offer, acceptance });
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<()> {
        let document = self.document();
        persist_document(&document, self.path.as_deref())
    }
}

fn random_nonce() -> String {
    hex::encode(uuid::Uuid::new_v4().as_bytes())
}

pub(crate) fn fingerprint_of(pubkey: &str) -> String {
    hex::encode(Sha256::digest(pubkey.as_bytes()))
}

/// A impressão digital que o operador confere: a da chave ancorada, nunca a
/// de quem trouxe o arquivo.
pub(crate) fn anchor_fingerprint(document: &AdminTrustDocument) -> String {
    fingerprint_of(&document.anchor.principal.pubkey)
}

/// Load the local identities used by both the CLI and first-access setup.
/// Keeping this in the trust module makes the browser and `pom admin-anchor`
/// use the same principal construction and prevents the two paths from
/// silently producing different public keys.
pub(crate) fn local_authority_identity() -> Result<(SigningKey, AuthorityPrincipal)> {
    let local_identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
    let pkarr = crate::pkarr_node::PkarrNode::load_or_create(&crate::identity::pkarr_key_path())
        .context("não foi possível carregar a identidade libp2p para a âncora")?;
    let signing_key = SigningKey::from_bytes(&pkarr.secret_key_bytes());
    let mut seed = signing_key.to_bytes();
    let keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..])
        .context("a identidade local não pôde ser convertida em chave libp2p")?;
    let principal = AuthorityPrincipal::from_signing_key(
        local_identity.node_id.0,
        keypair.public().to_peer_id().to_string(),
        &signing_key,
    );
    Ok((signing_key, principal))
}

/// Build the exact document the administrator sees in the setup wizard. The
/// document is returned without writing it, so changing profile or abandoning
/// setup cannot leave a trust file that blocks a later first-access attempt.
pub(crate) fn local_primary_document(
    cluster_id: impl Into<String>,
) -> Result<(AdminTrustDocument, String, String)> {
    let (signing_key, principal) = local_authority_identity()?;
    let anchor = TrustAnchor::for_principal(cluster_id, principal.clone());
    let mut grant = AdminGrant {
        version: TRUST_VERSION,
        grant_id: Uuid::new_v4().simple().to_string(),
        cluster_id: anchor.cluster_id.clone(),
        issuer: principal.clone(),
        subject: principal.clone(),
        profile: NodeProfile::Admin,
        level: AdminLevel::Primary,
        authority_epoch: 0,
        promotion_seq: 0,
        previous_grant_hash: None,
        command_id: Uuid::new_v4().simple().to_string(),
        nonce: random_nonce(),
        signature: String::new(),
    };
    grant.signature = sign_bytes(&signing_key, &grant_message(&grant));
    let document = AdminTrustDocument {
        version: TRUST_VERSION,
        anchor,
        authority_epoch: 0,
        promotion_seq: 0,
        primary: principal.clone(),
        primary_history: vec![principal.clone()],
        grants: vec![grant],
        demotions: Vec::new(),
        pending_transfer: None,
        completed_transfers: Vec::new(),
        replay: Vec::new(),
    };
    validate_document(&document)?;
    let fingerprint = anchor_fingerprint(&document);
    Ok((document, fingerprint, principal.pubkey))
}

/// Check that a document returned by the browser is the preview generated by
/// this node, not another valid administrator chain pasted into the request.
pub(crate) fn validate_local_primary_document(
    document: &AdminTrustDocument,
    cluster_id: &str,
) -> Result<()> {
    validate_document(document)?;
    let (_, principal) = local_authority_identity()?;
    if document.anchor.cluster_id != cluster_id {
        bail!("a raiz pertence a outra rede");
    }
    if document.anchor.principal != principal {
        bail!("a raiz não foi criada por este nó");
    }
    Ok(())
}

/// Lê uma cadeia transportada pelo operador. Decodifica e confere a coerência
/// interna da cadeia; a conferência que ancora a confiança continua sendo a
/// impressão digital, que não vem daqui.
pub(crate) fn parse_transported(content: &str) -> Result<AdminTrustDocument> {
    let document: AdminTrustDocument =
        serde_json::from_str(content).context("o conteúdo não é uma cadeia administrativa")?;
    validate_document(&document)?;
    Ok(document)
}

/// A conferência manual da impressão digital, uma só para o comando e para a
/// interface. Normaliza apenas o que a transcrição humana acrescenta -
/// espaços, dois-pontos e caixa - e nunca o valor em si: continua sendo
/// preciso trazer os 64 dígitos certos.
pub(crate) fn fingerprint_matches(typed: &str, expected: &str) -> bool {
    fn normalize(value: &str) -> String {
        value
            .chars()
            .filter(|glyph| !glyph.is_whitespace() && *glyph != ':')
            .flat_map(char::to_lowercase)
            .collect()
    }
    let typed = normalize(typed);
    !typed.is_empty() && typed == normalize(expected)
}

pub(crate) fn sign_control_envelope(
    signing_key: &SigningKey,
    issuer: AuthorityPrincipal,
    document: &AdminTrustDocument,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<ControlEnvelope> {
    validate_document(document)?;
    if issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
        bail!("a chave do envelope não corresponde ao emissor");
    }
    let mut envelope = ControlEnvelope {
        version: CONTROL_VERSION,
        command_id: Uuid::new_v4().simple().to_string(),
        nonce: random_nonce(),
        cluster_id: document.anchor.cluster_id.clone(),
        authority_epoch: document.authority_epoch,
        issuer,
        method: method.to_ascii_uppercase(),
        path: path.to_string(),
        body_hash: hex::encode(Sha256::digest(body)),
        signature: String::new(),
    };
    envelope.signature = sign_bytes(signing_key, &control_message(&envelope));
    Ok(envelope)
}

pub(crate) fn verify_control_envelope(
    envelope: &ControlEnvelope,
    document: &AdminTrustDocument,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<()> {
    validate_document(document)?;
    if envelope.version != CONTROL_VERSION
        || envelope.cluster_id != document.anchor.cluster_id
        || envelope.authority_epoch != document.authority_epoch
        || envelope.method != method.to_ascii_uppercase()
        || envelope.path != path
        || envelope.body_hash != hex::encode(Sha256::digest(body))
    {
        bail!("envelope de controle fora do contexto assinado");
    }
    verify_bytes(
        &envelope.issuer.pubkey,
        &control_message(envelope),
        &envelope.signature,
    )
}

/// Header-only verification used by axum's `FromRequestParts`. The complete
/// body hash is checked by the gateway middleware once the request body is
/// available; this keeps the extractor composable with `Json` handlers.
pub(crate) fn verify_control_envelope_metadata(
    envelope: &ControlEnvelope,
    document: &AdminTrustDocument,
    method: &str,
    path: &str,
) -> Result<()> {
    validate_document(document)?;
    if envelope.version != CONTROL_VERSION
        || envelope.cluster_id != document.anchor.cluster_id
        || envelope.authority_epoch != document.authority_epoch
        || envelope.method != method.to_ascii_uppercase()
        || envelope.path != path
        || envelope.body_hash.len() != 64
        || !envelope
            .body_hash
            .chars()
            .all(|glyph| glyph.is_ascii_hexdigit())
    {
        bail!("envelope de controle fora do contexto assinado");
    }
    verify_bytes(
        &envelope.issuer.pubkey,
        &control_message(envelope),
        &envelope.signature,
    )
}

pub(crate) fn control_issuer_is_admin(
    document: &AdminTrustDocument,
    issuer: &AuthorityPrincipal,
) -> bool {
    if validate_document(document).is_err() {
        return false;
    }
    if issuer == &document.primary {
        return true;
    }
    document.grants.iter().any(|grant| {
        grant.subject == *issuer
            && grant.level == AdminLevel::Secondary
            && !document.demotions.iter().any(|demotion| {
                demotion.subject == *issuer
                    && demotion.target_grant_hash == grant_hash(grant).unwrap_or_default()
            })
    })
}

fn sign_bytes(signing_key: &SigningKey, bytes: &[u8]) -> String {
    hex::encode(signing_key.sign(bytes).to_bytes())
}

#[derive(Serialize)]
struct ControlMessage<'a> {
    version: u32,
    command_id: &'a str,
    nonce: &'a str,
    cluster_id: &'a str,
    authority_epoch: u64,
    issuer: &'a AuthorityPrincipal,
    method: &'a str,
    path: &'a str,
    body_hash: &'a str,
}

fn control_message(envelope: &ControlEnvelope) -> Vec<u8> {
    serde_json::to_vec(&ControlMessage {
        version: envelope.version,
        command_id: &envelope.command_id,
        nonce: &envelope.nonce,
        cluster_id: &envelope.cluster_id,
        authority_epoch: envelope.authority_epoch,
        issuer: &envelope.issuer,
        method: &envelope.method,
        path: &envelope.path,
        body_hash: &envelope.body_hash,
    })
    .expect("canonical control envelope JSON")
}

fn verify_bytes(pubkey: &str, bytes: &[u8], signature: &str) -> Result<()> {
    let raw: [u8; 32] = hex::decode(pubkey)
        .context("assinatura usa uma chave inválida")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("chave de assinatura tem tamanho inválido"))?;
    let key = VerifyingKey::from_bytes(&raw).context("chave de assinatura inválida")?;
    let signature = Signature::from_slice(&hex::decode(signature).context("assinatura inválida")?)
        .context("assinatura inválida")?;
    key.verify(bytes, &signature)
        .context("assinatura administrativa não confere")
}

#[derive(Serialize)]
struct GrantMessage<'a> {
    version: u32,
    grant_id: &'a str,
    cluster_id: &'a str,
    issuer: &'a AuthorityPrincipal,
    subject: &'a AuthorityPrincipal,
    profile: NodeProfile,
    level: AdminLevel,
    authority_epoch: u64,
    promotion_seq: u64,
    previous_grant_hash: &'a Option<String>,
    command_id: &'a str,
    nonce: &'a str,
}

fn grant_message(grant: &AdminGrant) -> Vec<u8> {
    serde_json::to_vec(&GrantMessage {
        version: grant.version,
        grant_id: &grant.grant_id,
        cluster_id: &grant.cluster_id,
        issuer: &grant.issuer,
        subject: &grant.subject,
        profile: grant.profile,
        level: grant.level,
        authority_epoch: grant.authority_epoch,
        promotion_seq: grant.promotion_seq,
        previous_grant_hash: &grant.previous_grant_hash,
        command_id: &grant.command_id,
        nonce: &grant.nonce,
    })
    .expect("canonical grant JSON")
}

#[derive(Serialize)]
struct OfferMessage<'a> {
    version: u32,
    offer_id: &'a str,
    cluster_id: &'a str,
    issuer: &'a AuthorityPrincipal,
    successor: &'a AuthorityPrincipal,
    successor_grant_hash: &'a str,
    authority_epoch: u64,
    promotion_seq: u64,
    command_id: &'a str,
    nonce: &'a str,
}

fn offer_message(offer: &AdminTransferOffer) -> Vec<u8> {
    serde_json::to_vec(&OfferMessage {
        version: offer.version,
        offer_id: &offer.offer_id,
        cluster_id: &offer.cluster_id,
        issuer: &offer.issuer,
        successor: &offer.successor,
        successor_grant_hash: &offer.successor_grant_hash,
        authority_epoch: offer.authority_epoch,
        promotion_seq: offer.promotion_seq,
        command_id: &offer.command_id,
        nonce: &offer.nonce,
    })
    .expect("canonical transfer offer JSON")
}

#[derive(Serialize)]
struct AcceptanceMessage<'a> {
    version: u32,
    offer_id: &'a str,
    cluster_id: &'a str,
    successor: &'a AuthorityPrincipal,
    authority_epoch: u64,
    command_id: &'a str,
    nonce: &'a str,
}

#[derive(Serialize)]
struct DemotionMessage<'a> {
    version: u32,
    demotion_id: &'a str,
    cluster_id: &'a str,
    issuer: &'a AuthorityPrincipal,
    subject: &'a AuthorityPrincipal,
    target_grant_hash: &'a str,
    authority_epoch: u64,
    promotion_seq: u64,
    command_id: &'a str,
    nonce: &'a str,
}

#[derive(Serialize)]
struct NodePromotionMessage<'a> {
    version: u32,
    command_id: &'a str,
    cluster_id: &'a str,
    issuer: &'a AuthorityPrincipal,
    subject: &'a AuthorityPrincipal,
    authority_epoch: u64,
    nonce: &'a str,
}

#[derive(Serialize)]
struct NodePromotionRequestMessage<'a> {
    version: u32,
    command_id: &'a str,
    cluster_id: &'a str,
    subject: &'a AuthorityPrincipal,
    authority_epoch: u64,
    nonce: &'a str,
}

fn acceptance_message(acceptance: &AdminTransferAcceptance) -> Vec<u8> {
    serde_json::to_vec(&AcceptanceMessage {
        version: acceptance.version,
        offer_id: &acceptance.offer_id,
        cluster_id: &acceptance.cluster_id,
        successor: &acceptance.successor,
        authority_epoch: acceptance.authority_epoch,
        command_id: &acceptance.command_id,
        nonce: &acceptance.nonce,
    })
    .expect("canonical transfer acceptance JSON")
}

fn demotion_message(demotion: &AdminDemotion) -> Vec<u8> {
    serde_json::to_vec(&DemotionMessage {
        version: demotion.version,
        demotion_id: &demotion.demotion_id,
        cluster_id: &demotion.cluster_id,
        issuer: &demotion.issuer,
        subject: &demotion.subject,
        target_grant_hash: &demotion.target_grant_hash,
        authority_epoch: demotion.authority_epoch,
        promotion_seq: demotion.promotion_seq,
        command_id: &demotion.command_id,
        nonce: &demotion.nonce,
    })
    .expect("canonical demotion JSON")
}

fn node_promotion_message(command: &NodePromotionCommand) -> Vec<u8> {
    serde_json::to_vec(&NodePromotionMessage {
        version: command.version,
        command_id: &command.command_id,
        cluster_id: &command.cluster_id,
        issuer: &command.issuer,
        subject: &command.subject,
        authority_epoch: command.authority_epoch,
        nonce: &command.nonce,
    })
    .expect("canonical node promotion JSON")
}

fn node_promotion_request_message(request: &NodePromotionRequest) -> Vec<u8> {
    serde_json::to_vec(&NodePromotionRequestMessage {
        version: request.version,
        command_id: &request.command_id,
        cluster_id: &request.cluster_id,
        subject: &request.subject,
        authority_epoch: request.authority_epoch,
        nonce: &request.nonce,
    })
    .expect("canonical node promotion request JSON")
}

fn verify_grant_signature(grant: &AdminGrant) -> Result<()> {
    verify_bytes(
        &grant.issuer.pubkey,
        &grant_message(grant),
        &grant.signature,
    )
}

fn verify_offer_signature(offer: &AdminTransferOffer) -> Result<()> {
    verify_bytes(
        &offer.issuer.pubkey,
        &offer_message(offer),
        &offer.signature,
    )
}

fn verify_acceptance_signature(acceptance: &AdminTransferAcceptance) -> Result<()> {
    verify_bytes(
        &acceptance.successor.pubkey,
        &acceptance_message(acceptance),
        &acceptance.signature,
    )
}

fn verify_demotion_signature(demotion: &AdminDemotion) -> Result<()> {
    verify_bytes(
        &demotion.issuer.pubkey,
        &demotion_message(demotion),
        &demotion.signature,
    )
}

fn verify_node_promotion_signature(command: &NodePromotionCommand) -> Result<()> {
    verify_bytes(
        &command.issuer.pubkey,
        &node_promotion_message(command),
        &command.signature,
    )
}

fn verify_node_promotion_request_signature(request: &NodePromotionRequest) -> Result<()> {
    verify_bytes(
        &request.subject.pubkey,
        &node_promotion_request_message(request),
        &request.signature,
    )
}

fn grant_hash(grant: &AdminGrant) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(grant)?)))
}

fn consume_replay(
    document: &mut AdminTrustDocument,
    nonce: &str,
    command_id: &str,
    offer_id: Option<&str>,
) -> Result<()> {
    if document
        .replay
        .iter()
        .any(|record| record.nonce == nonce || record.command_id == command_id)
    {
        bail!("mutação administrativa repetida");
    }
    document.replay.push(ReplayRecord {
        nonce: nonce.to_string(),
        command_id: command_id.to_string(),
        offer_id: offer_id.map(str::to_string),
        authority_epoch: document.authority_epoch,
    });
    Ok(())
}

fn validate_document(document: &AdminTrustDocument) -> Result<()> {
    if document.version != TRUST_VERSION || !document.anchor.validates() {
        bail!("arquivo de confiança administrativa não tem uma âncora válida");
    }
    if document.primary_history.is_empty()
        || document.primary_history.first() != Some(&document.anchor.principal)
        || document.primary_history.last() != Some(&document.primary)
    {
        bail!("cadeia administrativa sem principal inicial");
    }
    if document.authority_epoch + 1 != document.primary_history.len() as u64
        || document.completed_transfers.len() + 1 != document.primary_history.len()
    {
        bail!("época administrativa não corresponde ao histórico");
    }
    let root = document
        .grants
        .first()
        .ok_or_else(|| anyhow::anyhow!("cadeia administrativa sem concessão raiz"))?;
    if root.version != TRUST_VERSION
        || root.cluster_id != document.anchor.cluster_id
        || root.issuer != document.anchor.principal
        || root.subject != document.anchor.principal
        || root.level != AdminLevel::Primary
        || root.authority_epoch != 0
        || root.promotion_seq != 0
        || root.previous_grant_hash.is_some()
        || root.profile != NodeProfile::Admin
    {
        bail!("concessão raiz administrativa inválida");
    }
    verify_grant_signature(root)?;
    let mut previous_hash = Some(grant_hash(root)?);
    let mut expected_promotion_seq: u64 = 0;
    let mut seen_subjects = vec![root.subject.clone()];
    for grant in document.grants.iter().skip(1) {
        expected_promotion_seq = expected_promotion_seq.saturating_add(1);
        let issuer_epoch = document
            .primary_history
            .iter()
            .position(|principal| principal == &grant.issuer)
            .map(|epoch| epoch as u64);
        if grant.version != TRUST_VERSION
            || grant.cluster_id != document.anchor.cluster_id
            || grant.level != AdminLevel::Secondary
            || grant.profile != NodeProfile::Admin
            || grant.subject == grant.issuer
            || grant.authority_epoch != issuer_epoch.unwrap_or(u64::MAX)
            || grant.promotion_seq != expected_promotion_seq
            || grant.previous_grant_hash != previous_hash
            || seen_subjects.contains(&grant.subject)
        {
            bail!("concessão secundária não foi emitida por um principal válido");
        }
        verify_grant_signature(grant)?;
        seen_subjects.push(grant.subject.clone());
        previous_hash = Some(grant_hash(grant)?);
    }
    if document.promotion_seq != expected_promotion_seq {
        bail!("contador de promoções inconsistente");
    }
    for (epoch, transfer) in document.completed_transfers.iter().enumerate() {
        let epoch = epoch as u64;
        let successor_grant = document.grants.iter().find(|grant| {
            grant_hash(grant).ok().as_deref() == Some(&transfer.offer.successor_grant_hash)
        });
        if transfer.offer.version != TRUST_VERSION
            || transfer.offer.cluster_id != document.anchor.cluster_id
            || transfer.offer.authority_epoch != epoch
            || transfer.offer.issuer != document.primary_history[epoch as usize]
            || transfer.offer.successor != document.primary_history[epoch as usize + 1]
            || transfer.offer.successor_grant_hash.is_empty()
            || transfer.offer.promotion_seq > document.promotion_seq
            || transfer.offer.successor != transfer.acceptance.successor
            || transfer.offer.offer_id != transfer.acceptance.offer_id
            || transfer.acceptance.version != TRUST_VERSION
            || transfer.acceptance.cluster_id != document.anchor.cluster_id
            || transfer.acceptance.authority_epoch != epoch
            || !successor_grant.is_some_and(|grant| {
                grant.subject == transfer.offer.successor
                    && grant.issuer == transfer.offer.issuer
                    && grant.authority_epoch == epoch
                    && grant.promotion_seq == transfer.offer.promotion_seq
            })
        {
            bail!("transferência administrativa inconsistente");
        }
        verify_offer_signature(&transfer.offer)?;
        verify_acceptance_signature(&transfer.acceptance)?;
    }
    for demotion in &document.demotions {
        let target_grant = document
            .grants
            .iter()
            .find(|grant| grant_hash(grant).ok().as_deref() == Some(&demotion.target_grant_hash));
        if demotion.version != TRUST_VERSION
            || demotion.cluster_id != document.anchor.cluster_id
            || demotion.authority_epoch > document.authority_epoch
            || demotion.promotion_seq > document.promotion_seq
            || !document
                .primary_history
                .get(demotion.authority_epoch as usize)
                .is_some_and(|principal| principal == &demotion.issuer)
            || !target_grant.is_some_and(|grant| {
                grant.subject == demotion.subject
                    && grant.level == AdminLevel::Secondary
                    && grant.promotion_seq == demotion.promotion_seq
            })
        {
            bail!("despromoção administrativa inconsistente");
        }
        verify_demotion_signature(demotion)?;
    }
    if let Some(offer) = &document.pending_transfer {
        let successor_grant = document.grants.last();
        if offer.version != TRUST_VERSION
            || offer.cluster_id != document.anchor.cluster_id
            || offer.issuer != document.primary
            || offer.authority_epoch != document.authority_epoch
            || offer.promotion_seq != document.promotion_seq
            || !successor_grant.is_some_and(|grant| {
                grant.subject == offer.successor
                    && grant.issuer == offer.issuer
                    && grant.authority_epoch == offer.authority_epoch
                    && grant.promotion_seq == offer.promotion_seq
                    && grant_hash(grant).ok().as_deref() == Some(&offer.successor_grant_hash)
            })
        {
            bail!("oferta pendente não pertence ao principal vigente");
        }
        verify_offer_signature(offer)?;
    }
    for record in &document.replay {
        if record.nonce.is_empty() || record.command_id.is_empty() {
            bail!("registro de replay administrativo incompleto");
        }
    }
    Ok(())
}

fn persist_document(document: &AdminTrustDocument, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else { return Ok(()) };
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("arquivo de confiança sem pasta pai"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    std::fs::write(&temp, serde_json::to_vec_pretty(document)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp, path)
        .with_context(|| format!("não foi possível instalar {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile_fallback::TempDir;

    mod tempfile_fallback {
        pub struct TempDir(std::path::PathBuf);
        impl TempDir {
            pub fn new() -> Self {
                let path =
                    std::env::temp_dir().join(format!("pom-admin-trust-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn principal(seed: u8, node: &str, peer: &str) -> AuthorityPrincipal {
        AuthorityPrincipal::from_signing_key(node, peer, &key(seed))
    }

    fn boot(path: &Path) -> (AdminTrust, SigningKey, AuthorityPrincipal) {
        let signing_key = key(1);
        let root = principal(1, "root", "peer-root");
        let trust =
            AdminTrust::bootstrap_primary(path, "cluster", root.clone(), &signing_key).unwrap();
        (trust, signing_key, root)
    }

    #[test]
    fn profile_admin_without_a_grant_has_no_effective_authority() {
        let trust = AdminTrust::empty();
        let local = principal(1, "node", "peer");
        assert_eq!(trust.effective_authority(&local, NodeProfile::Admin), None);
    }

    #[test]
    fn admission_requires_the_anchored_cluster_and_an_active_principal() {
        let dir = TempDir::new();
        let (trust, _, root) = boot(&dir.path().join("trust.json"));

        assert!(trust.admits_participant("cluster", &root));
        assert!(!trust.admits_participant("outro-cluster", &root));
        assert!(!trust.admits_participant("cluster", &principal(2, "attacker", "peer-attacker")));
        assert!(!AdminTrust::empty().admits_participant("cluster", &root));
    }

    #[test]
    fn a_secondary_cannot_issue_a_grant() {
        let dir = TempDir::new();
        let (trust, root_key, root) = boot(&dir.path().join("trust.json"));
        let secondary = principal(2, "secondary", "peer-secondary");
        let grant = trust
            .signed_secondary_grant(&root_key, secondary.clone(), "root")
            .unwrap();
        trust
            .apply_secondary_grant(grant, &secondary, NodeProfile::Node)
            .unwrap();
        let target = principal(3, "target", "peer-target");
        let mut forged = trust
            .signed_secondary_grant(&root_key, target.clone(), "root")
            .unwrap();
        forged.issuer = secondary;
        assert!(trust
            .apply_secondary_grant(forged, &target, NodeProfile::Node)
            .is_err());
        assert_eq!(trust.current_primary(), root);
    }

    #[test]
    fn a_replayed_grant_is_refused_after_reload() {
        let dir = TempDir::new();
        let path = dir.path().join("trust.json");
        let (trust, root_key, _root) = boot(&path);
        let secondary = principal(2, "secondary", "peer-secondary");
        let grant = trust
            .signed_secondary_grant(&root_key, secondary.clone(), "root")
            .unwrap();
        trust
            .apply_secondary_grant(grant.clone(), &secondary, NodeProfile::Node)
            .unwrap();
        let reloaded = AdminTrust::load(&path).unwrap();
        assert!(reloaded
            .apply_secondary_grant(grant, &secondary, NodeProfile::Node)
            .is_err());
    }

    #[test]
    fn transfer_requires_both_offer_and_acceptance_and_rebuilds_from_chain() {
        let dir = TempDir::new();
        let path = dir.path().join("trust.json");
        let (trust, root_key, root) = boot(&path);
        let secondary_key = key(2);
        let secondary = principal(2, "secondary", "peer-secondary");
        let grant = trust
            .signed_secondary_grant(&root_key, secondary.clone(), "root")
            .unwrap();
        trust
            .apply_secondary_grant(grant, &secondary, NodeProfile::Node)
            .unwrap();
        let offer = trust
            .begin_transfer(&root_key, secondary.clone(), true)
            .unwrap();
        assert_eq!(trust.current_primary(), root);
        let reloaded = AdminTrust::load(&path).unwrap();
        assert_eq!(reloaded.current_primary(), root);
        reloaded
            .accept_transfer(offer, &secondary_key, secondary.clone())
            .unwrap();
        assert_eq!(reloaded.current_primary(), secondary);
        let rebuilt = AdminTrust::load(&path).unwrap();
        assert_eq!(rebuilt.current_primary(), secondary);
    }

    #[test]
    fn acceptance_without_offer_does_not_change_primary() {
        let dir = TempDir::new();
        let (trust, root_key, root) = boot(&dir.path().join("trust.json"));
        let secondary = principal(2, "secondary", "peer-secondary");
        let grant = trust
            .signed_secondary_grant(&root_key, secondary.clone(), "root")
            .unwrap();
        trust
            .apply_secondary_grant(grant, &secondary, NodeProfile::Node)
            .unwrap();
        let offer = AdminTransferOffer {
            version: TRUST_VERSION,
            offer_id: "missing".into(),
            cluster_id: "cluster".into(),
            issuer: root.clone(),
            successor: secondary.clone(),
            successor_grant_hash: "missing".into(),
            authority_epoch: 0,
            promotion_seq: 1,
            command_id: "command".into(),
            nonce: "nonce".into(),
            signature: sign_bytes(&root_key, b"not-the-offer"),
        };
        assert!(trust.accept_transfer(offer, &key(2), secondary).is_err());
        assert_eq!(trust.current_primary(), root);
    }

    #[test]
    fn an_offline_node_rebuilds_the_principal_from_the_full_chain() {
        let dir = TempDir::new();
        let source_path = dir.path().join("source.json");
        let target_path = dir.path().join("target.json");
        let (source, root_key, root) = boot(&source_path);
        let successor_key = key(2);
        let successor = principal(2, "secondary", "peer-secondary");
        let grant = source
            .signed_secondary_grant(&root_key, successor.clone(), "root")
            .unwrap();
        source.record_secondary_grant(grant, &root_key).unwrap();
        let offer = source
            .begin_transfer(&root_key, successor.clone(), true)
            .unwrap();

        let target = boot(&target_path).0;
        target.reconcile(source.document()).unwrap();
        let acceptance = target
            .accept_transfer(offer.clone(), &successor_key, successor.clone())
            .unwrap();
        source.complete_transfer(offer, acceptance).unwrap();

        assert_eq!(target.current_primary(), successor);
        assert_eq!(source.current_primary(), target.current_primary());
        assert_ne!(source.current_primary(), root);
    }

    #[test]
    fn a_signed_secondary_demotion_removes_effective_authority_after_reload() {
        let dir = TempDir::new();
        let path = dir.path().join("trust.json");
        let (trust, root_key, _root) = boot(&path);
        let secondary = principal(2, "secondary", "peer-secondary");
        let grant = trust
            .signed_secondary_grant(&root_key, secondary.clone(), "root")
            .unwrap();
        trust
            .apply_secondary_grant(grant, &secondary, NodeProfile::Node)
            .unwrap();
        assert_eq!(
            trust.effective_authority(&secondary, NodeProfile::Admin),
            Some(EffectiveAuthority::Secondary)
        );
        trust
            .demote_secondary(&root_key, secondary.clone())
            .unwrap();
        let reloaded = AdminTrust::load(&path).unwrap();
        assert_eq!(
            reloaded.effective_authority(&secondary, NodeProfile::Admin),
            None
        );
    }

    /// A conferência que ancora a confiança tolera a transcrição humana -
    /// espaços, dois-pontos, caixa - e nada além disso.
    #[test]
    fn a_conferencia_manual_normaliza_transcricao_e_nada_mais() {
        let esperada = fingerprint_of(&principal(1, "root", "peer-root").pubkey);
        let com_ruido = format!("  {}  ", esperada.to_uppercase());
        assert!(fingerprint_matches(&com_ruido, &esperada));

        let separada = esperada
            .as_bytes()
            .chunks(8)
            .map(|parte| String::from_utf8_lossy(parte).to_string())
            .collect::<Vec<_>>()
            .join(":");
        assert!(fingerprint_matches(&separada, &esperada));

        assert!(!fingerprint_matches("", &esperada));
        assert!(!fingerprint_matches("   ", &esperada));
        assert!(!fingerprint_matches(&esperada[..63], &esperada));
        assert!(!fingerprint_matches(
            &format!("{}0", &esperada[1..]),
            &esperada
        ));
    }

    /// O nó que recebe: cadeia colada mais impressão digital conferida
    /// instala a raiz e a deixa no disco, com a autoridade valendo em memória
    /// sem reinício.
    #[test]
    fn adotar_com_a_impressao_digital_certa_ancora_o_no() {
        let origem = TempDir::new();
        let (admin, ..) = boot(&origem.path().join("admin.json"));
        let document = admin.document();
        let esperada = anchor_fingerprint(&document);

        let destino = TempDir::new();
        let path = destino.path().join("recebido.json");
        let no = AdminTrust::empty_at(Some(path.clone()));
        assert!(!no.is_anchored());

        no.adopt_anchor(document.clone(), &esperada).unwrap();

        assert!(no.is_anchored());
        assert_eq!(no.document().anchor, document.anchor);
        assert_eq!(AdminTrust::load(&path).unwrap().document(), document);
    }

    /// A recusa que não depende de HTTP: com a impressão digital errada nada
    /// é escrito, nem em memória nem no disco.
    #[test]
    fn adotar_com_a_impressao_digital_errada_nao_escreve_nada() {
        let origem = TempDir::new();
        let (admin, ..) = boot(&origem.path().join("admin.json"));
        let destino = TempDir::new();
        let path = destino.path().join("recebido.json");
        let no = AdminTrust::empty_at(Some(path.clone()));

        let erro = no
            .adopt_anchor(admin.document(), &"0".repeat(64))
            .unwrap_err();

        assert!(erro.to_string().contains("impressão digital"));
        assert!(!no.is_anchored());
        assert!(
            !path.exists(),
            "recusa não pode deixar arquivo de confiança"
        );
    }

    /// Trocar a raiz é reancoragem manual. Nem a cadeia em memória nem um
    /// arquivo já no disco podem ser substituídos por esta porta.
    #[test]
    fn adotar_sobre_um_no_ja_ancorado_e_recusado() {
        let local = TempDir::new();
        let path = local.path().join("meu.json");
        let (meu, ..) = boot(&path);
        let ancora_original = meu.document().anchor;

        let outro = TempDir::new();
        let (outro_admin, ..) = {
            let signing_key = key(3);
            let root = principal(3, "outro", "peer-outro");
            let trust = AdminTrust::bootstrap_primary(
                &outro.path().join("outro.json"),
                "outro-cluster",
                root.clone(),
                &signing_key,
            )
            .unwrap();
            (trust, signing_key, root)
        };
        let document = outro_admin.document();
        let esperada = anchor_fingerprint(&document);

        let erro = meu.adopt_anchor(document, &esperada).unwrap_err();
        assert!(erro.to_string().contains("reancoragem manual"));
        assert_eq!(meu.document().anchor, ancora_original);
    }

    /// Um arquivo ilegível não é âncora, mas também não é coisa que a
    /// interface apague por cima.
    #[test]
    fn adotar_por_cima_de_um_arquivo_existente_e_recusado() {
        let origem = TempDir::new();
        let (admin, ..) = boot(&origem.path().join("admin.json"));
        let destino = TempDir::new();
        let path = destino.path().join("corrompido.json");
        std::fs::write(&path, b"{ nao e json }").unwrap();

        let no = AdminTrust::load_or_empty(&path);
        assert!(!no.is_anchored());

        let document = admin.document();
        let esperada = anchor_fingerprint(&document);
        let erro = no.adopt_anchor(document, &esperada).unwrap_err();

        assert!(erro.to_string().contains("reancoragem manual"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{ nao e json }");
    }

    /// Uma cadeia adulterada é recusada na leitura, antes de qualquer
    /// conferência de impressão digital.
    #[test]
    fn uma_cadeia_adulterada_nao_e_lida() {
        let origem = TempDir::new();
        let (admin, ..) = boot(&origem.path().join("admin.json"));
        let mut document = admin.document();
        document.anchor.cluster_id = "cluster-do-atacante".into();
        let adulterada = serde_json::to_string(&document).unwrap();

        assert!(parse_transported(&adulterada).is_err());
        assert!(parse_transported("nem json e").is_err());
    }
}
