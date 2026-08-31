//! GGUF model splitter — Rust replacement for `split_model.py`.
//!
//! Reads the source model with mmap (never loads weights into RAM), selects
//! tensors for the assigned layer range, remaps `blk.(start+i).*` names to
//! `blk.i.*`, injects a fake identity `output_norm.weight` on non-last slices,
//! updates KV metadata, and streams the slice to disk. Architecture-specific
//! output contracts are checked before the destination is created: required
//! head tensors are copied from the source into every slice that needs them,
//! including non-final slices.
//!
//! Progress is reported via `on_progress(0.0..=100.0)` as bytes are written.

use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use tracing::info;

/// O que um fatiamento deixou em disco.
///
/// O `sha256` sai do mesmo passe que grava os bytes (ver
/// `pom::HashingWriter`), e não de uma segunda leitura: uma fatia tem dezenas
/// de GB, e reler tudo só para hashear dobrava o tempo do corte. É este hash
/// que vira o **nome** do arquivo e a identidade da parte no `.pom`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenSlice {
    pub bytes: u64,
    pub sha256: String,
}

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

    fn skip_array_payload(&mut self, element_type: u32, count: u64) -> Result<(usize, usize)> {
        let start = self.pos;
        if let Some(size) = scalar_size(element_type) {
            let bytes = count
                .checked_mul(size as u64)
                .ok_or_else(|| anyhow::anyhow!("overflow no array GGUF"))?;
            self.take(
                usize::try_from(bytes).map_err(|_| anyhow::anyhow!("array GGUF grande demais"))?,
            )?;
        } else {
            for _ in 0..count {
                self.skip_value(element_type)?;
            }
        }
        Ok((start, self.pos))
    }
}

// ── Data structures ───────────────────────────────────────────────────────────

struct TensorRecord {
    name: String,
    dims: Vec<u64>,
    dtype: u32,
    part_index: usize,
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

#[derive(Clone, Copy)]
struct ArraySource {
    element_type: u32,
    count: u64,
    payload_start: usize,
    payload_end: usize,
}

struct KvSource {
    start: usize,
    end: usize,
    key: String,
    vtype: u32,
    value_start: usize,
    array: Option<ArraySource>,
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

fn encode_kv_u32_array(key: &str, values: &[u32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + key.len() + 4 + 4 + 8 + values.len() * 4);
    let kb = key.as_bytes();
    buf.extend_from_slice(&(kb.len() as u64).to_le_bytes());
    buf.extend_from_slice(kb);
    buf.extend_from_slice(&T_ARR.to_le_bytes());
    buf.extend_from_slice(&T_U32.to_le_bytes());
    buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for &v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

fn source_scalar_u32(mm: &[u8], entry: &KvSource) -> Option<u32> {
    match entry.vtype {
        T_U8 => mm.get(entry.value_start).copied().map(u32::from),
        T_I8 => mm
            .get(entry.value_start)
            .copied()
            .map(|value| i8::from_le_bytes([value]))
            .and_then(|value| u32::try_from(value).ok()),
        T_U16 => mm
            .get(entry.value_start..entry.value_start + 2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_le_bytes)
            .map(u32::from),
        T_I16 => mm
            .get(entry.value_start..entry.value_start + 2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(i16::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_U32 => mm
            .get(entry.value_start..entry.value_start + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes),
        T_I32 => mm
            .get(entry.value_start..entry.value_start + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(i32::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_U64 => mm
            .get(entry.value_start..entry.value_start + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_I64 => mm
            .get(entry.value_start..entry.value_start + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(i64::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        _ => None,
    }
}

fn source_string(mm: &[u8], entry: &KvSource) -> Option<String> {
    if entry.vtype != T_STR || entry.value_start + 8 > mm.len() {
        return None;
    }
    let length = u64::from_le_bytes(
        mm[entry.value_start..entry.value_start + 8]
            .try_into()
            .ok()?,
    ) as usize;
    let start = entry.value_start + 8;
    let bytes = mm.get(start..start.checked_add(length)?)?;
    Some(std::str::from_utf8(bytes).ok()?.to_owned())
}

fn encode_array_slice(mm: &[u8], entry: &KvSource, start: u32, count: u32) -> Result<Vec<u8>> {
    let array = entry
        .array
        .ok_or_else(|| anyhow::anyhow!("array GGUF ausente para '{}'", entry.key))?;
    let element_size = scalar_size(array.element_type).ok_or_else(|| {
        anyhow::anyhow!(
            "array GGUF '{}' tem elementos aninhados; comprimento {} não pode ser fatiado",
            entry.key,
            array.count
        )
    })?;
    let start = usize::try_from(start).map_err(|_| anyhow::anyhow!("índice de array inválido"))?;
    let count = usize::try_from(count).map_err(|_| anyhow::anyhow!("array GGUF grande demais"))?;
    let byte_start = start
        .checked_mul(element_size)
        .and_then(|offset| array.payload_start.checked_add(offset))
        .ok_or_else(|| anyhow::anyhow!("overflow no array '{}'", entry.key))?;
    let byte_len = count
        .checked_mul(element_size)
        .ok_or_else(|| anyhow::anyhow!("overflow no array '{}'", entry.key))?;
    let byte_end = byte_start
        .checked_add(byte_len)
        .ok_or_else(|| anyhow::anyhow!("overflow no array '{}'", entry.key))?;
    if byte_end > array.payload_end {
        bail!("array '{}' truncado", entry.key);
    }
    let source = mm
        .get(byte_start..byte_end)
        .ok_or_else(|| anyhow::anyhow!("array '{}' truncado", entry.key))?;

    let key = entry.key.as_bytes();
    let mut encoded = Vec::with_capacity(8 + key.len() + 4 + 4 + 8 + source.len());
    encoded.extend_from_slice(&(key.len() as u64).to_le_bytes());
    encoded.extend_from_slice(key);
    encoded.extend_from_slice(&T_ARR.to_le_bytes());
    encoded.extend_from_slice(&array.element_type.to_le_bytes());
    encoded.extend_from_slice(&(count as u64).to_le_bytes());
    encoded.extend_from_slice(source);
    Ok(encoded)
}

fn layer_slice_bounds(
    array_count: u64,
    original_block_count: u32,
    start_layer: u32,
    layer_count: u32,
) -> Result<(usize, usize)> {
    let end_layer = start_layer
        .checked_add(layer_count)
        .ok_or_else(|| anyhow::anyhow!("overflow no intervalo de camadas"))?;
    if start_layer >= original_block_count || end_layer > original_block_count {
        bail!(
            "intervalo de camadas {}..{} fora da contagem original {}",
            start_layer,
            end_layer.saturating_sub(1),
            original_block_count
        );
    }
    if array_count < original_block_count as u64 {
        bail!(
            "array por camada tem comprimento {array_count}; esperado pelo menos {original_block_count}"
        );
    }
    Ok((start_layer as usize, end_layer as usize))
}

// Helper de teste que preserva a prova da seleção de valores por camada; o
// caminho de produção usa `layer_slice_bounds` diretamente ao serializar.
#[cfg(test)]
fn slice_layer_values<T: Clone>(
    values: &[T],
    original_block_count: u32,
    start_layer: u32,
    layer_count: u32,
) -> Result<Vec<T>> {
    let (start, end) = layer_slice_bounds(
        values.len() as u64,
        original_block_count,
        start_layer,
        layer_count,
    )?;
    Ok(values[start..end].to_vec())
}

fn is_global_array(key: &str, arch: &str) -> bool {
    !key.starts_with(&format!("{arch}.")) || key.contains(".rope.")
}

/// The absolute layer index of a `blk.N.ssm_conv1d.weight` tensor, if `name`
/// is one. Hybrid attention/SSM models (Qwen3-Next-style) mark which layers
/// are recurrent either via an explicit `<arch>.attention.recurrent_layers`
/// array or, absent that, via a pattern formula keyed to the *absolute*
/// layer index (llama.cpp's `set_recr_pattern`/`is_recr_impl`). A slice's
/// `blk.(start+i)` -> `blk.i` renumbering silently breaks that formula for
/// any `start_layer > 0`, so llama.cpp ends up expecting `ssm_conv1d` on the
/// wrong local layers and fails to load with "missing tensor". Presence of
/// this tensor in the source file is ground truth for which layers actually
/// are recurrent, independent of the formula or the arch's name.
fn ssm_conv1d_layer_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("blk.")?;
    let dot = rest.find('.')?;
    let idx: u32 = rest[..dot].parse().ok()?;
    (&rest[dot..] == ".ssm_conv1d.weight").then_some(idx)
}

fn tensor_header_size(name: &str, dims: &[u64]) -> usize {
    8 + name.len()          // len(u64) + bytes
    + 4                     // n_dims(u32)
    + 8 * dims.len()        // dims(u64 each)
    + 4                     // dtype(u32)
    + 8 // offset(u64)
}

fn want_tensor(name: &str, architecture: &str, start: u32, end: u32, is_last: bool) -> bool {
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
        _ if crate::gguf::is_required_non_final_tensor(architecture, name) => true,
        _ if crate::gguf::is_output_tensor(architecture, name) => is_last,
        n if n.contains("rope") || n.contains("pos_embd") => true,
        _ => false,
    }
}

/// GGUF tensor-table order is not a storage-order contract. Keep payload page
/// faults monotonic within each multipart source, which matters when mmap is
/// larger than RAM and the kernel has to evict old pages.
fn source_read_order(tensors: &[TensorRecord]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..tensors.len()).collect();
    order.sort_unstable_by_key(|&index| (tensors[index].part_index, tensors[index].src_offset));
    order
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

/// Validate the source tensors for one prospective slice without writing it.
///
/// This is used before reusing a path from an older manifest, so an existing
/// invalid slice cannot bypass the current architecture contract.
pub fn validate_slice_source(
    src: &Path,
    start_layer: u32,
    end_layer: u32,
    is_last: bool,
) -> Result<()> {
    let architecture = crate::gguf::read_info(src)?
        .architecture
        .unwrap_or_else(|| "llama".to_string());
    let source = crate::multipart::GgufSource::open(src)?;
    let tensor_names: HashSet<&str> = source
        .tensors()
        .iter()
        .map(|tensor| tensor.name.as_str())
        .collect();
    crate::gguf::validate_slice_tensors(&architecture, &tensor_names, is_last)?;
    if start_layer > end_layer {
        bail!("intervalo de camadas inválido L{start_layer}-{end_layer}");
    }
    Ok(())
}

/// Slice a GGUF model file.
///
/// Produces a valid standalone GGUF at `dst` covering layers
/// `start_layer..=end_layer` of the source `src`. `on_progress` receives
/// values in 0.0..=100.0 as tensor data is streamed to disk.
///
/// Returns the size and the sha256 of the output file.
pub fn split(
    src: &Path,
    dst: &Path,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
    on_progress: impl Fn(f64),
) -> Result<WrittenSlice> {
    let _ = is_first; // reserved; currently unused (no tensors differ on first vs. middle)
    let n_split = end_layer - start_layer + 1;

    info!(
        "✂️  fatiando camadas {start_layer}–{end_layer} de {}",
        src.display()
    );

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF sem partes"))?
        .map();

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
    // and strip/replace pom.* entries.

    let mut entries = Vec::with_capacity(n_kv);
    for _ in 0..n_kv {
        let start = r.pos;
        let key = r.string()?;
        let vtype = r.u32()?;
        let value_start = r.pos;
        let array = if vtype == T_ARR {
            let element_type = r.u32()?;
            let count = r.u64()?;
            let (payload_start, payload_end) = r.skip_array_payload(element_type, count)?;
            Some(ArraySource {
                element_type,
                count,
                payload_start,
                payload_end,
            })
        } else {
            r.skip_value(vtype)?;
            None
        };
        entries.push(KvSource {
            start,
            end: r.pos,
            key,
            vtype,
            value_start,
            array,
        });
    }

    let arch = entries
        .iter()
        .find(|entry| entry.key == "general.architecture")
        .and_then(|entry| source_string(mm, entry))
        .unwrap_or_else(|| "llama".to_string());
    let hidden_dim = entries
        .iter()
        .find(|entry| entry.key.ends_with(".embedding_length"))
        .and_then(|entry| source_scalar_u32(mm, entry))
        .unwrap_or(4096);
    let original_block_count = entries
        .iter()
        .find(|entry| entry.key.ends_with(".block_count"))
        .and_then(|entry| source_scalar_u32(mm, entry));

    let tensor_names: HashSet<&str> = source
        .tensors()
        .iter()
        .map(|tensor| tensor.name.as_str())
        .collect();
    crate::gguf::validate_slice_tensors(&arch, &tensor_names, is_last)?;

    let mut kv_out: Vec<KvOut> = Vec::with_capacity(n_kv + 4);
    let mut block_count_updated = false;
    let pom_prefix = format!("{}.", crate::brand::slug());
    for entry in &entries {
        let key = &entry.key;
        if key.starts_with("split.")
            || *key == crate::brand::gguf_key("original_block_count")
            || key.starts_with(&pom_prefix)
        {
            continue;
        }
        match crate::gguf::layer_count_semantics(key) {
            Some(crate::gguf::LayerCountSemantics::FromStart) => {
                let original = source_scalar_u32(mm, entry).ok_or_else(|| {
                    anyhow::anyhow!(
                        "metadata '{key}' conta camadas desde o início, mas não é um inteiro escalar não negativo que caiba em u32"
                    )
                })?;
                let local = original.saturating_sub(start_layer).min(n_split);
                kv_out.push(KvOut::Encoded(encode_kv_u32(key, local)));
                continue;
            }
            Some(crate::gguf::LayerCountSemantics::FromEnd) => {
                kv_out.push(if is_last {
                    KvOut::Raw(entry.start, entry.end)
                } else {
                    KvOut::Encoded(encode_kv_u32(key, 0))
                });
                continue;
            }
            None => {}
        }
        if key.ends_with(".block_count") {
            kv_out.push(KvOut::Encoded(encode_kv_u32(key, n_split)));
            block_count_updated = true;
            continue;
        }
        if key.ends_with(".attention.recurrent_layers") {
            // Recomputed below from the actual ssm_conv1d tensors.
            continue;
        }
        if let Some(array) = entry.array {
            // Only architecture-namespaced arrays are candidates for per-layer
            // slicing. Global arrays such as tokenizer.ggml.tokens must remain
            // byte-for-byte intact, even when their length exceeds block_count.
            if !is_global_array(key, &arch) {
                let original = original_block_count.ok_or_else(|| {
                    anyhow::anyhow!(
                        "array metadata '{}' tem comprimento {}, mas o GGUF não declara a contagem original de blocos",
                        key,
                        array.count
                    )
                })?;
                if array.count >= original as u64 {
                    let (slice_start, slice_end) =
                        layer_slice_bounds(array.count, original, start_layer, n_split)?;
                    let sliced = encode_array_slice(
                        mm,
                        entry,
                        slice_start as u32,
                        (slice_end - slice_start) as u32,
                    )?;
                    kv_out.push(KvOut::Encoded(sliced));
                    continue;
                }
                if array.count != 1 {
                    bail!(
                        "array metadata '{}' tem comprimento {}; esperado 1 ou pelo menos {} (contagem original de blocos)",
                        key,
                        array.count,
                        original
                    );
                }
            }
        }
        kv_out.push(KvOut::Raw(entry.start, entry.end));
    }

    if !block_count_updated {
        kv_out.push(KvOut::Encoded(encode_kv_u32(
            &format!("{arch}.block_count"),
            n_split,
        )));
    }
    if let Some(orig) = original_block_count {
        kv_out.push(KvOut::Encoded(encode_kv_u32(
            &crate::brand::gguf_key("original_block_count"),
            orig,
        )));
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("start_layer"),
        start_layer,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("end_layer"),
        end_layer,
    )));

    // ── Parse tensor table ────────────────────────────────────────────────────

    let all_tensors: Vec<TensorRecord> = source
        .tensors()
        .iter()
        .map(|tensor| TensorRecord {
            name: tensor.name.clone(),
            dims: tensor.dims.clone(),
            dtype: tensor.dtype,
            part_index: tensor.part_index,
            src_offset: tensor.offset,
            byte_size: tensor.byte_size,
        })
        .collect();

    // A hybrid model's recurrent layers, keyed by their *absolute* index in
    // the source file (see `ssm_conv1d_layer_index`). Empty for every plain
    // transformer — the common case — so this is a no-op there.
    let ssm_layers: HashSet<u32> = all_tensors
        .iter()
        .filter_map(|t| ssm_conv1d_layer_index(&t.name))
        .collect();
    if !ssm_layers.is_empty() {
        let flags: Vec<u32> = (start_layer..=end_layer)
            .map(|i| ssm_layers.contains(&i) as u32)
            .collect();
        kv_out.push(KvOut::Encoded(encode_kv_u32_array(
            &format!("{arch}.attention.recurrent_layers"),
            &flags,
        )));
    }

    // ── Select tensors ────────────────────────────────────────────────────────

    let selected: Vec<&TensorRecord> = source_read_order(&all_tensors)
        .into_iter()
        .map(|index| &all_tensors[index])
        .filter(|t| want_tensor(&t.name, &arch, start_layer, end_layer, is_last))
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
                part_index: t.part_index,
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
    // O hash por fora do buffer: assim ele vê exatamente a sequência de bytes
    // que o `BufWriter` entrega ao arquivo, uma vez cada.
    let mut w = crate::pom::HashingWriter::new(BufWriter::with_capacity(CHUNK, dst_file));

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
                let part = source.parts().get(rec.part_index).ok_or_else(|| {
                    anyhow::anyhow!("parte {} ausente para '{}'", rec.part_index, rec.name)
                })?;
                let start = part
                    .data_start()
                    .checked_add(rec.src_offset as usize)
                    .ok_or_else(|| anyhow::anyhow!("offset de tensor fora do arquivo"))?;
                let tensor_bytes = part
                    .map()
                    .get(start..start + size as usize)
                    .ok_or_else(|| anyhow::anyhow!("tensor '{}' truncado", rec.name))?;
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
    let (sha256, final_size) = w.finish();

    info!(
        "fatia pronta: {} ({:.0} MB, sha256 {})",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0),
        &sha256[..12]
    );

    Ok(WrittenSlice {
        bytes: final_size,
        sha256,
    })
}

// ── Tensor parallelism: shard por tensor ─────────────────────────────────────

/// Versão da regra de fatiamento por tensor, gravada em `pom.tp_format`.
///
/// **Suba este número sempre que mudar o que o shard contém** - eixo de corte,
/// hiperparâmetro reescrito, tensor que passa a ser replicado. É o que faz um
/// shard antigo ser regenerado em vez de reaproveitado em silêncio.
///
/// 1 = cabeça de saída replicada, `head_count`/`head_count_kv`/
/// `feed_forward_length` divididos por `tp_size`. Arquivos escritos antes deste
/// carimbo não têm a chave, e a ausência já significa "regra desconhecida".
pub const TP_SHARD_FORMAT: u32 = 1;

/// Papel de um tensor num shard de tensor parallelism (estilo Megatron).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TpRole {
    /// Split pela dimensão de saída (ne[1]): Q/K/V e MLP gate/up. Fatia
    /// contígua; a saída já nasce particionada, sem redução.
    Column,
    /// Split pela dimensão de entrada (ne[0], a contígua): projeção de saída
    /// da atenção e MLP down. Soma parcial a ser reduzida.
    ///
    /// Só vale para tensor cuja **entrada já chega particionada**: estes dois
    /// consomem a saída dos `Column` (`attn_q/k/v`, `ffn_gate/up`) do mesmo
    /// bloco. Um tensor que recebe o oculto inteiro não pode ser `Row` - ver a
    /// nota da cabeça de saída em `Shared`.
    Row,
    /// Replicado igual em todos os ranks: norms, token_embd, rope e a cabeça
    /// de saída (`output.weight`).
    ///
    /// A cabeça esteve em `Row` e era um defeito: a entrada dela é a saída de
    /// `output_norm.weight`, que é `Shared`, então o oculto que chega nela é
    /// **inteiro** (`n_embd`), nunca particionado. Cortá-la por `ne[0]` gerava
    /// uma cabeça esperando `n_embd/size` de entrada, largura que não existe em
    /// ponto nenhum do grafo, e o llama.cpp recusava o load:
    /// `check_tensor_dims: tensor 'output.weight' has wrong shape; expected
    /// 4096, 128256, got 2048, 128256` (Llama-3.1-8B-Q4_K_M, 17/08).
    /// `validate_tensor_parallel` não podia pegar isso: ela só confere se a
    /// dimensão divide, e 4096/2 divide.
    ///
    /// A alternativa clássica é cortar a cabeça por **vocabulário** (`ne[1]`) e
    /// juntar os logits com um all-gather. Não cabe na v1: a coletiva do fork é
    /// um reduce (soma), não um gather, e o llama.cpp confere `output.weight`
    /// contra `n_vocab` - cortar ali exigiria reescrever o vocabulário por rank
    /// e quebraria o tokenizer. Replicar custa memória e é correto;
    /// `token_embd.weight` tem o mesmo formato e já é replicada pelo mesmo
    /// motivo.
    Shared,
    /// Não shardável com segurança (MoE 3D, nome não reconhecido).
    Unsupported,
}

/// Classifica o papel de um tensor pelo nome (o que vem depois de `blk.N.`).
fn tp_role(name: &str) -> TpRole {
    let tail = match name.strip_prefix("blk.") {
        Some(rest) => rest.splitn(2, '.').nth(1).unwrap_or(name),
        None => name,
    };
    match tail {
        "attn_q.weight" | "attn_k.weight" | "attn_v.weight" | "ffn_gate.weight"
        | "ffn_up.weight" => TpRole::Column,
        "attn_output.weight" | "ffn_down.weight" => TpRole::Row,
        "attn_norm.weight" | "attn_norm.bias" | "ffn_norm.weight" | "ffn_norm.bias"
        | "output_norm.weight" | "output_norm.bias" | "token_embd.weight" | "output.weight" => {
            TpRole::Shared
        }
        t if t.contains("rope") || t.contains("pos_embd") || t.contains("_norm.") => TpRole::Shared,
        _ => TpRole::Unsupported,
    }
}

/// Dry-run da classificação de shard do caminho C.
///
/// Lê a tabela de tensores de `src` e responde se `shard_tensor_parallel`
/// conseguiria partir o modelo em `tp_size` ranks, **sem escrever nada**. É o
/// que a tela de implantar consulta antes de aceitar o deploy, para recusar
/// MoE/SSM/weight-tied com um erro que nomeia o tensor — em vez de deixar o
/// job falhar na metade do apply.
///
/// Espelha exatamente as recusas de `shard_tensor_parallel`; o shard chama esta
/// função como porta de entrada, então manter as duas em sincronia é esta
/// função ser a fonte da decisão.
pub fn validate_tensor_parallel(src: &Path, tp_size: u32) -> Result<()> {
    if tp_size < 2 {
        bail!("tp_size precisa ser >= 2");
    }

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF sem partes"))?
        .map();

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

    // Os hiperparâmetros que o shard reescreve têm que dividir por `tp_size`,
    // senão o arquivo sai com cabeçalho impossível. Conferir aqui é o que faz a
    // tela recusar antes do deploy, em vez de o job morrer no meio do apply -
    // mesma razão de existir desta função. Espelha `shard_tensor_parallel`.
    for _ in 0..n_kv {
        let key = r.string()?;
        let vtype = r.u32()?;
        let escalada = key.ends_with(".attention.head_count")
            || key.ends_with(".attention.head_count_kv")
            || key.ends_with(".feed_forward_length");
        if !escalada {
            r.skip_value(vtype)?;
            continue;
        }
        let full = match vtype {
            T_U32 => u32::from_le_bytes(r.take(4)?.try_into().unwrap()),
            T_U64 => r.u64()? as u32,
            _ => bail!("'{key}' não é inteiro no GGUF; não dá para dividir por {tp_size}"),
        };
        if full % tp_size != 0 {
            bail!("'{key}' = {full} não divide por {tp_size} ranks");
        }
    }

    let tensors: Vec<TensorRecord> = source
        .tensors()
        .iter()
        .map(|tensor| TensorRecord {
            name: tensor.name.clone(),
            dims: tensor.dims.clone(),
            dtype: tensor.dtype,
            part_index: tensor.part_index,
            src_offset: tensor.offset,
            byte_size: tensor.byte_size,
        })
        .collect();

    // Weight-tied output (output.weight compartilha offset com token_embd)
    // quebraria o shard: um fica inteiro e o outro partido. Recusa.
    let embd_offset = tensors
        .iter()
        .find(|t| t.name == "token_embd.weight")
        .map(|t| t.src_offset);
    if let Some(out) = tensors.iter().find(|t| t.name == "output.weight") {
        if embd_offset == Some(out.src_offset) {
            bail!("output.weight empatado com token_embd.weight não é suportado no shard (v1)");
        }
    }

    for t in &tensors {
        if t.byte_size == 0 {
            bail!(
                "tamanho desconhecido para o tensor '{}' (tipo ggml {})",
                t.name,
                t.dtype
            );
        }
        let role = tp_role(&t.name);
        match role {
            TpRole::Unsupported => {
                bail!("tensor '{}' não é shardável no caminho C (v1)", t.name);
            }
            TpRole::Shared => {}
            TpRole::Column | TpRole::Row => {
                if t.dims.len() < 2 {
                    bail!("tensor '{}' 1D não pode ser {role:?}", t.name);
                }
                let ne0 = t.dims[0];
                let ne1 = t.dims[1];
                if t.byte_size % ne1 != 0 {
                    bail!("tensor '{}' tem tamanho não alinhado a ne[1]={ne1}", t.name);
                }
                let row_stride = t.byte_size / ne1;
                match role {
                    TpRole::Column => {
                        if ne1 % tp_size as u64 != 0 {
                            bail!("ne[1]={ne1} de '{}' não divide por {tp_size}", t.name);
                        }
                    }
                    TpRole::Row => {
                        if row_stride % tp_size as u64 != 0 {
                            bail!(
                                "ne[0]={ne0} de '{}' (tipo ggml {}) não divide por {tp_size} em limite de bloco",
                                t.name,
                                t.dtype
                            );
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    Ok(())
}

/// Estimates the combined disk footprint of every TP rank without writing it.
///
/// The payload follows the same role classification as the writer: shared
/// tensors are replicated and column/row tensors are divided. The first GGUF
/// data offset is used as a per-rank header budget, with one extra MiB for the
/// TP metadata added by the writer. It is intentionally conservative because
/// this value guards disk space before a long operation starts.
pub fn estimate_tensor_parallel_output_bytes(src: &Path, tp_size: u32) -> Result<u64> {
    validate_tensor_parallel(src, tp_size)?;
    let source = crate::multipart::GgufSource::open(src)?;
    let header_budget = source
        .parts()
        .first()
        .map(|part| part.data_start() as u64)
        .ok_or_else(|| anyhow::anyhow!("GGUF sem partes"))?
        .saturating_add(1 << 20);
    let payload_per_rank = source.tensors().iter().try_fold(0u64, |sum, tensor| {
        let bytes = match tp_role(&tensor.name) {
            TpRole::Shared => tensor.byte_size,
            TpRole::Column | TpRole::Row => tensor.byte_size / u64::from(tp_size),
            TpRole::Unsupported => {
                return Err(anyhow::anyhow!(
                    "tensor '{}' não é shardável no caminho C (v1)",
                    tensor.name
                ));
            }
        };
        sum.checked_add(align_up(bytes))
            .ok_or_else(|| anyhow::anyhow!("overflow ao estimar saída TP"))
    })?;
    payload_per_rank
        .checked_add(header_budget)
        .and_then(|per_rank| per_rank.checked_mul(u64::from(tp_size)))
        .ok_or_else(|| anyhow::anyhow!("overflow ao estimar saída TP"))
}

/// Shard de um modelo GGUF por tensor parallelism.
///
/// Produz em `dst` um GGUF estruturalmente válido com os tensores do rank
/// `tp_rank` de um grupo de `tp_size`: projeções de saída partidas em fatias
/// contíguas (coluna), projeções de entrada partidas em fatias alinhadas ao
/// bloco do tipo ggml (linha) e o resto replicado. Todas as camadas continuam
/// presentes; não há remapeamento de nome nem mudança de `block_count`.
///
/// Um shard não é um modelo autossuficiente: o llama.cpp de hoje não o carrega
/// sozinho. Ele é a metade que o fork do llama.cpp (caminho C) carrega junto
/// com a coletiva all-reduce.
pub fn shard_tensor_parallel(
    src: &Path,
    dst: &Path,
    tp_size: u32,
    tp_rank: u32,
    on_progress: impl Fn(f64),
) -> Result<WrittenSlice> {
    if tp_size < 2 {
        bail!("tp_size precisa ser >= 2");
    }
    if tp_rank >= tp_size {
        bail!("tp_rank {tp_rank} fora de um grupo de {tp_size}");
    }
    // A decisão "este modelo dá para partir" é uma só: o dry-run acima é a
    // porta de entrada, e as checagens abaixo (redundantes depois dele) ficam
    // como rede de segurança do layout.
    validate_tensor_parallel(src, tp_size)?;

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF sem partes"))?
        .map();

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

    // KV: passa tudo como está, menos as chaves pom.* (re-adicionadas abaixo) e
    // os hiperparâmetros que descrevem uma **largura** particionada.
    //
    // `block_count` e `embedding_length` de fato não mudam: todas as camadas
    // seguem presentes, e o eixo do resíduo (`n_embd`) é o único que atravessa
    // o all-reduce inteiro. Mas três chaves descrevem exatamente as dimensões
    // que o shard corta, e deixá-las passar intactas foi o defeito de 17/08: o
    // tensor saía com metade da largura e o cabeçalho continuava prometendo a
    // largura cheia, então o llama.cpp recusava o load com
    // `'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got 4096, 2048`.
    //
    // - `attention.head_count`    -> colunas de `attn_q`   (n_head * head_dim)
    // - `attention.head_count_kv` -> colunas de `attn_k/v`  (n_head_kv * head_dim)
    // - `feed_forward_length`     -> colunas de `ffn_gate/up` e linhas de `ffn_down`
    //
    // Dividir a contagem de cabeças (e não `head_dim`) é o que mantém cada
    // cabeça inteira num rank só: cabeça partida ao meio daria atenção errada
    // sem nenhum erro de forma para acusar.
    let tp_scaled_key = |key: &str| -> bool {
        key.ends_with(".attention.head_count")
            || key.ends_with(".attention.head_count_kv")
            || key.ends_with(".feed_forward_length")
    };
    let mut kv_out: Vec<KvOut> = Vec::with_capacity(n_kv + 4);
    let pom_prefix = format!("{}.", crate::brand::slug());
    for _ in 0..n_kv {
        let kv_start = r.pos;
        let key = r.string()?;
        let vtype = r.u32()?;
        if tp_scaled_key(&key) {
            let full = match vtype {
                T_U32 => u32::from_le_bytes(r.take(4)?.try_into().unwrap()),
                T_U64 => r.u64()? as u32,
                _ => {
                    // Tipo inesperado para um hiperparâmetro numérico: recusar é
                    // a única saída honesta. Passar adiante produziria um shard
                    // que carrega e responde errado.
                    bail!("'{key}' não é inteiro no GGUF; não dá para dividir por {tp_size}");
                }
            };
            if full % tp_size != 0 {
                bail!("'{key}' = {full} não divide por {tp_size} ranks");
            }
            kv_out.push(KvOut::Encoded(encode_kv_u32(&key, full / tp_size)));
            continue;
        }
        r.skip_value(vtype)?;
        if !key.starts_with(&pom_prefix) && !key.starts_with("split.") {
            kv_out.push(KvOut::Raw(kv_start, r.pos));
        }
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("tp_shard"),
        1,
    )));
    // Qual **regra** escreveu este arquivo. Sem isto o reaproveitamento em
    // `plan::extract_tp_shard` era "existe e tem mais de 1 KB", que não sabe
    // distinguir um shard atual de um escrito por uma versão com defeito: em
    // 17/08 a ksia continuou carregando um shard de cabeçalho não dividido
    // durante três correções seguidas, e o log dizia "reaproveitando".
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("tp_format"),
        TP_SHARD_FORMAT,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("tp_size"),
        tp_size,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("tp_rank"),
        tp_rank,
    )));

    // Tensor table
    let all_tensors: Vec<TensorRecord> = source
        .tensors()
        .iter()
        .map(|tensor| TensorRecord {
            name: tensor.name.clone(),
            dims: tensor.dims.clone(),
            dtype: tensor.dtype,
            part_index: tensor.part_index,
            src_offset: tensor.offset,
            byte_size: tensor.byte_size,
        })
        .collect();
    let ordered_tensors = source_read_order(&all_tensors)
        .into_iter()
        .map(|index| &all_tensors[index])
        .collect::<Vec<_>>();

    // Weight-tied output (output.weight compartilha offset com token_embd)
    // quebraria o shard: um fica inteiro e o outro partido. Recusa.
    let embd_offset = all_tensors
        .iter()
        .find(|t| t.name == "token_embd.weight")
        .map(|t| t.src_offset);
    if let Some(out) = all_tensors.iter().find(|t| t.name == "output.weight") {
        if embd_offset == Some(out.src_offset) {
            bail!("output.weight empatado com token_embd.weight não é suportado no shard (v1)");
        }
    }

    // Segmento de bytes a copiar do fonte para o destino.
    #[derive(Debug, Clone, Copy)]
    struct Seg {
        part_index: usize,
        src: usize, // offset relativo à seção de dados da parte
        len: usize,
    }
    #[derive(Debug)]
    struct OutT {
        name: String,
        dims: Vec<u64>,
        dtype: u32,
        segs: Vec<Seg>,
    }

    let mut out_tensors: Vec<OutT> = Vec::with_capacity(n_tensors);
    for t in ordered_tensors {
        if t.byte_size == 0 {
            bail!(
                "tamanho desconhecido para o tensor '{}' (tipo ggml {})",
                t.name,
                t.dtype
            );
        }
        let role = tp_role(&t.name);
        match role {
            TpRole::Unsupported => {
                bail!("tensor '{}' não é shardável no caminho C (v1)", t.name);
            }
            TpRole::Shared => out_tensors.push(OutT {
                name: t.name.clone(),
                dims: t.dims.clone(),
                dtype: t.dtype,
                segs: vec![Seg {
                    part_index: t.part_index,
                    src: t.src_offset as usize,
                    len: t.byte_size as usize,
                }],
            }),
            TpRole::Column | TpRole::Row => {
                if t.dims.len() < 2 {
                    bail!("tensor '{}' 1D não pode ser {role:?}", t.name);
                }
                let ne0 = t.dims[0];
                let ne1 = t.dims[1];
                if t.byte_size % ne1 != 0 {
                    bail!("tensor '{}' tem tamanho não alinhado a ne[1]={ne1}", t.name);
                }
                let row_stride = t.byte_size / ne1; // bytes por linha completa
                let mut dims = t.dims.clone();
                let segs: Vec<Seg> = match role {
                    TpRole::Column => {
                        if ne1 % tp_size as u64 != 0 {
                            bail!("ne[1]={ne1} de '{}' não divide por {tp_size}", t.name);
                        }
                        let new_ne1 = ne1 / tp_size as u64;
                        dims[1] = new_ne1;
                        let start = tp_rank as u64 * new_ne1 * row_stride;
                        vec![Seg {
                            part_index: t.part_index,
                            src: t.src_offset as usize + start as usize,
                            len: (new_ne1 * row_stride) as usize,
                        }]
                    }
                    TpRole::Row => {
                        if row_stride % tp_size as u64 != 0 {
                            bail!(
                                "ne[0]={ne0} de '{}' (tipo ggml {}) não divide por {tp_size} em limite de bloco",
                                t.name,
                                t.dtype
                            );
                        }
                        let chunk = row_stride / tp_size as u64;
                        dims[0] = ne0 / tp_size as u64;
                        let rank_off = tp_rank as u64 * chunk;
                        (0..ne1)
                            .map(|j| Seg {
                                part_index: t.part_index,
                                src: t.src_offset as usize + (j * row_stride + rank_off) as usize,
                                len: chunk as usize,
                            })
                            .collect()
                    }
                    _ => unreachable!(),
                };
                out_tensors.push(OutT {
                    name: t.name.clone(),
                    dims,
                    dtype: t.dtype,
                    segs,
                });
            }
        }
    }

    // ── Layout e escrita ────────────────────────────────────────────────────
    let out_byte_size: u64 = out_tensors
        .iter()
        .map(|t| t.segs.iter().map(|s| s.len as u64).sum::<u64>())
        .sum();
    let new_offsets: Vec<u64> = {
        let mut v = Vec::with_capacity(out_tensors.len());
        let mut cur = 0u64;
        for t in &out_tensors {
            let size = t.segs.iter().map(|s| s.len as u64).sum::<u64>();
            v.push(cur);
            cur = align_up(cur + size);
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
    let tensor_header_bytes: usize = out_tensors
        .iter()
        .map(|t| tensor_header_size(&t.name, &t.dims))
        .sum();
    let meta_size = 4 + 4 + 8 + 8 + kv_bytes + tensor_header_bytes;
    let new_data_offset = align_up(meta_size as u64) as usize;

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("criar diretório {}", parent.display()))?;
    }
    let dst_file = File::create(dst).with_context(|| format!("criar {}", dst.display()))?;
    let mut w = crate::pom::HashingWriter::new(BufWriter::with_capacity(CHUNK, dst_file));

    w.write_all(&GGUF_MAGIC.to_le_bytes())?;
    w.write_all(&version.to_le_bytes())?;
    w.write_all(&(out_tensors.len() as u64).to_le_bytes())?;
    w.write_all(&(kv_out.len() as u64).to_le_bytes())?;
    for entry in &kv_out {
        match entry {
            KvOut::Raw(s, e) => w.write_all(&mm[*s..*e])?,
            KvOut::Encoded(v) => w.write_all(v)?,
        }
    }
    for (t, &off) in out_tensors.iter().zip(&new_offsets) {
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
    let header_written = meta_size;
    w.write_all(&vec![0u8; new_data_offset - header_written])?;

    let mut done: u64 = 0;
    for t in &out_tensors {
        let size = t.segs.iter().map(|s| s.len as u64).sum::<u64>();
        for seg in &t.segs {
            let part = source.parts().get(seg.part_index).ok_or_else(|| {
                anyhow::anyhow!("parte {} ausente ao escrever '{}'", seg.part_index, t.name)
            })?;
            let start = part
                .data_start()
                .checked_add(seg.src)
                .ok_or_else(|| anyhow::anyhow!("offset de tensor fora do arquivo"))?;
            let bytes = part
                .map()
                .get(start..start + seg.len)
                .ok_or_else(|| anyhow::anyhow!("tensor '{}' truncado", t.name))?;
            let mut pos = 0usize;
            while pos < bytes.len() {
                let n = CHUNK.min(bytes.len() - pos);
                w.write_all(&bytes[pos..pos + n])?;
                pos += n;
                done += n as u64;
                if out_byte_size > 0 {
                    on_progress(100.0 * done as f64 / out_byte_size as f64);
                }
            }
        }
        let aligned = align_up(size) as usize;
        let pad = aligned - size as usize;
        if pad > 0 {
            w.write_all(&vec![0u8; pad])?;
        }
    }
    w.flush()?;
    let (sha256, final_size) = w.finish();

    info!(
        "shard de tensor pronto: {} ({:.0} MB, sha256 {})",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0),
        &sha256[..12]
    );

    Ok(WrittenSlice {
        bytes: final_size,
        sha256,
    })
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
/// `*.block_count` and `pom.*` rewritten.
struct MergedSrc {
    version: u32,
    arch: String,
    original_block_count: Option<u32>,
    /// Every KV entry of this file, tagged with its key so the merge can drop
    /// `pom.*` and swap `*.block_count` before re-emitting.
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
        } else if key == crate::brand::gguf_key("original_block_count") {
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
            part_index: 0,
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
pub fn merge(parts: &[MergePart], dst: &Path, on_progress: impl Fn(f64)) -> Result<WrittenSlice> {
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

    let tensor_names: HashSet<&str> = parsed
        .iter()
        .flat_map(|part| part.tensors.iter().map(|tensor| tensor.name.as_str()))
        .collect();
    crate::gguf::validate_slice_tensors(&arch, &tensor_names, true)?;

    let start_first = parts[0].start_layer;
    let end_last = parts[parts.len() - 1].end_layer;
    let total = end_last - start_first + 1;

    // ── Header: the first slice's KVs, with the total and the metadata fixed ──
    let mut kv_out: Vec<KvOut> = Vec::new();
    for (key, entry) in &parsed[0].kv {
        if key.starts_with(&format!("{}.", crate::brand::slug()))
            || key.ends_with(".block_count")
            || key.ends_with(".attention.recurrent_layers")
        {
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
            &crate::brand::gguf_key("original_block_count"),
            orig,
        )));
    }
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("start_layer"),
        start_first,
    )));
    kv_out.push(KvOut::Encoded(encode_kv_u32(
        &crate::brand::gguf_key("end_layer"),
        end_last,
    )));

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
            let is_output = crate::gguf::is_output_tensor(&arch, &t.name);
            // Blocks come from every slice; the shared inputs only from the
            // first; the final output head only from the last. Intermediate
            // slices may carry real required head tensors for their loader,
            // but their copy must not shadow the final head. They also carry a
            // fake `output_norm` that would shadow the real one.
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

    // Same ground-truth recomputation as `split`, over the merged file's own
    // 0-based numbering (`out` names are already remapped to it above).
    let ssm_layers: HashSet<u32> = out
        .iter()
        .filter_map(|t| ssm_conv1d_layer_index(&t.name))
        .collect();
    if !ssm_layers.is_empty() {
        let flags: Vec<u32> = (0..total).map(|j| ssm_layers.contains(&j) as u32).collect();
        kv_out.push(KvOut::Encoded(encode_kv_u32_array(
            &format!("{arch}.attention.recurrent_layers"),
            &flags,
        )));
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
    let mut w = crate::pom::HashingWriter::new(BufWriter::with_capacity(CHUNK, dst_file));

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

    let (sha256, final_size) = w.finish();

    info!(
        "fatias juntadas: {} ({:.0} MB, sha256 {})",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0),
        &sha256[..12]
    );

    Ok(WrittenSlice {
        bytes: final_size,
        sha256,
    })
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
        // Um tensor `Row` de verdade (entrada já particionada pelos Column do
        // mesmo bloco). Sem ele o fixture não cobre o corte por ne[0]: a cabeça
        // de saída era o único Row aqui, e ela passou a ser replicada.
        tensors.push((
            format!("blk.{i}.ffn_down.weight"),
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
    let u32_array_kv = |key: &str, values: &[u32]| {
        let mut v = s(key);
        v.extend_from_slice(&T_ARR.to_le_bytes());
        v.extend_from_slice(&T_U32.to_le_bytes());
        v.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            v.extend_from_slice(&value.to_le_bytes());
        }
        v
    };
    let bool_array_kv = |key: &str, values: &[bool]| {
        let mut v = s(key);
        v.extend_from_slice(&T_ARR.to_le_bytes());
        v.extend_from_slice(&T_BOOL.to_le_bytes());
        v.extend_from_slice(&(values.len() as u64).to_le_bytes());
        v.extend(values.iter().map(|value| u8::from(*value)));
        v
    };

    let kv: Vec<Vec<u8>> = vec![
        str_kv("general.architecture", "llama"),
        u32_kv("llama.embedding_length", n as u32),
        u32_kv("llama.block_count", n_blocks),
        u32_kv("llama.vocab_size", n as u32),
        // As três larguras que um shard de tensor parallelism reescreve. Sem
        // elas o fixture não cobre a divisão do cabeçalho, que é metade da
        // correção: tensor cortado com cabeçalho intacto não carrega.
        u32_kv("llama.attention.head_count", 4),
        u32_kv("llama.attention.head_count_kv", 2),
        u32_kv("llama.feed_forward_length", 4),
        u32_array_kv(
            "llama.attention.head_count_kv_by_layer",
            &(0..n_blocks)
                .chain(n_blocks..n_blocks + 3)
                .collect::<Vec<_>>(),
        ),
        bool_array_kv(
            "llama.attention.sliding_window_pattern",
            &(0..n_blocks)
                .map(|layer| layer % 2 == 0)
                .chain([true, false, true])
                .collect::<Vec<_>>(),
        ),
        u32_kv(&crate::brand::gguf_key("original_block_count"), n_blocks),
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
        u32_kv(&crate::brand::gguf_key("original_block_count"), n_blocks),
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
/// A qwen35 GGUF shaped like the real Qwen3.8-27B: hybrid attention/SSM
/// layers, `ssm_conv1d.weight` only on the recurrent ones — every layer
/// except every `full_attn_interval`-th, matching llama.cpp's own fallback
/// pattern (`is_recr_impl[i] = (i+1) % full_attn_interval != 0`) — plus the
/// same trailing `nextn` (MTP) block as `qwen35_test_gguf_bytes`.
fn qwen35_hybrid_test_gguf_bytes(n_blocks: u32, full_attn_interval: u32) -> Vec<u8> {
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
        if (i + 1) % full_attn_interval != 0 {
            tensors.push((
                format!("blk.{i}.ssm_conv1d.weight"),
                vec![n, n],
                T_F32,
                f32v(&data),
            ));
        } else {
            tensors.push((
                format!("blk.{i}.attn_q.weight"),
                vec![n, n],
                T_F32,
                f32v(&data),
            ));
        }
    }
    let last = n_blocks - 1;
    tensors.push((
        format!("blk.{last}.nextn.eh_proj.weight"),
        vec![n, n],
        T_F32,
        f32v(&[4.0; 16]),
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
        u32_kv("qwen35.full_attention_interval", full_attn_interval),
        u32_kv(&crate::brand::gguf_key("original_block_count"), n_blocks),
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

    #[test]
    fn payload_reads_are_ordered_by_source_part_and_offset() {
        let tensors = vec![
            TensorRecord {
                name: "late".into(),
                dims: vec![1],
                dtype: 0,
                part_index: 1,
                src_offset: 8,
                byte_size: 1,
            },
            TensorRecord {
                name: "early".into(),
                dims: vec![1],
                dtype: 0,
                part_index: 0,
                src_offset: 32,
                byte_size: 1,
            },
            TensorRecord {
                name: "middle".into(),
                dims: vec![1],
                dtype: 0,
                part_index: 0,
                src_offset: 4,
                byte_size: 1,
            },
        ];

        assert_eq!(source_read_order(&tensors), vec![2, 1, 0]);
    }

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

    fn kv_u32_array(mm: &[u8], want: &str) -> Option<Vec<u32>> {
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
            if key == want && vtype == T_ARR {
                let elem = r.u32().ok()?;
                let count = r.u64().ok()? as usize;
                if elem != T_U32 {
                    return None;
                }
                return (0..count)
                    .map(|_| Some(u32::from_le_bytes(r.take(4).ok()?.try_into().unwrap())))
                    .collect();
            }
            r.skip_value(vtype).ok()?;
        }
        None
    }

    fn kv_bool_array(mm: &[u8], want: &str) -> Option<Vec<bool>> {
        let mut r = R { data: mm, pos: 0 };
        if r.u32().ok()? != GGUF_MAGIC {
            return None;
        }
        r.u32().ok()?;
        let _n_tensors = r.u64().ok()?;
        let n_kv = r.u64().ok()?;
        for _ in 0..n_kv {
            let key = r.string().ok()?;
            let vtype = r.u32().ok()?;
            if key == want && vtype == T_ARR {
                if r.u32().ok()? != T_BOOL {
                    return None;
                }
                let count = r.u64().ok()? as usize;
                return (0..count).map(|_| Some(r.take(1).ok()?[0] != 0)).collect();
            }
            r.skip_value(vtype).ok()?;
        }
        None
    }

    fn kv_raw(mm: &[u8], want: &str) -> Option<Vec<u8>> {
        let mut r = R { data: mm, pos: 0 };
        if r.u32().ok()? != GGUF_MAGIC {
            return None;
        }
        r.u32().ok()?;
        r.u64().ok()?;
        let n_kv = r.u64().ok()?;
        for _ in 0..n_kv {
            let start = r.pos;
            let key = r.string().ok()?;
            let vtype = r.u32().ok()?;
            r.skip_value(vtype).ok()?;
            if key == want {
                return Some(mm[start..r.pos].to_vec());
            }
        }
        None
    }

    fn with_global_string_array(base: &[u8], count: usize) -> (Vec<u8>, Vec<u8>) {
        let mut r = R { data: base, pos: 0 };
        assert_eq!(r.u32().unwrap(), GGUF_MAGIC);
        r.u32().unwrap();
        r.u64().unwrap();
        let n_kv = r.u64().unwrap();
        for _ in 0..n_kv {
            r.string().unwrap();
            let vtype = r.u32().unwrap();
            r.skip_value(vtype).unwrap();
        }
        let kv_end = r.pos;
        let n_tensors = u64::from_le_bytes(base[8..16].try_into().unwrap());
        for _ in 0..n_tensors {
            r.string().unwrap();
            let dimensions = r.u32().unwrap();
            for _ in 0..dimensions {
                r.u64().unwrap();
            }
            r.u32().unwrap();
            r.u64().unwrap();
        }
        let data_start = align_up(r.pos as u64) as usize;

        let string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let mut array = string("tokenizer.ggml.tokens");
        array.extend_from_slice(&T_ARR.to_le_bytes());
        array.extend_from_slice(&T_STR.to_le_bytes());
        array.extend_from_slice(&(count as u64).to_le_bytes());
        for index in 0..count {
            array.extend_from_slice(&string(&format!("token-{index}")));
        }

        let mut out = base[..24].to_vec();
        out[16..24].copy_from_slice(&(n_kv + 1).to_le_bytes());
        out.extend_from_slice(&base[24..kv_end]);
        out.extend_from_slice(&array);
        out.extend_from_slice(&base[kv_end..data_start]);
        while out.len() % ALIGN != 0 {
            out.push(0);
        }
        out.extend_from_slice(&base[data_start..]);
        (out, array)
    }

    fn unknown_layer_array_gguf() -> Vec<u8> {
        let string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let mut kv = Vec::new();
        let mut push_string = |key: &str, value: &str| {
            kv.extend_from_slice(&string(key));
            kv.extend_from_slice(&T_STR.to_le_bytes());
            kv.extend_from_slice(&string(value));
        };
        push_string("general.architecture", "llama");

        kv.extend_from_slice(&string("llama.block_count"));
        kv.extend_from_slice(&T_U32.to_le_bytes());
        kv.extend_from_slice(&4u32.to_le_bytes());

        kv.extend_from_slice(&string("llama.future_layer_metadata"));
        kv.extend_from_slice(&T_ARR.to_le_bytes());
        kv.extend_from_slice(&T_U32.to_le_bytes());
        kv.extend_from_slice(&3u64.to_le_bytes());
        kv.extend_from_slice(
            &[1u32, 2, 3]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        );

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&kv);
        while bytes.len() % ALIGN != 0 {
            bytes.push(0);
        }
        bytes
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

    fn deepseek4_test_gguf_bytes(n_blocks: u32, with_output_head: bool) -> Vec<u8> {
        let n = 4u64;
        let f32v = |values: &[f32]| -> Vec<u8> {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        };
        let mut tensors: Vec<(String, Vec<u64>, u32, Vec<u8>)> = vec![(
            "token_embd.weight".into(),
            vec![n, n],
            T_F32,
            f32v(&[0.5; 16]),
        )];
        for layer in 0..n_blocks {
            tensors.push((
                format!("blk.{layer}.attn_q.weight"),
                vec![n, n],
                T_F32,
                f32v(&[layer as f32; 16]),
            ));
        }
        tensors.push(("output_norm.weight".into(), vec![n], T_F32, f32v(&[2.0; 4])));
        if with_output_head {
            tensors.extend([
                ("output.weight".into(), vec![n, n], T_F32, f32v(&[3.0; 16])),
                (
                    "output_hc_fn.weight".into(),
                    vec![8, 2],
                    T_F32,
                    f32v(&[4.0; 16]),
                ),
                (
                    "output_hc_base.weight".into(),
                    vec![2],
                    T_F32,
                    f32v(&[5.0; 2]),
                ),
                (
                    "output_hc_scale.weight".into(),
                    vec![1],
                    T_F32,
                    f32v(&[6.0; 1]),
                ),
            ]);
        }

        let string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let string_kv = |key: &str, value: &str| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_STR.to_le_bytes());
            bytes.extend_from_slice(&string(value));
            bytes
        };
        let u32_kv = |key: &str, value: u32| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_U32.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes
        };
        let kv = vec![
            string_kv("general.architecture", "deepseek4"),
            u32_kv("deepseek4.embedding_length", n as u32),
            u32_kv("deepseek4.block_count", n_blocks),
            u32_kv("deepseek4.vocab_size", n as u32),
            u32_kv("deepseek4.dsv4_hc_mult", 2),
            u32_kv("deepseek4.hash_layer_count", 3),
            u32_kv("deepseek4.leading_dense_block_count", 3),
            u32_kv("deepseek4.n_deepstack_layers", 3),
            u32_kv(&crate::brand::gguf_key("original_block_count"), n_blocks),
        ];

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(kv.len() as u64).to_le_bytes());
        for entry in &kv {
            bytes.extend_from_slice(entry);
        }

        let mut offset = 0u64;
        let offsets: Vec<u64> = tensors
            .iter()
            .map(|(_, _, _, data)| {
                let current = offset;
                offset = align_up(offset + data.len() as u64);
                current
            })
            .collect();
        for ((name, dims, dtype, _), offset) in tensors.iter().zip(offsets) {
            bytes.extend_from_slice(&string(name));
            bytes.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for dim in dims {
                bytes.extend_from_slice(&dim.to_le_bytes());
            }
            bytes.extend_from_slice(&dtype.to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        for (_, _, _, data) in tensors {
            bytes.extend_from_slice(&data);
            bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        }
        bytes
    }

    #[test]
    fn per_layer_array_slice_uses_absolute_layer_indices() {
        let values = vec![0u32, 0, 4, 128, 4, 128, 4, 128, 4, 0, 0];

        assert_eq!(
            slice_layer_values(&values, 8, 2, 4).unwrap(),
            vec![4, 128, 4, 128]
        );
        assert_eq!(slice_layer_values(&values, 8, 7, 1).unwrap(), vec![128]);
    }

    #[test]
    fn per_layer_arrays_are_sliced_without_naming_specific_keys() {
        let dir = std::env::temp_dir().join(format!("pom-array-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        std::fs::write(&whole, test_gguf_bytes(4)).unwrap();

        split(&whole, &out, 1, 2, false, false, |_| {}).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(
            kv_u32_array(&bytes, "llama.attention.head_count_kv_by_layer"),
            Some(vec![1, 2])
        );
        assert_eq!(
            kv_bool_array(&bytes, "llama.attention.sliding_window_pattern"),
            Some(vec![false, true])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn global_string_array_is_copied_without_layer_slicing() {
        let dir =
            std::env::temp_dir().join(format!("pom-global-string-array-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        let (bytes, expected) = with_global_string_array(&test_gguf_bytes(1), 248_320);
        std::fs::write(&whole, bytes).unwrap();

        let layout = crate::gguf::read_layout(&whole).unwrap();
        split(&whole, &out, 0, 0, true, true, |_| {}).unwrap();
        let output = std::fs::read(&out).unwrap();

        assert_eq!(layout.slice_bytes(0, 0, true), output.len() as u64);
        assert_eq!(kv_raw(&output, "tokenizer.ggml.tokens"), Some(expected));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Splitting 0-1 and 2-3 off one model and joining them back has to give
    /// the original layers back their real numbers and their real bytes — the
    /// only property a pipeline hop actually depends on.
    #[test]
    fn merged_slices_round_trip_into_a_whole() {
        let dir = std::env::temp_dir().join(format!("pom-merge-{}", std::process::id()));
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
        assert!(size.bytes > 0);

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
                "blk.0.ffn_down.weight",
                "blk.1.attn_q.weight",
                "blk.1.ffn_down.weight",
                "blk.2.attn_q.weight",
                "blk.2.ffn_down.weight",
                "blk.3.attn_q.weight",
                "blk.3.ffn_down.weight",
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
        let dir = std::env::temp_dir().join(format!("pom-merge-bad-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("pom-nextn-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("pom-nextn-27b-{}", std::process::id()));
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

    /// A fatia 35-64 do Qwen3.8-27B-Q3_K_S — a forma exata do arquivo que
    /// falhou com "missing tensor 'blk.0.ssm_conv1d.weight'" em 2026-08-16.
    ///
    /// O modelo é híbrido: camadas recorrentes (SSM/linear attention) levam
    /// `ssm_conv1d.weight`, e llama.cpp decide qual é qual, na ausência de um
    /// array explícito, pela fórmula `(i+1) % full_attention_interval != 0`
    /// sobre o índice ABSOLUTO da camada. `split` renumeia `blk.35` para
    /// `blk.0`; sem o array explícito, o llama.cpp da fatia recalcula com
    /// i=0 (em vez de 35) e erra qual bloco local precisa do tensor.
    #[test]
    fn hybrid_slice_carries_an_explicit_ground_truth_recurrent_layers_array() {
        let dir = std::env::temp_dir().join(format!("pom-hybrid-27b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("qwen38-27b-hybrid.gguf");
        std::fs::write(&whole, qwen35_hybrid_test_gguf_bytes(65, 4)).unwrap();

        let tail = dir.join("qwen38_35_64.gguf");
        split(&whole, &tail, 35, 64, false, true, |_| {}).unwrap();
        let tail_bytes = std::fs::read(&tail).unwrap();

        // Global layer 35 is full attention ((35+1)%4 == 0, the exact block
        // that failed in production); 36,37,38 are recurrent. Local j =
        // global - 35.
        let flags = kv_u32_array(&tail_bytes, "qwen35.attention.recurrent_layers")
            .expect("fatia híbrida precisa do array explícito de camadas recorrentes");
        assert_eq!(&flags[..4], &[0, 1, 1, 1], "{flags:?}");

        let tail_src = parse_merged_src(&tail_bytes).unwrap();
        let names: std::collections::HashSet<&str> =
            tail_src.tensors.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains("blk.0.ssm_conv1d.weight"),
            "o bloco local 0 (original 35) é full-attention e não deveria ter ssm_conv1d — \
             é exatamente o tensor que o llama.cpp exigia erroneamente antes deste fix \
             (deduzido pela fórmula sobre o índice LOCAL em vez do original 35)"
        );
        assert!(
            names.contains("blk.1.ssm_conv1d.weight")
                && names.contains("blk.2.ssm_conv1d.weight")
                && names.contains("blk.3.ssm_conv1d.weight"),
            "os blocos locais 1-3 (originais 36-38) são recorrentes e têm o tensor: {names:?}"
        );
    }

    /// Sem tensor `ssm_conv1d` no arquivo inteiro (modelo comum, não híbrido),
    /// `split` não deve inventar a chave — o llama.cpp nem olha para ela.
    #[test]
    fn plain_transformer_slice_gets_no_recurrent_layers_array() {
        let dir = std::env::temp_dir().join(format!("pom-nonhybrid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(4)).unwrap();

        let a = dir.join("a.gguf");
        split(&whole, &a, 2, 3, false, true, |_| {}).unwrap();
        let bytes = std::fs::read(&a).unwrap();
        assert_eq!(
            kv_u32_array(&bytes, "llama.attention.recurrent_layers"),
            None,
            "modelo sem ssm_conv1d não deve ganhar essa chave"
        );
    }

    #[test]
    fn deepseek4_non_final_slice_carries_the_required_output_head() {
        let dir = std::env::temp_dir().join(format!("pom-deepseek4-mid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let output = dir.join("mid.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(4, true)).unwrap();

        let source_bytes = std::fs::read(&whole).unwrap();
        let source = parse_merged_src(&source_bytes).unwrap();
        split(&whole, &output, 0, 1, true, false, |_| {}).unwrap();

        let output_bytes = std::fs::read(&output).unwrap();
        let parsed = parse_merged_src(&output_bytes).unwrap();
        let names: HashSet<&str> = parsed
            .tensors
            .iter()
            .map(|tensor| tensor.name.as_str())
            .collect();
        for required in crate::gguf::required_output_tensors("deepseek4") {
            assert!(names.contains(required), "faltou {required}: {names:?}");
        }
        for copied in [
            "token_embd.weight",
            "output.weight",
            "output_hc_fn.weight",
            "output_hc_base.weight",
            "output_hc_scale.weight",
        ] {
            assert_eq!(
                tensor_data(&source_bytes, &source, copied),
                tensor_data(&output_bytes, &parsed, copied),
                "a fatia intermediária deve copiar os bytes reais de {copied}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek4_final_slice_carries_the_complete_output_head() {
        let dir = std::env::temp_dir().join(format!("pom-deepseek4-final-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let output = dir.join("final.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(2, true)).unwrap();

        split(&whole, &output, 0, 1, true, true, |_| {}).unwrap();
        let parsed = parse_merged_src(&std::fs::read(&output).unwrap()).unwrap();
        let names: HashSet<&str> = parsed
            .tensors
            .iter()
            .map(|tensor| tensor.name.as_str())
            .collect();
        for required in [
            "output_norm.weight",
            "output.weight",
            "output_hc_fn.weight",
            "output_hc_base.weight",
            "output_hc_scale.weight",
        ] {
            assert!(names.contains(required), "faltou {required}: {names:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn layer_counts_from_model_start_are_rebased_for_offset_slices() {
        let dir =
            std::env::temp_dir().join(format!("pom-layer-counts-offset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let prefix = dir.join("prefix.gguf");
        let output = dir.join("tail.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(4, true)).unwrap();

        split(&whole, &prefix, 0, 1, true, false, |_| {}).unwrap();
        let prefix_bytes = std::fs::read(&prefix).unwrap();
        for key in [
            "deepseek4.hash_layer_count",
            "deepseek4.leading_dense_block_count",
            "deepseek4.n_deepstack_layers",
        ] {
            assert_eq!(
                kv_u32(&prefix_bytes, key),
                Some(2),
                "a contagem desde o início deve ser limitada ao tamanho da fatia: {key}"
            );
        }

        split(&whole, &output, 2, 3, false, true, |_| {}).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        let layout = crate::gguf::read_layout(&whole).unwrap();
        assert_eq!(
            layout.slice_bytes(2, 3, true),
            std::fs::metadata(&output).unwrap().len(),
            "a estimativa precisa acompanhar a reserialização da metadata rebased"
        );
        for key in [
            "deepseek4.hash_layer_count",
            "deepseek4.leading_dense_block_count",
            "deepseek4.n_deepstack_layers",
        ] {
            assert_eq!(
                kv_u32(&bytes, key),
                Some(1),
                "a contagem relativa ao início precisa ser local para a fatia: {key}"
            );
        }

        let tail = dir.join("empty-start-count.gguf");
        split(&whole, &tail, 3, 3, false, true, |_| {}).unwrap();
        let tail_bytes = std::fs::read(&tail).unwrap();
        assert_eq!(kv_u32(&tail_bytes, "deepseek4.hash_layer_count"), Some(0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek4_layout_prices_the_required_head_in_intermediate_ranges() {
        let dir = std::env::temp_dir().join(format!("pom-deepseek4-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let mid = dir.join("mid.gguf");
        let output = dir.join("final.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(2, true)).unwrap();

        let layout = crate::gguf::read_layout(&whole).unwrap();
        layout.validate_slice_ranges(&[(0, 0)]).unwrap();
        split(&whole, &mid, 0, 0, true, false, |_| {}).unwrap();
        assert_eq!(
            layout.slice_bytes(0, 0, false),
            std::fs::metadata(&mid).unwrap().len()
        );
        split(&whole, &output, 0, 1, true, true, |_| {}).unwrap();
        assert_eq!(
            layout.slice_bytes(0, 1, true),
            std::fs::metadata(&output).unwrap().len()
        );
        assert!(layout.required_head_bytes() > 0);
        let plan = crate::plan::plan_by_size(
            &layout,
            &[
                crate::plan::SizedSlice {
                    target_mb: Some(0.0),
                },
                crate::plan::SizedSlice { target_mb: None },
            ],
        )
        .unwrap();
        assert_eq!(plan.required_head_bytes, Some(layout.required_head_bytes()));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek4_non_final_slice_reports_missing_output_head_tensors() {
        let dir =
            std::env::temp_dir().join(format!("pom-deepseek4-mid-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let output = dir.join("mid.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(2, false)).unwrap();

        let error = split(&whole, &output, 0, 0, true, false, |_| {}).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("deepseek4"), "{message}");
        for required in [
            "output.weight",
            "output_hc_fn.weight",
            "output_hc_base.weight",
            "output_hc_scale.weight",
        ] {
            assert!(message.contains(required), "faltou {required}: {message}");
        }
        assert!(!output.exists(), "uma recusa de preflight não cria a saída");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek4_final_slice_reports_all_missing_output_tensors() {
        let dir =
            std::env::temp_dir().join(format!("pom-deepseek4-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("deepseek4.gguf");
        let output = dir.join("final.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(2, false)).unwrap();

        let error = split(&whole, &output, 0, 1, true, true, |_| {}).unwrap_err();

        let message = error.to_string();
        for required in [
            "output.weight",
            "output_hc_fn.weight",
            "output_hc_base.weight",
            "output_hc_scale.weight",
        ] {
            assert!(message.contains(required), "faltou {required}: {message}");
        }
        assert!(!output.exists(), "uma recusa de preflight não cria a saída");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `merge` reconstrói o array híbrido para a numeração 0-based do arquivo
    /// unido — não pode só copiar o array (errado) da primeira fatia.
    #[test]
    fn merging_hybrid_slices_recomputes_recurrent_layers_for_the_union() {
        let dir = std::env::temp_dir().join(format!("pom-hybrid-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, qwen35_hybrid_test_gguf_bytes(8, 4)).unwrap();

        let a = dir.join("a.gguf");
        let b = dir.join("b.gguf");
        split(&whole, &a, 0, 3, true, false, |_| {}).unwrap();
        split(&whole, &b, 4, 7, false, true, |_| {}).unwrap();

        let merged = dir.join("merged.gguf");
        merge(
            &[
                MergePart {
                    path: a.clone(),
                    start_layer: 0,
                    end_layer: 3,
                },
                MergePart {
                    path: b.clone(),
                    start_layer: 4,
                    end_layer: 7,
                },
            ],
            &merged,
            |_| {},
        )
        .unwrap();

        let bytes = std::fs::read(&merged).unwrap();
        let flags = kv_u32_array(&bytes, "qwen35.attention.recurrent_layers").unwrap();
        assert_eq!(flags, vec![1, 1, 1, 0, 1, 1, 1, 0], "{flags:?}");
    }

    // ── Tensor parallelism: shard por tensor ──────────────────────────────

    /// Lê dims e dados f32 de um tensor de um GGUF no disco. O modelo de teste
    /// é todo f32, então os dados são interpretados como floats.
    fn read_shard_tensor(path: &std::path::Path, name: &str) -> (Vec<u64>, Vec<f32>) {
        let bytes = std::fs::read(path).unwrap();
        let mut r = R {
            data: &bytes,
            pos: 0,
        };
        assert_eq!(r.u32().unwrap(), GGUF_MAGIC);
        r.u32().unwrap(); // version
        let n_tensors = r.u64().unwrap() as usize;
        let n_kv = r.u64().unwrap() as usize;
        for _ in 0..n_kv {
            r.string().unwrap();
            let vtype = r.u32().unwrap();
            r.skip_value(vtype).unwrap();
        }
        let mut dims = Vec::new();
        let mut offset = 0u64;
        let mut found = false;
        for _ in 0..n_tensors {
            let tn = r.string().unwrap();
            let n_dims = r.u32().unwrap() as usize;
            let ds: Vec<u64> = (0..n_dims).map(|_| r.u64().unwrap()).collect();
            r.u32().unwrap(); // dtype
            let off = r.u64().unwrap();
            if tn == name {
                dims = ds;
                offset = off;
                found = true;
            }
        }
        assert!(found, "tensor {name} não encontrado no shard");
        let data_start = align_up(r.pos as u64) as usize;
        // Tamanho real vem das dims (o modelo de teste é f32), não do delta de
        // offset: o delta inclui o padding de alinhamento entre tensores.
        let size = dims.iter().product::<u64>() * 4;
        let start = data_start + offset as usize;
        let floats: Vec<f32> = bytes[start..start + size as usize]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        (dims, floats)
    }

    #[test]
    fn tp_role_classifies_each_tensor() {
        assert_eq!(tp_role("blk.0.attn_q.weight"), TpRole::Column);
        assert_eq!(tp_role("blk.3.ffn_gate.weight"), TpRole::Column);
        assert_eq!(tp_role("blk.0.attn_output.weight"), TpRole::Row);
        assert_eq!(tp_role("blk.0.ffn_down.weight"), TpRole::Row);
        // A cabeça de saída recebe o oculto **inteiro** (a entrada dela é
        // `output_norm`, que é Shared), então ela não pode ser Row: cortá-la
        // por ne[0] fazia o llama.cpp recusar o load com
        // `'output.weight' has wrong shape; expected 4096, got 2048`.
        assert_eq!(tp_role("output.weight"), TpRole::Shared);
        assert_eq!(tp_role("token_embd.weight"), TpRole::Shared);
        assert_eq!(tp_role("blk.0.attn_norm.weight"), TpRole::Shared);
        assert_eq!(tp_role("output_norm.weight"), TpRole::Shared);
        assert_eq!(tp_role("blk.0.ffn_gate_exps.weight"), TpRole::Unsupported);
        assert_eq!(tp_role("blk.0.ssm_conv1d.weight"), TpRole::Unsupported);
    }

    #[test]
    fn shard_tensor_parallel_rank_zero_takes_the_first_slices() {
        let dir = std::env::temp_dir().join(format!("pom-tp-rank0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        let out = dir.join("rank0.gguf");
        shard_tensor_parallel(&whole, &out, 2, 0, |_| {}).unwrap();

        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(kv_u32(&bytes, &crate::brand::gguf_key("tp_size")), Some(2));
        assert_eq!(kv_u32(&bytes, &crate::brand::gguf_key("tp_rank")), Some(0));
        assert_eq!(kv_u32(&bytes, &crate::brand::gguf_key("tp_shard")), Some(1));

        // Replicado: inteiro, igual ao fonte.
        let (dims, data) = read_shard_tensor(&out, "token_embd.weight");
        assert_eq!(dims, vec![4, 4]);
        assert_eq!(data.len(), 16);
        assert!(data.iter().all(|&v| (v - 0.5).abs() < 1e-6));

        let (dims, data) = read_shard_tensor(&out, "output_norm.weight");
        assert_eq!(dims, vec![4]);
        assert_eq!(data, vec![2.0; 4]);

        // Coluna: ne[1] 4 -> 2; rank 0 fica com as duas primeiras linhas.
        let (dims, data) = read_shard_tensor(&out, "blk.0.attn_q.weight");
        assert_eq!(dims, vec![4, 2]);
        assert_eq!(data, (0..8).map(|j| j as f32).collect::<Vec<_>>());

        // A cabeça é replicada inteira: o oculto que chega nela vem de
        // `output_norm` (Shared) e tem largura cheia. Cortar ne[0] aqui fazia o
        // llama.cpp recusar o load do shard.
        let (dims, data) = read_shard_tensor(&out, "output.weight");
        assert_eq!(dims, vec![4, 4]);
        assert_eq!(data, vec![3.0; 16]);
    }

    #[test]
    fn shard_tensor_parallel_rank_one_takes_the_second_slices() {
        let dir = std::env::temp_dir().join(format!("pom-tp-rank1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        let out = dir.join("rank1.gguf");
        shard_tensor_parallel(&whole, &out, 2, 1, |_| {}).unwrap();

        // Coluna: rank 1 fica com as duas últimas linhas (floats 8..16).
        let (dims, data) = read_shard_tensor(&out, "blk.0.attn_q.weight");
        assert_eq!(dims, vec![4, 2]);
        assert_eq!(data, (8..16).map(|j| j as f32).collect::<Vec<_>>());

        // A cabeça é replicada: todo rank calcula os logits a partir do oculto
        // já reduzido, então ela é idêntica à do rank 0 e à do fonte.
        let (dims, data) = read_shard_tensor(&out, "output.weight");
        assert_eq!(dims, vec![4, 4]);
        assert_eq!(data, vec![3.0; 16]);

        // Linha de verdade: entrada já particionada pelos Column do bloco.
        let (dims, _) = read_shard_tensor(&out, "blk.0.ffn_down.weight");
        assert_eq!(dims[0], 2, "ffn_down continua cortada por ne[0]");
    }

    /// O cabeçalho do shard tem que descrever o tensor que o shard contém.
    ///
    /// Cortar `attn_q` por coluna sem dividir `head_count` deixa o arquivo
    /// prometendo largura cheia, e o llama.cpp recusa o load:
    /// `'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got
    /// 4096, 2048`. `embedding_length` e `block_count` continuam intactos - o
    /// eixo do resíduo e a contagem de camadas não são particionados.
    #[test]
    fn the_shard_header_is_divided_with_the_tensors() {
        let dir = std::env::temp_dir().join(format!("pom-tp-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        let out = dir.join("rank0.gguf");
        shard_tensor_parallel(&whole, &out, 2, 0, |_| {}).unwrap();
        let bytes = std::fs::read(&out).unwrap();

        assert_eq!(kv_u32(&bytes, "llama.attention.head_count"), Some(2), "4/2");
        assert_eq!(
            kv_u32(&bytes, "llama.attention.head_count_kv"),
            Some(1),
            "2/2"
        );
        assert_eq!(kv_u32(&bytes, "llama.feed_forward_length"), Some(2), "4/2");

        // Não particionados: o resíduo atravessa o all-reduce inteiro e toda
        // camada segue presente em todo rank.
        assert_eq!(kv_u32(&bytes, "llama.embedding_length"), Some(4));
        assert_eq!(kv_u32(&bytes, "llama.block_count"), Some(2));
    }

    /// Largura que não divide pelo grupo é recusa nos dois lados, e a recusa
    /// nomeia a chave - o operador precisa saber qual hiperparâmetro trava o
    /// modelo, não só que "não deu".
    #[test]
    fn a_width_that_does_not_divide_is_refused_by_both_paths() {
        let dir = std::env::temp_dir().join(format!("pom-tp-odd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        // head_count_kv = 2 no fixture: um grupo de 4 não divide.
        let erro = validate_tensor_parallel(&whole, 4).unwrap_err().to_string();
        assert!(
            erro.contains("head_count_kv") && erro.contains("4"),
            "a recusa tem que nomear a chave e o tamanho: {erro}"
        );

        let out = dir.join("rank0.gguf");
        assert!(shard_tensor_parallel(&whole, &out, 4, 0, |_| {}).is_err());
        assert!(!out.exists(), "recusa não deve deixar arquivo pela metade");
    }

    #[test]
    fn shard_tensor_parallel_rejects_bad_group() {
        let dir = std::env::temp_dir().join(format!("pom-tp-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(1)).unwrap();
        let out = dir.join("bad.gguf");

        assert!(shard_tensor_parallel(&whole, &out, 1, 0, |_| {}).is_err());
        assert!(shard_tensor_parallel(&whole, &out, 2, 2, |_| {}).is_err());
        assert!(!out.exists(), "recusa não deve deixar arquivo pela metade");
    }

    #[test]
    fn validate_tensor_parallel_matches_the_shard_decision() {
        let dir = std::env::temp_dir().join(format!("pom-tp-validate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        // O modelo denso do fixture dá para partir; o dry-run concorda com o
        // shard de verdade (que chama esta mesma função como porta de entrada).
        assert!(validate_tensor_parallel(&whole, 2).is_ok());

        // tp_size < 2 é recusado aqui também.
        assert!(validate_tensor_parallel(&whole, 1).is_err());

        // Não-GGUF recusa pelo magic.
        let junk = dir.join("junk.bin");
        std::fs::write(&junk, b"not a gguf").unwrap();
        assert!(validate_tensor_parallel(&junk, 2).is_err());
    }

    #[test]
    fn preview_size_matches_the_serialized_slice() {
        let dir = std::env::temp_dir().join(format!("pom-slice-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        let layout = crate::gguf::read_layout(&whole).unwrap();
        let estimated = layout.slice_bytes(0, 0, false);
        let written = split(&whole, &out, 0, 0, true, false, |_| {}).unwrap();

        assert_eq!(
            estimated, written.bytes,
            "a preview is only valid when it equals the file on disk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "requer o GGUF real Gemma4 e vários GB livres em disco/RAM"]
    fn real_gemma4_slice_truncates_layer_metadata_and_loads() {
        let source = std::env::var_os("POM_REAL_GEMMA_GGUF")
            .expect("POM_REAL_GEMMA_GGUF deve apontar para o GGUF real");
        let source = Path::new(&source);
        let dir = std::env::temp_dir().join(format!("pom-gemma4-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("gemma4-0-11.gguf");

        split(source, &out, 0, 11, true, false, |_| {}).unwrap();
        let info = crate::gguf::read_info(&out).unwrap();
        assert_eq!(info.block_count, Some(12));
        assert_eq!(
            info.attention_head_count_kv.as_ref().map(Vec::len),
            Some(12)
        );

        unsafe { llama_cpp_sys_2::llama_backend_init() };
        let path = std::ffi::CString::new(out.to_string_lossy().as_bytes()).unwrap();
        let mut params = unsafe { llama_cpp_sys_2::llama_model_default_params() };
        params.n_gpu_layers = 0;
        let model = unsafe { llama_cpp_sys_2::llama_model_load_from_file(path.as_ptr(), params) };
        assert!(!model.is_null(), "a fatia Gemma4 real deveria carregar");
        unsafe { llama_cpp_sys_2::llama_model_free(model) };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "requer o GGUF real DeepSeek4 e vários GB livres em disco/RAM"]
    fn real_deepseek4_intermediate_slice_copies_heads_and_loads() {
        let source = std::env::var_os("POM_REAL_DEEPSEEK4_GGUF")
            .expect("POM_REAL_DEEPSEEK4_GGUF deve apontar para o GGUF real");
        let source = Path::new(&source);
        let dir = std::env::temp_dir().join(format!("pom-deepseek4-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("deepseek4-15-28.gguf");

        let layout = crate::gguf::read_layout(source).unwrap();
        assert!(
            layout.layer_count() > 28,
            "o GGUF real precisa da camada 28"
        );
        assert!(
            layout.required_head_bytes() > 0,
            "a cabeça DeepSeek4 deve estar na fonte"
        );
        split(source, &out, 15, 28, false, false, |_| {}).unwrap();

        unsafe { llama_cpp_sys_2::llama_backend_init() };
        let path = std::ffi::CString::new(out.to_string_lossy().as_bytes()).unwrap();
        let mut params = unsafe { llama_cpp_sys_2::llama_model_default_params() };
        params.n_gpu_layers = 0;
        let model = unsafe { llama_cpp_sys_2::llama_model_load_from_file(path.as_ptr(), params) };
        assert!(
            !model.is_null(),
            "uma fatia intermediária DeepSeek4 deveria carregar"
        );
        unsafe { llama_cpp_sys_2::llama_model_free(model) };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preview_refuses_an_unknown_dtype_by_name() {
        let mut bytes = test_gguf_bytes(1);
        let mut reader = R {
            data: &bytes,
            pos: 0,
        };
        reader.u32().unwrap();
        reader.u32().unwrap();
        let n_tensors = reader.u64().unwrap() as usize;
        let n_kv = reader.u64().unwrap() as usize;
        for _ in 0..n_kv {
            reader.string().unwrap();
            let kind = reader.u32().unwrap();
            reader.skip_value(kind).unwrap();
        }
        let mut dtype_pos = None;
        for _ in 0..n_tensors {
            let name = reader.string().unwrap();
            let n_dims = reader.u32().unwrap() as usize;
            for _ in 0..n_dims {
                reader.u64().unwrap();
            }
            let pos = reader.pos;
            reader.u32().unwrap();
            reader.u64().unwrap();
            if name == "token_embd.weight" {
                dtype_pos = Some(pos);
            }
        }
        let dtype_pos = dtype_pos.unwrap();
        bytes[dtype_pos..dtype_pos + 4].copy_from_slice(&99u32.to_le_bytes());

        let dir = std::env::temp_dir().join(format!("pom-slice-unknown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unknown.gguf");
        std::fs::write(&path, bytes).unwrap();
        let error = crate::gguf::read_layout(&path).unwrap_err().to_string();
        assert!(error.contains("dtype GGML 99") && error.contains("token_embd.weight"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_architecture_array_length_is_refused_by_name() {
        let bytes = unknown_layer_array_gguf();
        let dir = std::env::temp_dir().join(format!("pom-array-unknown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        std::fs::write(&whole, bytes).unwrap();

        let layout_error = crate::gguf::read_layout(&whole).unwrap_err().to_string();
        assert!(
            layout_error.contains("llama.future_layer_metadata")
                && layout_error.contains("comprimento 3")
        );
        let split_error = split(&whole, &out, 0, 0, true, true, |_| {})
            .unwrap_err()
            .to_string();
        assert!(
            split_error.contains("llama.future_layer_metadata")
                && split_error.contains("comprimento 3")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
