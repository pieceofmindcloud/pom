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
            T_U8 | T_I8 | T_BOOL => { self.take(1)?; }
            T_U16 | T_I16 => { self.take(2)?; }
            T_U32 | T_I32 | T_F32 => { self.take(4)?; }
            T_U64 | T_I64 | T_F64 => { self.take(8)?; }
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
    Fake { name: String, dims: Vec<u64>, dtype: u32, data: Vec<u8> },
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
    + 8                     // offset(u64)
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
    let src_mm = unsafe { Mmap::map(&src_file) }
        .with_context(|| format!("mmap {}", src.display()))?;
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
                _ => { r.skip_value(vtype)?; hidden_dim }
            };
            kv_out.push(KvOut::Raw(kv_start, r.pos));
        } else if key == "rdmap2p.original_block_count" {
            original_block_count = match vtype {
                T_U32 => Some(u32::from_le_bytes(r.take(4)?.try_into().unwrap())),
                _ => { r.skip_value(vtype)?; None }
            };
            // Drop — re-added below with updated values
        } else if key.starts_with("rdmap2p.") {
            r.skip_value(vtype)?;
            // Drop all rdmap2p.* — re-added below
        } else if key.ends_with(".block_count") {
            // Record original block count from the source file's header
            let source_bc = match vtype {
                T_U32 => Some(u32::from_le_bytes(r.take(4)?.try_into().unwrap())),
                T_U64 => Some(r.u64()? as u32),
                _ => { r.skip_value(vtype)?; None }
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
