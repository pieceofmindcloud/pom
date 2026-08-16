//! GGUF model splitter — Rust replacement for `split_model.py`.
//!
//! Reads the source model with mmap (never loads weights into RAM), selects
//! tensors for the assigned layer range, remaps `blk.(start+i).*` names to
//! `blk.i.*`, injects a fake identity `output_norm.weight` on non-last slices,
//! updates KV metadata, and streams the slice to disk.
//!
//! Progress is reported via `on_progress(0.0..=100.0)` as bytes are written.

use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use tracing::info;

const GGUF_MAGIC: u32 = 0x4655_4747;
const ALIGN: usize = 32;
const CHUNK: usize = 8 << 20; // 8 MB per write

// ── GGUF value type codes ─────────────────────────────────────────────────────
const T_U8: u32 = 0;
const T_I8: u32 = 1;
const T_U16: u32 = 2;
const T_I16: u32 = 3;
const T_U32: u32 = 4;
const T_I32: u32 = 5;
const T_F32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STR: u32 = 8;
const T_ARR: u32 = 9;
const T_U64: u32 = 10;
const T_I64: u32 = 11;
const T_F64: u32 = 12;

fn scalar_size(t: u32) -> Option<usize> {
    match t {
        T_U8 | T_I8 | T_BOOL => Some(1),
        T_U16 | T_I16 => Some(2),
        T_U32 | T_I32 | T_F32 => Some(4),
        T_U64 | T_I64 | T_F64 => Some(8),
        _ => None,
    }
}

// ── Byte reader ───────────────────────────────────────────────────────────────

struct R<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("GGUF truncado em byte {}", self.pos);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String> {
        let len = self.u64()? as usize;
        if len > 64 << 20 {
            bail!("string GGUF implausível: {len} bytes");
        }
        let b = self.take(len)?;
        Ok(std::str::from_utf8(b).unwrap_or("").to_owned())
    }
    fn skip_value(&mut self, vtype: u32) -> Result<()> {
        match vtype {
            T_U8 | T_I8 | T_BOOL => {
                self.take(1)?;
            }
            T_U16 | T_I16 => {
                self.take(2)?;
            }
            T_U32 | T_I32 | T_F32 => {
                self.take(4)?;
            }
            T_U64 | T_I64 | T_F64 => {
                self.take(8)?;
            }
            T_STR => {
                let len = self.u64()? as usize;
                self.take(len)?;
            }
            T_ARR => {
                let elem = self.u32()?;
                let count = self.u64()? as usize;
                if let Some(s) = scalar_size(elem) {
                    self.take(count * s)?;
                } else {
                    for _ in 0..count {
                        self.skip_value(elem)?;
                    }
                }
            }
            other => bail!("tipo GGUF desconhecido {other}"),
        }
        Ok(())
    }
}

// ── Data structures ───────────────────────────────────────────────────────────

struct TensorRecord {
    name: String,
    dims: Vec<u64>,
    dtype: u32,
    src_offset: u64,
    byte_size: u64, // from offset-delta; 0 for fake tensors
}

enum TensorOut {
    Real(TensorRecord),
    Fake {
        name: String,
        dims: Vec<u64>,
        dtype: u32,
        data: Vec<u8>,
    },
}

impl TensorOut {
    fn name(&self) -> &str {
        match self {
            TensorOut::Real(t) => &t.name,
            TensorOut::Fake { name, .. } => name,
        }
    }
    fn dims(&self) -> &[u64] {
        match self {
            TensorOut::Real(t) => &t.dims,
            TensorOut::Fake { dims, .. } => dims,
        }
    }
    fn dtype(&self) -> u32 {
        match self {
            TensorOut::Real(t) => t.dtype,
            TensorOut::Fake { dtype, .. } => *dtype,
        }
    }
    fn byte_size(&self) -> u64 {
        match self {
            TensorOut::Real(t) => t.byte_size,
            TensorOut::Fake { data, .. } => data.len() as u64,
        }
    }
}

// An entry in the output KV section: either raw bytes from the source or fresh.
#[derive(Clone)]
enum KvOut {
    Raw(usize, usize), // [start, end) in the source mmap
    Encoded(Vec<u8>),
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn align_up(n: u64) -> u64 {
    let a = ALIGN as u64;
    (n + a - 1) & !(a - 1)
}

fn encode_kv_u32(key: &str, value: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + key.len() + 4 + 4);
    let kb = key.as_bytes();
    buf.extend_from_slice(&(kb.len() as u64).to_le_bytes());
    buf.extend_from_slice(kb);
    buf.extend_from_slice(&T_U32.to_le_bytes());
    buf.extend_from_slice(&value.to_le_bytes());
    buf
}

fn tensor_header_size(name: &str, dims: &[u64]) -> usize {
    8 + name.len()          // len(u64) + bytes
    + 4                     // n_dims(u32)
    + 8 * dims.len()        // dims(u64 each)
    + 4                     // dtype(u32)
    + 8 // offset(u64)
}

fn want_tensor(name: &str, start: u32, end: u32, is_last: bool) -> bool {
    if let Some(rest) = name.strip_prefix("blk.") {
        let idx: u32 = rest
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(u32::MAX);
        return idx >= start && idx <= end;
    }
    match name {
        "token_embd.weight" => true,
        "output_norm.weight" | "output_norm.bias" | "output.weight" => is_last,
        n if n.contains("rope") || n.contains("pos_embd") => true,
        _ => false,
    }
}

fn remap_name(name: &str, start: u32) -> String {
    if let Some(rest) = name.strip_prefix("blk.") {
        if let Some(dot) = rest.find('.') {
            if let Ok(idx) = rest[..dot].parse::<u32>() {
                return format!("blk.{}{}", idx - start, &rest[dot..]);
            }
        }
    }
    name.to_string()
}

/// The inverse of `remap_name`: a slice names its blocks `blk.0..`, and joining
/// it after earlier slices gives each block back its real place.
fn remap_merged(name: &str, offset: u32) -> String {
    if let Some(rest) = name.strip_prefix("blk.") {
        if let Some(dot) = rest.find('.') {
            if let Ok(idx) = rest[..dot].parse::<u32>() {
                return format!("blk.{}{}", idx + offset, &rest[dot..]);
            }
        }
    }
    name.to_string()
}

fn identity_bytes(hidden_dim: u32, dtype: u32) -> (u32, Vec<u8>) {
    let n = hidden_dim as usize;
    match dtype {
        1 => {
            // F16: 1.0 = 0x3C00
            let mut d = vec![0u8; n * 2];
            for i in 0..n {
                d[i * 2..i * 2 + 2].copy_from_slice(&0x3C00u16.to_le_bytes());
            }
            (1, d)
        }
        30 => {
            // BF16: 1.0 = 0x3F80
            let mut d = vec![0u8; n * 2];
            for i in 0..n {
                d[i * 2..i * 2 + 2].copy_from_slice(&0x3F80u16.to_le_bytes());
            }
            (30, d)
        }
        _ => {
            // F32 (default)
            let mut d = vec![0u8; n * 4];
            for i in 0..n {
                d[i * 4..i * 4 + 4].copy_from_slice(&1.0f32.to_le_bytes());
            }
            (0, d)
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Slice a GGUF model file.
///
/// Produces a valid standalone GGUF at `dst` covering layers
/// `start_layer..=end_layer` of the source `src`. `on_progress` receives
/// values in 0.0..=100.0 as tensor data is streamed to disk.
///
/// Returns the byte size of the output file.
pub fn split(
    src: &Path,
    dst: &Path,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
    on_progress: impl Fn(f64),
) -> Result<u64> {
    let _ = is_first; // reserved; currently unused (no tensors differ on first vs. middle)
    let n_split = end_layer - start_layer + 1;

    info!(
        "✂️  fatiando camadas {start_layer}–{end_layer} de {}",
        src.display()
    );

    let src_file = File::open(src).with_context(|| format!("abrir {}", src.display()))?;
    let src_mm =
        unsafe { Mmap::map(&src_file) }.with_context(|| format!("mmap {}", src.display()))?;
    let mm: &[u8] = &src_mm;

    let mut r = R { data: mm, pos: 0 };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("não é um arquivo GGUF (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("versão GGUF {version} não suportada (esperado 2 ou 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("cabeçalho GGUF implausível ({n_tensors} tensors, {n_kv} kv)");
    }

    // ── Parse KV section ─────────────────────────────────────────────────────
    // Pass most entries through as raw byte slices. Only modify *.block_count
    // and strip/replace rdmap2p.* entries.

    let mut arch = "llama".to_string();
    let mut hidden_dim: u32 = 4096;
    let mut original_block_count: Option<u32> = None;
    let mut kv_out: Vec<KvOut> = Vec::with_capacity(n_kv + 4);
    let mut block_count_updated = false;

    for _ in 0..n_kv {
        let kv_start = r.pos;
        let key = r.string()?;
        let vtype = r.u32()?;

        if key == "general.architecture" && vtype == T_STR {
            arch = r.string()?;
            kv_out.push(KvOut::Raw(kv_start, r.pos));
        } else if key.ends_with(".embedding_length") {
            hidden_dim = match vtype {
                T_U32 => u32::from_le_bytes(r.take(4)?.try_into().unwrap()),
                T_U64 => r.u64()? as u32,
                _ => {
                    r.skip_value(vtype)?;
                    hidden_dim
                }
            };
            kv_out.push(KvOut::Raw(kv_start, r.pos));
        } else if key == "rdmap2p.original_block_count" {
            original_block_count = match vtype {
                T_U32 => Some(u32::from_le_bytes(r.take(4)?.try_into().unwrap())),
                _ => {
                    r.skip_value(vtype)?;
                    None
                }
            };
            // Drop — re-added below with updated values
        } else if key.starts_with("rdmap2p.") {
            r.skip_value(vtype)?;
            // Drop all rdmap2p.* — re-added below
        } else if key.ends_with(".nextn_predict_layers") {
            // NextN/MTP (Qwen3.5+): the predictor block lives only at the very
            // end of the model. llama.cpp derives the expected `blk.<last>.
            // nextn.*` tensors from this hyperparameter and aborts the load
            // ("missing tensor") when they are absent — a non-last slice has
            // them absent by construction. Zero it there; the last slice keeps
            // the original value and carries the real block.
            r.skip_value(vtype)?;
            kv_out.push(if is_last {
                KvOut::Raw(kv_start, r.pos)
            } else {
                KvOut::Encoded(encode_kv_u32(&key, 0))
            });
        } else if key.ends_with(".block_count") {
            // Record original block count from the source file's header
            let source_bc = match vtype {
                T_U32 => Some(u32::from_le_bytes(r.take(4)?.try_into().unwrap())),
                T_U64 => Some(r.u64()? as u32),
                _ => {
                    r.skip_value(vtype)?;
                    None
                }
            };
            if original_block_count.is_none() {
                original_block_count = source_bc;
            }
            // Replace with the split layer count
            kv_out.push(KvOut::Encoded(encode_kv_u32(&key, n_split)));
            block_count_updated = true;
        } else {
            r.skip_value(vtype)?;
            kv_out.push(KvOut::Raw(kv_start, r.pos));
        }
    }

    if !block_count_updated {
        kv_out.push(KvOut::Encoded(encode_kv_u32(
            &format!("{arch}.block_count"),
            n_split,
        )));
    }
    if let Some(orig) = original_block_count {
        kv_out.push(KvOut::Encoded(encode_kv_u32(
            "rdmap2p.original_block_count",
            orig,
        )));
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        "rdmap2p.start_layer",
        start_layer,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        "rdmap2p.end_layer",
        end_layer,
    )));

    // ── Parse tensor table ────────────────────────────────────────────────────

    let mut all_tensors: Vec<TensorRecord> = Vec::with_capacity(n_tensors);
    for _ in 0..n_tensors {
        let name = r.string()?;
        let n_dims = r.u32()? as usize;
        if n_dims > 8 {
            bail!("rank {n_dims} implausível para tensor '{name}'");
        }
        let dims: Vec<u64> = (0..n_dims).map(|_| r.u64()).collect::<Result<_>>()?;
        let dtype = r.u32()?;
        let src_offset = r.u64()?;
        all_tensors.push(TensorRecord {
            name,
            dims,
            dtype,
            src_offset,
            byte_size: 0,
        });
    }

    // The data section starts on an ALIGN boundary after the header
    let src_data_start = align_up(r.pos as u64) as usize;

    // Compute actual tensor sizes from offset deltas — more reliable than the
    // type table, handles unknown quants and weight-tied (shared-offset) tensors.
    {
        let total_data = (mm.len() - src_data_start) as u64;
        let mut unique: Vec<u64> = all_tensors.iter().map(|t| t.src_offset).collect();
        unique.sort_unstable();
        unique.dedup();
        let size_map: HashMap<u64, u64> = unique
            .iter()
            .enumerate()
            .map(|(i, &off)| {
                let next = unique.get(i + 1).copied().unwrap_or(total_data);
                (off, next - off)
            })
            .collect();
        for t in &mut all_tensors {
            t.byte_size = size_map.get(&t.src_offset).copied().unwrap_or(0);
        }
    }

    // ── Select tensors ────────────────────────────────────────────────────────

    let selected: Vec<&TensorRecord> = all_tensors
        .iter()
        .filter(|t| want_tensor(&t.name, start_layer, end_layer, is_last))
        .collect();

    if selected.is_empty() {
        bail!("nenhum tensor selecionado para camadas {start_layer}–{end_layer}");
    }
    for t in &selected {
        if t.byte_size == 0 {
            bail!(
                "tamanho desconhecido para o tensor '{}' (tipo ggml {}): \
                fatiar produziria uma fatia corrompida",
                t.name,
                t.dtype
            );
        }
    }

    let mut out_tensors: Vec<TensorOut> = selected
        .iter()
        .map(|&t| {
            TensorOut::Real(TensorRecord {
                name: t.name.clone(),
                dims: t.dims.clone(),
                dtype: t.dtype,
                src_offset: t.src_offset,
                byte_size: t.byte_size,
            })
        })
        .collect();

    if !is_last {
        let out_norm_dtype = all_tensors
            .iter()
            .find(|t| t.name == "output_norm.weight")
            .map(|t| t.dtype)
            .unwrap_or(0);
        let (dtype, data) = identity_bytes(hidden_dim, out_norm_dtype);
        out_tensors.push(TensorOut::Fake {
            name: "output_norm.weight".to_string(),
            dims: vec![hidden_dim as u64],
            dtype,
            data,
        });
    }

    // ── Compute output layout ─────────────────────────────────────────────────

    // New relative offsets for output tensors (aligned)
    let new_offsets: Vec<u64> = {
        let mut v = Vec::with_capacity(out_tensors.len());
        let mut cur: u64 = 0;
        for t in &out_tensors {
            v.push(cur);
            cur = align_up(cur + t.byte_size());
        }
        v
    };

    // Size of the output header
    let kv_bytes: usize = kv_out
        .iter()
        .map(|e| match e {
            KvOut::Raw(s, e) => e - s,
            KvOut::Encoded(v) => v.len(),
        })
        .sum();
    let tensor_header_bytes: usize = out_tensors
        .iter()
        .map(|t| tensor_header_size(&remap_name(t.name(), start_layer), t.dims()))
        .sum();
    let meta_size = 4 + 4 + 8 + 8 + kv_bytes + tensor_header_bytes;
    let new_data_offset = align_up(meta_size as u64) as usize;

    // ── Write output file ─────────────────────────────────────────────────────

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("criar diretório {}", parent.display()))?;
    }

    let dst_file = File::create(dst).with_context(|| format!("criar {}", dst.display()))?;
    let mut w = BufWriter::with_capacity(CHUNK, dst_file);

    // Fixed header
    w.write_all(&GGUF_MAGIC.to_le_bytes())?;
    w.write_all(&version.to_le_bytes())?;
    w.write_all(&(out_tensors.len() as u64).to_le_bytes())?;
    w.write_all(&(kv_out.len() as u64).to_le_bytes())?;

    // KV section
    for entry in &kv_out {
        match entry {
            KvOut::Raw(s, e) => w.write_all(&mm[*s..*e])?,
            KvOut::Encoded(v) => w.write_all(v)?,
        }
    }

    // Tensor headers
    for (t, &off) in out_tensors.iter().zip(&new_offsets) {
        let remapped = remap_name(t.name(), start_layer);
        let nb = remapped.as_bytes();
        w.write_all(&(nb.len() as u64).to_le_bytes())?;
        w.write_all(nb)?;
        w.write_all(&(t.dims().len() as u32).to_le_bytes())?;
        for &d in t.dims() {
            w.write_all(&d.to_le_bytes())?;
        }
        w.write_all(&t.dtype().to_le_bytes())?;
        w.write_all(&off.to_le_bytes())?;
    }

    // Padding to data section boundary
    let header_written = meta_size;
    let pad = new_data_offset - header_written;
    w.write_all(&vec![0u8; pad])?;

    // Tensor data
    let total_bytes: u64 = out_tensors.iter().map(|t| t.byte_size()).sum();
    let mut done: u64 = 0;

    for t in &out_tensors {
        let size = t.byte_size();

        match t {
            TensorOut::Fake { data, .. } => {
                w.write_all(data)?;
                done += size;
                if total_bytes > 0 {
                    on_progress(100.0 * done as f64 / total_bytes as f64);
                }
            }
            TensorOut::Real(rec) => {
                let start = src_data_start + rec.src_offset as usize;
                let tensor_bytes = &mm[start..start + size as usize];
                let mut pos = 0usize;
                while pos < tensor_bytes.len() {
                    let n = CHUNK.min(tensor_bytes.len() - pos);
                    w.write_all(&tensor_bytes[pos..pos + n])?;
                    pos += n;
                    done += n as u64;
                    if total_bytes > 0 {
                        on_progress(100.0 * done as f64 / total_bytes as f64);
                    }
                }
            }
        }

        // Alignment padding between tensors
        let aligned = align_up(size) as usize;
        let pad = aligned - size as usize;
        if pad > 0 {
            w.write_all(&vec![0u8; pad])?;
        }
    }

    w.flush()?;

    let final_size = std::fs::metadata(dst)
        .with_context(|| format!("stat {}", dst.display()))?
        .len();

    info!(
        "fatia pronta: {} ({:.0} MB)",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0)
    );

    Ok(final_size)
}

// ── Merge: the inverse of split ──────────────────────────────────────────────

/// One input slice of a `merge`, with its place inside the original model.
#[derive(Debug, Clone)]
pub struct MergePart {
    pub path: std::path::PathBuf,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// What a `merge` reads out of each slice: the parts that survive into the
/// joined file are its tensors, and the joined header is the first slice's with
/// `*.block_count` and `rdmap2p.*` rewritten.
struct MergedSrc {
    version: u32,
    arch: String,
    original_block_count: Option<u32>,
    /// Every KV entry of this file, tagged with its key so the merge can drop
    /// `rdmap2p.*` and swap `*.block_count` before re-emitting.
    kv: Vec<(String, KvOut)>,
    tensors: Vec<TensorRecord>,
    data_start: usize,
}

fn parse_merged_src(mm: &[u8]) -> Result<MergedSrc> {
    let mut r = R { data: mm, pos: 0 };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("não é um arquivo GGUF (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("versão GGUF {version} não suportada (esperado 2 ou 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("cabeçalho GGUF implausível ({n_tensors} tensors, {n_kv} kv)");
    }

    let mut arch = "llama".to_string();
    let mut original_block_count: Option<u32> = None;
    let mut kv: Vec<(String, KvOut)> = Vec::with_capacity(n_kv);
    for _ in 0..n_kv {
        let kv_start = r.pos;
        let key = r.string()?;
        let vtype = r.u32()?;

        if key == "general.architecture" && vtype == T_STR {
            arch = r.string()?;
        } else if key == "rdmap2p.original_block_count" {
            original_block_count = match vtype {
                T_U32 => Some(u32::from_le_bytes(r.take(4)?.try_into().unwrap())),
                T_U64 => Some(r.u64()? as u32),
                _ => {
                    r.skip_value(vtype)?;
                    None
                }
            };
        } else {
            r.skip_value(vtype)?;
        }
        kv.push((key, KvOut::Raw(kv_start, r.pos)));
    }

    let mut tensors: Vec<TensorRecord> = Vec::with_capacity(n_tensors);
    for _ in 0..n_tensors {
        let name = r.string()?;
        let n_dims = r.u32()? as usize;
        if n_dims > 8 {
            bail!("rank {n_dims} implausível para tensor '{name}'");
        }
        let dims: Vec<u64> = (0..n_dims).map(|_| r.u64()).collect::<Result<_>>()?;
        let dtype = r.u32()?;
        let src_offset = r.u64()?;
        tensors.push(TensorRecord {
            name,
            dims,
            dtype,
            src_offset,
            byte_size: 0,
        });
    }

    let data_start = align_up(r.pos as u64) as usize;
    {
        // Real sizes from offset deltas, exactly as `split` reads them — the
        // type table alone cannot be trusted with every quant.
        let total_data = (mm.len() - data_start) as u64;
        let mut unique: Vec<u64> = tensors.iter().map(|t| t.src_offset).collect();
        unique.sort_unstable();
        unique.dedup();
        let size_map: HashMap<u64, u64> = unique
            .iter()
            .enumerate()
            .map(|(i, &off)| {
                let next = unique.get(i + 1).copied().unwrap_or(total_data);
                (off, next - off)
            })
            .collect();
        for t in &mut tensors {
            t.byte_size = size_map.get(&t.src_offset).copied().unwrap_or(0);
        }
    }

    Ok(MergedSrc {
        version,
        arch,
        original_block_count,
        kv,
        tensors,
        data_start,
    })
}

/// Join adjacent slices of one model back into a single GGUF.
///
/// `split` wrote every slice with its blocks renamed `blk.0..`, so two slices
/// cannot be loaded side by side — a node serves one file. `merge` is the
/// inverse: it drops the duplicate `token_embd`/`output_*` tensors, renames
/// each later slice's blocks to their real places, and writes one file covering
/// the union range. `on_progress` receives 0.0..=100.0 as bytes are written.
///
/// Returns the byte size of the output file.
pub fn merge(parts: &[MergePart], dst: &Path, on_progress: impl Fn(f64)) -> Result<u64> {
    if parts.len() < 2 {
        bail!("juntar menos de duas fatias não faz sentido");
    }
    for w in parts.windows(2) {
        if w[1].start_layer != w[0].end_layer + 1 {
            bail!(
                "as fatias L{}-{} e L{}-{} não são vizinhas",
                w[0].start_layer,
                w[0].end_layer,
                w[1].start_layer,
                w[1].end_layer
            );
        }
    }

    let mut mmaps: Vec<Mmap> = Vec::with_capacity(parts.len());
    let mut parsed: Vec<MergedSrc> = Vec::with_capacity(parts.len());
    for p in parts {
        let src_file =
            File::open(&p.path).with_context(|| format!("abrir {}", p.path.display()))?;
        let mm = unsafe { Mmap::map(&src_file) }
            .with_context(|| format!("mmap {}", p.path.display()))?;
        parsed.push(parse_merged_src(&mm)?);
        mmaps.push(mm);
    }

    let arch = parsed[0].arch.clone();
    for (i, p) in parsed.iter().enumerate().skip(1) {
        if p.arch != arch {
            bail!(
                "as fatias não são do mesmo modelo: a arquitetura da primeira é {arch} e a da {}ª é {}",
                i + 1,
                p.arch
            );
        }
    }

    let start_first = parts[0].start_layer;
    let end_last = parts[parts.len() - 1].end_layer;
    let total = end_last - start_first + 1;

    // ── Header: the first slice's KVs, with the total and the metadata fixed ──
    let mut kv_out: Vec<KvOut> = Vec::new();
    for (key, entry) in &parsed[0].kv {
        if key.starts_with("rdmap2p.") || key.ends_with(".block_count") {
            continue; // re-added below
        }
        kv_out.push(entry.clone());
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &format!("{arch}.block_count"),
        total,
    )));
    if let Some(orig) = parsed.iter().filter_map(|p| p.original_block_count).max() {
        kv_out.push(KvOut::Encoded(encode_kv_u32(
            "rdmap2p.original_block_count",
            orig,
        )));
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        "rdmap2p.start_layer",
        start_first,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32("rdmap2p.end_layer", end_last)));

    // ── Tensors, one per physical file so the data streams from where it is ──
    struct Out {
        file: usize,
        name: String,
        dims: Vec<u64>,
        dtype: u32,
        src_offset: u64,
        byte_size: u64,
    }
    let mut out: Vec<Out> = Vec::new();
    let mut layers_before = 0u32;
    for (pi, part) in parts.iter().enumerate() {
        let p = &parsed[pi];
        let is_last = pi == parts.len() - 1;
        for t in &p.tensors {
            let is_blk = t.name.starts_with("blk.");
            let is_output = matches!(
                t.name.as_str(),
                "output_norm.weight" | "output_norm.bias" | "output.weight"
            );
            // Blocks come from every slice; the head tensors (token embeddings,
            // position embeddings, rope) only from the first; the head outputs
            // only from the last — the middle slices carry a fake `output_norm`
            // that would shadow the real one.
            let keep = is_blk || (is_output && is_last) || (pi == 0 && !is_output);
            if !keep {
                continue;
            }
            if t.byte_size == 0 {
                bail!(
                    "tamanho desconhecido para o tensor '{}' (tipo ggml {}): juntar \
                     produziria um arquivo corrompido",
                    t.name,
                    t.dtype
                );
            }
            out.push(Out {
                file: pi,
                name: if is_blk {
                    remap_merged(&t.name, layers_before)
                } else {
                    t.name.clone()
                },
                dims: t.dims.clone(),
                dtype: t.dtype,
                src_offset: t.src_offset,
                byte_size: t.byte_size,
            });
        }
        layers_before += part.end_layer - part.start_layer + 1;
    }

    if out.is_empty() {
        bail!("nenhum tensor para juntar");
    }

    // ── Layout and write ────────────────────────────────────────────────────
    let new_offsets: Vec<u64> = {
        let mut v = Vec::with_capacity(out.len());
        let mut cur: u64 = 0;
        for t in &out {
            v.push(cur);
            cur = align_up(cur + t.byte_size);
        }
        v
    };
    let kv_bytes: usize = kv_out
        .iter()
        .map(|e| match e {
            KvOut::Raw(s, e) => e - s,
            KvOut::Encoded(v) => v.len(),
        })
        .sum();
    let tensor_header_bytes: usize = out
        .iter()
        .map(|t| tensor_header_size(&t.name, &t.dims))
        .sum();
    let meta_size = 4 + 4 + 8 + 8 + kv_bytes + tensor_header_bytes;
    let data_offset = align_up(meta_size as u64) as usize;

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("criar diretório {}", parent.display()))?;
    }
    let dst_file = File::create(dst).with_context(|| format!("criar {}", dst.display()))?;
    let mut w = BufWriter::with_capacity(CHUNK, dst_file);

    w.write_all(&GGUF_MAGIC.to_le_bytes())?;
    w.write_all(&parsed[0].version.to_le_bytes())?;
    w.write_all(&(out.len() as u64).to_le_bytes())?;
    w.write_all(&(kv_out.len() as u64).to_le_bytes())?;

    for entry in &kv_out {
        match entry {
            KvOut::Raw(s, e) => w.write_all(&mmaps[0][*s..*e])?,
            KvOut::Encoded(v) => w.write_all(v)?,
        }
    }

    for (t, &off) in out.iter().zip(&new_offsets) {
        let nb = t.name.as_bytes();
        w.write_all(&(nb.len() as u64).to_le_bytes())?;
        w.write_all(nb)?;
        w.write_all(&(t.dims.len() as u32).to_le_bytes())?;
        for &d in &t.dims {
            w.write_all(&d.to_le_bytes())?;
        }
        w.write_all(&t.dtype.to_le_bytes())?;
        w.write_all(&off.to_le_bytes())?;
    }

    let pad = data_offset - meta_size;
    w.write_all(&vec![0u8; pad])?;

    let total_bytes: u64 = out.iter().map(|t| t.byte_size).sum();
    let mut done: u64 = 0;
    for t in &out {
        let start = parsed[t.file].data_start + t.src_offset as usize;
        let bytes = &mmaps[t.file][start..start + t.byte_size as usize];
        let mut pos = 0usize;
        while pos < bytes.len() {
            let n = CHUNK.min(bytes.len() - pos);
            w.write_all(&bytes[pos..pos + n])?;
            pos += n;
            done += n as u64;
            if total_bytes > 0 {
                on_progress(100.0 * done as f64 / total_bytes as f64);
            }
        }
        let aligned = align_up(t.byte_size) as usize;
        let pad = aligned - t.byte_size as usize;
        if pad > 0 {
            w.write_all(&vec![0u8; pad])?;
        }
    }

    w.flush()?;

    let final_size = std::fs::metadata(dst)
        .with_context(|| format!("stat {}", dst.display()))?
        .len();

    info!(
        "fatias juntadas: {} ({:.0} MB)",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0)
    );

    Ok(final_size)
}

#[cfg(test)]
/// A minimal but structurally valid llama GGUF with `n_blocks` layers.
///
/// Shared with `webui`'s tests so the merge execution path is exercised with
/// real files, not mocks.
#[cfg(test)]
pub(crate) fn test_gguf_bytes(n_blocks: u32) -> Vec<u8> {
    let n = 4u64;
    let f32v = |vals: &[f32]| -> Vec<u8> { vals.iter().flat_map(|v| v.to_le_bytes()).collect() };
    let mut tensors: Vec<(String, Vec<u64>, u32, Vec<u8>)> = Vec::new();
    tensors.push((
        "token_embd.weight".into(),
        vec![n, n],
        T_F32,
        f32v(&[0.5; 16]),
    ));
    for i in 0..n_blocks {
        let base = i as f32 * 10.0;
        let data: Vec<f32> = (0..16).map(|j| base + j as f32).collect();
        tensors.push((
            format!("blk.{i}.attn_q.weight"),
            vec![n, n],
            T_F32,
            f32v(&data),
        ));
    }
    tensors.push(("output_norm.weight".into(), vec![n], T_F32, f32v(&[2.0; 4])));
    tensors.push(("output.weight".into(), vec![n, n], T_F32, f32v(&[3.0; 16])));

    let s = |b: &str| -> Vec<u8> {
        let mut v = (b.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(b.as_bytes());
        v
    };
    let str_kv = |key: &str, val: &str| {
        let mut v = s(key);
        v.extend_from_slice(&T_STR.to_le_bytes());
        v.extend_from_slice(&s(val));
        v
    };
    let u32_kv = |key: &str, val: u32| {
        let mut v = s(key);
        v.extend_from_slice(&T_U32.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
        v
    };

    let kv: Vec<Vec<u8>> = vec![
        str_kv("general.architecture", "llama"),
        u32_kv("llama.embedding_length", n as u32),
        u32_kv("llama.block_count", n_blocks),
        u32_kv("llama.vocab_size", n as u32),
        u32_kv("rdmap2p.original_block_count", n_blocks),
    ];

    let mut header = Vec::new();
    header.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
    header.extend_from_slice(&3u32.to_le_bytes());
    header.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
    header.extend_from_slice(&(kv.len() as u64).to_le_bytes());
    for b in &kv {
        header.extend_from_slice(b);
    }

    let mut cur = 0u64;
    let offsets: Vec<u64> = tensors
        .iter()
        .map(|(_, _, _, bytes)| {
            let off = cur;
            cur = align_up(cur + bytes.len() as u64);
            off
        })
        .collect();
    for ((name, dims, dtype, _), off) in tensors.iter().zip(&offsets) {
        header.extend_from_slice(&s(name));
        header.extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for d in dims {
            header.extend_from_slice(&d.to_le_bytes());
        }
        header.extend_from_slice(&dtype.to_le_bytes());
        header.extend_from_slice(&off.to_le_bytes());
    }
    let pad = align_up(header.len() as u64) as usize - header.len();
    header.extend_from_slice(&vec![0u8; pad]);

    for (_, _, _, bytes) in &tensors {
        header.extend_from_slice(bytes);
        let aligned = align_up(bytes.len() as u64) as usize;
        header.extend_from_slice(&vec![0u8; aligned - bytes.len()]);
    }
    header
}

#[cfg(test)]
/// A minimal structurally valid qwen35 GGUF with `n_blocks` layers and a
/// `nextn` (MTP) block only on the last layer, mirroring the real model: the
/// predictor lives at the very end, never in the middle.
fn qwen35_test_gguf_bytes(n_blocks: u32) -> Vec<u8> {
    let n = 4u64;
    let f32v = |vals: &[f32]| -> Vec<u8> { vals.iter().flat_map(|v| v.to_le_bytes()).collect() };
    let mut tensors: Vec<(String, Vec<u64>, u32, Vec<u8>)> = Vec::new();
    tensors.push((
        "token_embd.weight".into(),
        vec![n, n],
        T_F32,
        f32v(&[0.5; 16]),
    ));
    for i in 0..n_blocks {
        let base = i as f32 * 10.0;
        let data: Vec<f32> = (0..16).map(|j| base + j as f32).collect();
        tensors.push((
            format!("blk.{i}.attn_q.weight"),
            vec![n, n],
            T_F32,
            f32v(&data),
        ));
    }
    let last = n_blocks - 1;
    tensors.push((
        format!("blk.{last}.nextn.eh_proj.weight"),
        vec![n, n],
        T_F32,
        f32v(&[4.0; 16]),
    ));
    tensors.push((
        format!("blk.{last}.nextn.enorm.weight"),
        vec![n],
        T_F32,
        f32v(&[5.0; 4]),
    ));
    tensors.push((
        format!("blk.{last}.nextn.hnorm.weight"),
        vec![n],
        T_F32,
        f32v(&[6.0; 4]),
    ));
    tensors.push(("output_norm.weight".into(), vec![n], T_F32, f32v(&[2.0; 4])));
    tensors.push(("output.weight".into(), vec![n, n], T_F32, f32v(&[3.0; 16])));

    let s = |b: &str| -> Vec<u8> {
        let mut v = (b.len() as u64).to_le_bytes().to_vec();
        v.extend_from_slice(b.as_bytes());
        v
    };
    let str_kv = |key: &str, val: &str| {
        let mut v = s(key);
        v.extend_from_slice(&T_STR.to_le_bytes());
        v.extend_from_slice(&s(val));
        v
    };
    let u32_kv = |key: &str, val: u32| {
        let mut v = s(key);
        v.extend_from_slice(&T_U32.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
        v
    };

    let kv: Vec<Vec<u8>> = vec![
        str_kv("general.architecture", "qwen35"),
        u32_kv("qwen35.embedding_length", n as u32),
        u32_kv("qwen35.block_count", n_blocks),
        u32_kv("qwen35.vocab_size", n as u32),
        u32_kv("qwen35.nextn_predict_layers", 1),
        u32_kv("qwen35.full_attention_interval", 4),
        u32_kv("rdmap2p.original_block_count", n_blocks),
    ];

    let mut header = Vec::new();
    header.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
    header.extend_from_slice(&3u32.to_le_bytes());
    header.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
    header.extend_from_slice(&(kv.len() as u64).to_le_bytes());
    for b in &kv {
        header.extend_from_slice(b);
    }

    let mut cur = 0u64;
    let offsets: Vec<u64> = tensors
        .iter()
        .map(|(_, _, _, bytes)| {
            let off = cur;
            cur = align_up(cur + bytes.len() as u64);
            off
        })
        .collect();
    for ((name, dims, dtype, _), off) in tensors.iter().zip(&offsets) {
        header.extend_from_slice(&s(name));
        header.extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for d in dims {
            header.extend_from_slice(&d.to_le_bytes());
        }
        header.extend_from_slice(&dtype.to_le_bytes());
        header.extend_from_slice(&off.to_le_bytes());
    }
    let pad = align_up(header.len() as u64) as usize - header.len();
    header.extend_from_slice(&vec![0u8; pad]);

    for (_, _, _, bytes) in &tensors {
        header.extend_from_slice(bytes);
        let aligned = align_up(bytes.len() as u64) as usize;
        header.extend_from_slice(&vec![0u8; aligned - bytes.len()]);
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv_u32(mm: &[u8], want: &str) -> Option<u32> {
        let mut r = R { data: mm, pos: 0 };
        let magic = r.u32().ok()?;
        if magic != GGUF_MAGIC {
            return None;
        }
        r.u32().ok()?; // version
        let _n_tensors = r.u64().ok()?;
        let n_kv = r.u64().ok()?;
        for _ in 0..n_kv {
            let key = r.string().ok()?;
            let vtype = r.u32().ok()?;
            if key == want {
                return match vtype {
                    T_U32 => Some(u32::from_le_bytes(r.take(4).ok()?.try_into().unwrap())),
                    T_U64 => Some(r.u64().ok()? as u32),
                    _ => None,
                };
            }
            r.skip_value(vtype).ok()?;
        }
        None
    }

    fn tensor_data<'a>(mm: &'a [u8], p: &MergedSrc, name: &str) -> &'a [u8] {
        let t = p
            .tensors
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tensor {name} não encontrado"));
        &mm[p.data_start + t.src_offset as usize
            ..p.data_start + t.src_offset as usize + t.byte_size as usize]
    }

    /// Splitting 0-1 and 2-3 off one model and joining them back has to give
    /// the original layers back their real numbers and their real bytes — the
    /// only property a pipeline hop actually depends on.
    #[test]
    fn merged_slices_round_trip_into_a_whole() {
        let dir = std::env::temp_dir().join(format!("rdmap2p-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(4)).unwrap();

        let a = dir.join("a.gguf");
        let b = dir.join("b.gguf");
        split(&whole, &a, 0, 1, true, false, |_| {}).unwrap();
        split(&whole, &b, 2, 3, false, true, |_| {}).unwrap();

        let merged = dir.join("merged.gguf");
        let size = merge(
            &[
                MergePart {
                    path: a.clone(),
                    start_layer: 0,
                    end_layer: 1,
                },
                MergePart {
                    path: b.clone(),
                    start_layer: 2,
                    end_layer: 3,
                },
            ],
            &merged,
            |_| {},
        )
        .unwrap();
        assert!(size > 0);

        let file = File::open(&merged).unwrap();
        let mm = unsafe { Mmap::map(&file).unwrap() };
        let p = parse_merged_src(&mm).unwrap();

        assert_eq!(p.arch, "llama");
        assert_eq!(
            kv_u32(&mm, "llama.block_count"),
            Some(4),
            "o total é a união das duas faixas"
        );
        assert_eq!(p.original_block_count, Some(4));

        let names: Vec<&str> = p.tensors.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "token_embd.weight",
                "blk.0.attn_q.weight",
                "blk.1.attn_q.weight",
                "blk.2.attn_q.weight",
                "blk.3.attn_q.weight",
                "output_norm.weight",
                "output.weight",
            ],
            "cada bloco volta ao seu lugar e a duplicata some"
        );

        // The second slice's blocks carry their own data, not the first's:
        // blk.2 in the join has to be the original layer 2 (values 20.0..).
        let whole_bytes = std::fs::read(&whole).unwrap();
        let whole_mm: &[u8] = &whole_bytes;
        let whole_p = parse_merged_src(whole_mm).unwrap();
        assert_eq!(
            tensor_data(&mm, &p, "blk.2.attn_q.weight"),
            tensor_data(whole_mm, &whole_p, "blk.2.attn_q.weight")
        );
        assert_eq!(
            tensor_data(&mm, &p, "token_embd.weight"),
            tensor_data(whole_mm, &whole_p, "token_embd.weight")
        );
        assert_eq!(
            tensor_data(&mm, &p, "output.weight"),
            tensor_data(whole_mm, &whole_p, "output.weight")
        );
    }

    #[test]
    fn merging_non_adjacent_slices_is_refused() {
        let dir = std::env::temp_dir().join(format!("rdmap2p-merge-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(6)).unwrap();
        let a = dir.join("a.gguf");
        let b = dir.join("b.gguf");
        split(&whole, &a, 0, 1, true, false, |_| {}).unwrap();
        split(&whole, &b, 4, 5, false, true, |_| {}).unwrap();

        let err = merge(
            &[
                MergePart {
                    path: a.clone(),
                    start_layer: 0,
                    end_layer: 1,
                },
                MergePart {
                    path: b.clone(),
                    start_layer: 4,
                    end_layer: 5,
                },
            ],
            &dir.join("bad.gguf"),
            |_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("não são vizinhas"), "{err}");
    }

    #[test]
    fn nextn_predict_layers_zeroed_on_non_last_slices() {
        let dir = std::env::temp_dir().join(format!("rdmap2p-nextn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, qwen35_test_gguf_bytes(4)).unwrap();

        let mid = dir.join("mid.gguf");
        split(&whole, &mid, 0, 2, true, false, |_| {}).unwrap();
        let last = dir.join("last.gguf");
        split(&whole, &last, 3, 3, false, true, |_| {}).unwrap();

        assert_eq!(
            kv_u32(&std::fs::read(&mid).unwrap(), "qwen35.nextn_predict_layers"),
            Some(0),
            "fatia intermediária não tem o bloco nextn (MTP): mantê-lo em 1 faria \
             o llama.cpp exigir 'blk.2.nextn.eh_proj.weight' e abortar o load"
        );
        assert_eq!(
            kv_u32(
                &std::fs::read(&last).unwrap(),
                "qwen35.nextn_predict_layers"
            ),
            Some(1),
            "a fatia final carrega o bloco nextn e preserva o valor original"
        );
    }

    /// A fatia 0-23 do Qwen3.8-27B-Q3_K_S, com a forma exata do modelo que
    /// falhou: 65 blocos, `qwen35.nextn_predict_layers = 1` e o bloco `nextn`
    /// só em `blk.64`. Copiar essa marca para a fatia 0-23 faz o llama.cpp
    /// derivar `blk.23.nextn.eh_proj.weight` do hiperparâmetro e abortar o
    /// load com "missing tensor" — foi o que aconteceu na salem-gpu em
    /// 2026-08-14, enquanto a 24-64 (`block_count = 41`, último bloco = o 64
    /// original) carregava na ksia.
    ///
    /// O teste menor acima prova a mesma regra; este prova que ela vale nos
    /// números reais, que é o que o defeito relatado citava: fatia não-final
    /// declara zero e não leva tensor `nextn` nenhum; a final declara 1 e leva
    /// o bloco, renumerado para `blk.40.*`.
    #[test]
    fn the_qwen38_27b_slice_0_23_carries_no_nextn_block_nor_its_hyperparameter() {
        let dir = std::env::temp_dir().join(format!("rdmap2p-nextn-27b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("qwen38-27b.gguf");
        std::fs::write(&whole, qwen35_test_gguf_bytes(65)).unwrap();

        let head = dir.join("qwen38_0_23.gguf");
        split(&whole, &head, 0, 23, true, false, |_| {}).unwrap();
        let tail = dir.join("qwen38_24_64.gguf");
        split(&whole, &tail, 24, 64, false, true, |_| {}).unwrap();

        let head_bytes = std::fs::read(&head).unwrap();
        let tail_bytes = std::fs::read(&tail).unwrap();

        assert_eq!(
            kv_u32(&head_bytes, "qwen35.nextn_predict_layers"),
            Some(0),
            "a fatia 0-23 não tem o bloco nextn: declarar 1 faz o llama.cpp \
             exigir 'blk.23.nextn.eh_proj.weight' e abortar o load"
        );
        assert_eq!(kv_u32(&head_bytes, "qwen35.block_count"), Some(24));
        let head_src = parse_merged_src(&head_bytes).unwrap();
        let head_nextn: Vec<&str> = head_src
            .tensors
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| n.contains(".nextn."))
            .collect();
        assert!(
            head_nextn.is_empty(),
            "a fatia 0-23 não deveria levar tensor nextn nenhum, levou {head_nextn:?}"
        );

        assert_eq!(
            kv_u32(&tail_bytes, "qwen35.nextn_predict_layers"),
            Some(1),
            "a fatia final carrega o bloco nextn de verdade e preserva a marca"
        );
        assert_eq!(kv_u32(&tail_bytes, "qwen35.block_count"), Some(41));
        let tail_src = parse_merged_src(&tail_bytes).unwrap();
        let tail_nextn: Vec<&str> = tail_src
            .tensors
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| n.contains(".nextn."))
            .collect();
        assert!(
            tail_nextn.contains(&"blk.40.nextn.eh_proj.weight"),
            "o bloco nextn do blk.64 original tem que virar blk.40 na fatia 24-64, veio {tail_nextn:?}"
        );
    }
}
