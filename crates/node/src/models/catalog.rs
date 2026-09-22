//! Discovery of GGUF models already installed on this machine.
//!
//! Two kinds of directory, and the difference is what lives in them:
//!
//! * **whole models** — every directory in `Config::model_dirs`. Until the
//!   operator edits that list it is [`default_model_dirs`]: the LM Studio and
//!   Ollama libraries found on this machine, which is what the scan always did.
//!   Each directory is read according to what it *is* (see [`detect_source`]),
//!   so a plain folder of `.gguf` files works as well as an Ollama blob store.
//! * **slices** — one directory, `Config::shard_dir`, default
//!   `<project_root>/models`. It is where local slicing writes, where received
//!   files land, and the only place a peer may pull from.
//!
//! Nothing here reads a fixed path any more: moving every GGUF to
//! `/Volumes/sdcard/models` is a configuration change, not a code change.
//!
//! Library layouts recognised automatically:
//!
//! * **LM Studio** — home from `$LMSTUDIO_HOME`, then `~/.lmstudio-home-pointer`,
//!   then `~/.lmstudio`; the directory is `downloadsFolder` in
//!   `<home>/settings.json` when that path exists, else `<home>/models`. On
//!   Linux every `/home/*/.lmstudio` and `/root/.lmstudio` is also considered,
//!   because the peer typically runs as a different user than the one that
//!   installed LM Studio.
//! * **Ollama** — `$OLLAMA_MODELS`, then `~/.ollama/models`, plus the service
//!   locations on Linux. Weights are blobs addressed by digest, so the friendly
//!   name is recovered by reading the manifest tree.

use crate::gguf::{self, GgufInfo, ModelCapabilityEvidence};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use tracing::debug;

/// Deepest directory level walked when looking for `.gguf` files.
const MAX_WALK_DEPTH: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    LmStudio,
    Ollama,
    /// The slice directory — the only source a peer is allowed to pull from.
    Local,
    /// Any other configured directory of `.gguf` files.
    Folder,
}

impl ModelSource {
    pub fn label(&self) -> &'static str {
        match self {
            ModelSource::LmStudio => "LM Studio",
            ModelSource::Ollama => "Ollama",
            ModelSource::Local => "Local",
            ModelSource::Folder => "Pasta",
        }
    }
}

/// Filesystem identity of the metadata generation already applied to a model.
///
/// This is deliberately not serialized. It only coordinates the in-process
/// catalog cache across inventory generations; the strong source hash belongs
/// to transfer verification, never to the metadata path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MetadataIdentity {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) mtime: u64,
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
    /// Adjacent llama.cpp multimodal projector, only when the catalog proved
    /// its identity and dimensional compatibility with this GGUF.
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// Motivo pelo qual o catálogo não conseguiu provar um projetor para este modelo.
    #[serde(default)]
    pub mmproj_reason: Option<String>,
    /// Nomes de identidade do cabeçalho, mantidos só em memória para relacionar
    /// fatias ao modelo lógico sem expor metadados duplicados na API.
    #[serde(skip)]
    pub(crate) identity_names: Vec<String>,
    pub size_bytes: u64,
    /// True when the source GGUF declares a tensor-split set with more than
    /// one part. The catalog exposes only the first part as the selectable
    /// model, because the set is one logical source.
    #[serde(default)]
    pub multipart: bool,
    /// Physical parts that make up a multipart source, in split order.
    #[serde(default)]
    pub multipart_parts: Vec<MultipartPart>,
    /// Concrete reason a declared multipart source cannot currently be used.
    #[serde(default)]
    pub multipart_error: Option<String>,
    /// Erro terminal ao ler o cabeçalho deste arquivo nesta geração do
    /// catálogo. Um modelo quebrado não impede os demais de aparecerem.
    #[serde(default)]
    pub metadata_error: Option<String>,
    /// Identity of the file for which the metadata fields below were read.
    /// Kept in memory so a refreshed inventory can reuse completed work.
    #[serde(skip)]
    pub(crate) metadata_identity: Option<MetadataIdentity>,
    pub architecture: Option<String>,
    /// True only when the GGUF header reports more than one expert.
    #[serde(default)]
    pub is_moe: bool,
    /// Layers of the *whole* model (shards report the original count).
    pub total_layers: Option<u32>,
    /// Quantisation tag parsed from the filename, e.g. "Q4_K_M".
    pub quant: Option<String>,
    /// `Some((start, end))` when this file is a slice of a bigger model.
    pub shard_range: Option<(u32, u32)>,
    /// Shards of the same model share this key.
    pub shard_group: Option<String>,
    /// sha256 da parte, quando este arquivo é uma parte endereçada por
    /// conteúdo (`<sha256>.gguf`). É a identidade que atravessa máquinas: dois
    /// nós com o mesmo hash têm, byte a byte, o mesmo arquivo.
    pub sha256: Option<String>,
    /// O rótulo do modelo de que esta parte saiu, tal como o `.pom` o guarda.
    /// `None` num arquivo que não pertence a manifesto nenhum.
    #[serde(default)]
    pub model_label: Option<String>,
    /// O manifesto do modelo original que produziu esta parte.
    #[serde(default)]
    pub pom_id: Option<String>,
    /// True when the file is a Tensor Parallelism shard, not a layer slice.
    #[serde(default)]
    pub tensor_parallel: bool,
    /// Rank metadata from the unified manifest, when this is a TP shard.
    #[serde(default)]
    pub tp_rank: Option<u32>,
    #[serde(default)]
    pub tp_size: Option<u32>,
    /// Evidência de capacidades derivada do cabeçalho do GGUF. A confirmação
    /// estrutural pela tabela de tensores fica nos leitores que precisam dela,
    /// fora da varredura geral do catálogo.
    #[serde(default)]
    pub capabilities: Option<ModelCapabilityEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultipartPart {
    pub index: u32,
    pub path: String,
    pub bytes: u64,
    pub tensor_count: u64,
}

impl DiscoveredModel {
    pub fn is_shard(&self) -> bool {
        self.shard_range.is_some()
    }

    pub fn is_tensor_parallel(&self) -> bool {
        self.tensor_parallel
    }
}

/// A set of slices that together (may) cover one model.
#[derive(Debug, Clone, Serialize)]
pub struct ShardSet {
    /// A chave do manifesto: `<modelo>-<hash curto>`, o mesmo nome da pasta em
    /// disco. Alternativas do mesmo modelo compartilham este conjunto.
    pub group: String,
    /// O rótulo do modelo, para a tela. O `group` identifica; este é o que se
    /// lê. Vazio numa fatia sem manifesto.
    #[serde(default)]
    pub model_label: String,
    /// Projector adjacent to a shard or to the whole model that originated it.
    /// The path is local to this peer and is only used by the pipeline head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_path: Option<String>,
    /// Motivo acionável quando nenhuma associação pôde ser provada.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_reason: Option<String>,
    /// O hash do modelo original. Identificação, não nome.
    #[serde(default)]
    pub pom_id: String,
    pub total_layers: Option<u32>,
    /// True when the total came from the slices' own coverage rather than from
    /// `pom.original_block_count`. Old slices carry no such metadata.
    pub total_layers_inferred: bool,
    /// Pipeline layer parts from the manifest's `parts` array.
    pub shards: Vec<DiscoveredModel>,
    /// Tensor-parallel ranks from the manifest's `tp_parts` array.
    pub tp_parts: Vec<DiscoveredModel>,
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
    /// False while the GGUF headers are being enriched asynchronously.
    pub metadata_complete: bool,
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

fn header_cache() -> &'static Mutex<HashMap<HeaderKey, Result<GgufInfo, String>>> {
    static CACHE: OnceLock<Mutex<HashMap<HeaderKey, Result<GgufInfo, String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn source_hash_cache() -> &'static Mutex<HashMap<MetadataIdentity, String>> {
    static CACHE: OnceLock<Mutex<HashMap<MetadataIdentity, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
static CONTENT_HASH_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn reset_content_hash_calls() {
    CONTENT_HASH_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn content_hash_calls() -> usize {
    CONTENT_HASH_CALLS.load(Ordering::SeqCst)
}

fn cached_content_hash(path: &Path) -> Option<String> {
    #[cfg(test)]
    CONTENT_HASH_CALLS.fetch_add(1, Ordering::SeqCst);
    let metadata = std::fs::metadata(path).ok()?;
    let identity = metadata_identity(path, &metadata);
    if let Some(hash) = source_hash_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&identity).cloned())
    {
        return Some(hash);
    }
    let (hash, _) = crate::pom::hash_file(path).ok()?;
    if let Ok(mut cache) = source_hash_cache().lock() {
        cache.insert(identity, hash.clone());
    }
    Some(hash)
}

fn cached_info(path: &Path, size: u64, mtime: u64) -> Result<GgufInfo, String> {
    let key = (path.to_string_lossy().into_owned(), size, mtime);
    if let Some(hit) = header_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&key).cloned())
    {
        return hit;
    }
    let info = gguf::read_info(path).map_err(|error| format!("{error:#}"));
    if let Ok(mut c) = header_cache().lock() {
        c.insert(key, info.clone());
    }
    info
}

// ── Path resolution ──────────────────────────────────────────────────────────

/// O diretório do usuário, do jeito que cada sistema o chama.
///
/// No Windows não existe `HOME`: quem responde é `USERPROFILE`. Sem isto o
/// catálogo não achava nem o LM Studio nem o Ollama daquela máquina e a tela
/// de Modelos voltava vazia, sem nada que explicasse por quê.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("USERPROFILE").map(PathBuf::from)
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
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(error) => {
            debug!(
                path = %root.display(),
                error = %error,
                "ignorando diretório ilegível durante a descoberta de modelos"
            );
            return;
        }
    };
    for entry in rd {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                debug!(
                    path = %root.display(),
                    error = %error,
                    "ignorando entrada ilegível durante a descoberta de modelos"
                );
                continue;
            }
        };
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(error) => {
                debug!(
                    path = %path.display(),
                    error = %error,
                    "ignorando entrada cujo tipo não pôde ser lido durante a descoberta de modelos"
                );
                continue;
            }
        };
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
/// Só faz sentido no nome de um modelo inteiro. Uma parte se chama pelo hash,
/// que não tem tag nenhuma para achar - a quantização dela vem do `.pom`, que a
/// copiou daqui na hora de fatiar.
pub fn parse_quant(filename: &str) -> Option<String> {
    let stem = filename.trim_end_matches(".gguf").to_string();

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

// ── Building the catalog ─────────────────────────────────────────────────────

/// O que o `.pom` diz sobre um arquivo que é parte de um fatiamento.
///
/// É a fonte de verdade da identidade de uma parte: nome do modelo, faixa de
/// camadas e total. O arquivo em si se chama pelo hash - e hash não é nome de
/// nada que um operador precise ler.
#[derive(Debug, Clone)]
pub struct PartContext {
    pub model_label: String,
    pub total_layers: u32,
    pub start_layer: u32,
    pub end_layer: u32,
    pub sha256: String,
    pub pom_id: String,
    /// Identifica o manifesto: `<modelo>-<hash curto>`, o mesmo nome da pasta.
    /// É a chave pela qual as fatias alternativas se agrupam.
    pub run_key: String,
    /// Quantização do modelo de origem, copiada pelo fatiador para o manifesto.
    /// O nome de uma parte é o hash, então não há de onde deduzi-la.
    pub quant: Option<String>,
    pub tensor_parallel: bool,
    pub tp_rank: Option<u32>,
    pub tp_size: Option<u32>,
}

impl PartContext {
    /// Como esta parte se chama na tela: o modelo e as camadas que ela traz.
    ///
    /// **Nunca o hash.** O nome do arquivo é o conteúdo porque isso resolve
    /// identidade entre máquinas; para quem lê, `a3de…f1.gguf` não diz nem de
    /// que modelo é nem que pedaço dele. O hash continua no `sha256`, para
    /// quem precisar casar duas cópias.
    pub fn display_name(&self) -> String {
        format!(
            "{} · L{}-{}",
            self.model_label, self.start_layer, self.end_layer
        )
    }
}

fn describe(
    path: &Path,
    name: String,
    source: ModelSource,
    part: Option<&PartContext>,
) -> Option<DiscoveredModel> {
    let meta = std::fs::metadata(path).ok()?;
    let size_bytes = meta.len();
    // A GGUF header alone is a few hundred bytes; anything this small is junk.
    if size_bytes < 1024 {
        return None;
    }
    let filename = path.file_name()?.to_string_lossy().into_owned();

    // Projector sidecars (`mmproj-*`, architecture "clip") are not language
    // models - they cannot be sliced or served, so they do not belong in a list
    // of things to run.
    if filename.starts_with("mmproj") {
        return None;
    }

    // MTP draft sidecars (`mtp-*`, e.g. Unsloth's "shared MTP module") reuse
    // the main model's `embed_tokens` and are not complete language models
    // either - see `find_mtp_draft_sibling`.
    if filename.to_ascii_lowercase().starts_with("mtp-") {
        return None;
    }

    // Quem manda é o `.pom`. O nome do arquivo é o hash, e hash não diz de que
    // modelo é nem que pedaço dele traz; o manifesto diz as duas coisas. Sem
    // manifesto o arquivo não é parte de fatiamento nenhum - é um GGUF que
    // alguém deixou na pasta, e aparece pelo nome dele mesmo.
    let sha256 = part.map(|c| c.sha256.clone()).or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string())
    });

    let shard_range = match part {
        Some(c) => Some((c.start_layer, c.end_layer)),
        // Sem manifesto, o carimbo do cabeçalho GGUF ainda identifica uma
        // fatia solta - é o que sobra de um `.pom` perdido.
        None => None,
    };
    // A chave do conjunto é o manifesto do modelo original. Fatiar o mesmo
    // modelo duas vezes com cortes diferentes adiciona alternativas ao mesmo
    // inventário, que é exatamente o que o planejador pode escolher.
    let shard_group = part.map(|c| c.run_key.clone());

    // Header-derived layer information is filled by `enrich` after the fast
    // inventory is published. Manifest-backed slices already know their total.
    let total_layers = part.map(|c| c.total_layers);

    // O nome que o operador lê. Numa parte ele vem do manifesto; sem manifesto
    // é o nome do arquivo, como sempre foi.
    let name = match part {
        Some(c) => c.display_name(),
        None => name,
    };

    Some(DiscoveredModel {
        id: format!("{:?}:{}", source, path.display()).to_lowercase(),
        name,
        source,
        source_label: source.label(),
        path: path.to_string_lossy().into_owned(),
        mmproj_path: None,
        mmproj_reason: Some("aguardando leitura dos metadados GGUF para validar o projetor".into()),
        identity_names: Vec::new(),
        size_bytes,
        multipart: false,
        multipart_parts: Vec::new(),
        multipart_error: None,
        metadata_error: None,
        metadata_identity: None,
        architecture: None,
        is_moe: false,
        total_layers,
        // Numa parte o nome do arquivo é o hash, e hash não tem tag de
        // quantização para achar: quem sabe é o manifesto.
        quant: match part {
            Some(c) => c.quant.clone(),
            None => parse_quant(&filename),
        },
        shard_range,
        shard_group,
        sha256,
        model_label: part.map(|c| c.model_label.clone()),
        pom_id: part.map(|c| c.pom_id.clone()),
        tensor_parallel: part.is_some_and(|c| c.tensor_parallel),
        tp_rank: part.and_then(|c| c.tp_rank),
        tp_size: part.and_then(|c| c.tp_size),
        capabilities: None,
    })
}

#[derive(Debug)]
struct ProjectorResolution {
    path: Option<String>,
    reason: Option<String>,
}

/// Resolve a sibling only when GGUF identity evidence, or a unique dimensional
/// match across the directory, supports the association. Quantization is never
/// used to select a projector.
fn resolve_mmproj_sibling(model: &Path, model_info: &GgufInfo) -> ProjectorResolution {
    let Some(parent) = model.parent() else {
        return no_projector("o modelo não tem diretório pai para procurar projetores");
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return no_projector("não foi possível ler o diretório do modelo para procurar projetores");
    };
    let mut projectors = Vec::new();
    let mut sibling_models = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if path == model || !path.is_file() || !extension.eq_ignore_ascii_case("gguf") {
            continue;
        }
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(info) = cached_info(&path, metadata.len(), file_mtime_fingerprint(&metadata)) else {
            continue;
        };
        let is_projector = info.architecture.as_deref() == Some("clip")
            || filename.to_ascii_lowercase().contains("mmproj");
        if is_projector {
            projectors.push((path, filename, info));
        } else if let Some(dim) = info.embedding_length {
            sibling_models.push((dim, filename, info.identity_names));
        }
    }
    if projectors.is_empty() {
        return no_projector("nenhum projetor GGUF foi encontrado neste diretório");
    }

    let model_name = model
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut identity_matches = Vec::new();
    let mut dimension_matches = Vec::new();
    let mut identity_dimension_mismatches = Vec::new();
    for (path, filename, info) in &projectors {
        let dimensions_known =
            model_info.embedding_length.is_some() && info.projector_projection_dim.is_some();
        let dimensions_match = match (model_info.embedding_length, info.projector_projection_dim) {
            (Some(model_dim), Some(projector_dim)) => model_dim == projector_dim,
            _ => true,
        };
        let identity_match = identities_match(&model_info.identity_names, &info.identity_names);
        let filename_match = identities_match(
            &[filename_identity(&model_name, false)],
            &[filename_identity(filename, true)],
        );
        let metadata_conflict = has_identity_evidence(&model_info.identity_names)
            && has_identity_evidence(&info.identity_names)
            && !identity_match;
        if identity_match {
            if dimensions_match {
                identity_matches.push(path.clone());
            } else {
                identity_dimension_mismatches
                    .push((model_info.embedding_length, info.projector_projection_dim));
            }
        } else if !metadata_conflict && filename_match && dimensions_known && dimensions_match {
            // Names corroborate dimensions but are never accepted by themselves.
            dimension_matches.push(path.clone());
        }
    }

    let selected = if identity_matches.len() == 1 {
        identity_matches.pop()
    } else if identity_matches.len() > 1 {
        return no_projector(
            "mais de um projetor tem identidade GGUF compatível; renomeie ou separe os arquivos para desambiguar",
        );
    } else if dimension_matches.len() == 1 {
        dimension_matches.pop()
    } else if dimension_matches.len() > 1 {
        return no_projector(
            "mais de um projetor tem nome e dimensão compatíveis; inclua uma identidade GGUF correspondente",
        );
    } else {
        // Generic sidecar names remain supported when both dimensions are
        // present and uniquely identify this model among its directory peers.
        let matching_projectors = projectors
            .iter()
            .filter(|(_, _, info)| {
                model_info.embedding_length.is_some()
                    && info.projector_projection_dim == model_info.embedding_length
                    && (!has_identity_evidence(&model_info.identity_names)
                        || !has_identity_evidence(&info.identity_names)
                        || identities_match(&model_info.identity_names, &info.identity_names))
            })
            .count();
        let model_name_identities = model_info
            .identity_names
            .iter()
            .cloned()
            .chain(std::iter::once(filename_identity(&model_name, false)))
            .collect::<Vec<_>>();
        let matching_models = model_info.embedding_length.map_or(0, |dimension| {
            1 + sibling_models
                .iter()
                .filter(|(candidate_dim, _, _)| *candidate_dim == dimension)
                .filter(|(_, filename, identities)| {
                    !identities_match(
                        &model_name_identities,
                        &identities
                            .iter()
                            .cloned()
                            .chain(std::iter::once(filename_identity(filename, false)))
                            .collect::<Vec<_>>(),
                    )
                })
                .count()
        });
        if matching_projectors == 1 && matching_models == 1 {
            projectors
                .iter()
                .find(|(_, _, info)| info.projector_projection_dim == model_info.embedding_length)
                .map(|(path, _, _)| path.clone())
        } else {
            None
        }
    };

    if let Some(path) = selected {
        return ProjectorResolution {
            path: Some(path.to_string_lossy().into_owned()),
            reason: None,
        };
    }
    if let Some((model_dim, projector_dim)) = identity_dimension_mismatches.first() {
        return no_projector(&format!(
            "projetor identificado para este modelo tem dimensão incompatível: o modelo exige embedding_length={}, mas o mmproj declara clip.vision.projection_dim={}; use o projetor compatível",
            model_dim.map_or_else(|| "desconhecido".into(), |value| value.to_string()),
            projector_dim.map_or_else(|| "desconhecida".into(), |value| value.to_string()),
        ));
    }
    no_projector(
        "não foi possível provar a associação do projetor; confirme general.name/base_model nos GGUF ou dimensões únicas compatíveis, e separe modelos ambíguos",
    )
}

pub(crate) fn find_verified_mmproj_sibling(model: &Path) -> Result<String, String> {
    let info = gguf::read_info(model)
        .map_err(|error| format!("não foi possível ler o cabeçalho GGUF do modelo: {error:#}"))?;
    let resolution = resolve_mmproj_sibling(model, &info);
    resolution.path.ok_or_else(|| {
        resolution
            .reason
            .unwrap_or_else(|| "não foi possível provar a associação do projetor".into())
    })
}

fn no_projector(reason: &str) -> ProjectorResolution {
    ProjectorResolution {
        path: None,
        reason: Some(reason.to_string()),
    }
}

fn filename_identity(filename: &str, projector: bool) -> String {
    let stem = filename
        .strip_suffix(".gguf")
        .or_else(|| filename.strip_suffix(".GGUF"))
        .unwrap_or(filename);
    let mut value = stem.to_ascii_lowercase();
    if projector {
        value = value.replace("mmproj", " ");
    }
    strip_quant_suffix(&value)
}

fn strip_quant_suffix(value: &str) -> String {
    let Some(quant) = parse_quant(value) else {
        return value.to_string();
    };
    let uppercase = value.to_ascii_uppercase();
    uppercase
        .find(&quant)
        .map(|index| value[..index].to_string())
        .unwrap_or_else(|| value.to_string())
}

fn identities_match(left: &[String], right: &[String]) -> bool {
    left.iter().any(|left| {
        let a = identity_tokens(left);
        right.iter().any(|right| {
            let b = identity_tokens(right);
            if a.is_empty() || b.is_empty() {
                return false;
            }
            let robust_whole_name = |tokens: &[String]| {
                tokens.len() >= 2 || tokens.iter().any(|token| token.len() >= 5)
            };
            if (a == b && robust_whole_name(&a))
                || (b.len() >= 2 && contains_tokens(&a, &b))
                || (a.len() >= 2 && contains_tokens(&b, &a))
            {
                return true;
            }
            let shared = a
                .iter()
                .filter(|token| b.contains(token))
                .collect::<Vec<_>>();
            shared.len() >= 2
        })
    })
}

fn has_identity_evidence(names: &[String]) -> bool {
    names.iter().any(|name| !identity_tokens(name).is_empty())
}

fn contains_tokens(haystack: &[String], needle: &[String]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn identity_tokens(value: &str) -> Vec<String> {
    let stop_words = [
        "base",
        "chat",
        "clip",
        "for",
        "gguf",
        "instruct",
        "mmproj",
        "model",
        "multimodal",
        "projector",
    ];
    strip_quant_suffix(value)
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|token| {
            !stop_words.contains(&token.as_str())
                && parse_quant(token).is_none()
                && !token.chars().all(|character| character.is_ascii_digit())
        })
        .collect()
}

/// Validate an explicitly persisted projector against the selected GGUF. This
/// catches stale configurations before the inference server is touched.
pub(crate) fn validate_mmproj_pair(model: &Path, projector: &Path) -> anyhow::Result<()> {
    let model_info = gguf::read_info(model)
        .map_err(|error| anyhow::anyhow!("não foi possível ler o GGUF do modelo: {error:#}"))?;
    let projector_info = gguf::read_info(projector)
        .map_err(|error| anyhow::anyhow!("não foi possível ler o GGUF mmproj: {error:#}"))?;
    anyhow::ensure!(
        projector_info.architecture.as_deref() == Some("clip")
            || projector.file_name().is_some_and(|name| name
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("mmproj")),
        "o arquivo configurado não se identifica como projetor multimodal GGUF"
    );
    let dimensions_match = if let (Some(model_dim), Some(projector_dim)) = (
        model_info.embedding_length,
        projector_info.projector_projection_dim,
    ) {
        anyhow::ensure!(
            model_dim == projector_dim,
            "mmproj incompatível: o modelo exige embedding_length={model_dim}, mas o projetor declara clip.vision.projection_dim={projector_dim}"
        );
        true
    } else {
        false
    };
    let metadata_identity_match =
        identities_match(&model_info.identity_names, &projector_info.identity_names);
    anyhow::ensure!(
        metadata_identity_match
            || (dimensions_match
                && (!has_identity_evidence(&model_info.identity_names)
                    || !has_identity_evidence(&projector_info.identity_names))),
        "não foi possível provar que este mmproj pertence ao modelo; associe metadados general.name/base_model compatíveis e dimensões GGUF iguais"
    );
    Ok(())
}

/// A `.gguf` sibling in the same directory whose filename starts with
/// `mtp-`, choosing the closest quantisation when more than one candidate
/// exists. This is separate from the verified projector association above:
/// some architectures ship an MTP draft head ("shared MTP module") instead of
/// embedding `nextn` weights in the main GGUF - see `crate::llm::mtp`.
///
/// The `mtp-` prefix is the convention observed in the wild (e.g. Unsloth's
/// `mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf`); `mmproj`'s convention is a
/// substring match instead of a prefix because llama.cpp itself only checks
/// `contains`, but no such precedent exists for MTP drafts, so a prefix is
/// used here to avoid false positives on an unrelated model whose name
/// happens to contain "mtp".
pub(crate) fn find_mtp_draft_sibling(model: &Path) -> Option<String> {
    let parent = model.parent()?;
    let model_name = model.file_name()?.to_string_lossy();
    let model_bits = parse_quant_bits(&model_name);
    let mut candidates = std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if path == model
                || !path.is_file()
                || !name.to_ascii_lowercase().starts_with("mtp-")
                || !name.to_ascii_lowercase().ends_with(".gguf")
            {
                return None;
            }
            let distance = match (model_bits, parse_quant_bits(&name)) {
                (Some(model_bits), Some(draft_bits)) => model_bits.abs_diff(draft_bits),
                _ => u32::MAX,
            };
            Some((distance, name, path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .first()
        .map(|(_, _, path)| path.to_string_lossy().into_owned())
}

fn parse_quant_bits(filename: &str) -> Option<u32> {
    let quant = parse_quant(filename)?;
    let digits = quant
        .strip_prefix("IQ")
        .or_else(|| quant.strip_prefix('Q'))
        .or_else(|| quant.strip_prefix('F'))?;
    digits
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn file_mtime_fingerprint(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn metadata_identity(path: &Path, meta: &std::fs::Metadata) -> MetadataIdentity {
    MetadataIdentity {
        path: path.to_string_lossy().into_owned(),
        size: meta.len(),
        mtime: file_mtime_fingerprint(meta),
    }
}

pub(crate) fn current_metadata_identity(model: &DiscoveredModel) -> Option<MetadataIdentity> {
    let path = Path::new(&model.path);
    let meta = std::fs::metadata(path).ok()?;
    Some(metadata_identity(path, &meta))
}

/// Whether a model's metadata still describes the bytes currently on disk.
pub(crate) fn metadata_is_current(model: &DiscoveredModel) -> bool {
    model
        .metadata_identity
        .as_ref()
        .is_some_and(|expected| current_metadata_identity(model).as_ref() == Some(expected))
}

enum EnrichResult {
    /// The header and optional capability evidence were read successfully.
    Ready,
    /// The file could not be read in this generation. Keep it visible with a
    /// concrete error, and let a later invalidation create a new generation.
    Failed,
    /// The fast scan found an item that is not a runnable model.
    Drop,
}

/// Read the header for one item after the fast inventory is already visible.
fn enrich_model(model: &mut DiscoveredModel) -> EnrichResult {
    let path = PathBuf::from(&model.path);
    let Some(meta) = std::fs::metadata(&path).ok() else {
        model.metadata_identity = None;
        model.metadata_error = Some(format!("arquivo não encontrado: {}", path.display()));
        return EnrichResult::Failed;
    };
    let identity = metadata_identity(&path, &meta);
    model.metadata_identity = Some(identity);
    model.size_bytes = meta.len();
    if model.size_bytes < 1024 {
        model.metadata_error = Some(format!(
            "arquivo GGUF pequeno demais: {} bytes",
            model.size_bytes
        ));
        return EnrichResult::Failed;
    }
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let info = match cached_info(&path, model.size_bytes, file_mtime_fingerprint(&meta)) {
        Ok(info) => info,
        Err(error) => {
            model.metadata_error = Some(format!("não foi possível ler o cabeçalho GGUF: {error}"));
            return EnrichResult::Failed;
        }
    };
    if info.architecture.as_deref() == Some("clip")
        || filename.starts_with("mmproj")
        || filename.to_ascii_lowercase().starts_with("mtp-")
    {
        return EnrichResult::Drop;
    }

    model.identity_names = info.identity_names.clone();
    let projector = resolve_mmproj_sibling(&path, &info);
    model.mmproj_path = projector.path;
    model.mmproj_reason = projector.reason;
    model.architecture = info.architecture.clone();
    model.is_moe = info.is_moe();
    if let Err(error) = fill_multipart_metadata(model, &path, &info) {
        if info.split_count.unwrap_or(1) > 1 {
            let split_count = info.split_count.unwrap_or_default();
            model.multipart = true;
            model.multipart_parts.clear();
            model.multipart_error = Some(format!(
                "GGUF multipart declara split.count={split_count}, mas o conjunto não pôde ser lido: {error}. Coloque todas as partes no mesmo diretório e selecione a primeira parte."
            ));
            // The first physical file is not the model size. Zero makes the UI
            // refuse to present a false byte count until the set is complete.
            model.size_bytes = 0;
        } else {
            model.metadata_error = Some(format!(
                "não foi possível descrever a fonte GGUF: {error:#}"
            ));
            return EnrichResult::Failed;
        }
    } else {
        model.multipart_error = None;
    }
    model.metadata_error = None;
    // The catalog is a header index, not a model validator. Layer count,
    // architecture and the MTP hint are all available in the KV header. The
    // tensor table and payload stay behind the slicing/inference boundaries;
    // scanning tensor names for every model made a slow volume hold
    // `metadata_complete` open for no catalog field that needed it.
    model.capabilities = Some(ModelCapabilityEvidence {
        thinking: info.thinking_capability(),
        mtp: info.nextn_predict_layers.map(|layers| layers > 0),
    });
    if model.shard_range.is_none() {
        model.shard_range = info.shard_range;
        model.total_layers = match model.shard_range {
            Some(_) => info.original_block_count,
            None => info.total_layers(),
        };
    }
    EnrichResult::Ready
}

/// Enrich one already-listed model without walking the rest of the catalog.
///
/// Deployment uses this when the operator has selected a whole model while
/// unrelated files are still waiting for their headers. It deliberately shares
/// the same header-only path as the background catalog task.
pub(crate) fn enrich_selected(model: &mut DiscoveredModel) -> Result<(), String> {
    match enrich_model(model) {
        EnrichResult::Ready => Ok(()),
        EnrichResult::Failed => Err(model
            .metadata_error
            .clone()
            .unwrap_or_else(|| "não foi possível ler os metadados do GGUF".to_string())),
        EnrichResult::Drop => Err("o arquivo não é um modelo GGUF implantável".to_string()),
    }
}

fn fill_multipart_metadata(
    model: &mut DiscoveredModel,
    path: &Path,
    info: &GgufInfo,
) -> anyhow::Result<()> {
    let metadata = crate::multipart::catalog_metadata(path, info)?;
    model.multipart = metadata.multipart;
    let source_bytes = metadata.source_bytes;
    model.multipart_parts = metadata
        .parts
        .into_iter()
        .map(|part| MultipartPart {
            index: part.index,
            path: part.path.to_string_lossy().into_owned(),
            bytes: part.bytes,
            tensor_count: part.tensor_count,
        })
        .collect();
    if metadata.multipart {
        model.size_bytes = source_bytes;
    }
    Ok(())
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
                0 => path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                1 => comps[0].clone(),
                _ => format!("{}/{}", comps[0], comps[comps.len() - 2]),
            }
        }
        Err(_) => path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    }
}

/// The libraries this machine already has, used until the operator edits the
/// list. Only directories that really exist are offered: the configuration
/// screen refuses paths that are not there, so proposing a phantom default
/// would hand the user a list they cannot save.
pub fn default_model_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(home) = home_dir() {
        push(home.join(".pom").join("models"));
    }
    for home in lmstudio_homes() {
        push(lmstudio_models_dir(&home));
    }
    for dir in ollama_model_dirs() {
        push(dir);
    }
    out
}

/// What a configured directory turns out to be, decided by looking at it.
///
/// An Ollama store has to be read through its manifests, since the weights are
/// digest-named blobs; LM Studio and anything else are just trees of `.gguf`
/// files — which is exactly what the captain's `/Volumes/sdcard/models` will be.
///
/// LM Studio is checked first on purpose: its model directory also carries
/// `blobs/` and `manifests/`, both empty, so the Ollama layout alone would
/// misread it and the library would silently come back empty.
pub fn detect_source(dir: &Path) -> ModelSource {
    let text = dir.to_string_lossy().to_ascii_lowercase();
    if text.contains("lmstudio") || text.contains("lm-studio") {
        return ModelSource::LmStudio;
    }
    if text.contains(".ollama") || text.ends_with("ollama/models") || is_ollama_store(dir) {
        return ModelSource::Ollama;
    }
    ModelSource::Folder
}

/// A store with manifests *and* blobs in it. Empty directories prove nothing.
fn is_ollama_store(dir: &Path) -> bool {
    let has_entries = |p: PathBuf| {
        std::fs::read_dir(p)
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
    };
    has_entries(dir.join("manifests")) && has_entries(dir.join("blobs"))
}

/// Walk one directory of `.gguf` files.
fn scan_gguf_dir(
    dir: &Path,
    source: ModelSource,
    out: &mut Vec<DiscoveredModel>,
    roots: &mut Vec<ScannedRoot>,
) {
    let exists = dir.is_dir();
    let mut files = Vec::new();
    if exists {
        find_gguf(dir, 0, &mut files);
    }
    let mut found = 0;
    for f in files {
        // LM Studio stores `<publisher>/<repo>/<file>.gguf` and that pairing is
        // the name people know. Anywhere else the filename is the name — and it
        // has to be, since two slices in one subfolder would otherwise both be
        // called after the folder and stop being telling apart.
        let name = match source {
            ModelSource::LmStudio => lmstudio_name(dir, &f),
            _ => f
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        };
        if let Some(m) = describe(&f, name, source, None) {
            if !out.iter().any(|e| e.path == m.path) {
                out.push(m);
                found += 1;
            }
        }
    }
    roots.push(ScannedRoot {
        source,
        path: display_path(dir),
        exists,
        found,
    });
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

fn scan_ollama_dir(
    models_dir: &Path,
    out: &mut Vec<DiscoveredModel>,
    roots: &mut Vec<ScannedRoot>,
) {
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
            if let Some(m) = describe(&blob, name, ModelSource::Ollama, None) {
                if !out.iter().any(|e| e.path == m.path) {
                    out.push(m);
                    found += 1;
                }
            }
        }
    }

    roots.push(ScannedRoot {
        source: ModelSource::Ollama,
        path: display_path(models_dir),
        exists,
        found,
    });
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
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(error) => {
            debug!(
                path = %root.display(),
                error = %error,
                "ignorando diretório ilegível durante a descoberta de manifests"
            );
            return;
        }
    };
    for entry in rd {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                debug!(
                    path = %root.display(),
                    error = %error,
                    "ignorando entrada ilegível durante a descoberta de manifests"
                );
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_files(&path, depth + 1, out),
            Ok(ft) if ft.is_file() => out.push(path),
            Ok(_) => {}
            Err(error) => {
                debug!(
                    path = %path.display(),
                    error = %error,
                    "ignorando entrada cujo tipo não pôde ser lido durante a descoberta de manifests"
                );
            }
        }
    }
}

/// A pasta de fatias: um diretório por modelo, e dentro dele as partes
/// (`<sha256>.gguf`) mais os manifestos (`.pom`) das rodadas de fatiamento.
///
/// Quem manda aqui é o manifesto, não o nome do arquivo. Uma parte listada num
/// `.pom` entra no catálogo sabendo de qual modelo é e que faixa cobre; um
/// `.gguf` solto na pasta continua aparecendo (o operador pode ter largado um
/// modelo inteiro ali), só que sem grupo - e sem grupo ele não é fatia de nada.
fn scan_shard_dir(dir: &Path, out: &mut Vec<DiscoveredModel>, roots: &mut Vec<ScannedRoot>) {
    let exists = dir.is_dir();
    let mut found = 0;

    // Os manifestos primeiro, sempre. É deles que sai a identidade de cada
    // parte - de que modelo é, que camadas traz, qual a quantização -, e sem
    // isso um `<sha256>.gguf` no catálogo é um arquivo de nome ilegível que a
    // tela mostraria como se fosse um modelo inteiro.
    let index = part_index(dir);

    let mut files = Vec::new();
    if exists {
        find_gguf(dir, 0, &mut files);
    }
    for f in files {
        let name = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let canonico = f.canonicalize().unwrap_or_else(|_| f.clone());
        let ctx = index.get(&canonico).or_else(|| index.get(&f));
        if let Some(m) = describe(&f, name, ModelSource::Local, ctx) {
            if !out.iter().any(|e| e.path == m.path) {
                out.push(m);
                found += 1;
            }
        }
    }
    roots.push(ScannedRoot {
        source: ModelSource::Local,
        path: display_path(dir),
        exists,
        found,
    });
}

/// Todo arquivo de parte que os `.pom` da pasta de fatias descrevem.
///
/// Varre `<shard_dir>/<modelo>/*.pom` e devolve, por caminho de arquivo, o que
/// o manifesto diz sobre aquela parte. É esta varredura que faz o nó, ao
/// arrancar, reconstruir o catálogo a partir dos manifestos em vez de tentar
/// adivinhar coisa nenhuma pelo nome dos arquivos.
///
/// Uma parte listada num manifesto mas ausente do disco simplesmente não entra:
/// o índice descreve o que existe.
pub fn part_index(shard_dir: &Path) -> HashMap<PathBuf, PartContext> {
    let mut index = HashMap::new();
    let rd = match std::fs::read_dir(shard_dir) {
        Ok(rd) => rd,
        Err(error) => {
            debug!(
                path = %shard_dir.display(),
                error = %error,
                "ignorando pasta de fatias ilegível durante a descoberta de manifests"
            );
            return index;
        }
    };
    for entrada in rd {
        let entrada = match entrada {
            Ok(entrada) => entrada,
            Err(error) => {
                debug!(
                    path = %shard_dir.display(),
                    error = %error,
                    "ignorando entrada ilegível durante a indexação de fatias"
                );
                continue;
            }
        };
        let pasta = entrada.path();
        if !pasta.is_dir() {
            continue;
        }
        for m in crate::pom::load_dir(&pasta) {
            for (parte, tensor_parallel) in m
                .parts
                .iter()
                .map(|parte| (parte, false))
                .chain(m.tp_parts.iter().map(|parte| (parte, true)))
            {
                let Ok(caminho) = crate::pom::part_path(&pasta, &parte.sha256) else {
                    continue;
                };
                if !caminho.is_file() {
                    continue;
                }
                let ctx = PartContext {
                    model_label: m.model.label.clone(),
                    total_layers: m.model.total_layers,
                    start_layer: parte.start_layer,
                    end_layer: parte.end_layer,
                    sha256: parte.sha256.clone(),
                    pom_id: m.pom_id.clone(),
                    run_key: format!(
                        "{}-{}",
                        crate::pom::sanitize_name(&m.model.name),
                        crate::pom::short_id(&m.pom_id)
                    ),
                    quant: m.model.quant.clone(),
                    tensor_parallel,
                    tp_rank: parte.tp.map(|tp| tp.rank),
                    tp_size: parte.tp.map(|tp| tp.size),
                };
                let chave = caminho.canonicalize().unwrap_or(caminho);
                index.insert(chave, ctx);
            }
        }
    }
    index
}

/// The path as configured — what the operator typed and will recognise.
///
/// Deliberately not resolved: the captain's `~/.ollama/models` is a symlink into
/// the LM Studio library, and reporting the far end of it made the screen name a
/// directory nobody had configured.
fn display_path(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

/// The path as the filesystem sees it, used only to tell two entries apart.
fn identity_path(dir: &Path) -> PathBuf {
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
}

/// Group slices by their shared base name and report whether the set covers the
/// model end to end.
fn build_shard_sets(models: &[DiscoveredModel]) -> Vec<ShardSet> {
    build_shard_sets_with_source_hash(models, true)
}

/// Build the fast inventory without reading an entire source model just to
/// compare its content hash with a manifest. The background catalog may use
/// the slower verified form after the inventory is visible.
fn build_shard_sets_fast(models: &[DiscoveredModel]) -> Vec<ShardSet> {
    build_shard_sets_with_source_hash(models, false)
}

fn build_shard_sets_with_source_hash(
    models: &[DiscoveredModel],
    verify_source_hash: bool,
) -> Vec<ShardSet> {
    let mut groups: HashMap<String, (Vec<DiscoveredModel>, Vec<DiscoveredModel>)> = HashMap::new();
    for m in models.iter().filter(|m| m.is_shard()) {
        let key = m.shard_group.clone().unwrap_or_else(|| m.name.clone());
        let entry = groups.entry(key).or_default();
        if m.is_tensor_parallel() {
            entry.1.push(m.clone());
        } else {
            entry.0.push(m.clone());
        }
    }

    let mut sets: Vec<ShardSet> = groups
        .into_iter()
        .map(|(group, (mut shards, mut tp_parts))| {
            shards.sort_by_key(|s| s.shard_range.map(|r| r.0).unwrap_or(0));
            // Keep alternative TP groups together first, then order each group
            // by rank. The UI can render each group as 1ofN, 2ofN, etc.
            tp_parts
                .sort_by_key(|s| (s.tp_size.unwrap_or(u32::MAX), s.tp_rank.unwrap_or(u32::MAX)));

            // How far at least one exact pipeline chain reaches without a gap.
            // A source manifest can contain alternatives such as 0-15 and
            // 0-19; stopping at the first sorted row would incorrectly call
            // the aggregate incomplete even when 0-15 + 16-31 is complete.
            let mut contiguous_to = 0u32;
            while let Some(end) = shards
                .iter()
                .filter_map(|s| match s.shard_range {
                    Some((start, end)) if start == contiguous_to => Some(end),
                    _ => None,
                })
                .max()
            {
                contiguous_to = end.saturating_add(1);
            }

            // Slices written by split_model.py record the original layer count.
            // Older ones do not, so fall back to what the set itself covers —
            // which is the honest reading: "these slices reach layer N".
            let declared = shards
                .iter()
                .chain(tp_parts.iter())
                .find_map(|s| s.total_layers);
            let total_layers = declared.or(if contiguous_to > 0 {
                Some(contiguous_to)
            } else {
                None
            });

            let pipeline_complete = matches!(total_layers, Some(t) if t > 0 && contiguous_to >= t);
            let mut tp_sizes: Vec<u32> = tp_parts.iter().filter_map(|s| s.tp_size).collect();
            tp_sizes.sort_unstable();
            tp_sizes.dedup();
            let tensor_complete = tp_sizes.into_iter().any(|size| {
                let mut ranks: Vec<u32> = tp_parts
                    .iter()
                    .filter(|part| part.tp_size == Some(size))
                    .filter_map(|part| part.tp_rank)
                    .collect();
                ranks.sort_unstable();
                ranks.dedup();
                ranks == (0..size).collect::<Vec<_>>()
            });
            let complete = if shards.is_empty() {
                tensor_complete
            } else {
                pipeline_complete
            };

            let model_label = shards
                .iter()
                .chain(tp_parts.iter())
                .find_map(|s| s.model_label.clone())
                .unwrap_or_else(|| group.clone());
            let pom_id = shards
                .iter()
                .chain(tp_parts.iter())
                .find_map(|s| s.pom_id.clone())
                .unwrap_or_default();
            let direct_projectors = shards
                .iter()
                .chain(tp_parts.iter())
                .filter_map(|shard| shard.mmproj_path.clone())
                .collect::<std::collections::HashSet<_>>();
            let (mmproj_path, mmproj_reason) = if direct_projectors.len() == 1 {
                (direct_projectors.into_iter().next(), None)
            } else if direct_projectors.len() > 1 {
                (None, Some("as fatias do mesmo conjunto declaram projetores diferentes; revise o manifesto e os arquivos locais".into()))
            } else if let Some(label) = shards
                .iter()
                .chain(tp_parts.iter())
                .find_map(|shard| shard.model_label.as_deref())
            {
                let label_matches = models
                    .iter()
                    .filter(|model| !model.is_shard())
                    .filter(|model| {
                        identities_match(
                            &model
                                .identity_names
                                .iter()
                                .cloned()
                                .chain(std::iter::once(model.name.clone()))
                                .collect::<Vec<_>>(),
                            &[label.to_string()],
                        )
                    })
                    .collect::<Vec<_>>();
                let matches = if verify_source_hash && crate::pom::is_sha256_hex(&pom_id) {
                    label_matches
                        .into_iter()
                        .filter(|model| {
                            cached_content_hash(Path::new(&model.path)).as_deref()
                                == Some(pom_id.as_str())
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                match matches.as_slice() {
                    [source] => (
                        source.mmproj_path.clone(),
                        source.mmproj_reason.clone().filter(|_| source.mmproj_path.is_none()),
                    ),
                    [] => (None, Some("não foi possível provar pelo pom_id que um modelo GGUF inteiro no catálogo é a origem destas fatias; mantenha a cópia original cujo SHA-256 corresponda ao manifesto".into())),
                    _ => (None, Some("mais de um modelo inteiro corresponde ao rótulo do manifesto; a associação do projetor é ambígua".into())),
                }
            } else {
                (None, Some("as fatias não têm identidade de manifesto suficiente para herdar um projetor".into()))
            };

            ShardSet {
                model_label,
                mmproj_path,
                mmproj_reason,
                pom_id,
                group,
                total_layers,
                total_layers_inferred: declared.is_none(),
                shards,
                tp_parts,
                complete,
            }
        })
        .collect();

    sets.sort_by(|a, b| a.group.cmp(&b.group));
    sets
}

/// Scan the configured directories without opening any GGUF header.
///
/// `shard_dir` is where this node keeps its own slices; `model_dirs` are the
/// libraries of whole models. The slice directory is scanned first so a file
/// reachable from both is reported as a slice, which is the reading that
/// decides whether a peer may pull it.
pub fn discover_fast(model_dirs: &[PathBuf], shard_dir: &Path) -> Catalog {
    let mut models: Vec<DiscoveredModel> = Vec::new();
    let mut scanned: Vec<ScannedRoot> = Vec::new();

    scan_shard_dir(shard_dir, &mut models, &mut scanned);

    // Two entries pointing at the same directory — a symlink, or the slice
    // directory listed again — must be scanned once. Compared resolved, because
    // that is the only way to see that they are the same place.
    let mut seen = vec![identity_path(shard_dir)];
    for dir in model_dirs {
        let id = identity_path(dir);
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        match detect_source(dir) {
            ModelSource::Ollama => scan_ollama_dir(dir, &mut models, &mut scanned),
            source => scan_gguf_dir(dir, source, &mut models, &mut scanned),
        }
    }

    models.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.path.cmp(&b.path))
    });

    let shard_sets = build_shard_sets_fast(&models);
    Catalog {
        models,
        shard_sets,
        scanned,
        metadata_complete: false,
    }
}

/// Enrich an already published inventory with GGUF header information.
///
/// This stays synchronous so callers can run it in `spawn_blocking`; keeping
/// it separate from `discover_fast` is what lets the HTTP catalog answer with
/// names and paths while a slow volume is still being read.
pub fn enrich(mut catalog: Catalog) -> Catalog {
    catalog
        .models
        .retain_mut(|model| !matches!(enrich_model(model), EnrichResult::Drop));
    catalog.shard_sets = build_shard_sets(&catalog.models);
    for set in &catalog.shard_sets {
        for shard in catalog
            .models
            .iter_mut()
            .filter(|model| model.shard_group.as_deref() == Some(set.group.as_str()))
        {
            shard.mmproj_path = set.mmproj_path.clone();
            shard.mmproj_reason = set.mmproj_reason.clone();
        }
    }
    catalog.metadata_complete = true;
    catalog
}

/// Rebuild the derived slice grouping after metadata from one generation is
/// copied into another.
pub(crate) fn rebuild_shard_sets(catalog: &mut Catalog) {
    catalog.shard_sets = build_shard_sets(&catalog.models);
}

/// Rebuild the derived slice groups without hashing whole source models.
/// Background inventory publication uses this form so manifest verification
/// cannot hold the cache lock while a large model is read from disk.
pub(crate) fn rebuild_shard_sets_fast(catalog: &mut Catalog) {
    catalog.shard_sets = build_shard_sets_fast(&catalog.models);
}

/// Full discovery for non-UI callers that still need a complete answer before
/// continuing, such as transfer path resolution and existing tests.
pub fn discover(model_dirs: &[PathBuf], shard_dir: &Path) -> Catalog {
    enrich(discover_fast(model_dirs, shard_dir))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn gguf_header_fixture(metadata: &[(&str, u32)], strings: &[(&str, &str)]) -> Vec<u8> {
        const GGUF_MAGIC: u32 = 0x4655_4747;
        const TYPE_UINT32: u32 = 4;
        const TYPE_STRING: u32 = 8;
        let encoded_string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let mut entries = Vec::new();
        for (key, value) in strings {
            let mut entry = encoded_string(key);
            entry.extend_from_slice(&TYPE_STRING.to_le_bytes());
            entry.extend_from_slice(&encoded_string(value));
            entries.push(entry);
        }
        for (key, value) in metadata {
            let mut entry = encoded_string(key);
            entry.extend_from_slice(&TYPE_UINT32.to_le_bytes());
            entry.extend_from_slice(&value.to_le_bytes());
            entries.push(entry);
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.resize(4096, 0);
        bytes
    }

    pub(crate) fn text_model_fixture(
        name: &str,
        architecture: &str,
        embedding_length: u32,
    ) -> Vec<u8> {
        let embedding_key = format!("{architecture}.embedding_length");
        let layers_key = format!("{architecture}.block_count");
        gguf_header_fixture(
            &[
                (embedding_key.as_str(), embedding_length),
                (layers_key.as_str(), 32),
            ],
            &[
                ("general.architecture", architecture),
                ("general.name", name),
            ],
        )
    }

    pub(crate) fn projector_fixture(name: &str, projection_dim: u32) -> Vec<u8> {
        gguf_header_fixture(
            &[("clip.vision.projection_dim", projection_dim)],
            &[("general.architecture", "clip"), ("general.name", name)],
        )
    }

    fn generic_projector_fixture(projection_dim: u32) -> Vec<u8> {
        gguf_header_fixture(
            &[("clip.vision.projection_dim", projection_dim)],
            &[("general.architecture", "clip"), ("general.name", "mmproj")],
        )
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
    fn skips_secondary_parts_only() {
        assert!(!is_secondary_part(Path::new("m-00001-of-00003.gguf")));
        assert!(is_secondary_part(Path::new("m-00002-of-00003.gguf")));
        assert!(!is_secondary_part(Path::new("plain.gguf")));
    }

    #[test]
    fn catalog_records_the_verified_mmproj_sibling_without_listing_it() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-catalog-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("gemma-3-4b-it-Q4_K_M.gguf");
        let projector = models.join("mmproj-gemma-3-4b-it-f16.gguf");
        std::fs::write(&model, text_model_fixture("Gemma 3 4B IT", "gemma", 2560)).unwrap();
        std::fs::write(&projector, projector_fixture("Gemma 3 4B IT", 2560)).unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].mmproj_path.as_deref(), projector.to_str());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_pairs_models_with_their_projectors_instead_of_nearest_quantization() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-model-pairing-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();

        let ornith = models.join("Ornith-1.5-Q4_K_M.gguf");
        let qwen = models.join("Qwen3.8-Flash-Next-Q4_K_M.gguf");
        let ornith_projector = models.join("mmproj-Ornith-1.5-F16.gguf");
        let qwen_projector = models.join("mmproj-Qwen3.8-Flash-Next-Q4_K_M.gguf");
        std::fs::write(&ornith, text_model_fixture("Ornith 1.5", "llama", 3072)).unwrap();
        std::fs::write(
            &qwen,
            text_model_fixture("Qwen3.8 Flash Next", "qwen3", 2048),
        )
        .unwrap();
        std::fs::write(&ornith_projector, projector_fixture("Ornith 1.5", 3072)).unwrap();
        std::fs::write(
            &qwen_projector,
            projector_fixture("Qwen3.8 Flash Next", 2048),
        )
        .unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        let model = |path: &Path| {
            catalog
                .models
                .iter()
                .find(|model| Path::new(&model.path) == path)
                .unwrap()
        };
        assert_eq!(
            catalog.models.len(),
            2,
            "projetores não são modelos listáveis"
        );
        assert_eq!(
            model(&ornith).mmproj_path.as_deref(),
            Some(ornith_projector.to_string_lossy().as_ref())
        );
        assert_eq!(
            model(&qwen).mmproj_path.as_deref(),
            Some(qwen_projector.to_string_lossy().as_ref())
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_supports_generic_projector_name_when_dimension_identifies_one_model() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-generic-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("base.gguf");
        let projector = models.join("vision.gguf");
        std::fs::write(&model, text_model_fixture("Base Model", "llama", 4096)).unwrap();
        std::fs::write(&projector, generic_projector_fixture(4096)).unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        let discovered = catalog
            .models
            .iter()
            .find(|item| item.path == model.to_string_lossy())
            .unwrap();
        assert_eq!(discovered.mmproj_path.as_deref(), projector.to_str());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_does_not_use_a_filename_without_header_evidence() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-filename-only-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("Model-X-Q4_K_M.gguf");
        let projector = models.join("mmproj-Model-X-F16.gguf");
        std::fs::write(&model, text_model_fixture("Model X", "llama", 3072)).unwrap();
        std::fs::write(
            &projector,
            gguf_header_fixture(&[], &[("general.architecture", "clip")]),
        )
        .unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        let discovered = catalog
            .models
            .iter()
            .find(|item| item.path == model.to_string_lossy())
            .unwrap();
        assert!(discovered.mmproj_path.is_none());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_refuses_ambiguous_generic_projectors_and_models() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-ambiguous-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("base-a.gguf");
        std::fs::write(&model, text_model_fixture("Base A", "llama", 4096)).unwrap();
        std::fs::write(
            models.join("base-b.gguf"),
            text_model_fixture("Base B", "llama", 4096),
        )
        .unwrap();
        std::fs::write(
            models.join("vision-a.gguf"),
            generic_projector_fixture(4096),
        )
        .unwrap();
        std::fs::write(
            models.join("vision-b.gguf"),
            generic_projector_fixture(4096),
        )
        .unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        let discovered = catalog
            .models
            .iter()
            .find(|item| item.path == model.to_string_lossy())
            .unwrap();
        assert!(discovered.mmproj_path.is_none());
        assert!(discovered
            .mmproj_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("provar") || reason.contains("ambíguo")));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_rejects_identity_matched_projector_with_incompatible_dimension() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-dimension-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("model-Q4_K_M.gguf");
        std::fs::write(&model, text_model_fixture("Shared Identity", "llama", 3072)).unwrap();
        std::fs::write(
            models.join("mmproj-model-F16.gguf"),
            projector_fixture("Shared Identity", 2048),
        )
        .unwrap();

        let catalog = discover(std::slice::from_ref(&models), &shards);
        let discovered = catalog
            .models
            .iter()
            .find(|item| item.path == model.to_string_lossy())
            .unwrap();
        assert!(discovered.mmproj_path.is_none());
        assert!(discovered
            .mmproj_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("dimensão incompatível")));
        assert!(validate_mmproj_pair(&model, &models.join("mmproj-model-F16.gguf")).is_err());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn find_mtp_draft_sibling_matches_the_unsloth_naming_convention() {
        let root = std::env::temp_dir().join(format!(
            "pom-mtp-draft-sibling-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let model = root.join("Qwen3.8-Flash-Next-Q8_0.gguf");
        let draft = root.join("mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf");
        std::fs::write(&model, vec![0u8; 4096]).unwrap();
        std::fs::write(&draft, vec![0u8; 4096]).unwrap();

        assert_eq!(find_mtp_draft_sibling(&model).as_deref(), draft.to_str());

        // A file that merely contains "mtp" without the leading prefix must
        // not match: the convention is a prefix, not a substring, precisely
        // to avoid false positives on an unrelated model.
        std::fs::remove_file(&draft).unwrap();
        let decoy = root.join("qwen-mtp-experiment-Q8_0.gguf");
        std::fs::write(&decoy, vec![0u8; 4096]).unwrap();
        assert_eq!(find_mtp_draft_sibling(&model), None);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_excludes_the_mtp_draft_sidecar_from_listing() {
        let root = std::env::temp_dir().join(format!(
            "pom-mtp-draft-catalog-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        let model = models.join("Qwen3.8-Flash-Next-Q8_0.gguf");
        let draft = models.join("mtp-Qwen3.8-Flash-Next-shared-Q8_0.gguf");
        std::fs::write(&model, crate::splitter::test_gguf_bytes(8)).unwrap();
        std::fs::write(&draft, vec![0u8; 4096]).unwrap();

        let catalog = discover_fast(std::slice::from_ref(&models), &shards);
        assert_eq!(
            catalog.models.len(),
            1,
            "o sidecar de rascunho não é um modelo"
        );
        assert_eq!(catalog.models[0].path, model.to_string_lossy());

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn catalog_does_not_pair_a_projector_from_another_directory() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-scope-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let other = root.join("other");
        let shards = root.join("shards");
        for directory in [&models, &other, &shards] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            models.join("model-Q4_K_M.gguf"),
            crate::splitter::test_gguf_bytes(4),
        )
        .unwrap();
        std::fs::write(other.join("mmproj-model-f16.gguf"), vec![0u8; 4096]).unwrap();

        let catalog = discover_fast(std::slice::from_ref(&models), &shards);
        assert_eq!(catalog.models[0].mmproj_path, None);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn shard_set_completeness() {
        let mk = |start: u32, end: u32, total: u32| DiscoveredModel {
            id: format!("s{start}"),
            name: format!("s_{start}_{end}.gguf"),
            source: ModelSource::Local,
            source_label: "Local",
            path: format!("/tmp/s_{start}_{end}.gguf"),
            mmproj_path: None,
            mmproj_reason: None,
            identity_names: Vec::new(),
            size_bytes: 4096,
            architecture: None,
            is_moe: false,
            total_layers: Some(total),
            quant: None,
            shard_range: Some((start, end)),
            shard_group: Some("s".into()),
            sha256: None,
            model_label: None,
            pom_id: None,
            tensor_parallel: false,
            tp_rank: None,
            tp_size: None,
            capabilities: None,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
        };
        let complete = build_shard_sets(&[mk(0, 15, 32), mk(16, 31, 32)]);
        assert!(complete[0].complete);
        assert!(!complete[0].total_layers_inferred);

        let with_gap = build_shard_sets(&[mk(0, 15, 32), mk(20, 31, 32)]);
        assert!(!with_gap[0].complete);
    }

    #[test]
    fn shard_set_does_not_inherit_projector_without_verified_manifest_identity() {
        let source = DiscoveredModel {
            id: "source".into(),
            name: "vision-model.gguf".into(),
            source: ModelSource::Local,
            source_label: "Local".into(),
            path: "/models/vision-model.gguf".into(),
            mmproj_path: Some("/models/mmproj-vision-model.gguf".into()),
            mmproj_reason: None,
            identity_names: vec!["vision-model".into()],
            size_bytes: 4096,
            architecture: None,
            is_moe: false,
            total_layers: Some(32),
            quant: None,
            shard_range: None,
            shard_group: None,
            sha256: None,
            model_label: None,
            pom_id: None,
            tensor_parallel: false,
            tp_rank: None,
            tp_size: None,
            capabilities: None,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
        };
        let mut slice = source.clone();
        slice.id = "slice".into();
        slice.name = "vision-0-15.gguf".into();
        slice.path = "/shards/vision-0-15.gguf".into();
        slice.mmproj_path = None;
        slice.shard_range = Some((0, 15));
        slice.shard_group = Some("vision-slices".into());
        slice.model_label = Some("vision-model".into());
        let mut second = slice.clone();
        second.id = "slice-2".into();
        second.name = "vision-16-31.gguf".into();
        second.path = "/shards/vision-16-31.gguf".into();
        second.shard_range = Some((16, 31));

        let sets = build_shard_sets(&[source, slice, second]);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].mmproj_path, None);
        assert!(sets[0]
            .mmproj_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("pom_id")));
    }

    #[test]
    fn shard_set_finds_projector_for_a_quantized_original_model() {
        let root = std::env::temp_dir().join(format!(
            "pom-mmproj-shard-pom-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let source_path = root.join("vision-model-Q4_K_M.gguf");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&source_path, b"the source GGUF bytes").unwrap();
        let (pom_id, _) = crate::pom::hash_file(&source_path).unwrap();
        let source = DiscoveredModel {
            id: "source".into(),
            name: "vision-model-Q4_K_M.gguf".into(),
            source: ModelSource::Local,
            source_label: "Local",
            path: source_path.to_string_lossy().into_owned(),
            mmproj_path: Some("/models/mmproj-vision-model-f16.gguf".into()),
            mmproj_reason: None,
            identity_names: vec!["vision-model".into()],
            size_bytes: 4096,
            architecture: None,
            is_moe: false,
            total_layers: Some(32),
            quant: Some("Q4_K_M".into()),
            shard_range: None,
            shard_group: None,
            sha256: None,
            model_label: None,
            pom_id: None,
            tensor_parallel: false,
            tp_rank: None,
            tp_size: None,
            capabilities: None,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
        };
        let mut slice = source.clone();
        slice.id = "slice".into();
        slice.name = "vision-part-0.gguf".into();
        slice.path = "/shards/vision-part-0.gguf".into();
        slice.mmproj_path = None;
        slice.shard_range = Some((0, 15));
        slice.shard_group = Some(format!("vision-model-{}", crate::pom::short_id(&pom_id)));
        slice.model_label = Some("vision-model".into());
        slice.pom_id = Some(pom_id);
        let mut second = slice.clone();
        second.id = "slice-2".into();
        second.name = "vision-part-1.gguf".into();
        second.path = "/shards/vision-part-1.gguf".into();
        second.shard_range = Some((16, 31));

        let sets = build_shard_sets(&[source, slice, second]);
        assert_eq!(
            sets[0].mmproj_path.as_deref(),
            Some("/models/mmproj-vision-model-f16.gguf")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn shard_set_separates_pipeline_parts_from_tensor_parallel_parts() {
        let mk = |id: &str, start: u32, end: u32, tensor_parallel: bool| {
            let mut model = DiscoveredModel {
                id: id.into(),
                name: format!("{id}.gguf"),
                source: ModelSource::Local,
                source_label: "Local".into(),
                path: format!("/tmp/{id}.gguf"),
                mmproj_reason: None,
                identity_names: Vec::new(),
                size_bytes: 4096,
                architecture: None,
                is_moe: false,
                total_layers: Some(32),
                quant: None,
                shard_range: Some((start, end)),
                shard_group: Some("model-aaaa".into()),
                sha256: Some(id.repeat(64 / id.len())),
                model_label: Some("model".into()),
                pom_id: Some("a".repeat(64)),
                tensor_parallel,
                tp_rank: tensor_parallel.then_some(if id == "c" { 0 } else { 1 }),
                tp_size: tensor_parallel.then_some(2),
                capabilities: None,
                multipart: false,
                multipart_parts: Vec::new(),
                multipart_error: None,
                metadata_error: None,
                mmproj_path: None,
                metadata_identity: None,
            };
            if tensor_parallel {
                model.name = format!("{id}-tp.gguf");
            }
            model
        };

        let sets = build_shard_sets(&[
            mk("a", 0, 15, false),
            mk("b", 16, 31, false),
            mk("c", 0, 31, true),
            mk("d", 0, 31, true),
        ]);

        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].shards.len(), 2);
        assert_eq!(sets[0].tp_parts.len(), 2);
        assert!(sets[0].complete);
    }

    #[test]
    fn a_directory_is_read_according_to_what_it_is() {
        let base = std::env::temp_dir().join(format!("pom-cat-kind-{}", std::process::id()));
        let ollama = base.join("store/models");
        std::fs::create_dir_all(ollama.join("manifests")).unwrap();
        std::fs::create_dir_all(ollama.join("blobs")).unwrap();
        std::fs::write(ollama.join("manifests/x"), b"{}").unwrap();
        std::fs::write(ollama.join("blobs/sha256-x"), b"x").unwrap();

        let plain = base.join("sdcard/models");
        std::fs::create_dir_all(&plain).unwrap();

        // LM Studio também tem `blobs/` e `manifests/`, vazios — o layout do
        // Ollama sozinho leria a biblioteca dele como se não tivesse nada.
        let lmstudio = base.join("home/.lmstudio/models");
        std::fs::create_dir_all(lmstudio.join("manifests")).unwrap();
        std::fs::create_dir_all(lmstudio.join("blobs")).unwrap();

        assert_eq!(detect_source(&ollama), ModelSource::Ollama);
        assert_eq!(detect_source(&lmstudio), ModelSource::LmStudio);
        assert_eq!(
            detect_source(&plain),
            ModelSource::Folder,
            "uma pasta qualquer de .gguf não é uma biblioteca de ninguém"
        );
        assert_eq!(
            detect_source(&base.join("vazio-mas-ollama/.ollama/models")),
            ModelSource::Ollama,
            "um store do Ollama ainda sem blobs continua sendo do Ollama"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// The configuration is what governs the scan: a directory nobody listed is
    /// invisible, and one that was listed is read even with nothing familiar
    /// about it.
    #[test]
    fn only_the_configured_directories_are_scanned() {
        let base = std::env::temp_dir().join(format!("pom-cat-dirs-{}", std::process::id()));
        let listed = base.join("listada");
        let unlisted = base.join("nao-listada");
        let shards = base.join("fatias");
        for d in [&listed, &unlisted, &shards] {
            std::fs::create_dir_all(d).unwrap();
        }
        // Big enough to pass the "this is not a real GGUF" floor in `describe`.
        let filler = vec![0u8; 4096];
        std::fs::write(listed.join("visivel.gguf"), &filler).unwrap();
        std::fs::write(unlisted.join("invisivel.gguf"), &filler).unwrap();
        // Um GGUF de verdade: a faixa da fatia agora sai do cabeçalho que o
        // fatiador carimba, não do nome do arquivo.
        std::fs::write(
            shards.join("m_0_3.gguf"),
            crate::splitter::test_gguf_bytes(4),
        )
        .unwrap();

        let catalog = discover(&[listed.clone()], &shards);
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"visivel.gguf"));
        assert!(names.contains(&"m_0_3.gguf"));
        assert!(!names.contains(&"invisivel.gguf"));

        // And the slice keeps the source that lets a peer pull it.
        let shard = catalog
            .models
            .iter()
            .find(|m| m.name == "m_0_3.gguf")
            .unwrap();
        assert_eq!(shard.source, ModelSource::Local);
        // Sem carimbo `pom.start_layer` no cabeçalho não há faixa: o nome do
        // arquivo deixou de ser prova de coisa nenhuma.
        assert_eq!(shard.shard_range, None);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn old_slices_without_metadata_infer_the_total_from_coverage() {
        // Slices predating `pom.original_block_count` report no total; the
        // set must not claim the shard's own block count is the model's.
        let mk = |start: u32, end: u32| DiscoveredModel {
            id: format!("s{start}"),
            name: format!("qwen_{start}_{end}.gguf"),
            source: ModelSource::Local,
            source_label: "Local",
            path: format!("/tmp/qwen_{start}_{end}.gguf"),
            mmproj_path: None,
            mmproj_reason: None,
            identity_names: Vec::new(),
            size_bytes: 4096,
            architecture: None,
            is_moe: false,
            total_layers: None,
            quant: None,
            shard_range: Some((start, end)),
            shard_group: Some("qwen".into()),
            sha256: None,
            model_label: None,
            pom_id: None,
            tensor_parallel: false,
            tp_rank: None,
            tp_size: None,
            capabilities: None,
            multipart: false,
            multipart_parts: Vec::new(),
            multipart_error: None,
            metadata_error: None,
            metadata_identity: None,
        };
        let sets = build_shard_sets(&[mk(0, 15), mk(16, 31)]);
        assert_eq!(sets[0].total_layers, Some(32));
        assert!(sets[0].total_layers_inferred);
        assert!(sets[0].complete);
    }

    #[test]
    fn fast_discovery_exposes_model_before_header_enrichment() {
        let root =
            std::env::temp_dir().join(format!("pom-cat-fast-{}", uuid::Uuid::new_v4().simple()));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        std::fs::write(
            models.join("model.gguf"),
            crate::splitter::test_gguf_bytes(8),
        )
        .unwrap();

        let fast = discover_fast(&[models], &shards);
        assert!(!fast.metadata_complete);
        assert_eq!(fast.models.len(), 1);
        assert_eq!(fast.models[0].name, "model.gguf");
        assert_eq!(fast.models[0].total_layers, None);

        let complete = enrich(fast);
        assert!(complete.metadata_complete);
        assert_eq!(complete.models[0].total_layers, Some(8));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn fast_discovery_does_not_hash_whole_models_to_build_shard_sets() {
        let root = std::env::temp_dir().join(format!(
            "pom-cat-fast-no-hash-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let models = root.join("models");
        let shards = root.join("shards");
        let run = shards.join("model-aaaaaaaa");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&run).unwrap();

        // The source model is deliberately not named by its content hash. A
        // manifest-backed slice with the same label used to make the fast
        // inventory hash this entire file before publishing any catalog data.
        std::fs::write(
            models.join("model-thing-Q4_K_M.gguf"),
            vec![0u8; 1024 * 1024],
        )
        .unwrap();
        let part_sha = "b".repeat(64);
        let part = crate::pom::part_path(&run, &part_sha).unwrap();
        std::fs::write(&part, vec![1u8; 4096]).unwrap();
        let manifest = crate::pom::PomManifest::new(
            crate::pom::PomModel {
                name: "model-thing".into(),
                label: "model thing".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            crate::pom::PomKind::Layers,
            "test",
            "a".repeat(64),
            vec![crate::pom::PomPart {
                sha256: part_sha,
                bytes: 4096,
                start_layer: 0,
                end_layer: 31,
                is_first: true,
                is_last: true,
                tp: None,
            }],
        );
        crate::pom::write_atomic(&run, &manifest).unwrap();

        reset_content_hash_calls();
        let fast = discover_fast(std::slice::from_ref(&models), &shards);

        assert_eq!(fast.models.len(), 2);
        assert_eq!(
            content_hash_calls(),
            0,
            "a varredura rápida não pode ler o payload inteiro do modelo de origem"
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn enrichment_marks_unreadable_model_terminal_without_blocking_catalog() {
        let root =
            std::env::temp_dir().join(format!("pom-cat-invalid-{}", uuid::Uuid::new_v4().simple()));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        std::fs::write(models.join("broken.gguf"), vec![0u8; 1024]).unwrap();
        std::fs::write(
            models.join("valid.gguf"),
            crate::splitter::test_gguf_bytes(1),
        )
        .unwrap();

        let enriched = enrich(discover_fast(&[models], &shards));
        assert!(enriched.metadata_complete);
        assert_eq!(enriched.models.len(), 2);
        let broken = enriched
            .models
            .iter()
            .find(|model| model.name == "broken.gguf")
            .unwrap();
        assert_eq!(broken.architecture, None);
        assert!(broken.metadata_error.is_some());
        let valid = enriched
            .models
            .iter()
            .find(|model| model.name == "valid.gguf")
            .unwrap();
        assert_eq!(valid.architecture.as_deref(), Some("llama"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn enrichment_does_not_require_validating_tensor_payload() {
        let root =
            std::env::temp_dir().join(format!("pom-cat-header-{}", uuid::Uuid::new_v4().simple()));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();

        // `read_info` only needs the GGUF KV header. The tensor table below is
        // intentionally unusable so the catalog test proves that enrichment
        // does not open the full tensor source just to publish architecture.
        let string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x4655_4747u32.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&string("general.architecture"));
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&string("llama"));
        bytes.extend_from_slice(&string("blk.0.weight"));
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&4u64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.resize(1024, 0);
        let path = models.join("header-only.gguf");
        std::fs::write(&path, bytes).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1 << 30)
            .unwrap();

        let enriched = enrich(discover_fast(&[models], &shards));
        assert!(enriched.metadata_complete);
        assert_eq!(enriched.models[0].architecture.as_deref(), Some("llama"));

        std::fs::remove_dir_all(root).ok();
    }
}

/// O catálogo se reconstrói a partir dos `.pom`, e o hash nunca vira nome.
#[cfg(test)]
mod part_naming_tests {
    use super::*;
    use crate::pom::{PomKind, PomManifest, PomModel, PomPart};

    /// Grava um GGUF de verdade nomeado pelo próprio hash e o manifesto ao lado.
    fn modelo_fatiado(raiz: &Path) -> (PomManifest, Vec<String>) {
        // Como a produção faz: corta numa pasta de trabalho e, com o manifesto
        // pronto, muda a rodada para `<modelo>-<id da rodada>`.
        let pasta = crate::pom::scratch_dir(raiz);
        std::fs::create_dir_all(&pasta).unwrap();

        let mut partes = Vec::new();
        let mut hashes = Vec::new();
        // Contagens de camadas diferentes de propósito: `test_gguf_bytes(n)` é
        // determinística, então duas fatias de 16 camadas dariam **o mesmo
        // hash** - e, com o nome sendo o conteúdo, um arquivo só. O que
        // importa aqui é a identidade vir do manifesto, e é ele que declara as
        // faixas.
        for (inicio, fim, camadas) in [(0u32, 15u32, 16u32), (16, 31, 12)] {
            let bytes = crate::splitter::test_gguf_bytes(camadas);
            let tmp = pasta.join(format!("tmp-{inicio}"));
            std::fs::write(&tmp, &bytes).unwrap();
            let (sha, n) = crate::pom::hash_file(&tmp).unwrap();
            std::fs::rename(&tmp, crate::pom::part_path(&pasta, &sha).unwrap()).unwrap();
            partes.push(PomPart {
                sha256: sha.clone(),
                bytes: n,
                start_layer: inicio,
                end_layer: fim,
                is_first: inicio == 0,
                is_last: fim == 31,
                tp: None,
            });
            hashes.push(sha);
        }

        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            "a".repeat(64),
            partes,
        );
        crate::pom::finalize_run(raiz, &pasta, &m).unwrap();
        (m, hashes)
    }

    /// O defeito relatado: a fatia aparecia com nome de hash, como se fosse um
    /// modelo inteiro. O nome que o operador lê tem que dizer **qual modelo** e
    /// **quais camadas**; o hash é identificação interna.
    #[test]
    fn uma_parte_se_chama_pelo_modelo_e_pelas_camadas_nunca_pelo_hash() {
        let raiz = std::env::temp_dir().join(format!("pom-cat-{}", uuid::Uuid::new_v4().simple()));
        let (_m, hashes) = modelo_fatiado(&raiz);

        let cat = discover(&[], &raiz);
        assert_eq!(cat.models.len(), 2, "as duas partes entram no catálogo");

        for m in &cat.models {
            assert!(
                !crate::pom::is_sha256_hex(m.name.trim_end_matches(".gguf")),
                "o nome não pode ser o hash: {}",
                m.name
            );
            assert!(
                m.name.contains("qwen3:8b"),
                "o rótulo do modelo: {}",
                m.name
            );
            // E o hash continua disponível, como informação lateral.
            assert!(m
                .sha256
                .as_deref()
                .is_some_and(|h| hashes.contains(&h.to_string())));
            assert!(m.is_shard(), "com manifesto, é fatia - não modelo inteiro");
            assert_eq!(m.total_layers, Some(32));
            assert_eq!(m.quant.as_deref(), Some("Q4_K_M"));
        }

        let primeira = cat
            .models
            .iter()
            .find(|m| m.shard_range == Some((0, 15)))
            .expect("a faixa vem do manifesto");
        assert!(primeira.name.contains("L0-15"), "{}", primeira.name);

        // E o conjunto se fecha, que é o que a tela de implantar precisa.
        assert_eq!(cat.shard_sets.len(), 1);
        let set = &cat.shard_sets[0];
        assert!(set.complete);
        // A chave do conjunto é a fonte original, para todas as alternativas
        // de pipeline e os shards TP do mesmo modelo compartilharem o grupo.
        assert_eq!(
            set.group,
            format!("qwen3-8b-{}", crate::pom::short_id(&_m.pom_id))
        );
        // ...mas quem a tela mostra é o rótulo do modelo.
        assert_eq!(set.model_label, "qwen3:8b");
        assert_eq!(set.pom_id, _m.pom_id);

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// O mesmo arquivo original, fatiado de duas formas, compartilha a pasta e
    /// o manifesto; o catálogo preserva todas as alternativas de `parts`.
    #[test]
    fn duas_fatiagens_do_mesmo_arquivo_ficam_na_mesma_pasta() {
        let raiz = std::env::temp_dir().join(format!("pom-col-{}", uuid::Uuid::new_v4().simple()));

        // Duas rodadas do mesmo modelo, com cortes diferentes.
        let uma = fatiar(
            &raiz,
            "qwen3-8b",
            "qwen3:8b",
            32,
            &[(0, 15, 16), (16, 31, 12)],
        );
        let outra = fatiar(
            &raiz,
            "qwen3-8b",
            "qwen3:8b",
            32,
            &[(0, 19, 20), (20, 31, 9)],
        );

        assert_eq!(uma.pom_id, outra.pom_id, "a fonte é a identidade do modelo");
        let pasta_uma = crate::pom::dir_for(&raiz, &uma);
        let pasta_outra = crate::pom::dir_for(&raiz, &outra);
        assert_eq!(pasta_uma, pasta_outra, "um manifesto único por fonte");
        assert!(pasta_uma.is_dir());

        // As quatro partes aparecem no mesmo manifesto, cada uma com sua faixa.
        let cat = discover(&[], &raiz);
        assert_eq!(cat.models.len(), 4);
        assert_eq!(cat.shard_sets.len(), 1, "um conjunto por arquivo original");
        assert!(cat.shard_sets[0].complete);
        assert_eq!(cat.shard_sets[0].shards.len(), 4);

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Corta um modelo em faixas e sela a rodada, como a produção faz.
    fn fatiar(
        raiz: &Path,
        nome: &str,
        rotulo: &str,
        total: u32,
        cortes: &[(u32, u32, u32)],
    ) -> crate::pom::PomManifest {
        let trabalho = crate::pom::scratch_dir(raiz);
        std::fs::create_dir_all(&trabalho).unwrap();

        let mut partes = Vec::new();
        for (inicio, fim, camadas) in cortes {
            let bytes = crate::splitter::test_gguf_bytes(*camadas);
            let tmp = trabalho.join(format!("tmp-{inicio}"));
            std::fs::write(&tmp, &bytes).unwrap();
            let (sha, n) = crate::pom::hash_file(&tmp).unwrap();
            std::fs::rename(&tmp, crate::pom::part_path(&trabalho, &sha).unwrap()).unwrap();
            partes.push(crate::pom::PomPart {
                sha256: sha,
                bytes: n,
                start_layer: *inicio,
                end_layer: *fim,
                is_first: *inicio == 0,
                is_last: *fim + 1 >= total,
                tp: None,
            });
        }

        let m = crate::pom::PomManifest::new(
            crate::pom::PomModel {
                name: nome.into(),
                label: rotulo.into(),
                total_layers: total,
                quant: None,
            },
            crate::pom::PomKind::Layers,
            "no-1",
            "a".repeat(64),
            partes,
        );
        crate::pom::finalize_run(raiz, &trabalho, &m).unwrap();
        m
    }

    /// Sem manifesto o arquivo não é fatia de nada: é um GGUF que alguém
    /// deixou na pasta, e continua aparecendo pelo nome dele.
    #[test]
    fn um_gguf_solto_na_pasta_de_fatias_nao_vira_parte() {
        let raiz = std::env::temp_dir().join(format!("pom-cat-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&raiz).unwrap();
        std::fs::write(
            raiz.join("modelo-inteiro.gguf"),
            crate::splitter::test_gguf_bytes(8),
        )
        .unwrap();

        let cat = discover(&[], &raiz);
        assert_eq!(cat.models.len(), 1);
        assert_eq!(cat.models[0].name, "modelo-inteiro.gguf");
        assert!(cat.models[0].sha256.is_none());
        assert!(cat.models[0].shard_group.is_none());

        std::fs::remove_dir_all(&raiz).ok();
    }

    /// Uma parte listada no manifesto mas ausente do disco não entra no índice:
    /// ele descreve o que existe, não o que deveria existir.
    #[test]
    fn o_indice_so_conhece_parte_que_esta_em_disco() {
        let raiz = std::env::temp_dir().join(format!("pom-cat-{}", uuid::Uuid::new_v4().simple()));
        let (m, hashes) = modelo_fatiado(&raiz);

        let pasta = crate::pom::dir_for(&raiz, &m);
        std::fs::remove_file(crate::pom::part_path(&pasta, &hashes[1]).unwrap()).unwrap();

        let index = part_index(&raiz);
        assert_eq!(index.len(), 1);
        assert!(index.values().all(|c| c.sha256 == hashes[0]));
        assert_eq!(m.parts.len(), 2, "o manifesto segue descrevendo as duas");

        std::fs::remove_dir_all(&raiz).ok();
    }
}
