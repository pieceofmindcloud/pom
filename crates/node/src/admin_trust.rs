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
    /// Unix seconds when the issuer signed. Outside `CONTROL_WINDOW_SECS` of
    /// the receiver's clock the envelope is refused, which is what lets the
    /// replay ledger forget a nonce once its window has passed.
    pub(crate) issued_at: i64,
    pub(crate) signature: String,
}

/// Validity of a control envelope, on either side of the receiver's clock. It
/// covers the clock difference between nodes; a captured envelope can only be
/// replayed inside it, and that is the window the local replay ledger
/// remembers it for.
pub(crate) const CONTROL_WINDOW_SECS: i64 = 5 * 60;

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Nonces and command ids already consumed by **this** node. It lives in a
/// local file next to the chain (`<chain>.replay.json`), never in the trust
/// document: the document is shared (download, local discovery, public
/// directory) and replaced wholesale by `reconcile`, while this ledger is the
/// state of a single node.
///
/// Control-envelope entries expire with the envelope's window; administrative
/// mutation entries (grant, demotion, transfer, infer admission) do not expire,
/// because those signed records carry no clock - and they are rare.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ReplayLedger {
    entries: Vec<ReplayEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayEntry {
    nonce: String,
    command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offer_id: Option<String>,
    /// Unix seconds after which the entry can be forgotten; `None` for an
    /// administrative mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

/// Where a chain's replay ledger lives: next to it.
pub(crate) fn replay_path_for(trust_path: &Path) -> PathBuf {
    trust_path.with_extension("replay.json")
}

fn load_replay_ledger(trust_path: Option<&Path>) -> ReplayLedger {
    let Some(path) = trust_path.map(replay_path_for) else {
        return ReplayLedger::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|error| {
            tracing::warn!("unreadable replay ledger at {}: {error}", path.display());
            ReplayLedger::default()
        }),
        Err(_) => ReplayLedger::default(),
    }
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

/// The initial network descriptor travels with the trust file so a joining
/// node does not have to retype identity or discovery data. It is signed by
/// the authority that created the trust chain and is intentionally smaller
/// than `NetworkSettings`: optional editorial fields belong to administration,
/// not to first access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TrustNetworkMetadata {
    pub(crate) public_name: String,
    #[serde(default)]
    pub(crate) organization: String,
    pub(crate) visibility: crate::config::NetworkVisibility,
    pub(crate) access_policy: crate::config::NetworkAccessPolicy,
    #[serde(default)]
    pub(crate) country_code: String,
    #[serde(default)]
    pub(crate) country_name: String,
    #[serde(default)]
    pub(crate) city: String,
    #[serde(default)]
    pub(crate) geo_label: String,
    /// `pk:` keys of the dedicated relays the network uses. A key, never an
    /// address: the document carries nobody's IP. A joining node resolves the
    /// principal (and these relays) through pkarr at dial time.
    #[serde(default)]
    pub(crate) relay_hints: Vec<String>,
    #[serde(default)]
    pub(crate) directory_replicas: Vec<String>,
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
pub(crate) struct CompletedTransfer {
    offer: AdminTransferOffer,
    acceptance: AdminTransferAcceptance,
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
    /// Network and discovery values captured when the administrator creates
    /// the chain. Existing authority chains may omit this until regenerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) setup: Option<TrustNetworkMetadata>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) setup_signature: String,
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
    /// Local replay ledger; see `ReplayLedger`. Always locked after `state`
    /// when both are taken.
    replay: Arc<Mutex<ReplayLedger>>,
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
                setup: None,
                setup_signature: String::new(),
            })),
            replay: Arc::new(Mutex::new(load_replay_ledger(path.as_deref()))),
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
            replay: Arc::new(Mutex::new(load_replay_ledger(Some(&path)))),
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

    /// Há uma cadeia local válida ancorada exatamente neste `cluster_id`?
    ///
    /// Ao contrário de [`Self::admits_participant`], não exige que o peer tenha
    /// uma concessão de autoridade primária ou secundária. É a prova de
    /// participação de `access_policy = anyone_with_anchor`: sob essa política o
    /// operador declarou que segurar a âncora correspondente já é a prova de
    /// pertencimento, então um nó worker comum que nunca recebeu concessão
    /// continua sendo participante legítimo. A identidade do peer (nó ↔ chave ↔
    /// PeerId, e o Announce assinado por esse PeerId) é conferida à parte por
    /// quem chama.
    pub(crate) fn anchors_cluster(&self, cluster_id: &str) -> bool {
        let document = self.document();
        validate_document(&document).is_ok() && document.anchor.cluster_id == cluster_id
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
            replay: Arc::new(Mutex::new(load_replay_ledger(Some(path.as_ref())))),
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
            replay: Arc::new(Mutex::new(ReplayLedger::default())),
        })
    }

    pub(crate) fn document(&self) -> AdminTrustDocument {
        self.state
            .lock()
            .expect("admin trust lock poisoned")
            .clone()
    }

    /// Completa uma cadeia primária antiga com os metadados que o wizard já
    /// persistiu na configuração local. A operação só é permitida ao
    /// principal vigente da própria cadeia, pois esses dados passam a fazer
    /// parte do documento assinado que será transportado para novos nós.
    pub(crate) fn ensure_setup_metadata(
        &self,
        setup: TrustNetworkMetadata,
        signing_key: &SigningKey,
        principal: &AuthorityPrincipal,
    ) -> Result<AdminTrustDocument> {
        let mut current = self.state.lock().expect("admin trust lock poisoned");
        if current.setup.is_some() {
            return Ok(current.clone());
        }
        if current.anchor.principal != *principal || current.primary != *principal {
            bail!("somente o principal vigente pode completar os metadados da rede");
        }
        if principal.verifying_key()? != signing_key.verifying_key() {
            bail!("a chave local não corresponde ao principal vigente");
        }
        current.setup = Some(setup);
        current.setup_signature = sign_bytes(signing_key, &setup_message(&current));
        validate_document(&current)?;
        persist_document(&current, self.path.as_deref())?;
        Ok(current.clone())
    }

    /// Consume a signed control command before its handler mutates anything.
    /// The nonce and command id are durable in the local replay ledger until
    /// the envelope's time window closes, so retrying a captured envelope
    /// cannot replay a control action, not even after a process restart.
    pub(crate) fn consume_control_replay(&self, envelope: &ControlEnvelope) -> Result<()> {
        if !self.record_replay(
            &envelope.nonce,
            &envelope.command_id,
            None,
            Some(envelope.issued_at.saturating_add(CONTROL_WINDOW_SECS)),
        )? {
            bail!("comando de controle já foi processado");
        }
        Ok(())
    }

    /// Administrative mutation: the signed record has no clock, so the entry
    /// does not expire.
    fn consume_admin_replay(
        &self,
        nonce: &str,
        command_id: &str,
        offer_id: Option<&str>,
    ) -> Result<()> {
        if !self.record_replay(nonce, command_id, offer_id, None)? {
            bail!("repeated administrative mutation");
        }
        Ok(())
    }

    /// Records the nonce/command pair in the local ledger and saves it. `false`
    /// when either was already there. Expired entries are dropped here.
    fn record_replay(
        &self,
        nonce: &str,
        command_id: &str,
        offer_id: Option<&str>,
        expires_at: Option<i64>,
    ) -> Result<bool> {
        if nonce.is_empty() || command_id.is_empty() {
            bail!("registro de replay administrativo incompleto");
        }
        let now = unix_now();
        let mut ledger = self.replay.lock().expect("replay ledger lock poisoned");
        ledger
            .entries
            .retain(|entry| entry.expires_at.is_none_or(|expires| expires >= now));
        if ledger
            .entries
            .iter()
            .any(|entry| entry.nonce == nonce || entry.command_id == command_id)
        {
            return Ok(false);
        }
        ledger.entries.push(ReplayEntry {
            nonce: nonce.to_string(),
            command_id: command_id.to_string(),
            offer_id: offer_id.map(str::to_string),
            expires_at,
        });
        let path = self.path.as_deref().map(replay_path_for);
        persist_json(&*ledger, path.as_deref(), false)?;
        Ok(true)
    }

    /// How many entries the local replay ledger holds.
    #[cfg(test)]
    pub(crate) fn replay_len(&self) -> usize {
        self.replay
            .lock()
            .expect("replay ledger lock poisoned")
            .entries
            .len()
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
            setup: None,
            setup_signature: String::new(),
        };
        validate_document(&document)?;
        let trust = Self {
            state: Arc::new(Mutex::new(document)),
            path: Some(path.as_ref().to_path_buf()),
            replay: Arc::new(Mutex::new(load_replay_ledger(Some(path.as_ref())))),
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
            setup: None,
            setup_signature: String::new(),
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
        self.consume_admin_replay(&grant.nonce, &grant.command_id, None)?;
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
        self.consume_admin_replay(&grant.nonce, &grant.command_id, None)?;
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
        self.consume_admin_replay(&demotion.nonce, &demotion.command_id, None)?;
        document.demotions.push(demotion.clone());
        validate_document(&document)?;
        persist_document(&document, self.path.as_deref())?;
        Ok(demotion)
    }

    /// The version of this copy of the chain:
    /// `<epoch>.<grants>.<demotions>.<open transfer>-<content hash>`. The
    /// counters order two copies (every change of authority moves one); the
    /// hash tells apart two copies the counters call equal.
    pub(crate) fn chain_version(&self) -> String {
        chain_version_of(&self.document())
    }

    /// Adopts `incoming` as the principal's own copy would be adopted.
    pub(crate) fn reconcile(&self, incoming: AdminTrustDocument) -> Result<()> {
        self.reconcile_from(incoming, true).map(|_| ())
    }

    /// Adopts a copy of the chain that is newer than the local one;
    /// `Ok(false)` when it is the same copy. Every signature is checked from
    /// the anchor, so the copy may come from any member, not only from the
    /// principal - which is what lets a node that was away catch up while the
    /// principal is offline. Order is decided by the counters. Two different
    /// copies with equal counters cannot be ordered: only the principal's
    /// (`authoritative`) replaces the local one then, since it is the only
    /// writer of the chain.
    pub(crate) fn reconcile_from(
        &self,
        incoming: AdminTrustDocument,
        authoritative: bool,
    ) -> Result<bool> {
        validate_document(&incoming)?;
        let mut document = self.state.lock().expect("admin trust lock poisoned");
        if document.anchor != incoming.anchor {
            bail!("a cadeia recebida não corresponde à âncora manual deste nó");
        }
        match chain_counters(&incoming).cmp(&chain_counters(&document)) {
            std::cmp::Ordering::Less => bail!("a cadeia recebida é anterior ao estado local"),
            std::cmp::Ordering::Equal => {
                if chain_version_of(&incoming) == chain_version_of(&document) {
                    return Ok(false);
                }
                if !authoritative {
                    bail!("same chain version with different content: only the principal's copy decides");
                }
            }
            std::cmp::Ordering::Greater => {}
        }
        persist_document(&incoming, self.path.as_deref())?;
        *document = incoming;
        Ok(true)
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
        self.consume_admin_replay(&offer.nonce, &offer.command_id, Some(&offer.offer_id))?;
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
        self.consume_admin_replay(
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
        self.consume_admin_replay(
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
    // The node key is the authority key: see `identity.rs`.
    let local_identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
    let principal = AuthorityPrincipal::from_signing_key(
        local_identity.node_id.0.clone(),
        local_identity.peer_id(),
        &local_identity.signing_key,
    );
    Ok((local_identity.signing_key, principal))
}

/// Build the exact document the administrator sees in the setup wizard. The
/// document is returned without writing it, so changing profile or abandoning
/// setup cannot leave a trust file that blocks a later first-access attempt.
pub(crate) fn local_primary_document(
    cluster_id: impl Into<String>,
    setup: Option<TrustNetworkMetadata>,
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
    let mut document = AdminTrustDocument {
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
        setup,
        setup_signature: String::new(),
    };
    if document.setup.is_some() {
        document.setup_signature = sign_bytes(&signing_key, &setup_message(&document));
    }
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
    sign_control_envelope_at(
        signing_key,
        issuer,
        document,
        method,
        path,
        body,
        unix_now(),
    )
}

fn sign_control_envelope_at(
    signing_key: &SigningKey,
    issuer: AuthorityPrincipal,
    document: &AdminTrustDocument,
    method: &str,
    path: &str,
    body: &[u8],
    issued_at: i64,
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
        issued_at,
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
    check_control_window(envelope)?;
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
    check_control_window(envelope)?;
    verify_bytes(
        &envelope.issuer.pubkey,
        &control_message(envelope),
        &envelope.signature,
    )
}

/// Was the envelope signed inside the window, by this node's clock?
fn check_control_window(envelope: &ControlEnvelope) -> Result<()> {
    if (unix_now() - envelope.issued_at).abs() > CONTROL_WINDOW_SECS {
        bail!("control envelope outside the time window (clocks out of sync?)");
    }
    Ok(())
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

/// The counters that order two copies of a chain: principal transfers,
/// grants, demotions.
fn chain_counters(document: &AdminTrustDocument) -> (u64, u64, u64) {
    (
        document.authority_epoch,
        document.promotion_seq,
        document.demotions.len() as u64,
    )
}

pub(crate) fn chain_version_of(document: &AdminTrustDocument) -> String {
    let (epoch, grants, demotions) = chain_counters(document);
    let content = serde_json::to_vec(document).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(&content));
    format!(
        "{epoch}.{grants}.{demotions}.{}-{}",
        u8::from(document.pending_transfer.is_some()),
        &digest[..16]
    )
}

/// The ordering part of a version string, for comparing with a version a
/// peer announces. `None` for anything that is not a version.
pub(crate) fn version_counters(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('-').next()?.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    Some((next()?, next()?, next()?))
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
    issued_at: i64,
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
        issued_at: envelope.issued_at,
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
struct SetupMessage<'a> {
    cluster_id: &'a str,
    setup: &'a TrustNetworkMetadata,
}

fn setup_message(document: &AdminTrustDocument) -> Vec<u8> {
    let setup = document
        .setup
        .as_ref()
        .expect("setup message requires network metadata");
    serde_json::to_vec(&SetupMessage {
        cluster_id: &document.anchor.cluster_id,
        setup,
    })
    .expect("canonical setup metadata JSON")
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

fn grant_hash(grant: &AdminGrant) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(grant)?)))
}

fn validate_document(document: &AdminTrustDocument) -> Result<()> {
    if document.version != TRUST_VERSION || !document.anchor.validates() {
        bail!("arquivo de confiança administrativa não tem uma âncora válida");
    }
    match (&document.setup, document.setup_signature.is_empty()) {
        (Some(_), true) => bail!("metadados iniciais da rede sem assinatura"),
        (Some(_), false) => verify_bytes(
            &document.anchor.principal.pubkey,
            &setup_message(document),
            &document.setup_signature,
        )?,
        (None, false) => bail!("assinatura de metadados sem conteúdo"),
        (None, true) => {}
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
    Ok(())
}

fn persist_document(document: &AdminTrustDocument, path: Option<&Path>) -> Result<()> {
    persist_json(document, path, true)
}

/// Writes through a temporary file + rename, with `0600` permission on Unix.
fn persist_json<T: Serialize>(value: &T, path: Option<&Path>, pretty: bool) -> Result<()> {
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
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)?
    } else {
        serde_json::to_vec(value)?
    };
    std::fs::write(&temp, bytes)?;
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

    /// An envelope signed outside the window is refused, on either side of the
    /// clock; inside it, it passes.
    #[test]
    fn a_control_envelope_outside_the_time_window_is_refused() {
        let dir = TempDir::new();
        let (trust, signing_key, root) = boot(&dir.path().join("trust.json"));
        let document = trust.document();
        let at = |issued_at| {
            sign_control_envelope_at(
                &signing_key,
                root.clone(),
                &document,
                "POST",
                "/x",
                b"{}",
                issued_at,
            )
            .unwrap()
        };
        let now = unix_now();
        for issued_at in [
            now,
            now - CONTROL_WINDOW_SECS + 5,
            now + CONTROL_WINDOW_SECS - 5,
        ] {
            verify_control_envelope(&at(issued_at), &document, "POST", "/x", b"{}").unwrap();
        }
        for issued_at in [now - CONTROL_WINDOW_SECS - 5, now + CONTROL_WINDOW_SECS + 5] {
            assert!(
                verify_control_envelope(&at(issued_at), &document, "POST", "/x", b"{}").is_err()
            );
            assert!(
                verify_control_envelope_metadata(&at(issued_at), &document, "POST", "/x").is_err()
            );
        }
        // The time is part of the signature: changing it invalidates the
        // envelope.
        let mut moved = at(now);
        moved.issued_at += 1;
        assert!(verify_control_envelope(&moved, &document, "POST", "/x", b"{}").is_err());
    }

    /// The replay ledger lives next to the chain, never in the document, and
    /// survives a restart.
    #[test]
    fn the_replay_ledger_is_local_and_survives_a_reload() {
        let dir = TempDir::new();
        let path = dir.path().join("trust.json");
        let (trust, signing_key, root) = boot(&path);
        let envelope =
            sign_control_envelope(&signing_key, root, &trust.document(), "POST", "/x", b"{}")
                .unwrap();
        trust.consume_control_replay(&envelope).unwrap();
        assert!(trust.consume_control_replay(&envelope).is_err());

        let document = std::fs::read_to_string(&path).unwrap();
        assert!(
            !document.contains(&envelope.nonce),
            "the nonce leaked into the chain"
        );
        assert!(!document.contains("\"replay\""));
        assert!(replay_path_for(&path).exists());

        let reloaded = AdminTrust::load(&path).unwrap();
        assert!(reloaded.consume_control_replay(&envelope).is_err());
    }

    /// An envelope entry leaves the ledger when its window closes; an
    /// administrative mutation entry stays.
    #[test]
    fn expired_control_entries_are_forgotten() {
        let dir = TempDir::new();
        let (trust, _signing_key, _root) = boot(&dir.path().join("trust.json"));
        let past = unix_now() - 1;
        assert!(trust
            .record_replay("n-old", "c-old", None, Some(past))
            .unwrap());
        assert!(trust
            .record_replay("n-admin", "c-admin", None, None)
            .unwrap());
        assert!(trust
            .record_replay(
                "n-new",
                "c-new",
                None,
                Some(unix_now() + CONTROL_WINDOW_SECS)
            )
            .unwrap());
        assert_eq!(
            trust.replay_len(),
            2,
            "a vencida saiu, a administrativa ficou"
        );
        assert!(trust
            .record_replay("n-old", "c-old", None, Some(past))
            .unwrap());
    }

    #[test]
    fn signed_setup_metadata_is_part_of_the_trust_document() {
        let dir = TempDir::new();
        let (trust, signing_key, _root) = boot(&dir.path().join("trust.json"));
        let mut document = trust.document();
        document.setup = Some(TrustNetworkMetadata {
            public_name: "POM-ABCDE".into(),
            organization: String::new(),
            visibility: crate::config::NetworkVisibility::Private,
            access_policy: crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
            country_code: String::new(),
            country_name: String::new(),
            city: String::new(),
            geo_label: String::new(),
            relay_hints: Vec::new(),
            directory_replicas: Vec::new(),
        });
        document.setup_signature = sign_bytes(&signing_key, &setup_message(&document));
        validate_document(&document).unwrap();

        document.setup.as_mut().unwrap().public_name = "forged".into();
        assert!(validate_document(&document).is_err());
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
