//! Image model files (diffusion models, text encoders, VAEs, ...) in the
//! configured model folders.
//!
//! A safetensors file starts with an 8-byte little-endian length and a JSON
//! table of tensor names, dtypes and offsets. Only that header is read, never
//! the weights, and the category comes from the tensor names, not from the
//! folder or the file name. Pickle formats (`.ckpt`, `.pt`, `.pth`, `.bin`)
//! are never opened: they are listed by `stat` as `other`.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::catalog::ModelSource;

/// A header above this is refused: real ones are a few MB at most.
pub const MAX_SAFETENSORS_HEADER: u64 = 100 * 1024 * 1024;
/// Same depth the GGUF walk uses.
const MAX_WALK_DEPTH: usize = 6;
const HEADER_EXTENSIONS: [&str; 2] = ["safetensors", "sft"];
const PICKLE_EXTENSIONS: [&str; 4] = ["ckpt", "pt", "pth", "bin"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageCategory {
    Diffusion,
    TextEncoder,
    Vae,
    /// One file with a diffusion model, a VAE and a text encoder.
    Checkpoint,
    Lora,
    Controlnet,
    ClipVision,
    Upscale,
    Other,
}

/// One image model file found on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageModelFile {
    pub id: String,
    /// Path relative to its root, `/`-separated (`<publisher>/<repo>/<file>`).
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub source: ModelSource,
    pub source_label: String,
    /// The extension: `safetensors`, `sft`, `ckpt`, `pt`, `pth` or `bin`.
    pub format: String,
    pub category: ImageCategory,
    /// Best-effort model family (`flux`, `qwen_image`, `z_image`, `sdxl`,
    /// `sd15`), for display only.
    pub family: Option<String>,
    /// The dtype most tensors use, lowercase (`bf16`, `f8_e4m3`, ...).
    pub dtype: Option<String>,
    pub metadata_error: Option<String>,
}

/// The parts of a safetensors header the classification reads.
#[derive(Debug, Clone, Default)]
pub struct SafetensorsHeader {
    /// (tensor name, dtype)
    pub tensors: Vec<(String, String)>,
    pub metadata: BTreeMap<String, String>,
}

pub fn read_safetensors_header(path: &Path) -> Result<SafetensorsHeader, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let mut len = [0u8; 8];
    file.read_exact(&mut len)
        .map_err(|_| "too short to be a safetensors file".to_owned())?;
    let len = u64::from_le_bytes(len);
    if len > MAX_SAFETENSORS_HEADER {
        return Err(format!(
            "header of {len} bytes is larger than {MAX_SAFETENSORS_HEADER}"
        ));
    }
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if 8 + len > size {
        return Err("header is cut short (an incomplete file?)".to_owned());
    }
    let mut body = vec![0u8; len as usize];
    file.read_exact(&mut body)
        .map_err(|_| "header is cut short (an incomplete file?)".to_owned())?;
    let table: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&body).map_err(|e| format!("header is not a JSON object: {e}"))?;
    let mut header = SafetensorsHeader::default();
    for (name, value) in table {
        if name == "__metadata__" {
            if let Some(map) = value.as_object() {
                for (key, value) in map {
                    if let Some(text) = value.as_str() {
                        header.metadata.insert(key.clone(), text.to_owned());
                    }
                }
            }
            continue;
        }
        let dtype = value["dtype"].as_str().unwrap_or_default().to_owned();
        header.tensors.push((name, dtype));
    }
    Ok(header)
}

/// Category, family and dominant dtype of a header. The rules run in a fixed
/// order: a checkpoint holds a VAE and a denoiser, a LoRA or ControlNet carries
/// denoiser names, so they are recognized before the plain components.
pub fn classify(header: &SafetensorsHeader) -> (ImageCategory, Option<String>, Option<String>) {
    let names: Vec<&str> = header.tensors.iter().map(|(n, _)| n.as_str()).collect();
    let any = |test: &dyn Fn(&str) -> bool| names.iter().any(|n| test(n));
    let starts = |prefix: &str| any(&|n: &str| n.starts_with(prefix));
    let contains = |part: &str| any(&|n: &str| n.contains(part));

    let denoiser_root = |n: &str| {
        let n = n
            .strip_prefix("model.diffusion_model.")
            .or_else(|| n.strip_prefix("diffusion_model."))
            .unwrap_or(n);
        [
            "double_blocks.",
            "single_blocks.",
            "joint_blocks.",
            "transformer_blocks.",
            "input_blocks.",
            "output_blocks.",
            "time_embed.",
            "t_embedder.",
            "x_embedder.",
            "img_in.",
            "noise_refiner.",
        ]
        .iter()
        .any(|p| n.starts_with(p))
    };

    let category = if starts("model.diffusion_model.") && starts("first_stage_model.") {
        ImageCategory::Checkpoint
    } else if contains("lora_up")
        || contains("lora_down")
        || contains(".lora_A.")
        || contains(".lora_B.")
        || contains(".lora.up.")
        || contains(".lora.down.")
    {
        ImageCategory::Lora
    } else if starts("control_model.")
        || contains("input_hint_block")
        || starts("controlnet_")
        || contains("controlnet_cond_embedding")
    {
        ImageCategory::Controlnet
    } else if starts("decoder.conv_in.") || (starts("decoder.up") && starts("encoder.down")) {
        ImageCategory::Vae
    } else if any(&denoiser_root) {
        ImageCategory::Diffusion
    } else if starts("vision_model.") && !starts("text_model.") {
        ImageCategory::ClipVision
    } else if starts("model.embed_tokens.")
        || contains(".self_attn.")
        || starts("encoder.block.")
        || starts("text_model.encoder.")
        || starts("transformer.resblocks.")
        || starts("shared.weight")
    {
        ImageCategory::TextEncoder
    } else if starts("conv_first.") || contains(".rdb1.") || starts("conv_up1.") {
        ImageCategory::Upscale
    } else {
        ImageCategory::Other
    };

    // The names give a canonical family code; `modelspec.architecture` is
    // free text, used only when the names say nothing.
    let family = family_from_names(&starts).or_else(|| {
        header
            .metadata
            .get("modelspec.architecture")
            .map(|arch| arch.to_lowercase())
    });
    (category, family, dominant_dtype(header))
}

fn family_from_names(starts: &dyn Fn(&str) -> bool) -> Option<String> {
    let family = if starts("cap_embedder.") && starts("noise_refiner.") {
        "z_image"
    } else if starts("txt_norm.") && starts("img_in.") && starts("transformer_blocks.") {
        "qwen_image"
    } else if starts("double_blocks.") || starts("model.diffusion_model.double_blocks.") {
        "flux"
    } else if starts("conditioner.embedders.1.") {
        "sdxl"
    } else if starts("cond_stage_model.transformer.") {
        "sd15"
    } else {
        return None;
    };
    Some(family.to_owned())
}

fn dominant_dtype(header: &SafetensorsHeader) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, dtype) in &header.tensors {
        if !dtype.is_empty() {
            *counts.entry(dtype.as_str()).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(dtype, _)| dtype.to_lowercase())
}

type HeaderKey = (String, u64, u64);
type Classified = Result<(ImageCategory, Option<String>, Option<String>), String>;

fn header_cache() -> &'static Mutex<HashMap<HeaderKey, Classified>> {
    static CACHE: OnceLock<Mutex<HashMap<HeaderKey, Classified>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_classification(path: &Path, size: u64, mtime: u64) -> Classified {
    let key = (path.to_string_lossy().into_owned(), size, mtime);
    if let Some(hit) = header_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&key).cloned())
    {
        return hit;
    }
    let result = read_safetensors_header(path).map(|header| classify(&header));
    if let Ok(mut cache) = header_cache().lock() {
        cache.insert(key, result.clone());
    }
    result
}

fn mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn image_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    (HEADER_EXTENSIONS.contains(&ext.as_str()) || PICKLE_EXTENSIONS.contains(&ext.as_str()))
        .then_some(ext)
}

fn find_image_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        debug!(path = %root.display(), "ignoring an unreadable directory during image discovery");
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries, the slicer's working folders and anything partial.
        if name.starts_with('.') || name.starts_with(crate::pom::SCRATCH_PREFIX) {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            find_image_files(&path, depth + 1, out);
        } else if image_extension(&path).is_some() {
            out.push(path);
        }
    }
}

/// Every image model file under these roots. A folder reached twice (listed
/// twice, or through a symbolic link) is read once.
pub fn discover_image_files(roots: &[(PathBuf, ModelSource)]) -> Vec<ImageModelFile> {
    let mut seen_roots = Vec::new();
    let mut seen_files = std::collections::HashSet::new();
    let mut files = Vec::new();
    for (root, source) in roots {
        let resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen_roots.contains(&resolved) || !root.is_dir() {
            continue;
        }
        seen_roots.push(resolved);
        let mut paths = Vec::new();
        find_image_files(root, 0, &mut paths);
        for path in paths {
            let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen_files.insert(resolved) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let format = image_extension(&path).unwrap_or_default();
            let (category, family, dtype, metadata_error) =
                if HEADER_EXTENSIONS.contains(&format.as_str()) {
                    match cached_classification(&path, meta.len(), mtime(&meta)) {
                        Ok((category, family, dtype)) => (category, family, dtype, None),
                        Err(error) => (ImageCategory::Other, None, None, Some(error)),
                    }
                } else {
                    (ImageCategory::Other, None, None, None)
                };
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push(ImageModelFile {
                id: format!("{:?}:{}", source, path.display()).to_lowercase(),
                name,
                path: path.to_string_lossy().into_owned(),
                size_bytes: meta.len(),
                source: *source,
                source_label: source.label().to_owned(),
                format,
                category,
                family,
                dtype,
                metadata_error,
            });
        }
    }
    files.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.path.cmp(&b.path))
    });
    files
}

#[cfg(test)]
/// A safetensors file with this header and a sparse data section as long
/// as the offsets say, so no weights are written.
pub(crate) fn write_test_safetensors(dir: &Path, file: &str, header: &str) -> PathBuf {
    use std::io::Write;
    let data_len = serde_json::from_str::<serde_json::Value>(header)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|map| {
            map.iter()
                .filter(|(k, _)| k.as_str() != "__metadata__")
                .filter_map(|(_, v)| v["data_offsets"][1].as_u64())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = std::fs::File::create(&path).unwrap();
    out.write_all(&(header.len() as u64).to_le_bytes()).unwrap();
    out.write_all(header.as_bytes()).unwrap();
    out.set_len(8 + header.len() as u64 + data_len).unwrap();
    path
}

/// Test support: the header of a real file in `fixtures/safetensors/`.
#[cfg(test)]
pub(crate) fn test_fixture(name: &str) -> String {
    let path = format!(
        "{}/src/models/fixtures/safetensors/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        test_fixture(name)
    }

    fn folder(dir: &Path) -> Vec<(PathBuf, ModelSource)> {
        vec![(dir.to_path_buf(), ModelSource::Folder)]
    }

    #[test]
    fn real_headers_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        for (name, expected) in [
            ("z_image_diffusion", ImageCategory::Diffusion),
            ("qwen_image_diffusion", ImageCategory::Diffusion),
            ("flux_diffusion", ImageCategory::Diffusion),
            ("qwen3_4b_text_encoder", ImageCategory::TextEncoder),
            ("t5xxl_text_encoder", ImageCategory::TextEncoder),
            ("clip_l_text_encoder", ImageCategory::TextEncoder),
            ("flux_ae_vae", ImageCategory::Vae),
            ("sdxl_vae", ImageCategory::Vae),
            ("sdxl_checkpoint", ImageCategory::Checkpoint),
            ("sd15_checkpoint", ImageCategory::Checkpoint),
            ("lora", ImageCategory::Lora),
            ("lora_peft", ImageCategory::Lora),
            ("controlnet", ImageCategory::Controlnet),
            ("clip_vision", ImageCategory::ClipVision),
            ("esrgan_upscale", ImageCategory::Upscale),
        ] {
            let path =
                write_test_safetensors(dir.path(), &format!("{name}.safetensors"), &fixture(name));
            let header = read_safetensors_header(&path).unwrap();
            let (category, _, _) = classify(&header);
            assert_eq!(category, expected, "{name}");
        }
    }

    #[test]
    fn families_and_dtypes_come_from_the_header() {
        let dir = tempfile::tempdir().unwrap();
        for (name, family, dtype) in [
            ("z_image_diffusion", Some("z_image"), "bf16"),
            ("qwen_image_diffusion", Some("qwen_image"), "f8_e4m3"),
            ("flux_diffusion", Some("flux"), "f8_e4m3"),
            ("sdxl_checkpoint", Some("sdxl"), "f16"),
            ("sd15_checkpoint", Some("sd15"), "f16"),
            ("flux_ae_vae", Some("flux.1-ae"), "f32"),
        ] {
            let path =
                write_test_safetensors(dir.path(), &format!("{name}.safetensors"), &fixture(name));
            let (_, got_family, got_dtype) = classify(&read_safetensors_header(&path).unwrap());
            assert_eq!(got_family.as_deref(), family, "{name}");
            assert_eq!(got_dtype.as_deref(), Some(dtype), "{name}");
        }
    }

    #[test]
    fn an_unknown_tensor_table_is_other() {
        let dir = tempfile::tempdir().unwrap();
        let header = r#"{"foo.weight":{"dtype":"F32","shape":[1],"data_offsets":[0,4]}}"#;
        let path = write_test_safetensors(dir.path(), "foo.safetensors", header);
        let (category, family, _) = classify(&read_safetensors_header(&path).unwrap());
        assert_eq!(category, ImageCategory::Other);
        assert_eq!(family, None);
    }

    #[test]
    fn truncated_and_oversized_headers_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.safetensors");
        std::fs::write(&short, [1u8, 2, 3, 4, 5]).unwrap();
        let huge = dir.path().join("huge.safetensors");
        std::fs::write(&huge, u64::MAX.to_le_bytes()).unwrap();
        let over = dir.path().join("over.safetensors");
        std::fs::write(&over, (MAX_SAFETENSORS_HEADER + 1).to_le_bytes()).unwrap();
        let cut = dir.path().join("cut.safetensors");
        let mut bytes = 100u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{\"a\":");
        std::fs::write(&cut, bytes).unwrap();
        let array = write_test_safetensors(dir.path(), "array.safetensors", "[1,2,3]");
        for path in [&short, &huge, &over, &cut, &array] {
            assert!(read_safetensors_header(path).is_err(), "{}", path.display());
        }
        let files = discover_image_files(&folder(dir.path()));
        assert_eq!(files.len(), 5);
        assert!(files.iter().all(|file| file.metadata_error.is_some()));
        assert!(files
            .iter()
            .all(|file| file.category == ImageCategory::Other));
    }

    #[test]
    fn partial_downloads_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let vae = fixture("flux_ae_vae");
        write_test_safetensors(dir.path(), "pub/repo/ae.safetensors", &vae);
        write_test_safetensors(dir.path(), "pub/repo/ae2.safetensors.part", &vae);
        write_test_safetensors(dir.path(), "pub/repo/.hidden.safetensors", &vae);
        let files = discover_image_files(&folder(dir.path()));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "pub/repo/ae.safetensors");
        assert_eq!(files[0].category, ImageCategory::Vae);
        assert_eq!(files[0].format, "safetensors");
        assert_eq!(files[0].source, ModelSource::Folder);
    }

    #[test]
    fn pickle_formats_are_listed_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ckpt"), b"\x80\x02not really a pickle").unwrap();
        let files = discover_image_files(&folder(dir.path()));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].format, "ckpt");
        assert_eq!(files[0].category, ImageCategory::Other);
        assert_eq!(files[0].metadata_error, None);
        assert_eq!(files[0].size_bytes, 21);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_reached_twice_lists_each_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        write_test_safetensors(&models, "pub/repo/ae.safetensors", &fixture("flux_ae_vae"));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&models, &link).unwrap();
        let roots = vec![(models, ModelSource::Folder), (link, ModelSource::Folder)];
        assert_eq!(discover_image_files(&roots).len(), 1);
    }
}
