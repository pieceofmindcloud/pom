//! The deployment as a thing that exists.
//!
//! Until now a deployment was not represented anywhere: it was
//! *deduced*, at each point of use, from the model's name. Whoever picks the nodes
//! of a request compared `p.model == model_name` (`pipeline::plan_pipeline`),
//! `deploy.rs` carried a `group: String` as the key, and undeploying with
//! two models live answered "there is more than one pipeline up; say which one".
//! Two deployments **of the same model** - on different machines, or with a
//! different configuration - were inexpressible.
//!
//! The `deploy_id` is that missing identity: it ties a set of nodes to a
//! model, says in what form (whole model, tensor parallelism or a set
//! of slices) and keeps **which configuration each node received**. Without it
//! the operator's intent was discarded the instant the launch finished,
//! and nothing in the cluster could say why a machine held a range.
//!
//! The mold is `TpIdentity` (`network.rs`), which already did exactly this for
//! tensor parallelism groups: an id that names a set of nodes, minted
//! at deploy, carried in the announce and in `NodeConfig`, surviving a restart.
//! Here the same idea applies to every deployment.
//!
//! **Legacy source:** one file per deployment, in
//! `<POM_HOME>/deployments/`, on each participating node. The current runtime
//! writes and reads the record in `backend::db::PomDb`; these file functions
//! remain only as a safety net for the throwaway preservation tool.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Format version. It goes up when the meaning of a field changes.
///
/// v2 added `versions`/`active_version`: a deployment started keeping
/// the history of configurations it has already run under the same `deploy_id`.
/// A v1 record (without those fields) is read with `versions` empty and
/// [`Deployment::ensure_versions`] synthesizes v1 from `nodes`.
pub const DEPLOYMENT_VERSION: u32 = 2;

/// Prefix of the id, so that it is recognizable loose in a log or in a URL.
const ID_PREFIX: &str = "d-";

/// A new `deploy_id`.
///
/// Random, and **not** derived from the content: two deployments that differ
/// only in the choice of machines must be distinguishable, and deploying the
/// same arrangement twice on purpose must not collide. It is the opposite of
/// the `pom_id` rule, which is derived precisely because there the same
/// content *is* the same thing.
pub fn new_deploy_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{ID_PREFIX}{}", &u[..8])
}

/// A plausible id: the prefix plus 8 hex digits.
///
/// It serves as input validation **and** as a path defense - this is what
/// keeps an id coming from the network from becoming `../` when naming a file.
pub fn is_deploy_id(s: &str) -> bool {
    match s.strip_prefix(ID_PREFIX) {
        Some(rest) => {
            rest.len() == 8
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// In what form the model is live.
///
/// Mirrors `deploy::DeployKind`, which is what the deployment request already
/// used; here it becomes part of the record instead of dying with the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentKind {
    /// A set of slices that together cover the model.
    Shards,
    /// A whole GGUF on a single machine.
    Whole,
    /// A model split by tensor parallelism between a primary and K workers.
    TensorParallel,
    /// A deployment with one whole-model route and another of slices.
    Mixed,
}

/// Which model this deployment serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeploymentIdentity {
    /// Hash of the whole GGUF, computed on the file chosen by the operator.
    Sha256 { sha256: String },
    /// Identity of the manifest that produced the slices.
    PomId { pom_id: String },
}

/// Which model this deployment serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentModel {
    /// The label the operator sees and that the requests name.
    pub label: String,
    pub total_layers: u32,
    /// The slicing run that produced the parts, when there is one. It ties the
    /// deployment to the exact artifact it is running. See `models/pom.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pom_id: Option<String>,
    /// Proof of the artifact's identity. Old records do not have this field
    /// and cannot be used to undeploy safely without a path proof
    /// on the same peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DeploymentIdentity>,
}

/// A node of the deployment, and the configuration it came up with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentNode {
    pub node_id: String,
    /// Readable name of the machine at deployment time, so the screen does
    /// not have to resolve a 64-character `node_id`.
    #[serde(default)]
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub peer_index: u32,
    pub peer_count: u32,
    #[serde(default)]
    pub primary: bool,
    /// The part this node loads, content-addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_sha256: Option<String>,
    /// The file this node loaded, on its own disk. For a whole model it is the
    /// GGUF itself; for a slice without `part_sha256` (old run) it is the only
    /// pointer to the file. Old records do not have this field and
    /// `redeploy` resolves the file through the local catalog in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_path: Option<String>,
    /// Rank and size of the group, in a tensor parallelism deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tp_rank: Option<u32>,
    /// The **effective** configuration of this node - with the per-machine
    /// adjustments (`n_gpu_layers`, `offload_kqv`) already resolved over the
    /// deploy's parameters.
    ///
    /// Storing the result, and not the base parameters plus the adjustments, is
    /// what makes "with what configuration did this machine come up?" a read
    /// instead of a reconstruction.
    pub config: crate::webui::RuntimeParams,
}

/// A configuration this deployment has already run, under the same `deploy_id`.
///
/// What distinguishes one version from another are **the parameters of the
/// whole deployment** (`params`). Per-machine adjustments (`n_gpu_layers`/`offload_kqv`)
/// go into the `nodes` snapshot but do not fork a version. Redeploying with
/// parameters equal to those of an existing version reactivates that version
/// instead of appending another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeploymentVersion {
    /// Monotonic within the deployment, starts at 1.
    pub version: u32,
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    /// The parameters of the whole deployment this version came up with.
    pub params: crate::webui::RuntimeParams,
    /// Who received which range and with what effective configuration, in this version.
    pub nodes: Vec<DeploymentNode>,
    /// One machine's changes to `params` (by `node_id`): a field set here wins over the version's value for that machine
    /// only. Changing one edits this version in place; a version that starts from another one keeps the overrides of
    /// the machines it still has.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub overrides: std::collections::BTreeMap<String, NodeOverrides>,
}

/// The fields of a deployment's parameters that one machine may set for itself. A field left out inherits the version's
/// value. Changing one reloads that machine with the merged parameters (`reconfigure`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_gpu_layers: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload_kqv: Option<bool>,
    /// The machine's whole configuration, as its inspector set it when it joined. It replaces the version's parameters
    /// for this machine only (the version keeps its own); the scalar fields above still apply on top of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<crate::webui::RuntimeParams>,
}

impl NodeOverrides {
    pub fn is_empty(&self) -> bool {
        self.n_ctx.is_none()
            && self.max_sessions.is_none()
            && self.n_gpu_layers.is_none()
            && self.offload_kqv.is_none()
            && self.params.is_none()
    }

    /// The parameters this machine runs with: its own whole configuration when it has one (else `params`), with its own
    /// scalar fields in place.
    pub fn apply(&self, params: &crate::webui::RuntimeParams) -> crate::webui::RuntimeParams {
        let mut merged = self.params.clone().unwrap_or_else(|| params.clone());
        if let Some(v) = self.n_ctx {
            merged.n_ctx = v;
        }
        if let Some(v) = self.max_sessions {
            merged.max_sessions = v;
        }
        if let Some(v) = self.n_gpu_layers {
            merged.n_gpu_layers = v;
        }
        if let Some(v) = self.offload_kqv {
            merged.offload_kqv = v;
        }
        merged
    }

    /// Refuses values the engine cannot run with, before anything is reloaded.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.n_ctx == Some(0) {
            return Err("n_ctx must be at least 1".into());
        }
        if self.max_sessions == Some(0) {
            return Err("max_sessions must be at least 1".into());
        }
        Ok(())
    }
}

/// The record of a deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub deployment_version: u32,
    pub deploy_id: String,
    pub kind: DeploymentKind,
    pub model: DeploymentModel,
    pub created_at: String,
    /// `node_id` of whoever coordinated the deployment.
    #[serde(default)]
    pub created_by: String,
    /// When this deployment became the preferred one for its model.
    ///
    /// It is what decides, between two deployments of the same model, which one
    /// serves a request that only says `"qwen3:8b"`. **Derived, never elected**: the
    /// active one is the one with the most recent `activated_at`, so two nodes that
    /// disagree converge on the same answer with no consensus protocol. See [`active`].
    pub activated_at: String,
    /// The flattened list of the active version - kept in sync with
    /// `versions[active_version]` by [`Deployment::sync_active_nodes`]. The whole
    /// runtime (redeploy, undeploy, routing, screen) reads from here.
    pub nodes: Vec<DeploymentNode>,
    /// The history of configurations of this `deploy_id`. A v1 record arrives
    /// here empty and [`Deployment::ensure_versions`] synthesizes v1.
    #[serde(default)]
    pub versions: Vec<DeploymentVersion>,
    /// Which version is live. `1` when `versions` has not been synthesized yet.
    #[serde(default = "one")]
    pub active_version: u32,
}

fn one() -> u32 {
    1
}

impl Deployment {
    pub fn new(
        deploy_id: String,
        kind: DeploymentKind,
        model: DeploymentModel,
        created_by: impl Into<String>,
        nodes: Vec<DeploymentNode>,
    ) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        let created_by = created_by.into();
        Self {
            deployment_version: DEPLOYMENT_VERSION,
            deploy_id,
            kind,
            model,
            created_at: now.clone(),
            created_by: created_by.clone(),
            activated_at: now.clone(),
            versions: vec![DeploymentVersion {
                version: 1,
                created_at: now,
                created_by,
                params: crate::webui::RuntimeParams::default(),
                nodes: nodes.clone(),
                overrides: Default::default(),
            }],
            active_version: 1,
            nodes,
        }
    }

    /// Sets the parameters of v1. Used by `build_record`, which only knows
    /// the parameters after building the nodes.
    pub fn with_params(mut self, params: crate::webui::RuntimeParams) -> Self {
        if let Some(v1) = self.versions.first_mut() {
            v1.params = params;
        }
        self
    }

    pub fn node(&self, node_id: &str) -> Option<&DeploymentNode> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }

    /// The version that is live.
    pub fn active(&self) -> Option<&DeploymentVersion> {
        self.versions
            .iter()
            .find(|v| v.version == self.active_version)
    }

    /// The overrides of the machines that `nodes` still holds, from the active version.
    fn carried_overrides(
        &self,
        nodes: &[DeploymentNode],
    ) -> std::collections::BTreeMap<String, NodeOverrides> {
        let active = self.active_version;
        self.versions
            .iter()
            .find(|v| v.version == active)
            .map(|v| {
                v.overrides
                    .iter()
                    .filter(|(node_id, _)| nodes.iter().any(|n| &n.node_id == *node_id))
                    .map(|(node_id, o)| (node_id.clone(), o.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One machine's overrides in the active version (empty when it has none).
    pub fn node_overrides(&self, node_id: &str) -> NodeOverrides {
        let active = self.active_version;
        self.versions
            .iter()
            .find(|v| v.version == active)
            .and_then(|v| v.overrides.get(node_id).cloned())
            .unwrap_or_default()
    }

    /// Sets one machine's overrides in the active version. Empty removes them. It edits the version in place: a change
    /// of overrides is not a new version. Error if the machine is not in the deployment.
    pub fn set_node_overrides(&mut self, node_id: &str, overrides: NodeOverrides) -> Result<()> {
        self.ensure_versions();
        if !self.nodes.iter().any(|n| n.node_id == node_id) {
            bail!("the machine {node_id} is not in this deployment");
        }
        let active = self.active_version;
        if let Some(v) = self.versions.iter_mut().find(|v| v.version == active) {
            if overrides.is_empty() {
                v.overrides.remove(node_id);
            } else {
                v.overrides.insert(node_id.to_owned(), overrides);
            }
        }
        Ok(())
    }

    /// Writes `self.nodes` back into the active version's snapshot. Used after
    /// the redeploy records, per node, the configuration the machine actually
    /// accepted.
    pub fn commit_active_nodes(&mut self) {
        let active = self.active_version;
        if let Some(v) = self.versions.iter_mut().find(|v| v.version == active) {
            v.nodes = self.nodes.clone();
        }
    }

    /// Copies the active version's nodes into `self.nodes`, the list the runtime reads.
    fn sync_active_nodes(&mut self) {
        if let Some(nodes) = self
            .versions
            .iter()
            .find(|v| v.version == self.active_version)
            .map(|v| v.nodes.clone())
        {
            self.nodes = nodes;
        }
    }

    /// Synthesizes v1 of a legacy v1 record (or one coming from an old peer),
    /// which arrives with `versions` empty. Idempotent.
    pub fn ensure_versions(&mut self) {
        if !self.versions.is_empty() {
            return;
        }
        self.versions.push(DeploymentVersion {
            version: 1,
            created_at: self.created_at.clone(),
            created_by: self.created_by.clone(),
            params: self
                .nodes
                .first()
                .map(|n| n.config.clone())
                .unwrap_or_default(),
            nodes: self.nodes.clone(),
            overrides: Default::default(),
        });
        self.active_version = 1;
    }

    /// Makes version `v` the active one, syncs `self.nodes` and restamps
    /// `activated_at`. Error if `v` does not exist.
    pub fn set_active_version(&mut self, v: u32) -> Result<()> {
        if !self.versions.iter().any(|ver| ver.version == v) {
            let available: Vec<u32> = self.versions.iter().map(|ver| ver.version).collect();
            bail!("version {v} does not exist (available: {available:?})");
        }
        self.active_version = v;
        self.activated_at = chrono::Utc::now().to_rfc3339();
        self.sync_active_nodes();
        Ok(())
    }

    /// Redeploys with the whole deployment's `params`.
    ///
    /// If a version with those `params` already exists, it reuses its number,
    /// applies the new `nodes` (which carry the per-machine adjustments) and makes
    /// it active. Otherwise it appends `max(version) + 1`. It always restamps
    /// `activated_at` and syncs `self.nodes`. Returns the active version's number.
    pub fn upsert_version(
        &mut self,
        params: crate::webui::RuntimeParams,
        nodes: Vec<DeploymentNode>,
        created_by: impl Into<String>,
    ) -> u32 {
        self.ensure_versions();
        let now = chrono::Utc::now().to_rfc3339();
        let target = match self.versions.iter().position(|v| v.params == params) {
            Some(i) => {
                self.versions[i].nodes = nodes;
                self.versions[i].version
            }
            None => {
                let number = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
                let overrides = self.carried_overrides(&nodes);
                self.versions.push(DeploymentVersion {
                    version: number,
                    created_at: now.clone(),
                    created_by: created_by.into(),
                    params,
                    nodes,
                    overrides,
                });
                number
            }
        };
        self.active_version = target;
        self.activated_at = now;
        self.sync_active_nodes();
        target
    }

    /// Appends a version even when the parameters did not change.
    ///
    /// Topology changes, such as adding or removing a machine, are
    /// history events in their own right. Reusing the version because the
    /// parameters are equal would erase precisely the set of machines the
    /// operation changed.
    pub fn append_version(
        &mut self,
        params: crate::webui::RuntimeParams,
        nodes: Vec<DeploymentNode>,
        created_by: impl Into<String>,
    ) -> u32 {
        self.ensure_versions();
        let now = chrono::Utc::now().to_rfc3339();
        let number = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
        let overrides = self.carried_overrides(&nodes);
        self.versions.push(DeploymentVersion {
            version: number,
            created_at: now.clone(),
            created_by: created_by.into(),
            params,
            nodes,
            overrides,
        });
        self.active_version = number;
        self.activated_at = now;
        self.sync_active_nodes();
        number
    }

    /// Do the nodes' ranges cover `0..total_layers` with no gap?
    ///
    /// In a tensor parallelism group everyone covers the whole model, so
    /// the question is a different one: it is enough for there to be a node.
    pub fn covers_the_model(&self) -> bool {
        if self.kind == DeploymentKind::TensorParallel {
            return !self.nodes.is_empty();
        }
        let mut ranges: Vec<(u32, u32)> = self
            .nodes
            .iter()
            .map(|n| (n.start_layer, n.end_layer))
            .collect();
        ranges.sort_unstable();
        let mut next = 0u32;
        for (start, end) in ranges {
            if start > next {
                return false;
            }
            next = next.max(end.saturating_add(1));
        }
        next == self.model.total_layers
    }

    pub fn validate(&self) -> Result<()> {
        if self.deployment_version > DEPLOYMENT_VERSION {
            bail!(
                "record version {} is newer than this build (supports up to {DEPLOYMENT_VERSION})",
                self.deployment_version
            );
        }
        if !is_deploy_id(&self.deploy_id) {
            bail!("invalid deploy_id: {:?}", self.deploy_id);
        }
        if self.model.label.trim().is_empty() {
            bail!("deployment without a model label");
        }
        if self.nodes.is_empty()
            && self
                .active()
                .is_none_or(|version| !version.nodes.is_empty())
        {
            bail!("deployment without nodes");
        }
        for n in &self.nodes {
            if n.start_layer > n.end_layer {
                bail!(
                    "{} has an inverted range ({}-{})",
                    n.node_id,
                    n.start_layer,
                    n.end_layer
                );
            }
        }
        if !self.versions.is_empty() {
            let mut seen = HashSet::new();
            for v in &self.versions {
                if v.version == 0 {
                    bail!("version 0 is not valid");
                }
                if !seen.insert(v.version) {
                    bail!("version {} repeated", v.version);
                }
                for n in &v.nodes {
                    if n.start_layer > n.end_layer {
                        bail!(
                            "version {}: {} has an inverted range ({}-{})",
                            v.version,
                            n.node_id,
                            n.start_layer,
                            n.end_layer
                        );
                    }
                }
            }
            if !seen.contains(&self.active_version) {
                bail!(
                    "active_version {} is not among the versions {:?}",
                    self.active_version,
                    seen
                );
            }
        }
        Ok(())
    }
}

/// The deployment with the same **shape** as a new request: same `kind`,
/// same proven model identity and the same exact set of
/// `(node_id, start_layer, end_layer, tp_rank)`. It is what decides whether a new
/// `POST /api/ui/deploy` reuses a `deploy_id` (appending a version) or
/// mints a new id.
///
/// A pure function like `active`: no network, so that it is testable and so
/// the nodes converge without a protocol.
pub fn matching_shape<'a>(
    deployments: &'a [Deployment],
    kind: DeploymentKind,
    identity: &DeploymentIdentity,
    ranges: &[(String, u32, u32, Option<u32>)],
) -> Option<&'a Deployment> {
    let mut target: Vec<(String, u32, u32, Option<u32>)> = ranges.to_vec();
    target.sort();
    deployments.iter().find(|d| {
        if d.kind != kind || d.model.identity.as_ref() != Some(identity) {
            return false;
        }
        let mut own: Vec<(String, u32, u32, Option<u32>)> = d
            .nodes
            .iter()
            .map(|n| (n.node_id.clone(), n.start_layer, n.end_layer, n.tp_rank))
            .collect();
        own.sort();
        own == target
    })
}

/// Which deployment serves a request that only says the model's name.
///
/// The most recently activated among those with that label. A tie is broken by
/// the `deploy_id`, so that the answer does not depend on the order in which the
/// records were read from disk - two machines have to reach the same conclusion.
pub fn active<'a>(deployments: &'a [Deployment], label: &str) -> Option<&'a Deployment> {
    deployments
        .iter()
        .filter(|d| d.model.label == label && !d.nodes.is_empty())
        .max_by(|a, b| {
            a.activated_at
                .cmp(&b.activated_at)
                .then_with(|| a.deploy_id.cmp(&b.deploy_id))
        })
}

/// Splits `"qwen3:8b@d-4f2a91c3"` into `("qwen3:8b", Some("d-4f2a91c3"))`.
///
/// Without the suffix, the request did not choose a deployment and the one that
/// answers is the active one - which is how every client written before this
/// keeps working. An `@` followed by something that is not an id stays **in the
/// name**: there are model labels with `@` in the real world, and silently
/// swallowing a piece of one would be worse than not finding the model.
pub fn split_selector(model: &str) -> (&str, Option<&str>) {
    match model.rsplit_once('@') {
        Some((name, id)) if is_deploy_id(id) => (name, Some(id)),
        _ => (model, None),
    }
}

// ── On disk ──────────────────────────────────────────────────────────────────

/// The legacy folder of the records: `<POM_HOME>/deployments/`.
#[allow(dead_code)]
pub fn dir() -> PathBuf {
    crate::config::project_root().join("deployments")
}

#[allow(dead_code)]
pub fn path_in(dir: &Path, deploy_id: &str) -> Result<PathBuf> {
    if !is_deploy_id(deploy_id) {
        bail!("invalid deploy_id: {deploy_id:?}");
    }
    Ok(dir.join(format!("{deploy_id}.json")))
}

/// Writes the record without leaving a half-written file if the machine goes down midway.
///
/// Same pattern as `pom::write_atomic`: a temporary file beside it and `rename`,
/// which is atomic on the same file system.
#[allow(dead_code)]
pub fn save_in(dir: &Path, d: &Deployment) -> Result<PathBuf> {
    d.validate()?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let destination = path_in(dir, &d.deploy_id)?;
    let temp = dir.join(format!(".{}.{}.tmp", d.deploy_id, std::process::id()));
    let json = serde_json::to_vec_pretty(d).context("serialize deployment")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destination) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("rename to {}", destination.display()));
    }
    Ok(destination)
}

#[allow(dead_code)]
pub fn save(d: &Deployment) -> Result<PathBuf> {
    save_in(&dir(), d)
}

#[allow(dead_code)]
pub fn load(path: &Path) -> Result<Deployment> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let d: Deployment =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    d.validate()
        .with_context(|| format!("invalid record in {}", path.display()))?;
    Ok(d)
}

/// All the records of a folder, sorted by id so the listing is
/// stable across calls.
#[allow(dead_code)]
pub fn load_dir(dir: &Path) -> Vec<Deployment> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load(&p) {
            Ok(d) => out.push(d),
            // A broken record must not bring down the whole listing: the
            // other deployments are still live and need to show up.
            Err(e) => tracing::warn!("ignoring record {}: {e:#}", p.display()),
        }
    }
    out.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
    out
}

#[allow(dead_code)]
pub fn all() -> Vec<Deployment> {
    load_dir(&dir())
}

/// Forgets a deployment on this node. Idempotent.
#[allow(dead_code)]
pub fn remove_in(dir: &Path, deploy_id: &str) -> Result<()> {
    let p = path_in(dir, deploy_id)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remover {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no(id: &str, start: u32, end: u32, index: u32, total: u32) -> DeploymentNode {
        DeploymentNode {
            node_id: id.into(),
            hostname: id.into(),
            start_layer: start,
            end_layer: end,
            peer_index: index,
            peer_count: total,
            primary: index == 0,
            part_sha256: None,
            source_model_path: None,
            tp_rank: None,
            config: crate::webui::RuntimeParams::default(),
        }
    }

    fn deployment(id: &str, label: &str, nos: Vec<DeploymentNode>) -> Deployment {
        Deployment::new(
            id.into(),
            DeploymentKind::Shards,
            DeploymentModel {
                label: label.into(),
                total_layers: 32,
                pom_id: None,
                identity: None,
            },
            "coordenador",
            nos,
        )
    }

    #[test]
    fn a_new_id_is_valid_and_does_not_repeat() {
        let a = new_deploy_id();
        let b = new_deploy_id();
        assert!(is_deploy_id(&a), "{a}");
        assert_ne!(a, b);
    }

    /// An id coming from the network becomes a file name: if `..` got through, a
    /// peer would choose where to write on the receiver's disk.
    #[test]
    fn an_id_that_is_not_an_id_does_not_become_a_path() {
        assert!(!is_deploy_id("../../etc/passwd"));
        assert!(!is_deploy_id("d-../../x"));
        assert!(!is_deploy_id("d-ABCDEF12"), "hex is lowercase");
        assert!(!is_deploy_id("d-123"), "too short");
        assert!(!is_deploy_id("4f2a91c3"), "without a prefix");
        assert!(path_in(Path::new("/tmp/d"), "../x").is_err());
    }

    /// The case that did not exist: **the same model**, two deployments.
    #[test]
    fn two_deployments_of_the_same_model_coexist() {
        let one = deployment("d-aaaaaaaa", "qwen3:8b", vec![no("n1", 0, 31, 0, 1)]);
        let mut other = deployment(
            "d-bbbbbbbb",
            "qwen3:8b",
            vec![no("n2", 0, 15, 0, 2), no("n3", 16, 31, 1, 2)],
        );
        other.activated_at = "2030-01-01T00:00:00Z".into();

        let all = vec![one, other];
        assert_eq!(
            active(&all, "qwen3:8b").unwrap().deploy_id,
            "d-bbbbbbbb",
            "the active one is the one with the most recent activation"
        );
        assert!(active(&all, "nao-existe").is_none());
    }

    /// A tie has to be broken the same way on both machines, otherwise one sends
    /// the request to one deployment and the other to the other.
    #[test]
    fn an_activation_tie_is_resolved_stably() {
        let mut a = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        let mut b = deployment("d-bbbbbbbb", "m", vec![no("n2", 0, 31, 0, 1)]);
        a.activated_at = "2026-01-01T00:00:00Z".into();
        b.activated_at = "2026-01-01T00:00:00Z".into();

        let in_one_order = vec![a.clone(), b.clone()];
        let on_the_other = vec![b, a];
        assert_eq!(
            active(&in_one_order, "m").unwrap().deploy_id,
            active(&on_the_other, "m").unwrap().deploy_id
        );
    }

    #[test]
    fn the_selector_separates_model_from_deployment() {
        assert_eq!(split_selector("qwen3:8b"), ("qwen3:8b", None));
        assert_eq!(
            split_selector("qwen3:8b@d-4f2a91c3"),
            ("qwen3:8b", Some("d-4f2a91c3"))
        );
        // A label with `@` that does not end in an id stays whole.
        assert_eq!(
            split_selector("org@example/modelo"),
            ("org@example/modelo", None)
        );
        assert_eq!(split_selector("modelo@sei-la"), ("modelo@sei-la", None));
    }

    #[test]
    fn layer_coverage() {
        assert!(deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]).covers_the_model());
        assert!(deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)]
        )
        .covers_the_model());
        // Gap at 16-19.
        assert!(!deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 20, 31, 1, 2)]
        )
        .covers_the_model());
    }

    #[test]
    fn write_reread_and_forget() {
        let tmp =
            std::env::temp_dir().join(format!("pom-deploy-{}", uuid::Uuid::new_v4().simple()));
        let d = deployment(
            "d-4f2a91c3",
            "qwen3:8b",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)],
        );

        let written = save_in(&tmp, &d).unwrap();
        assert_eq!(load(&written).unwrap(), d);
        assert_eq!(load_dir(&tmp), vec![d.clone()]);

        remove_in(&tmp, &d.deploy_id).unwrap();
        assert!(load_dir(&tmp).is_empty());
        // Forgetting what was already forgotten is not an error.
        remove_in(&tmp, &d.deploy_id).unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_record_without_a_node_or_with_a_malformed_id_is_refused() {
        let mut d = deployment("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.nodes.clear();
        assert!(d.validate().is_err());

        let mut d = deployment("d-4f2a91c3", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.deploy_id = "nao-e-id".into();
        assert!(d.validate().is_err());
    }

    fn params_with_ctx(n_ctx: u32) -> crate::webui::RuntimeParams {
        crate::webui::RuntimeParams {
            n_ctx,
            ..Default::default()
        }
    }

    /// A legacy v1 record arrives without `versions`; v1 is synthesized from `nodes`.
    #[test]
    fn ensure_versions_synthesizes_the_v1_of_a_legacy_record() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.versions.clear();
        d.active_version = 7; // junk value from an old record

        d.ensure_versions();

        assert_eq!(d.versions.len(), 1);
        assert_eq!(d.versions[0].version, 1);
        assert_eq!(d.active_version, 1);
        assert_eq!(d.versions[0].nodes, d.nodes);
        // Idempotent.
        d.ensure_versions();
        assert_eq!(d.versions.len(), 1);
    }

    #[test]
    fn node_overrides_apply_only_the_fields_they_set() {
        let base = params_with_ctx(8192);
        let merged = NodeOverrides { n_ctx: Some(4096), offload_kqv: Some(false), ..Default::default() }.apply(&base);
        assert_eq!(merged.n_ctx, 4096);
        assert!(!merged.offload_kqv);
        assert_eq!(merged.max_sessions, base.max_sessions, "a field left out inherits the version's value");
        assert!(NodeOverrides::default().is_empty());
        assert!(NodeOverrides { n_ctx: Some(0), ..Default::default() }.validate().is_err());
        assert!(NodeOverrides { max_sessions: Some(0), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn a_machine_override_edits_the_active_version_without_forking_it() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1), no("n2", 32, 47, 1, 1)]);
        d.set_node_overrides("n1", NodeOverrides { n_ctx: Some(2048), ..Default::default() }).unwrap();
        assert_eq!(d.versions.len(), 1, "a change of overrides is not a new version");
        assert_eq!(d.node_overrides("n1").n_ctx, Some(2048));
        assert!(d.node_overrides("n2").is_empty());
        // clearing removes them
        d.set_node_overrides("n1", NodeOverrides::default()).unwrap();
        assert!(d.node_overrides("n1").is_empty());
        // a machine that is not in the deployment is refused
        assert!(d.set_node_overrides("ghost", NodeOverrides { n_ctx: Some(1), ..Default::default() }).is_err());
    }

    #[test]
    fn a_new_version_keeps_the_overrides_of_the_machines_it_still_has() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.set_node_overrides("n1", NodeOverrides { max_sessions: Some(4), ..Default::default() }).unwrap();
        let params = d.active().unwrap().params.clone();
        let nodes = vec![no("n1", 0, 31, 0, 1), no("n2", 0, 31, 0, 1)];
        let v = d.append_version(params, nodes, "operador");
        assert_eq!(v, 2);
        assert_eq!(d.node_overrides("n1").max_sessions, Some(4), "the machine kept its override");
        assert!(d.node_overrides("n2").is_empty());
    }

    #[test]
    fn upsert_version_reuses_the_version_with_equal_params_and_appends_when_they_differ() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.versions[0].params = params_with_ctx(8192);
        d.sync_active_nodes();
        let enabled_v1 = d.activated_at.clone();

        // Same params -> same version, without appending.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let v = d.upsert_version(params_with_ctx(8192), d.nodes.clone(), "n1");
        assert_eq!(v, 1);
        assert_eq!(d.versions.len(), 1);
        assert_ne!(d.activated_at, enabled_v1, "activated_at restamped");

        // Different params -> v2, active, nodes synced.
        let nodes_v2 = vec![no("n1", 0, 31, 0, 1)];
        let v = d.upsert_version(params_with_ctx(16384), nodes_v2.clone(), "n1");
        assert_eq!(v, 2);
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.active_version, 2);
        assert_eq!(d.nodes, nodes_v2);
        assert_eq!(d.active().unwrap().params.n_ctx, 16384);
    }

    #[test]
    fn append_version_records_a_topology_change_even_with_equal_params() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        let params = d.active().unwrap().params.clone();
        let nodes = vec![no("n1", 0, 31, 0, 1), no("n2", 0, 31, 0, 1)];

        let version = d.append_version(params.clone(), nodes.clone(), "operador");

        assert_eq!(version, 2);
        assert_eq!(d.active_version, 2);
        assert_eq!(d.nodes, nodes);
        assert_eq!(d.versions.len(), 2);
        assert_eq!(d.versions[1].params, params);
        assert_eq!(d.versions[1].created_by, "operador");
    }

    #[test]
    fn a_mixed_deployment_covers_both_heads_and_the_last_removal_is_valid() {
        let mut d = deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("slices", 0, 15, 0, 2), no("cauda", 16, 31, 1, 2)],
        );
        d.kind = DeploymentKind::Mixed;
        let whole = no("inteiro", 0, 31, 0, 1);

        let version = d.append_version(
            d.active().unwrap().params.clone(),
            vec![whole, d.nodes[0].clone(), d.nodes[1].clone()],
            "operador",
        );
        assert_eq!(version, 2);
        assert!(d.covers_the_model());

        let version = d.append_version(
            d.active().unwrap().params.clone(),
            vec![d.nodes[0].clone(), d.nodes[1].clone()],
            "operador",
        );
        assert_eq!(version, 3);
        assert!(d.validate().is_ok());
    }

    #[test]
    fn a_deployment_without_machines_is_only_inactive_in_the_default_selection() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.append_version(d.active().unwrap().params.clone(), Vec::new(), "operador");

        assert!(d.nodes.is_empty());
        assert!(d.validate().is_ok());
        assert!(active(&[d], "m").is_none());
    }

    #[test]
    fn set_active_version_swaps_the_nodes_and_refuses_a_missing_version() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), vec![no("n1", 0, 15, 0, 1)], "n1");
        assert_eq!(d.active_version, 2);

        d.set_active_version(1).unwrap();
        assert_eq!(d.active_version, 1);
        assert_eq!(d.nodes[0].end_layer, 31);

        assert!(d.set_active_version(9).is_err());
    }

    #[test]
    fn validate_refuses_a_repeated_version_and_an_orphan_active() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), d.nodes.clone(), "n1");

        let mut repeated = d.clone();
        repeated.versions[1].version = 1;
        assert!(repeated.validate().is_err());

        let mut orphan = d.clone();
        orphan.active_version = 99;
        assert!(orphan.validate().is_err());
    }

    #[test]
    fn matching_shape_matches_by_identity_and_ranges() {
        let mut d = deployment(
            "d-aaaaaaaa",
            "m",
            vec![no("n1", 0, 15, 0, 2), no("n2", 16, 31, 1, 2)],
        );
        d.model.identity = Some(DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        });
        let all = vec![d];
        let id = DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        };

        // Same shape, order of the targets swapped -> matches.
        let ranges = vec![
            ("n2".to_string(), 16, 31, None),
            ("n1".to_string(), 0, 15, None),
        ];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &ranges).is_some());

        // A different range -> does not match.
        let others = vec![
            ("n1".to_string(), 0, 19, None),
            ("n2".to_string(), 20, 31, None),
        ];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &others).is_none());

        // Different identity -> does not match.
        let other_id = DeploymentIdentity::PomId {
            pom_id: "pom-2".into(),
        };
        assert!(matching_shape(&all, DeploymentKind::Shards, &other_id, &ranges).is_none());
    }

    /// A record without `identity` (a run from before the proof) never deduplicates.
    #[test]
    fn matching_shape_ignores_a_record_without_identity() {
        let d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        assert!(d.model.identity.is_none());
        let all = vec![d];
        let id = DeploymentIdentity::PomId {
            pom_id: "pom-1".into(),
        };
        let ranges = vec![("n1".to_string(), 0, 31, None)];
        assert!(matching_shape(&all, DeploymentKind::Shards, &id, &ranges).is_none());
    }

    #[test]
    fn versions_survive_serde() {
        let mut d = deployment("d-aaaaaaaa", "m", vec![no("n1", 0, 31, 0, 1)]);
        d.upsert_version(params_with_ctx(16384), d.nodes.clone(), "n1");
        let json = serde_json::to_string(&d).unwrap();
        let loaded: Deployment = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, d);
        assert_eq!(loaded.versions.len(), 2);
        assert_eq!(loaded.active_version, 2);
    }
}

#[cfg(test)]
mod node_overrides_tests {
    use super::*;

    #[test]
    fn a_machine_whole_configuration_replaces_the_versions_for_that_machine_only() {
        let version: crate::webui::RuntimeParams = serde_json::from_value(serde_json::json!({})).unwrap();
        let mut machine = version.clone();
        machine.n_ctx = 4096;
        machine.offload_kqv = false;
        let overrides = NodeOverrides { params: Some(machine.clone()), ..Default::default() };
        assert!(!overrides.is_empty());
        assert_eq!(overrides.apply(&version), machine);
        // the scalar fields still win over the machine's whole configuration
        let scalar = NodeOverrides { params: Some(machine.clone()), n_ctx: Some(2048), ..Default::default() };
        assert_eq!(scalar.apply(&version).n_ctx, 2048);
        assert!(NodeOverrides::default().is_empty());
    }
}

