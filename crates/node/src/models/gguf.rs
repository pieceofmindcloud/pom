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
    /// `pom.original_block_count` — layers in the model this file was sliced from.
    /// Only present on shards written by `split_model.py`.
    pub original_block_count: Option<u32>,
    /// `pom.start_layer` / `pom.end_layer` on shards.
    pub shard_range: Option<(u32, u32)>,
    /// `<arch>.embedding_length`.
    pub embedding_length: Option<u32>,
    /// `general.size_label`, e.g. "35B".
    pub size_label: Option<String>,
    /// `pom.tp_format`/`pom.tp_size`/`pom.tp_rank` de um shard de tensor
    /// parallelism: qual versão da regra de fatiamento escreveu este arquivo, e
    /// que posição do grupo ele é.
    ///
    /// `None` em qualquer arquivo que não seja shard de TP - **e também** nos
    /// shards escritos antes de `tp_format` existir, que é o ponto: um shard
    /// sem carimbo é um shard de regra desconhecida, e regra desconhecida se
    /// regenera. Ver `splitter::TP_SHARD_FORMAT`.
    pub tp_stamp: Option<TpStamp>,
}

/// Carimbo de origem de um shard de tensor parallelism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpStamp {
    pub format: u32,
    pub size: u32,
    pub rank: u32,
}

impl GgufInfo {
    /// Layers of the *whole* model: the shard metadata when present, else the
    /// block count of the file itself.
    pub fn total_layers(&self) -> Option<u32> {
        self.original_block_count.or(self.block_count)
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
    let mut tp_format: Option<u32> = None;
    let mut tp_size: Option<u32> = None;
    let mut tp_rank: Option<u32> = None;

    for _ in 0..n_kv {
        let key = r.string()?;
        let vtype = r.u32()?;

        match key.as_str() {
            "general.architecture" if vtype == T_STRING => info.architecture = Some(r.string()?),
            "general.name" if vtype == T_STRING => info.name = Some(r.string()?),
            "general.size_label" if vtype == T_STRING => info.size_label = Some(r.string()?),
            // Chaves nossas: o prefixo vem da marca (`brand.json`), não de um
            // literal, para renomear o produto não deixar metade do leitor
            // procurando o nome velho.
            k if k == crate::brand::gguf_key("original_block_count") => {
                info.original_block_count = r.value_as_u32(vtype)?
            }
            k if k == crate::brand::gguf_key("tp_format") => tp_format = r.value_as_u32(vtype)?,
            k if k == crate::brand::gguf_key("tp_size") => tp_size = r.value_as_u32(vtype)?,
            k if k == crate::brand::gguf_key("tp_rank") => tp_rank = r.value_as_u32(vtype)?,
            k if k == crate::brand::gguf_key("start_layer") => {
                start_layer = r.value_as_u32(vtype)?
            }
            k if k == crate::brand::gguf_key("end_layer") => end_layer = r.value_as_u32(vtype)?,
            // `<arch>.block_count` / `<arch>.embedding_length` — the arch prefix
            // varies per model, so match on the suffix.
            k if k.ends_with(".block_count") => info.block_count = r.value_as_u32(vtype)?,
            k if k.ends_with(".embedding_length") => {
                info.embedding_length = r.value_as_u32(vtype)?
            }
            _ => r.skip_value(vtype)?,
        }
    }

    if let (Some(s), Some(e)) = (start_layer, end_layer) {
        info.shard_range = Some((s, e));
    }
    // Sem os tres, nao ha carimbo: um shard antigo trazia `tp_size`/`tp_rank`
    // mas nenhum `tp_format`, e e exatamente esse arquivo que nao pode passar
    // por atual.
    if let (Some(f), Some(sz), Some(rk)) = (tp_format, tp_size, tp_rank) {
        info.tp_stamp = Some(TpStamp {
            format: f,
            size: sz,
            rank: rk,
        });
    }

    Ok(info)
}

// ── Tensor layout ────────────────────────────────────────────────────────────

/// Bytes-per-block and elements-per-block for every GGML tensor type.
///
/// Codes are `enum ggml_type`; sizes are what `ggml_type_size`/`ggml_blck_size`
/// report. Both were read from the llama.cpp in `./venv`, not from memory: the
/// IQ block used to start at 19 (every IQ code off by three, because the two
/// removed Q4_2/Q4_3 slots were counted away) which measured IQ3_S tensors with
/// IQ3_XXS's size and priced an IQ model's layers at roughly 60% of the truth.
///
/// This table and `_GGML_TYPE_INFO` in `split_model.py` must agree; the two
/// live in different languages, so `split_model_type_table_matches` below reads
/// the Python one and fails the build when they drift.
fn ggml_type_info(t: u32) -> Option<(u64, u64)> {
    Some(match t {
        0 => (4, 1),      // F32
        1 => (2, 1),      // F16
        2 => (18, 32),    // Q4_0
        3 => (20, 32),    // Q4_1
        6 => (22, 32),    // Q5_0
        7 => (24, 32),    // Q5_1
        8 => (34, 32),    // Q8_0
        9 => (36, 32),    // Q8_1
        10 => (84, 256),  // Q2_K
        11 => (110, 256), // Q3_K
        12 => (144, 256), // Q4_K
        13 => (176, 256), // Q5_K  — was 112: qs is QK_K/2=128 bytes, not 64
        14 => (210, 256), // Q6_K
        15 => (292, 256), // Q8_K
        16 => (66, 256),  // IQ2_XXS — was labelled BF16 (2,1); BF16 is 30
        17 => (74, 256),  // IQ2_XS
        18 => (98, 256),  // IQ3_XXS
        19 => (50, 256),  // IQ1_S
        20 => (18, 32),   // IQ4_NL
        21 => (110, 256), // IQ3_S
        22 => (82, 256),  // IQ2_S
        23 => (136, 256), // IQ4_XS
        24 => (1, 1),     // I8
        25 => (2, 1),     // I16
        26 => (4, 1),     // I32
        27 => (8, 1),     // I64
        28 => (8, 1),     // F64
        29 => (56, 256),  // IQ1_M
        30 => (2, 1),     // BF16
        34 => (54, 256),  // TQ1_0
        35 => (66, 256),  // TQ2_0
        39 => (17, 32),   // MXFP4
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
    /// `layer_kinds[n]` — sorted tensor suffixes of `blk.<n>.*`, so two layers
    /// that are different *kinds* of layer compare unequal. Hybrid models
    /// (attention interleaved with Gated DeltaNet / Mamba) are not a stack of
    /// identical blocks, and that decides where a slice may start.
    pub layer_kinds: Vec<Vec<String>>,
    /// `<arch>.nextn_predict_layers` — how many trailing layers form the
    /// NextN/MTP (multi-token prediction) block. On Qwen3.5+ the predictor
    /// tensors live only in the very last layer; a slice that does not end on
    /// the model's last layer must carry this hyperparameter zeroed or
    /// llama.cpp aborts the load looking for `blk.<last>.nextn.*`.
    pub nextn_predict_layers: u32,
}

impl GgufLayout {
    pub fn layer_count(&self) -> u32 {
        self.layer_bytes.len() as u32
    }

    /// Indices where the layer kind changes — the architectural boundaries.
    ///
    /// llama.cpp decides a layer's kind from its *index*, and a slice renumbers
    /// `blk.<start+i>` to `blk.<i>`. A slice boundary is only safe where the
    /// kind sequence is continuous: cutting a hybrid model mid-group shifts every
    /// subsequent layer onto the wrong kind — sometimes a hard `missing tensor`
    /// error, sometimes silently wrong answers.
    ///
    /// For [8 Mamba, 8 Attention, 8 Mamba, 8 Attention] this returns [8, 16, 24].
    pub fn arch_boundaries(&self) -> Vec<u32> {
        let n = self.layer_kinds.len();
        (1..n)
            .filter(|&i| self.layer_kinds[i] != self.layer_kinds[i - 1])
            .map(|i| i as u32)
            .collect()
    }

    /// Snap `end` so that `end + 1` falls on a valid slice-start position.
    ///
    /// Valid starts are layer 0, every architectural boundary, and `total_layers`
    /// (end of model). When a budget's natural cut falls mid-group, this walks
    /// back to the last boundary that is ≤ `end + 1` — the slice shrinks to stay
    /// whole-group, rather than handing split_model.py a cut it will refuse.
    pub fn snap_end_layer(&self, start: u32, end: u32, total: u32) -> u32 {
        if end + 1 >= total {
            return end;
        }
        let boundaries = self.arch_boundaries();
        if boundaries.is_empty() {
            return end;
        }
        let next_start = end + 1;
        if let Some(&b) = boundaries
            .iter()
            .rev()
            .find(|&&b| b <= next_start && b > start)
        {
            b - 1
        } else {
            end
        }
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
    // The exception is the NextN hyperparameter: it has no tensor of its own,
    // yet it decides whether a slice loads at all (see `nextn_predict_layers`).
    let mut layout = GgufLayout::default();
    for _ in 0..n_kv {
        let key = r.string()?;
        let vtype = r.u32()?;
        if key.ends_with(".nextn_predict_layers") {
            layout.nextn_predict_layers = r.value_as_u32(vtype)?.unwrap_or(0);
        } else {
            r.skip_value(vtype)?;
        }
    }
    let mut layers: HashMap<u32, u64> = HashMap::new();
    let mut kinds: HashMap<u32, Vec<String>> = HashMap::new();
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
            // `blk.12.attn_q.weight` -> `attn_q.weight`
            if let Some(suffix) = name.splitn(3, '.').nth(2) {
                kinds.entry(n).or_default().push(suffix.to_string());
            }
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
        layout.layer_kinds = (0..=max_layer as u32)
            .map(|n| {
                let mut v = kinds.remove(&n).unwrap_or_default();
                v.sort(); // tensor order in the file is not meaningful here
                v
            })
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
            layer_kinds: vec![],
            nextn_predict_layers: 0,
        };
        // Middle slice: 2 layers + embedding.
        assert_eq!(layout.slice_bytes(0, 1, false), 250);
        // Last slice also carries the output head.
        assert_eq!(layout.slice_bytes(2, 3, true), 280);
    }

    #[test]
    fn rejects_non_gguf() {
        let mut p = std::env::temp_dir();
        p.push("pom-not-a-gguf.bin");
        std::fs::write(&p, b"hello world, definitely not gguf").unwrap();
        assert!(read_info(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    /// A header-only GGUF: `n_tensors = 0` and whatever KV pairs are given.
    fn header_only_gguf(kvs: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes()); // version
        b.extend_from_slice(&0u64.to_le_bytes()); // n_tensors
        b.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (k, vtype, v) in kvs {
            b.extend_from_slice(&(k.len() as u64).to_le_bytes());
            b.extend_from_slice(k.as_bytes());
            b.extend_from_slice(&vtype.to_le_bytes());
            if *vtype == T_STRING {
                b.extend_from_slice(&(v.len() as u64).to_le_bytes());
            }
            b.extend_from_slice(v);
        }
        b
    }

    #[test]
    fn read_layout_sees_nextn_predict_layers() {
        let dir = std::env::temp_dir();
        let mut p = dir.clone();
        p.push("pom-nextn-layout.gguf");
        std::fs::write(
            &p,
            header_only_gguf(&[
                ("general.architecture", T_STRING, "qwen35".into()),
                (
                    "qwen35.nextn_predict_layers",
                    T_UINT32,
                    1u32.to_le_bytes().to_vec(),
                ),
            ]),
        )
        .unwrap();
        let layout = read_layout(&p).unwrap();
        assert_eq!(layout.nextn_predict_layers, 1);
        let _ = std::fs::remove_file(&p);

        // Absent or non-scalar means "no predictor", not a broken plan.
        let mut q = dir.clone();
        q.push("pom-no-nextn-layout.gguf");
        std::fs::write(
            &q,
            header_only_gguf(&[("general.architecture", T_STRING, "llama".into())]),
        )
        .unwrap();
        let layout = read_layout(&q).unwrap();
        assert_eq!(layout.nextn_predict_layers, 0);
        let _ = std::fs::remove_file(&q);
    }

    /// Every type this project claims to measure, with the `(type_size,
    /// blck_size)` ggml itself reports. Read out of the llama.cpp shipped in
    /// `./venv` via `ggml_type_size`/`ggml_blck_size` — the point of writing
    /// them down again here is that a typo has to be made twice to survive.
    const GGML_TRUTH: &[(u32, u64, u64, &str)] = &[
        (0, 4, 1, "F32"),
        (1, 2, 1, "F16"),
        (2, 18, 32, "Q4_0"),
        (3, 20, 32, "Q4_1"),
        (6, 22, 32, "Q5_0"),
        (7, 24, 32, "Q5_1"),
        (8, 34, 32, "Q8_0"),
        (9, 36, 32, "Q8_1"),
        (10, 84, 256, "Q2_K"),
        (11, 110, 256, "Q3_K"),
        (12, 144, 256, "Q4_K"),
        (13, 176, 256, "Q5_K"),
        (14, 210, 256, "Q6_K"),
        (15, 292, 256, "Q8_K"),
        (16, 66, 256, "IQ2_XXS"),
        (17, 74, 256, "IQ2_XS"),
        (18, 98, 256, "IQ3_XXS"),
        (19, 50, 256, "IQ1_S"),
        (20, 18, 32, "IQ4_NL"),
        (21, 110, 256, "IQ3_S"),
        (22, 82, 256, "IQ2_S"),
        (23, 136, 256, "IQ4_XS"),
        (24, 1, 1, "I8"),
        (25, 2, 1, "I16"),
        (26, 4, 1, "I32"),
        (27, 8, 1, "I64"),
        (28, 8, 1, "F64"),
        (29, 56, 256, "IQ1_M"),
        (30, 2, 1, "BF16"),
        (34, 54, 256, "TQ1_0"),
        (35, 66, 256, "TQ2_0"),
        (39, 17, 32, "MXFP4"),
    ];

    #[test]
    fn ggml_type_table_matches_ggml() {
        for &(code, bytes, elems, name) in GGML_TRUTH {
            assert_eq!(
                ggml_type_info(code),
                Some((bytes, elems)),
                "ggml type {code} ({name}) has the wrong block size"
            );
        }
    }

    /// The codes ggml removed, plus the ones whose layout we chose not to cite.
    /// They must stay unknown: a file using one has to be refused, and a size
    /// invented here would be spent slicing it into garbage instead.
    #[test]
    fn removed_and_uncited_ggml_types_stay_unknown() {
        for code in [4, 5, 31, 32, 33, 36, 37, 38, 40, 41, 42, 99] {
            assert_eq!(ggml_type_info(code), None, "type {code} should be unknown");
        }
    }

    /// IQ3_S is what the captain's IQ3_M model is mostly made of, and the two
    /// numbers this pins are exactly the ones that were wrong: a 5120×248320
    /// embedding priced with IQ3_XXS's 66-byte block instead of IQ3_S's 110.
    #[test]
    fn tensor_bytes_per_type() {
        let bytes = |code: u32, elements: u64| {
            let (b, e) = ggml_type_info(code).expect("known type");
            elements / e * b
        };
        // token_embd.weight of the real IQ3_M model: 546 MB, not the 327 MB the
        // old table reported for it by handing IQ3_S a 66-byte block.
        assert_eq!(bytes(21, 5120 * 248320), 546_304_000);
        // BF16 output.weight — 2 bytes per element, not a 256-element block.
        assert_eq!(bytes(30, 5120 * 248320), 2_542_796_800);
        // A Q5_K layer, the entry that was booked at 64% of its true size
        // (11_534_336 here against the 7_340_032 the 112-byte block gave).
        assert_eq!(bytes(13, 4096 * 4096), 11_534_336);
        assert_eq!(bytes(12, 4096 * 4096), 9_437_184); // Q4_K, always correct
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
