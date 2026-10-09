//! The manifest of a slicing: the `.pom`.
//!
//! Before this, the identity of a slice was the **file name**,
//! `<base>_<inicio>_<fim>.gguf`, built in one place and read back in another. A
//! model whose name already ended in two numbers left the range ambiguous, the
//! model name only existed in the file name and in the folder that held it, and
//! two different models cut at the same range were indistinguishable to whoever
//! picks the destination machine. Nothing, at any point, checked that the bytes
//! were the ones they claimed to be.
//!
//! Here the identity becomes the **content**: each part is written with its own
//! sha256 as its name, and the `.pom` is the list of those parts - the role a
//! `.torrent` plays. Whoever deploys sends the manifest (a few kB) and the list
//! of hashes each node has to load; the node fetches the missing parts from
//! **any** peer that has them, and checks the hash it asked for against the
//! bytes that arrived.
//!
//! The `pom_id` is the sha256 of the original GGUF. One folder per model keeps
//! all the slicing alternatives and also the Tensor Parallelism shards in the
//! separate `tp_parts` field.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Format version. The product has not been released yet, so this is the only
/// accepted contract: identity by the hash of the original GGUF.
pub const POM_VERSION: u32 = 1;

/// Manifest extension.
pub const POM_EXT: &str = "pom";

/// How the model was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PomKind {
    /// Cut by layers: each part covers `start_layer..=end_layer`.
    Layers,
    /// Cut by tensor: each part is one rank of the full width.
    TensorParallel,
    /// The manifest contains layer slices and tensor shards.
    Combined,
}

/// Which rank of a tensor-parallel group a part belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomTp {
    pub rank: u32,
    pub size: u32,
}

/// One part: a GGUF file, named after its own hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomPart {
    /// sha256 of the file, in lowercase hex. It is also its name on disk.
    pub sha256: String,
    pub bytes: u64,
    pub start_layer: u32,
    pub end_layer: u32,
    /// Carries the embeddings.
    #[serde(default)]
    pub is_first: bool,
    /// Carries the output head.
    #[serde(default)]
    pub is_last: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tp: Option<PomTp>,
}

/// Which model these parts came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomModel {
    /// Short name, also the name of the folder on disk.
    pub name: String,
    /// Label the pipeline planner matches across peers.
    pub label: String,
    pub total_layers: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

/// The manifest of an original model and of the parts available locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PomManifest {
    pub pom_version: u32,
    /// sha256 of the original GGUF that all the parts came from.
    pub pom_id: String,
    pub model: PomModel,
    pub kind: PomKind,
    pub created_at: String,
    /// `node_id` of whoever coordinated the round.
    #[serde(default)]
    pub created_by: String,
    pub parts: Vec<PomPart>,
    /// Tensor Parallelism shards of the same original GGUF.
    #[serde(default)]
    pub tp_parts: Vec<PomPart>,
}

impl PomManifest {
    /// Builds the single manifest of the original GGUF.
    pub fn new(
        mut model: PomModel,
        kind: PomKind,
        created_by: impl Into<String>,
        source_sha256: impl Into<String>,
        parts: Vec<PomPart>,
    ) -> Self {
        model.name = canonical_model_name(&model.name);
        Self {
            pom_version: POM_VERSION,
            pom_id: source_sha256.into(),
            model,
            kind,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: created_by.into(),
            parts,
            tp_parts: Vec::new(),
        }
    }

    /// Merges what two machines know about the same original model.
    ///
    /// The local manifest is the base: a part that only exists in it is never
    /// removed because the remote peer does not know it yet.
    pub fn merge_from(&mut self, incoming: PomManifest) -> Result<()> {
        if self.pom_version != POM_VERSION || incoming.pom_version != POM_VERSION {
            bail!("merge requires the manifest of version {POM_VERSION}");
        }
        if self.pom_id != incoming.pom_id {
            bail!(
                "manifests belong to different models: {} and {}",
                &self.pom_id[..12.min(self.pom_id.len())],
                &incoming.pom_id[..12.min(incoming.pom_id.len())]
            );
        }
        self.validate()?;
        incoming.validate()?;
        if self.model.total_layers != incoming.model.total_layers {
            bail!(
                "manifests of model {} declare different layer totals: {} and {}",
                self.model.name,
                self.model.total_layers,
                incoming.model.total_layers
            );
        }
        if self.model.quant.is_some()
            && incoming.model.quant.is_some()
            && self.model.quant != incoming.model.quant
        {
            bail!(
                "manifests of model {} declare different quantizations",
                self.model.name
            );
        }

        merge_parts(&mut self.parts, incoming.parts, "parts")?;
        merge_parts(&mut self.tp_parts, incoming.tp_parts, "tp_parts")?;
        if self.model.label.trim().is_empty() {
            self.model.label = incoming.model.label;
        }
        if self.created_by.trim().is_empty() {
            self.created_by = incoming.created_by;
        }
        self.kind = kind_for(&self.parts, &self.tp_parts);
        self.validate()
    }

    /// Do the parts cover `0..total_layers` with no gap and no overlap?
    ///
    /// An incomplete manifest is legitimate while the round has not finished;
    /// it is whoever deploys that cannot bring up a pipeline with holes.
    pub fn complete(&self) -> bool {
        let mut next = 0u32;
        loop {
            let Some(end) = self
                .parts
                .iter()
                .filter(|p| p.start_layer == next)
                .map(|p| p.end_layer)
                .max()
            else {
                return false;
            };
            next = end.saturating_add(1);
            if next >= self.model.total_layers {
                return next == self.model.total_layers;
            }
        }
    }

    /// Tells whether a TP group of size `size` has all its ranks.
    pub fn tp_complete(&self, size: u32) -> bool {
        tp_complete_from(&self.tp_parts, Some(size))
    }

    /// Checks what can be checked without reading the files.
    pub fn validate(&self) -> Result<()> {
        if self.pom_version != POM_VERSION {
            bail!(
                "manifest version {} is not the active version {POM_VERSION}",
                self.pom_version
            );
        }
        if self.model.name.trim().is_empty() {
            bail!("manifest without a model name");
        }
        if self.parts.is_empty() && self.tp_parts.is_empty() {
            bail!("manifest without parts");
        }
        for p in self.parts.iter().chain(self.tp_parts.iter()) {
            if !is_sha256_hex(&p.sha256) {
                bail!("part with an invalid hash: {:?}", p.sha256);
            }
            if p.start_layer > p.end_layer {
                bail!(
                    "part {} has an inverted range ({}-{})",
                    &p.sha256[..12],
                    p.start_layer,
                    p.end_layer
                );
            }
        }
        if !is_sha256_hex(&self.pom_id) {
            bail!("invalid pom_id of the original GGUF: {:?}", self.pom_id);
        }
        if self.parts.iter().any(|p| p.tp.is_some()) {
            bail!("parts cannot contain Tensor Parallelism shards");
        }
        if self.tp_parts.iter().any(|p| p.tp.is_none()) {
            bail!("tp_parts requires the rank and size of the TP group");
        }
        let expected = kind_for(&self.parts, &self.tp_parts);
        if self.kind != expected {
            bail!(
                "manifest kind does not match the parts: {:?}, expected {:?}",
                self.kind,
                expected
            );
        }
        Ok(())
    }
}

fn kind_for(parts: &[PomPart], tp_parts: &[PomPart]) -> PomKind {
    match (parts.is_empty(), tp_parts.is_empty()) {
        (false, true) => PomKind::Layers,
        (true, false) => PomKind::TensorParallel,
        (false, false) => PomKind::Combined,
        (true, true) => PomKind::Layers,
    }
}

fn merge_parts(target: &mut Vec<PomPart>, incoming: Vec<PomPart>, field: &str) -> Result<()> {
    for part in incoming {
        if let Some(existing) = target.iter().find(|p| p.sha256 == part.sha256) {
            if existing != &part {
                bail!(
                    "{field} has different metadata for part {}",
                    &part.sha256[..12]
                );
            }
            continue;
        }
        target.push(part);
    }
    target.sort_by(|a, b| {
        (a.start_layer, a.end_layer, &a.sha256).cmp(&(b.start_layer, b.end_layer, &b.sha256))
    });
    Ok(())
}

fn tp_complete_from(parts: &[PomPart], requested_size: Option<u32>) -> bool {
    let Some(size) = requested_size.or_else(|| parts.first().and_then(|p| p.tp).map(|t| t.size))
    else {
        return false;
    };
    let mut ranks: Vec<u32> = parts
        .iter()
        .filter_map(|p| p.tp)
        .filter(|tp| tp.size == size)
        .map(|tp| tp.rank)
        .collect();
    ranks.sort_unstable();
    ranks.dedup();
    ranks == (0..size).collect::<Vec<_>>()
}

/// A hash is a hash: 64 lowercase hex digits and nothing else.
///
/// It works as input validation **and** as path defense: this is what
/// guarantees that a `sha256` coming from the network never becomes `../` when
/// it turns into a file name in [`part_path`].
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where a part lives: `<dir do modelo>/<sha256>.gguf`.
///
/// The extension stays so that `catalog::find_gguf` and the llama.cpp loader
/// need no exception at all; what identifies it is the stem.
pub fn part_path(model_dir: &Path, sha256: &str) -> Result<PathBuf> {
    if !is_sha256_hex(sha256) {
        bail!("invalid part hash: {sha256:?}");
    }
    Ok(model_dir.join(format!("{sha256}.gguf")))
}

/// The short identifier of the original file, the one shown in the folder name.
pub fn short_id(pom_id: &str) -> &str {
    &pom_id[..8.min(pom_id.len())]
}

/// Canonical name used in the folder and the manifest, without the GGUF file suffix.
pub fn canonical_model_name(name: &str) -> String {
    let stem = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))
        .unwrap_or(name);
    sanitize_name(stem)
}

/// The folder identified by the start of the full hash of the original GGUF.
pub fn source_run_dir(shard_dir: &Path, model_name: &str, source_sha256: &str) -> PathBuf {
    shard_dir.join(format!(
        "{}-{}",
        canonical_model_name(model_name),
        short_id(source_sha256)
    ))
}

/// Looks for the folder by the full hash, without trusting the readable name.
pub fn locate_source_dir(shard_dir: &Path, source_sha256: &str) -> Option<PathBuf> {
    if !is_sha256_hex(source_sha256) {
        return None;
    }
    for entry in std::fs::read_dir(shard_dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if load_dir(&path)
            .iter()
            .any(|m| m.pom_version == POM_VERSION && m.pom_id == source_sha256)
        {
            return Some(path);
        }
    }
    None
}

/// The folder this manifest describes.
pub fn dir_for(shard_dir: &Path, m: &PomManifest) -> PathBuf {
    source_run_dir(shard_dir, &m.model.name, &m.pom_id)
}

/// The single manifest file inside the model folder.
pub fn manifest_path(model_dir: &Path, m: &PomManifest) -> PathBuf {
    let folder = model_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}-{}",
                canonical_model_name(&m.model.name),
                short_id(&m.pom_id)
            )
        });
    model_dir.join(format!("{folder}.{POM_EXT}"))
}

struct PomLock(PathBuf);

impl Drop for PomLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_pom_lock(shard_dir: &Path, source_sha256: &str) -> Result<PomLock> {
    std::fs::create_dir_all(shard_dir)
        .with_context(|| format!("create the slices directory {}", shard_dir.display()))?;
    let path = shard_dir.join(format!(".pom-{source_sha256}.lock"));
    for _ in 0..500 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(PomLock(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e).with_context(|| format!("create lock {}", path.display())),
        }
    }
    bail!(
        "timed out waiting for the manifest lock {}",
        source_sha256
    )
}

/// Merges the manifest into the local inventory of the same original model.
pub fn merge_manifest(shard_dir: &Path, incoming: &PomManifest) -> Result<(PathBuf, PomManifest)> {
    if incoming.pom_version != POM_VERSION {
        bail!("merge requires the manifest of version {POM_VERSION}");
    }
    incoming.validate()?;
    let _lock = acquire_pom_lock(shard_dir, &incoming.pom_id)?;
    let destination = locate_source_dir(shard_dir, &incoming.pom_id)
        .unwrap_or_else(|| source_run_dir(shard_dir, &incoming.model.name, &incoming.pom_id));
    std::fs::create_dir_all(&destination)
        .with_context(|| format!("create the model directory {}", destination.display()))?;

    let existing = load_dir(&destination)
        .into_iter()
        .find(|m| m.pom_version == POM_VERSION && m.pom_id == incoming.pom_id);
    let mut aggregate = existing.unwrap_or_else(|| incoming.clone());
    if aggregate.parts != incoming.parts || aggregate.tp_parts != incoming.tp_parts {
        aggregate.merge_from(incoming.clone())?;
    }
    write_atomic(&destination, &aggregate)?;
    Ok((destination, aggregate))
}

/// Moves the parts from a working folder to the model folder and merges the
/// manifest. The identity comes from the hash of the original GGUF, so a new
/// way of partitioning the same model adds parts to the same folder.
/// Repeated parts are discarded by hash.
pub fn finalize_run(shard_dir: &Path, temp: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    let destination = locate_source_dir(shard_dir, &m.pom_id)
        .unwrap_or_else(|| source_run_dir(shard_dir, &m.model.name, &m.pom_id));
    std::fs::create_dir_all(&destination).with_context(|| format!("create {}", destination.display()))?;

    for part in m.parts.iter().chain(m.tp_parts.iter()) {
        let from = part_path(temp, &part.sha256)?;
        let to = part_path(&destination, &part.sha256)?;
        if !from.is_file() {
            // The part was reused from an earlier round and never went
            // through the working folder: nothing to move.
            continue;
        }
        if to.is_file() {
            let _ = std::fs::remove_file(&from);
            continue;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
    }

    let (destination, _) = merge_manifest(shard_dir, m)?;
    // The working folder only goes away if it is empty: an unexpected file
    // there is a sign of a defect, and deleting it would hide the evidence.
    let _ = std::fs::remove_dir(temp);
    Ok(destination)
}

/// A working folder for a round that does not have a name yet.
pub fn scratch_dir(shard_dir: &Path) -> PathBuf {
    shard_dir.join(format!(".slicing-{}", uuid::Uuid::new_v4().simple()))
}

/// A name that can become a folder without surprises.
pub fn sanitize_name(name: &str) -> String {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let clean = clean.trim_matches(['-', '.', '_']).to_string();
    if clean.is_empty() {
        "model".to_string()
    } else {
        clean
    }
}

/// Writes the manifest without leaving a half file if the machine crashes midway.
///
/// Same pattern as the merge in `webui.rs`: it writes to a temporary file next
/// to it and only then renames, because `rename` on the same file system is
/// atomic and a truncated `.pom` would be read as "model without parts".
pub fn write_atomic(model_dir: &Path, m: &PomManifest) -> Result<PathBuf> {
    m.validate()?;
    std::fs::create_dir_all(model_dir).with_context(|| format!("create {}", model_dir.display()))?;

    let destination = manifest_path(model_dir, m);
    let temp = model_dir.join(format!(".{}.{}.tmp", m.pom_id, std::process::id()));

    let json = serde_json::to_vec_pretty(m).context("serialize manifest")?;
    {
        let mut f =
            std::fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        f.write_all(&json)?;
        // Without this the rename can reach the disk before the content.
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, &destination) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("rename to {}", destination.display()));
    }
    Ok(destination)
}

/// Reads and validates a manifest.
pub fn load(path: &Path) -> Result<PomManifest> {
    let raw = std::fs::read(path).with_context(|| format!("ler {}", path.display()))?;
    let m: PomManifest =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    m.validate()
        .with_context(|| format!("invalid manifest in {}", path.display()))?;
    Ok(m)
}

/// All the manifests of a model folder.
pub fn load_dir(model_dir: &Path) -> Vec<PomManifest> {
    let Ok(rd) = std::fs::read_dir(model_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some(POM_EXT) {
            continue;
        }
        match load(&p) {
            Ok(m) => out.push(m),
            // A broken manifest cannot take down the whole scan: the rest of
            // the folder stays usable.
            Err(e) => tracing::warn!("ignoring manifest {}: {e:#}", p.display()),
        }
    }
    out
}

/// Locates the v1 manifest that knows the part stored at this path.
///
/// A part may have arrived by transfer or been produced by a merge. In both
/// cases the parent directory is the source of truth for the original model;
/// we must not compute the `pom_id` from the resulting part.
pub fn manifest_for_part_path(path: &Path) -> Option<PomManifest> {
    let sha256 = path.file_stem()?.to_str()?;
    if !is_sha256_hex(sha256) {
        return None;
    }
    load_dir(path.parent()?).into_iter().find(|manifest| {
        manifest.pom_version == POM_VERSION
            && manifest
                .parts
                .iter()
                .chain(manifest.tp_parts.iter())
                .any(|part| part.sha256 == sha256)
    })
}

// ── Streaming hash ───────────────────────────────────────────────────────────

/// A `Write` that computes the sha256 of what goes through it as it goes.
///
/// It exists so that the slice hash does **not** cost a second read of the
/// file: a model slice has tens of GB, and rereading everything just to hash
/// doubled the slicing time. By wrapping the `splitter` writer, the digest
/// comes out ready in the same pass in which the bytes go to disk.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// Consumes the writer and returns `(sha256 em hex, bytes escritos)`.
    pub fn finish(self) -> (String, u64) {
        (hex::encode(self.hasher.finalize()), self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        // Only what was actually accepted: a short `write` that hashed the
        // whole buffer would give a digest of bytes that were never written.
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The sha256 of a file that is already on disk.
///
/// A verification path, not a production one: whoever just wrote the part
/// already has the digest from the [`HashingWriter`]. Used to validate what
/// arrived over the network and what the operator dropped in the folder by hand.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> String {
        seed.repeat(64 / seed.len())
    }

    fn layer_part(seed: &str, start: u32, end: u32) -> PomPart {
        PomPart {
            sha256: hash(seed),
            bytes: 1000,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: false,
            tp: None,
        }
    }

    fn tp_part(seed: &str, rank: u32, size: u32) -> PomPart {
        PomPart {
            sha256: hash(seed),
            bytes: 1000,
            start_layer: 0,
            end_layer: 31,
            is_first: true,
            is_last: true,
            tp: Some(PomTp { rank, size }),
        }
    }

    fn source_manifest(
        source_seed: &str,
        parts: Vec<PomPart>,
        tp_parts: Vec<PomPart>,
    ) -> PomManifest {
        let kind = match (parts.is_empty(), tp_parts.is_empty()) {
            (false, true) => PomKind::Layers,
            (true, false) => PomKind::TensorParallel,
            (false, false) => PomKind::Combined,
            (true, true) => PomKind::Layers,
        };
        let mut manifest = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            kind,
            "no-1",
            hash(source_seed),
            parts,
        );
        manifest.tp_parts = tp_parts;
        manifest
    }

    fn part(sha: &str, start: u32, end: u32) -> PomPart {
        PomPart {
            sha256: sha.repeat(64 / sha.len()),
            bytes: 1000,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: false,
            tp: None,
        }
    }

    fn manifesto(parts: Vec<PomPart>, total: u32) -> PomManifest {
        PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: total,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            hash("a"),
            parts,
        )
    }

    #[test]
    fn the_pom_id_is_the_hash_of_the_original_file() {
        let source_sha256 = hash("a");
        let manifest = source_manifest("a", vec![part("b", 0, 31)], vec![]);
        assert_eq!(manifest.pom_version, POM_VERSION);
        assert_eq!(manifest.pom_id, source_sha256);
        manifest.validate().unwrap();
    }

    #[test]
    fn the_manifest_survives_a_json_round_trip() {
        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let json = serde_json::to_string(&m).unwrap();
        let restored: PomManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, restored);
        restored.validate().unwrap();
    }

    #[test]
    fn a_pom_id_that_is_not_a_hash_is_refused() {
        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.pom_id = "nao-e-hash".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn an_inverted_range_and_a_malformed_hash_are_refused() {
        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.parts[0].start_layer = 40; // after the `end_layer`, which is 31
        assert!(m.validate().is_err());

        let mut m = manifesto(vec![part("a", 0, 31)], 32);
        m.parts[0].sha256 = "nao-e-hash".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn completeness_and_a_hole_in_the_coverage() {
        assert!(manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32).complete());
        // Gap at 16-19.
        assert!(!manifesto(vec![part("a", 0, 15), part("b", 20, 31)], 32).complete());
        // Covers up to 31 but the model has 36 layers.
        assert!(!manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 36).complete());
    }

    /// A `sha256` arrives over the network and becomes a file name: if `..`
    /// got through, a peer would choose where to write on the downloader's disk.
    #[test]
    fn a_hash_that_is_not_a_hash_does_not_become_a_path() {
        let dir = Path::new("/tmp/models/qwen");
        assert!(part_path(dir, "../../etc/passwd").is_err());
        assert!(part_path(dir, "ABCD").is_err());
        assert!(part_path(dir, &"a".repeat(63)).is_err());
        let ok = part_path(dir, &"a".repeat(64)).unwrap();
        assert_eq!(ok.parent().unwrap(), dir);
    }

    #[test]
    fn the_writer_hashes_what_goes_through_it() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"abc").unwrap();
        let (sha, bytes) = w.finish();
        assert_eq!(bytes, 3);
        // sha256("abc"), the well-known test vector.
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn write_and_reread_a_manifest() {
        let tmp = std::env::temp_dir().join(format!("pom-teste-{}", uuid::Uuid::new_v4().simple()));
        let _ = std::fs::remove_dir_all(&tmp);
        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let dir = dir_for(&tmp, &m);

        let written = write_atomic(&dir, &m).unwrap();
        assert_eq!(load(&written).unwrap(), m);
        assert_eq!(load_dir(&dir), vec![m.clone()]);
        assert_eq!(
            written.file_name().unwrap().to_string_lossy(),
            "qwen3-8b-aaaaaaaa.pom"
        );
        // The label's `:` cannot become a folder, and the source hash goes into the name.
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("qwen3-8b-{}", short_id(&m.pom_id)));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// Different slicings of the **same original file** share the folder and
    /// the manifest, because the source hash is the model's identity.
    #[test]
    fn two_slicings_of_the_same_file_share_a_folder() {
        let root = Path::new("/tmp/slices");
        let um = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let other = manifesto(vec![part("c", 0, 19), part("d", 20, 31)], 32);

        assert_eq!(um.pom_id, other.pom_id);
        assert_eq!(dir_for(root, &um), dir_for(root, &other));
    }

    /// Re-slicing in exactly the same way does **not** duplicate anything: same
    /// content, same `pom_id`, same folder. It is the same round.
    #[test]
    fn reslicing_the_same_way_lands_in_the_same_folder() {
        let root = Path::new("/tmp/slices");
        let um = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        let again = manifesto(vec![part("b", 16, 31), part("a", 0, 15)], 32);
        assert_eq!(dir_for(root, &um), dir_for(root, &again));
    }

    /// The round only moves to the definitive name once it is whole, and the
    /// parts go along with it.
    #[test]
    fn finalizing_moves_the_round_to_the_folder_with_the_id() {
        let root = std::env::temp_dir().join(format!("pom-fin-{}", uuid::Uuid::new_v4().simple()));
        let work = scratch_dir(&root);
        std::fs::create_dir_all(&work).unwrap();

        let m = manifesto(vec![part("a", 0, 15), part("b", 16, 31)], 32);
        for p in &m.parts {
            std::fs::write(part_path(&work, &p.sha256).unwrap(), b"conteudo").unwrap();
        }

        let destination = finalize_run(&root, &work, &m).unwrap();
        assert_eq!(destination, dir_for(&root, &m));
        for p in &m.parts {
            assert!(part_path(&destination, &p.sha256).unwrap().is_file());
        }
        assert_eq!(load_dir(&destination), vec![m]);
        assert!(!work.exists(), "the work folder is not left behind");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_model_name_drops_the_gguf_suffix() {
        let model_hash = hash("a");
        let manifest = PomManifest::new(
            PomModel {
                name: "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "node-1",
            model_hash.clone(),
            vec![layer_part("b", 0, 7)],
        );

        assert_eq!(manifest.pom_version, POM_VERSION);
        assert_eq!(manifest.pom_id, model_hash);
        assert_eq!(manifest.model.name, "Llama-3.1-8B-Instruct-Q4_K_M");
        manifest.validate().unwrap();
    }

    #[test]
    fn merge_preserves_local_alternatives_and_adds_tp_parts() {
        let mut local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let remote = source_manifest("a", vec![layer_part("c", 0, 3)], vec![tp_part("d", 0, 2)]);

        local.merge_from(remote).unwrap();

        assert_eq!(local.parts.len(), 2);
        assert_eq!(local.tp_parts.len(), 1);
        assert!(local
            .parts
            .iter()
            .any(|p| p.start_layer == 0 && p.end_layer == 7));
        assert!(local
            .parts
            .iter()
            .any(|p| p.start_layer == 0 && p.end_layer == 3));
        assert_eq!(local.kind, PomKind::Combined);
    }

    #[test]
    fn a_repeated_merge_does_not_duplicate_the_sha() {
        let part = layer_part("b", 0, 7);
        let mut local = source_manifest("a", vec![part.clone()], vec![]);
        local
            .merge_from(source_manifest("a", vec![part], vec![]))
            .unwrap();
        assert_eq!(local.parts.len(), 1);
    }

    #[test]
    fn merge_manifest_reuses_the_folder_when_the_readable_name_changes() {
        let root =
            std::env::temp_dir().join(format!("pom-source-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&root, "nome-antigo", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (merged_dir, merged) = merge_manifest(&root, &incoming).unwrap();

        assert_eq!(merged_dir, local_dir);
        assert_eq!(merged.parts.len(), 2);
        assert_eq!(load_dir(&local_dir).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn merge_manifest_preserves_the_local_content() {
        let root =
            std::env::temp_dir().join(format!("pom-merge-{}", uuid::Uuid::new_v4().simple()));
        let local = source_manifest("a", vec![layer_part("b", 0, 7)], vec![]);
        let local_dir = source_run_dir(&root, "qwen3-8b", &local.pom_id);
        std::fs::create_dir_all(&local_dir).unwrap();
        write_atomic(&local_dir, &local).unwrap();

        let incoming = source_manifest("a", vec![layer_part("c", 0, 3)], vec![]);
        let (_, merged) = merge_manifest(&root, &incoming).unwrap();

        assert!(merged.parts.iter().any(|p| p.sha256 == hash("b")));
        assert!(merged.parts.iter().any(|p| p.sha256 == hash("c")));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
