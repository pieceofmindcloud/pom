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
use crate::multipart::GgufSource;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// Evidência de capacidades derivada do header e da tabela de tensores do
    /// GGUF. Ausente quando a leitura não pôde ser confirmada.
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

fn header_cache() -> &'static Mutex<HashMap<HeaderKey, Option<GgufInfo>>> {
    static CACHE: OnceLock<Mutex<HashMap<HeaderKey, Option<GgufInfo>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_info(path: &Path, size: u64, mtime: u64) -> Option<GgufInfo> {
    let key = (path.to_string_lossy().into_owned(), size, mtime);
    if let Some(hit) = header_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&key).cloned())
    {
        return hit;
    }
    let info = gguf::read_info(path).ok();
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
        size_bytes,
        multipart: false,
        multipart_parts: Vec::new(),
        multipart_error: None,
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

fn file_mtime_fingerprint(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

enum EnrichResult {
    /// The header and optional capability evidence were read successfully.
    Ready,
    /// The file could not be read yet. Keep it visible and retry the phase.
    Retry,
    /// The fast scan found an item that is not a runnable model.
    Drop,
}

/// Read the header for one item after the fast inventory is already visible.
fn enrich_model(model: &mut DiscoveredModel) -> EnrichResult {
    let path = Path::new(&model.path);
    let Some(meta) = std::fs::metadata(path).ok() else {
        return EnrichResult::Retry;
    };
    model.size_bytes = meta.len();
    if model.size_bytes < 1024 {
        return EnrichResult::Drop;
    }
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let Some(info) = cached_info(path, model.size_bytes, file_mtime_fingerprint(&meta)) else {
        return EnrichResult::Retry;
    };
    if info.architecture.as_deref() == Some("clip") || filename.starts_with("mmproj") {
        return EnrichResult::Drop;
    }

    let source = match GgufSource::open(path) {
        Ok(source) => source,
        Err(error) => {
            if let Ok(Some(split_count)) = crate::multipart::declared_split_count(path) {
                model.multipart = true;
                model.multipart_parts.clear();
                model.multipart_error = Some(format!(
                    "GGUF multipart declara split.count={split_count}, mas o conjunto não pôde ser lido: {error}. Coloque todas as partes no mesmo diretório e selecione a primeira parte."
                ));
                // The first physical file is not the model size. Zero makes
                // the UI refuse to present a false byte count until the set is
                // complete.
                model.size_bytes = 0;
                return EnrichResult::Ready;
            }
            return EnrichResult::Retry;
        }
    };
    model.multipart_error = None;
    model.multipart = source.is_multipart();
    model.multipart_parts = source
        .parts()
        .iter()
        .map(|part| MultipartPart {
            index: part.index,
            path: part.path.to_string_lossy().into_owned(),
            bytes: part.bytes,
            tensor_count: part.tensor_count,
        })
        .collect();
    if model.multipart {
        model.size_bytes = source.source_bytes();
    }

    model.architecture = info.architecture.clone();
    model.is_moe = info.is_moe();
    // The catalog's second phase must finish after one lightweight header and
    // tensor-name pass. Reading the whole tensor layout here made a slow volume hold
    // `metadata_complete` false for the entire catalog, even though the
    // selected shard ranges already came from their manifests. MTP evidence
    // uses only the lightweight tensor-name scan; the full layout pass remains
    // available through `gguf::read_capability_evidence` callers that need it.
    let tensor_mtp = gguf::has_nextn_tensors(path).ok();
    model.capabilities = Some(ModelCapabilityEvidence {
        thinking: info.thinking_capability(),
        mtp: match (info.nextn_predict_layers, tensor_mtp) {
            (_, Some(true)) => Some(true),
            (Some(layers), _) => Some(layers > 0),
            (None, Some(false)) => Some(false),
            (None, None) => None,
        },
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

            ShardSet {
                model_label: shards
                    .iter()
                    .chain(tp_parts.iter())
                    .find_map(|s| s.model_label.clone())
                    .unwrap_or_else(|| group.clone()),
                pom_id: shards
                    .iter()
                    .chain(tp_parts.iter())
                    .find_map(|s| s.pom_id.clone())
                    .unwrap_or_default(),
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

    let shard_sets = build_shard_sets(&models);
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
    let mut complete = true;
    catalog
        .models
        .retain_mut(|model| match enrich_model(model) {
            EnrichResult::Ready => true,
            EnrichResult::Retry => {
                complete = false;
                true
            }
            EnrichResult::Drop => false,
        });
    catalog.shard_sets = build_shard_sets(&catalog.models);
    catalog.metadata_complete = complete;
    catalog
}

/// Full discovery for non-UI callers that still need a complete answer before
/// continuing, such as transfer path resolution and existing tests.
pub fn discover(model_dirs: &[PathBuf], shard_dir: &Path) -> Catalog {
    enrich(discover_fast(model_dirs, shard_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn shard_set_completeness() {
        let mk = |start: u32, end: u32, total: u32| DiscoveredModel {
            id: format!("s{start}"),
            name: format!("s_{start}_{end}.gguf"),
            source: ModelSource::Local,
            source_label: "Local",
            path: format!("/tmp/s_{start}_{end}.gguf"),
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
        };
        let complete = build_shard_sets(&[mk(0, 15, 32), mk(16, 31, 32)]);
        assert!(complete[0].complete);
        assert!(!complete[0].total_layers_inferred);

        let with_gap = build_shard_sets(&[mk(0, 15, 32), mk(20, 31, 32)]);
        assert!(!with_gap[0].complete);
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
    fn enrichment_keeps_unreadable_model_pending() {
        let root =
            std::env::temp_dir().join(format!("pom-cat-invalid-{}", uuid::Uuid::new_v4().simple()));
        let models = root.join("models");
        let shards = root.join("shards");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&shards).unwrap();
        std::fs::write(models.join("broken.gguf"), vec![0u8; 1024]).unwrap();

        let enriched = enrich(discover_fast(&[models], &shards));
        assert!(!enriched.metadata_complete);
        assert_eq!(enriched.models.len(), 1);
        assert_eq!(enriched.models[0].architecture, None);

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
