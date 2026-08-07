//! Discovery of GGUF models already installed on this machine.
//!
//! Three sources, each resolved from the paths the tools actually use rather
//! than a guess:
//!
//! * **LM Studio** — home comes from `$LMSTUDIO_HOME`, then the
//!   `~/.lmstudio-home-pointer` file, then `~/.lmstudio`. The model directory is
//!   `downloadsFolder` in `<home>/settings.json` when that path exists, else
//!   `<home>/models`. On Linux every `/home/*/.lmstudio` and `/root/.lmstudio`
//!   is also scanned, because the peer typically runs as a different user than
//!   the one that installed LM Studio.
//! * **Ollama** — `$OLLAMA_MODELS`, then `~/.ollama/models`, plus the service
//!   locations on Linux. Weights are blobs addressed by digest, so the friendly
//!   name is recovered by reading the manifest tree.
//! * **Local shards** — `.gguf` files under `./models`, including the
//!   `<base>_<start>_<end>.gguf` slices produced by `split_model.py`.

use crate::gguf::{self, GgufInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Deepest directory level walked when looking for `.gguf` files.
const MAX_WALK_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    LmStudio,
    Ollama,
    Local,
}

impl ModelSource {
    pub fn label(&self) -> &'static str {
        match self {
            ModelSource::LmStudio => "LM Studio",
            ModelSource::Ollama => "Ollama",
            ModelSource::Local => "Local",
        }
    }
}

/// One GGUF file found on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    /// Stable identifier, unique per machine.
    pub id: String,
    /// Human-friendly name — the Ollama tag, or publisher/repo for LM Studio.
    pub name: String,
    pub source: ModelSource,
    pub source_label: &'static str,
    /// Absolute path of the `.gguf` file.
    pub path: String,
    pub size_bytes: u64,
    pub architecture: Option<String>,
    /// Layers of the *whole* model (shards report the original count).
    pub total_layers: Option<u32>,
    /// Quantisation tag parsed from the filename, e.g. "Q4_K_M".
    pub quant: Option<String>,
    /// `Some((start, end))` when this file is a slice of a bigger model.
    pub shard_range: Option<(u32, u32)>,
    /// Shards of the same model share this key.
    pub shard_group: Option<String>,
}

impl DiscoveredModel {
    pub fn is_shard(&self) -> bool {
        self.shard_range.is_some()
    }
}

/// A set of slices that together (may) cover one model.
#[derive(Debug, Clone, Serialize)]
pub struct ShardSet {
    /// Shared prefix of the slice filenames, e.g. "qwen".
    pub group: String,
    pub total_layers: Option<u32>,
    /// True when the total came from the slices' own coverage rather than from
    /// `rdmap2p.original_block_count`. Old slices carry no such metadata.
    pub total_layers_inferred: bool,
    pub shards: Vec<DiscoveredModel>,
    /// True when the slices cover 0..total_layers with no gap or overlap.
    pub complete: bool,
}

/// Everything found on this machine, plus where we looked.
#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub models: Vec<DiscoveredModel>,
    pub shard_sets: Vec<ShardSet>,
    /// Directories actually scanned — shown in the UI so an empty list is
    /// explainable rather than mysterious.
    pub scanned: Vec<ScannedRoot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScannedRoot {
    pub source: ModelSource,
    pub path: String,
    pub exists: bool,
    pub found: usize,
}

// ── GGUF header cache ────────────────────────────────────────────────────────
// The UI polls the catalog; re-reading every header each time is wasteful.
// Keyed by path + mtime + size so an edited file is re-read.

type HeaderKey = (String, u64, u64);

fn header_cache() -> &'static Mutex<HashMap<HeaderKey, Option<GgufInfo>>> {
    static CACHE: OnceLock<Mutex<HashMap<HeaderKey, Option<GgufInfo>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_info(path: &Path, size: u64, mtime: u64) -> Option<GgufInfo> {
    let key = (path.to_string_lossy().into_owned(), size, mtime);
    if let Some(hit) = header_cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let info = gguf::read_info(path).ok();
    if let Ok(mut c) = header_cache().lock() {
        c.insert(key, info.clone());
    }
    info
}

// ── Path resolution ──────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Every plausible LM Studio home on this machine, most specific first.
fn lmstudio_homes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    if let Some(env) = std::env::var_os("LMSTUDIO_HOME") {
        push(PathBuf::from(env));
    }

    if let Some(home) = home_dir() {
        // The pointer file holds the path LM Studio is actually using. It can go
        // stale (it survives a home that was removed), so it is a hint, not law.
        if let Ok(txt) = std::fs::read_to_string(home.join(".lmstudio-home-pointer")) {
            let p = PathBuf::from(txt.trim());
            if p.is_dir() {
                push(p);
            }
        }
        push(home.join(".lmstudio"));
        // Pre-0.3 layout.
        push(home.join(".cache").join("lm-studio"));
    }

    // The peer often runs as root while LM Studio was installed by a desktop
    // user, so root's own home has nothing. Look inside the other homes too —
    // only at `<home>/.lmstudio`, never at every subdirectory of a home.
    if cfg!(target_os = "linux") {
        for home in child_dirs(Path::new("/home")) {
            let candidate = home.join(".lmstudio");
            if candidate.is_dir() {
                push(candidate);
            }
        }
        let root_home = PathBuf::from("/root/.lmstudio");
        if root_home.is_dir() {
            push(root_home);
        }
    }

    out
}

/// The directory LM Studio downloads models into for a given home.
fn lmstudio_models_dir(home: &Path) -> PathBuf {
    // `downloadsFolder` can point at an external volume that is not mounted;
    // only trust it when it is really there.
    if let Ok(txt) = std::fs::read_to_string(home.join("settings.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(folder) = v.get("downloadsFolder").and_then(|f| f.as_str()) {
                let p = PathBuf::from(folder);
                if p.is_dir() {
                    return p;
                }
            }
        }
    }
    home.join("models")
}

/// Every plausible Ollama models directory on this machine.
fn ollama_model_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    if let Some(env) = std::env::var_os("OLLAMA_MODELS") {
        push(PathBuf::from(env));
    }
    if let Some(home) = home_dir() {
        push(home.join(".ollama").join("models"));
    }
    if cfg!(target_os = "linux") {
        // Installed as a systemd service, Ollama runs as its own user.
        push(PathBuf::from("/usr/share/ollama/.ollama/models"));
        push(PathBuf::from("/var/lib/ollama/.ollama/models"));
        for dir in child_dirs(Path::new("/home")) {
            push(dir.join(".ollama").join("models"));
        }
        push(PathBuf::from("/root/.ollama/models"));
    }

    out
}

fn child_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

// ── Filesystem walking ───────────────────────────────────────────────────────

/// Collect `.gguf` files under `root`, bounded in depth and skipping the extra
/// parts of multi-part models (only `-00001-of-*` is kept).
fn find_gguf(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            find_gguf(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("gguf") {
            if is_secondary_part(&path) {
                continue;
            }
            out.push(path);
        }
    }
}

/// True for `…-00002-of-00003.gguf` and later — llama.cpp loads the whole set
/// from part one, so listing the rest would just be noise.
fn is_secondary_part(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
    // rsplitn yields reversed: [total, "of", index, rest…]
    if parts.len() < 3 || parts[1] != "of" {
        return false;
    }
    match parts[2].parse::<u32>() {
        Ok(n) => n > 1,
        Err(_) => false,
    }
}

// ── Name / quantisation parsing ──────────────────────────────────────────────

/// Pull a quantisation tag such as `Q4_K_M`, `IQ3_XXS` or `BF16` from a filename.
///
/// Any `_<start>_<end>` shard suffix is removed first, otherwise a slice of a
/// `Q4_K_M` model reports its quantisation as `Q4_K_M_0_19`.
fn parse_quant(filename: &str) -> Option<String> {
    let stem = filename.trim_end_matches(".gguf");
    let stem = match parse_shard_name(filename) {
        Some((base, _, _)) => base,
        None => stem.to_string(),
    };

    for raw in stem.split(['-', '.', '_']) {
        let up = raw.to_ascii_uppercase();
        if up == "BF16" || up == "F16" || up == "F32" {
            return Some(up);
        }
        let looks_quant = (up.starts_with('Q') && up.len() >= 2 && up[1..2].parse::<u8>().is_ok())
            || (up.starts_with("IQ") && up.len() >= 3 && up[2..3].parse::<u8>().is_ok());
        if looks_quant {
            // Re-attach the suffix parts that belong to the tag (K_M, XXS…),
            // which are separated by '_' in every GGUF naming convention.
            let idx = stem.to_ascii_uppercase().find(&up)?;
            let tag: String = stem[idx..]
                .split(['-', '.'])
                .next()
                .unwrap_or(&up)
                .to_ascii_uppercase();
            return Some(tag);
        }
    }
    None
}

/// `qwen_0_15.gguf` → `("qwen", 0, 15)`. Mirrors `parse_layers_from_filename`
/// in `main.rs`, which is what actually drives the assignment at startup.
fn parse_shard_name(filename: &str) -> Option<(String, u32, u32)> {
    let stem = filename.strip_suffix(".gguf")?;
    let mut parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 3 {
        return None;
    }
    let end: u32 = parts.pop()?.parse().ok()?;
    let start: u32 = parts.pop()?.parse().ok()?;
    if start > end || end - start > 500 {
        return None;
    }
    Some((parts.join("_"), start, end))
}

// ── Building the catalog ─────────────────────────────────────────────────────

fn describe(path: &Path, name: String, source: ModelSource) -> Option<DiscoveredModel> {
    let meta = std::fs::metadata(path).ok()?;
    let size_bytes = meta.len();
    // A GGUF header alone is a few hundred bytes; anything this small is junk.
    if size_bytes < 1024 {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let info = cached_info(path, size_bytes, mtime);
    let filename = path.file_name()?.to_string_lossy().into_owned();

    // Projector sidecars (`mmproj-*`, architecture "clip") are not language
    // models — they cannot be sliced or served, so they do not belong in a list
    // of things to run.
    if info.as_ref().and_then(|i| i.architecture.as_deref()) == Some("clip")
        || filename.starts_with("mmproj")
    {
        return None;
    }

    // Shard range: trust GGUF metadata first, fall back to the filename
    // convention that `main.rs` already relies on.
    let from_name = parse_shard_name(&filename);
    let shard_range = info
        .as_ref()
        .and_then(|i| i.shard_range)
        .or_else(|| from_name.as_ref().map(|(_, s, e)| (*s, *e)));
    let shard_group = from_name.as_ref().map(|(base, _, _)| base.clone());

    // For a slice, `block_count` counts the layers *inside the file*, not the
    // model it came from. Only `rdmap2p.original_block_count` — written by
    // split_model.py — is a real total. Older slices lack it, so report unknown
    // rather than a number that is wrong; `build_shard_sets` infers it from the
    // coverage of the whole set instead.
    let total_layers = match (shard_range, info.as_ref()) {
        (Some(_), Some(i)) => i.original_block_count,
        (None, Some(i)) => i.total_layers(),
        _ => None,
    };

    Some(DiscoveredModel {
        id: format!("{:?}:{}", source, path.display()).to_lowercase(),
        name,
        source,
        source_label: source.label(),
        path: path.to_string_lossy().into_owned(),
        size_bytes,
        architecture: info.as_ref().and_then(|i| i.architecture.clone()),
        total_layers,
        quant: parse_quant(&filename),
        shard_range,
        shard_group,
    })
}

/// LM Studio stores models as `<publisher>/<repo>/<file>.gguf`; show that.
fn lmstudio_name(models_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(models_dir) {
        Ok(rel) => {
            let comps: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            match comps.len() {
                0 => path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                1 => comps[0].clone(),
                _ => format!("{}/{}", comps[0], comps[comps.len() - 2]),
            }
        }
        Err(_) => path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
    }
}

fn scan_lmstudio(out: &mut Vec<DiscoveredModel>, roots: &mut Vec<ScannedRoot>) {
    for home in lmstudio_homes() {
        let models_dir = lmstudio_models_dir(&home);
        if roots.iter().any(|r| r.path == models_dir.to_string_lossy()) {
            continue;
        }
        let exists = models_dir.is_dir();
        let mut files = Vec::new();
        if exists {
            find_gguf(&models_dir, 0, &mut files);
        }
        let mut found = 0;
        for f in files {
            let name = lmstudio_name(&models_dir, &f);
            if let Some(m) = describe(&f, name, ModelSource::LmStudio) {
                if !out.iter().any(|e| e.path == m.path) {
                    out.push(m);
                    found += 1;
                }
            }
        }
        roots.push(ScannedRoot {
            source: ModelSource::LmStudio,
            path: models_dir.to_string_lossy().into_owned(),
            exists,
            found,
        });
    }
}

/// Ollama manifest: a JSON document whose `layers` array carries the digest of
/// the weights blob. Everything else (params, template, license) is skipped.
#[derive(Deserialize)]
struct OllamaManifest {
    #[serde(default)]
    layers: Vec<OllamaLayer>,
}

#[derive(Deserialize)]
struct OllamaLayer {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    #[serde(default)]
    #[allow(dead_code)]
    size: u64,
}

fn scan_ollama(out: &mut Vec<DiscoveredModel>, roots: &mut Vec<ScannedRoot>) {
    for models_dir in ollama_model_dirs() {
        if roots.iter().any(|r| r.path == models_dir.to_string_lossy()) {
            continue;
        }
        let manifests = models_dir.join("manifests");
        let blobs = models_dir.join("blobs");
        let exists = manifests.is_dir() && blobs.is_dir();
        let mut found = 0;

        if exists {
            let mut files = Vec::new();
            collect_files(&manifests, 0, &mut files);
            for manifest_path in files {
                let Some((name, blob)) = resolve_ollama_model(&manifests, &blobs, &manifest_path)
                else {
                    continue;
                };
                if let Some(m) = describe(&blob, name, ModelSource::Ollama) {
                    if !out.iter().any(|e| e.path == m.path) {
                        out.push(m);
                        found += 1;
                    }
                }
            }
        }

        roots.push(ScannedRoot {
            source: ModelSource::Ollama,
            path: models_dir.to_string_lossy().into_owned(),
            exists,
            found,
        });
    }
}

/// `manifests/<registry>/<namespace>/<name>/<tag>` → ("name:tag", blob path).
fn resolve_ollama_model(
    manifests_root: &Path,
    blobs: &Path,
    manifest_path: &Path,
) -> Option<(String, PathBuf)> {
    let txt = std::fs::read_to_string(manifest_path).ok()?;
    let manifest: OllamaManifest = serde_json::from_str(&txt).ok()?;
    let layer = manifest
        .layers
        .iter()
        .find(|l| l.media_type == "application/vnd.ollama.image.model")?;

    // Blobs are stored with the ':' of the digest replaced by '-'.
    let blob = blobs.join(layer.digest.replace(':', "-"));
    if !blob.is_file() {
        // Cloud-hosted tags have a manifest but no local weights.
        return None;
    }

    let rel = manifest_path.strip_prefix(manifests_root).ok()?;
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // [registry, namespace, name, tag]
    let name = match comps.len() {
        n if n >= 4 => {
            let tag = &comps[n - 1];
            let model = &comps[n - 2];
            let ns = &comps[n - 3];
            if ns == "library" {
                format!("{model}:{tag}")
            } else {
                format!("{ns}/{model}:{tag}")
            }
        }
        _ => comps.join("/"),
    };

    Some((name, blob))
}

fn collect_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_files(&path, depth + 1, out),
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {}
        }
    }
}

fn scan_local(dir: &Path, out: &mut Vec<DiscoveredModel>, roots: &mut Vec<ScannedRoot>) {
    let exists = dir.is_dir();
    let mut files = Vec::new();
    if exists {
        find_gguf(dir, 0, &mut files);
    }
    let mut found = 0;
    for f in files {
        let name = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if let Some(m) = describe(&f, name, ModelSource::Local) {
            if !out.iter().any(|e| e.path == m.path) {
                out.push(m);
                found += 1;
            }
        }
    }
    roots.push(ScannedRoot {
        source: ModelSource::Local,
        path: dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).to_string_lossy().into_owned(),
        exists,
        found,
    });
}

/// Group slices by their shared base name and report whether the set covers the
/// model end to end.
fn build_shard_sets(models: &[DiscoveredModel]) -> Vec<ShardSet> {
    let mut groups: HashMap<String, Vec<DiscoveredModel>> = HashMap::new();
    for m in models.iter().filter(|m| m.is_shard()) {
        let key = m.shard_group.clone().unwrap_or_else(|| m.name.clone());
        groups.entry(key).or_default().push(m.clone());
    }

    let mut sets: Vec<ShardSet> = groups
        .into_iter()
        .map(|(group, mut shards)| {
            shards.sort_by_key(|s| s.shard_range.map(|r| r.0).unwrap_or(0));

            // How far the slices reach without a gap, starting from layer 0.
            let mut contiguous_to = 0u32;
            for s in &shards {
                match s.shard_range {
                    Some((start, end)) if start == contiguous_to => contiguous_to = end + 1,
                    _ => break,
                }
            }

            // Slices written by split_model.py record the original layer count.
            // Older ones do not, so fall back to what the set itself covers —
            // which is the honest reading: "these slices reach layer N".
            let declared = shards.iter().find_map(|s| s.total_layers);
            let total_layers = declared.or(if contiguous_to > 0 {
                Some(contiguous_to)
            } else {
                None
            });

            let complete = matches!(total_layers, Some(t) if t > 0 && contiguous_to >= t);

            ShardSet {
                group,
                total_layers,
                total_layers_inferred: declared.is_none(),
                shards,
                complete,
            }
        })
        .collect();

    sets.sort_by(|a, b| a.group.cmp(&b.group));
    sets
}

/// Scan every known location and return what is installed on this machine.
///
/// `local_models_dir` is where this node keeps its own slices — usually
/// `./models` relative to the working directory.
pub fn discover(local_models_dir: &Path) -> Catalog {
    let mut models: Vec<DiscoveredModel> = Vec::new();
    let mut scanned: Vec<ScannedRoot> = Vec::new();

    scan_local(local_models_dir, &mut models, &mut scanned);
    scan_lmstudio(&mut models, &mut scanned);
    scan_ollama(&mut models, &mut scanned);

    models.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.path.cmp(&b.path))
    });

    let shard_sets = build_shard_sets(&models);
    Catalog {
        models,
        shard_sets,
        scanned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shard_filenames() {
        assert_eq!(
            parse_shard_name("qwen_0_15.gguf"),
            Some(("qwen".to_string(), 0, 15))
        );
        assert_eq!(
            parse_shard_name("Qwen3.6-35B-A3B-Q4_K_M_20_39.gguf"),
            Some(("Qwen3.6-35B-A3B-Q4_K_M".to_string(), 20, 39))
        );
        assert_eq!(parse_shard_name("phi-4-Q6_K.gguf"), None);
        assert_eq!(parse_shard_name("model.gguf"), None);
    }

    #[test]
    fn parses_quant_tags() {
        assert_eq!(parse_quant("phi-4-Q6_K.gguf").as_deref(), Some("Q6_K"));
        assert_eq!(
            parse_quant("Qwen3.6-35B-A3B-UD-IQ3_S.gguf").as_deref(),
            Some("IQ3_S")
        );
        assert_eq!(parse_quant("model-BF16.gguf").as_deref(), Some("BF16"));
    }

    #[test]
    fn quant_ignores_the_shard_suffix() {
        assert_eq!(
            parse_quant("Qwen3.6-35B-A3B-Q4_K_M_0_19.gguf").as_deref(),
            Some("Q4_K_M"),
            "a fatia tem a mesma quantização do modelo de origem"
        );
    }

    #[test]
    fn skips_secondary_parts_only() {
        assert!(!is_secondary_part(Path::new("m-00001-of-00003.gguf")));
        assert!(is_secondary_part(Path::new("m-00002-of-00003.gguf")));
        assert!(!is_secondary_part(Path::new("plain.gguf")));
    }

    #[test]
    fn shard_set_completeness() {
        let mk = |start: u32, end: u32, total: u32| DiscoveredModel {
            id: format!("s{start}"),
            name: format!("s_{start}_{end}.gguf"),
            source: ModelSource::Local,
            source_label: "Local",
            path: format!("/tmp/s_{start}_{end}.gguf"),
            size_bytes: 4096,
            architecture: None,
            total_layers: Some(total),
            quant: None,
            shard_range: Some((start, end)),
            shard_group: Some("s".into()),
        };
        let complete = build_shard_sets(&[mk(0, 15, 32), mk(16, 31, 32)]);
        assert!(complete[0].complete);
        assert!(!complete[0].total_layers_inferred);

        let with_gap = build_shard_sets(&[mk(0, 15, 32), mk(20, 31, 32)]);
        assert!(!with_gap[0].complete);
    }

    #[test]
    fn old_slices_without_metadata_infer_the_total_from_coverage() {
        // Slices predating `rdmap2p.original_block_count` report no total; the
        // set must not claim the shard's own block count is the model's.
        let mk = |start: u32, end: u32| DiscoveredModel {
            id: format!("s{start}"),
            name: format!("qwen_{start}_{end}.gguf"),
            source: ModelSource::Local,
            source_label: "Local",
            path: format!("/tmp/qwen_{start}_{end}.gguf"),
            size_bytes: 4096,
            architecture: None,
            total_layers: None,
            quant: None,
            shard_range: Some((start, end)),
            shard_group: Some("qwen".into()),
        };
        let sets = build_shard_sets(&[mk(0, 15), mk(16, 31)]);
        assert_eq!(sets[0].total_layers, Some(32));
        assert!(sets[0].total_layers_inferred);
        assert!(sets[0].complete);
    }
}
