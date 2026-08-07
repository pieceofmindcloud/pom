//! Minimal GGUF metadata reader.
//!
//! Reads only the key/value header of a `.gguf` file — enough to learn the
//! architecture and the real block (layer) count without loading any weights.
//! Mirrors the reader in `split.py`, so both agree on what a model looks like.
//!
//! Only the KV section is parsed; the tensor table and tensor data are skipped.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const GGUF_MAGIC: u32 = 0x4655_4747; // "GGUF" little-endian

// GGUF value type codes.
const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

/// Fixed byte width of every scalar GGUF value type.
fn scalar_size(t: u32) -> Option<u64> {
    match t {
        T_UINT8 | T_INT8 | T_BOOL => Some(1),
        T_UINT16 | T_INT16 => Some(2),
        T_UINT32 | T_INT32 | T_FLOAT32 => Some(4),
        T_UINT64 | T_INT64 | T_FLOAT64 => Some(8),
        _ => None,
    }
}

/// What we care about from a GGUF header.
#[derive(Debug, Clone, Default)]
pub struct GgufInfo {
    /// `general.architecture`, e.g. "qwen3" / "llama".
    pub architecture: Option<String>,
    /// `general.name` as written by the producer.
    pub name: Option<String>,
    /// `<arch>.block_count` — layers present *in this file*.
    pub block_count: Option<u32>,
    /// `rdmap2p.original_block_count` — layers in the model this file was sliced from.
    /// Only present on shards written by `split_model.py`.
    pub original_block_count: Option<u32>,
    /// `rdmap2p.start_layer` / `rdmap2p.end_layer` on shards.
    pub shard_range: Option<(u32, u32)>,
    /// `<arch>.embedding_length`.
    pub embedding_length: Option<u32>,
    /// `general.size_label`, e.g. "35B".
    pub size_label: Option<String>,
}

impl GgufInfo {
    /// Layers of the *whole* model: the shard metadata when present, else the
    /// block count of the file itself.
    pub fn total_layers(&self) -> Option<u32> {
        self.original_block_count.or(self.block_count)
    }

    /// True when this file is a slice produced by `split_model.py`.
    pub fn is_shard(&self) -> bool {
        self.original_block_count.is_some() || self.shard_range.is_some()
    }
}

struct Reader<R: Read + Seek> {
    inner: R,
}

impl<R: Read + Seek> Reader<R> {
    fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u64()?;
        // Guard against a corrupt length turning into a huge allocation.
        if len > 64 * 1024 * 1024 {
            bail!("implausible GGUF string length {len}");
        }
        let mut buf = vec![0u8; len as usize];
        self.inner.read_exact(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        self.inner.seek(SeekFrom::Current(n as i64))?;
        Ok(())
    }

    /// Skip a value of the given type without materialising it.
    fn skip_value(&mut self, vtype: u32) -> Result<()> {
        if let Some(sz) = scalar_size(vtype) {
            return self.skip(sz);
        }
        match vtype {
            T_STRING => {
                let len = self.u64()?;
                self.skip(len)
            }
            T_ARRAY => {
                let elem = self.u32()?;
                let count = self.u64()?;
                if let Some(sz) = scalar_size(elem) {
                    self.skip(sz.saturating_mul(count))
                } else {
                    for _ in 0..count {
                        self.skip_value(elem)?;
                    }
                    Ok(())
                }
            }
            other => bail!("unknown GGUF value type {other}"),
        }
    }

    /// Read a scalar as u32 when it is an integer type; otherwise skip it.
    fn value_as_u32(&mut self, vtype: u32) -> Result<Option<u32>> {
        let v = match vtype {
            T_UINT8 | T_INT8 => {
                let mut b = [0u8; 1];
                self.inner.read_exact(&mut b)?;
                Some(b[0] as u32)
            }
            T_UINT16 | T_INT16 => {
                let mut b = [0u8; 2];
                self.inner.read_exact(&mut b)?;
                Some(u16::from_le_bytes(b) as u32)
            }
            T_UINT32 | T_INT32 => Some(self.u32()?),
            T_UINT64 | T_INT64 => Some(self.u64()? as u32),
            _ => {
                self.skip_value(vtype)?;
                None
            }
        };
        Ok(v)
    }
}

/// Parse the KV header of a GGUF file.
///
/// Cheap: reads a few hundred KB at most, never the tensor data.
pub fn read_info(path: &Path) -> Result<GgufInfo> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut r = Reader {
        inner: BufReader::with_capacity(1 << 16, file),
    };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic {magic:#x})");
    }
    let _version = r.u32()?;
    let _n_tensors = r.u64()?;
    let n_kv = r.u64()?;
    if n_kv > 100_000 {
        bail!("implausible GGUF kv count {n_kv}");
    }

    let mut info = GgufInfo::default();
    let mut start_layer: Option<u32> = None;
    let mut end_layer: Option<u32> = None;

    for _ in 0..n_kv {
        let key = r.string()?;
        let vtype = r.u32()?;

        match key.as_str() {
            "general.architecture" if vtype == T_STRING => info.architecture = Some(r.string()?),
            "general.name" if vtype == T_STRING => info.name = Some(r.string()?),
            "general.size_label" if vtype == T_STRING => info.size_label = Some(r.string()?),
            "rdmap2p.original_block_count" => info.original_block_count = r.value_as_u32(vtype)?,
            "rdmap2p.start_layer" => start_layer = r.value_as_u32(vtype)?,
            "rdmap2p.end_layer" => end_layer = r.value_as_u32(vtype)?,
            // `<arch>.block_count` / `<arch>.embedding_length` — the arch prefix
            // varies per model, so match on the suffix.
            k if k.ends_with(".block_count") => info.block_count = r.value_as_u32(vtype)?,
            k if k.ends_with(".embedding_length") => info.embedding_length = r.value_as_u32(vtype)?,
            _ => r.skip_value(vtype)?,
        }
    }

    if let (Some(s), Some(e)) = (start_layer, end_layer) {
        info.shard_range = Some((s, e));
    }

    Ok(info)
}

// ── Tensor layout ────────────────────────────────────────────────────────────

/// Bytes-per-block and elements-per-block for every GGML tensor type.
/// Same table as `split_model.py`, verified against `ggml-quants.h`.
fn ggml_type_info(t: u32) -> Option<(u64, u64)> {
    Some(match t {
        0 => (4, 1),      // F32
        1 => (2, 1),      // F16
        2 => (18, 32),    // Q4_0
        3 => (20, 32),    // Q4_1
        6 => (22, 32),    // Q5_0
        7 => (24, 32),    // Q5_1
        8 => (34, 32),    // Q8_0
        10 => (84, 256),  // Q2_K
        11 => (110, 256), // Q3_K
        12 => (144, 256), // Q4_K
        13 => (112, 256), // Q5_K
        14 => (210, 256), // Q6_K
        15 => (292, 256), // Q8_K
        16 => (2, 1),     // BF16
        19 => (66, 256),  // IQ2_XXS
        20 => (74, 256),  // IQ2_XS
        21 => (66, 256),  // IQ3_XXS
        22 => (50, 256),  // IQ1_S
        23 => (56, 32),   // IQ4_NL
        24 => (98, 256),  // IQ3_S
        25 => (82, 256),  // IQ2_S
        26 => (110, 256), // IQ4_XS
        29 => (52, 256),  // IQ1_M
        _ => return None,
    })
}

/// Real on-disk weight cost of a model, broken down by transformer block.
///
/// This is what makes an honest memory budget possible: instead of guessing
/// "≈380 MB per layer", each layer's actual quantised size is summed from the
/// tensor table.
#[derive(Debug, Clone, Default)]
pub struct GgufLayout {
    /// `layer_bytes[n]` — total bytes of every `blk.<n>.*` tensor.
    pub layer_bytes: Vec<u64>,
    /// `token_embd.weight` — every slice carries it (see `split_model.py`).
    pub embedding_bytes: u64,
    /// `output.weight` + `output_norm.weight` — only the last slice carries them.
    pub head_bytes: u64,
    /// Tensors belonging to none of the above.
    pub other_bytes: u64,
    /// Sum of everything, i.e. the size of the full model's weights.
    pub total_bytes: u64,
}

impl GgufLayout {
    pub fn layer_count(&self) -> u32 {
        self.layer_bytes.len() as u32
    }

    /// Bytes a slice covering `start..=end` would occupy on disk.
    ///
    /// Mirrors what `split_model.py` writes: the layers in range, plus
    /// `token_embd` on every slice, plus the output head on the last one.
    pub fn slice_bytes(&self, start: u32, end: u32, is_last: bool) -> u64 {
        let lo = start as usize;
        let hi = (end as usize).min(self.layer_bytes.len().saturating_sub(1));
        let layers: u64 = self
            .layer_bytes
            .get(lo..=hi)
            .map(|s| s.iter().sum())
            .unwrap_or(0);
        layers + self.embedding_bytes + if is_last { self.head_bytes } else { 0 }
    }

    /// Mean bytes per transformer block — used for a first guess in the UI.
    pub fn avg_layer_bytes(&self) -> u64 {
        if self.layer_bytes.is_empty() {
            0
        } else {
            self.layer_bytes.iter().sum::<u64>() / self.layer_bytes.len() as u64
        }
    }
}

/// Read the tensor table and total up the bytes each transformer block costs.
///
/// Reads the header only — the tensor *data* is never touched.
pub fn read_layout(path: &Path) -> Result<GgufLayout> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut r = Reader {
        inner: BufReader::with_capacity(1 << 16, file),
    };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic {magic:#x})");
    }
    let _version = r.u32()?;
    let n_tensors = r.u64()?;
    let n_kv = r.u64()?;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("implausible GGUF header (tensors={n_tensors}, kv={n_kv})");
    }

    // Skip the KV section wholesale — `read_info` is the one that reads it.
    for _ in 0..n_kv {
        let _key = r.string()?;
        let vtype = r.u32()?;
        r.skip_value(vtype)?;
    }

    let mut layout = GgufLayout::default();
    let mut layers: HashMap<u32, u64> = HashMap::new();
    let mut max_layer: i64 = -1;

    for _ in 0..n_tensors {
        let name = r.string()?;
        let n_dims = r.u32()?;
        if n_dims > 8 {
            bail!("implausible tensor rank {n_dims} for {name}");
        }
        let mut elements: u64 = 1;
        for _ in 0..n_dims {
            elements = elements.saturating_mul(r.u64()?);
        }
        let dtype = r.u32()?;
        let _offset = r.u64()?;

        let bytes = match ggml_type_info(dtype) {
            Some((block_bytes, block_elems)) if block_elems > 0 => {
                elements / block_elems * block_bytes
            }
            // Unknown quantisation: better to under-report this tensor than to
            // invent a size, and the caller sees it in `other_bytes`.
            _ => 0,
        };

        layout.total_bytes += bytes;

        if let Some(n) = block_index(&name) {
            *layers.entry(n).or_insert(0) += bytes;
            max_layer = max_layer.max(n as i64);
        } else if name == "token_embd.weight" {
            layout.embedding_bytes += bytes;
        } else if name == "output.weight" || name == "output_norm.weight" {
            layout.head_bytes += bytes;
        } else {
            layout.other_bytes += bytes;
        }
    }

    if max_layer >= 0 {
        layout.layer_bytes = (0..=max_layer as u32)
            .map(|n| layers.get(&n).copied().unwrap_or(0))
            .collect();
    }

    Ok(layout)
}

/// `blk.17.attn_q.weight` → `Some(17)`.
fn block_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("blk.")?;
    let idx = rest.split('.').next()?;
    idx.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_indices() {
        assert_eq!(block_index("blk.0.attn_q.weight"), Some(0));
        assert_eq!(block_index("blk.31.ffn_up.weight"), Some(31));
        assert_eq!(block_index("token_embd.weight"), None);
        assert_eq!(block_index("blk.x.weight"), None);
    }

    #[test]
    fn slice_bytes_accounts_for_embedding_and_head() {
        let layout = GgufLayout {
            layer_bytes: vec![100; 4],
            embedding_bytes: 50,
            head_bytes: 30,
            other_bytes: 0,
            total_bytes: 480,
        };
        // Middle slice: 2 layers + embedding.
        assert_eq!(layout.slice_bytes(0, 1, false), 250);
        // Last slice also carries the output head.
        assert_eq!(layout.slice_bytes(2, 3, true), 280);
        assert_eq!(layout.avg_layer_bytes(), 100);
    }

    #[test]
    fn rejects_non_gguf() {
        let mut p = std::env::temp_dir();
        p.push("rdma-p2p-not-a-gguf.bin");
        std::fs::write(&p, b"hello world, definitely not gguf").unwrap();
        assert!(read_info(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn scalar_sizes_cover_all_scalar_types() {
        for t in [
            T_UINT8, T_INT8, T_UINT16, T_INT16, T_UINT32, T_INT32, T_FLOAT32, T_BOOL, T_UINT64,
            T_INT64, T_FLOAT64,
        ] {
            assert!(scalar_size(t).is_some(), "type {t} has no size");
        }
        assert!(scalar_size(T_STRING).is_none());
        assert!(scalar_size(T_ARRAY).is_none());
    }
}
