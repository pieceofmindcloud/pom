//! GGUF model splitter — Rust replacement for `split_model.py`.
//!
//! Reads the source model with mmap (never loads weights into RAM), selects
//! tensors for the assigned layer range, remaps `blk.(start+i).*` names to
//! `blk.i.*`, injects a fake identity `output_norm.weight` on non-last slices
//! (skipped for `qwen4exp`, whose loader never creates that tensor — see
//! `gguf::uses_output_norm`), updates KV metadata, and streams the slice to
//! disk. Architecture-specific output contracts are checked before the
//! destination is created: required head tensors are copied from the source
//! into every slice that needs them, including non-final slices.
//!
//! Progress is reported via `on_progress(0.0..=100.0)` as bytes are written.

use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use tracing::info;

/// What a slicing left on disk.
///
/// The `sha256` comes out of the same pass that writes the bytes (see
/// `pom::HashingWriter`), not from a second read: a slice is tens of GB, and
/// re-reading everything just to hash it doubled the cut time. This hash is
/// what becomes the file's **name** and the part's identity in the `.pom`.
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
            bail!("GGUF truncated at byte {}", self.pos);
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
            bail!("implausible GGUF string: {len} bytes");
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
            other => bail!("unknown GGUF type {other}"),
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
                usize::try_from(bytes).map_err(|_| anyhow::anyhow!("GGUF array too large"))?,
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

/// A non-negative scalar written back with the integer type it had in the
/// source: the loader checks the type, and the DeepSeek-V4.1 candidate source
/// is an INT32 that a UINT32 copy made unreadable.
fn encode_kv_like_source(key: &str, source_type: u32, value: u32) -> Vec<u8> {
    match (source_type, i32::try_from(value)) {
        (T_I32, Ok(signed)) => {
            let mut buf = Vec::with_capacity(8 + key.len() + 4 + 4);
            let kb = key.as_bytes();
            buf.extend_from_slice(&(kb.len() as u64).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&T_I32.to_le_bytes());
            buf.extend_from_slice(&signed.to_le_bytes());
            buf
        }
        _ => encode_kv_u32(key, value),
    }
}

fn encode_kv_u32_array(key: &str, values: &[u32]) -> Vec<u8> {
    let payload: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    encode_kv_array(key, T_U32, &payload)
}

fn encode_kv_u32_array_with_type(key: &str, element_type: u32, values: &[u32]) -> Result<Vec<u8>> {
    let payload: Vec<u8> = match element_type {
        T_U32 => values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect(),
        T_I32 => values
            .iter()
            .map(|&value| {
                i32::try_from(value)
                    .map(i32::to_le_bytes)
                    .map_err(|_| anyhow::anyhow!("array '{}' exceeds i32", key))
            })
            .collect::<Result<Vec<[u8; 4]>>>()?
            .into_iter()
            .flatten()
            .collect(),
        other => bail!(
            "array '{}' has type {}; expected a u32 or i32 integer",
            key,
            other
        ),
    };
    Ok(encode_kv_array(key, element_type, &payload))
}

fn encode_kv_string_array(key: &str, values: &[String]) -> Vec<u8> {
    let kb = key.as_bytes();
    let mut buf = Vec::with_capacity(8 + kb.len() + 16 + values.iter().map(|v| 8 + v.len()).sum::<usize>());
    buf.extend_from_slice(&(kb.len() as u64).to_le_bytes());
    buf.extend_from_slice(kb);
    buf.extend_from_slice(&T_ARR.to_le_bytes());
    buf.extend_from_slice(&T_STR.to_le_bytes());
    buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for value in values {
        buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
        buf.extend_from_slice(value.as_bytes());
    }
    buf
}

fn encode_kv_array(key: &str, element_type: u32, payload: &[u8]) -> Vec<u8> {
    let element_size =
        scalar_size(element_type).expect("rebuilt metadata arrays need scalar elements");
    assert_eq!(payload.len() % element_size, 0);
    let mut buf = Vec::with_capacity(8 + key.len() + 4 + 4 + 8 + payload.len());
    let kb = key.as_bytes();
    buf.extend_from_slice(&(kb.len() as u64).to_le_bytes());
    buf.extend_from_slice(kb);
    buf.extend_from_slice(&T_ARR.to_le_bytes());
    buf.extend_from_slice(&element_type.to_le_bytes());
    buf.extend_from_slice(&(u64::try_from(payload.len() / element_size).unwrap()).to_le_bytes());
    buf.extend_from_slice(payload);
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

fn source_u32_array(mm: &[u8], entry: &KvSource) -> Result<Vec<u32>> {
    let array = entry
        .array
        .ok_or_else(|| anyhow::anyhow!("metadata '{}' is not an array", entry.key))?;
    let element_size = scalar_size(array.element_type).ok_or_else(|| {
        anyhow::anyhow!(
            "metadata '{}' has nested elements, expected an integer array",
            entry.key
        )
    })?;
    let mut values = Vec::with_capacity(
        usize::try_from(array.count)
            .map_err(|_| anyhow::anyhow!("array '{}' too large", entry.key))?,
    );
    for index in 0..array.count {
        let byte_offset = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(element_size))
            .ok_or_else(|| anyhow::anyhow!("overflow no array '{}'", entry.key))?;
        let start = array
            .payload_start
            .checked_add(byte_offset)
            .ok_or_else(|| anyhow::anyhow!("overflow no array '{}'", entry.key))?;
        let value = match array.element_type {
            T_U32 => u32::from_le_bytes(
                mm.get(start..start + 4)
                    .ok_or_else(|| anyhow::anyhow!("array '{}' truncated", entry.key))?
                    .try_into()
                    .unwrap(),
            ),
            T_I32 => u32::try_from(i32::from_le_bytes(
                mm.get(start..start + 4)
                    .ok_or_else(|| anyhow::anyhow!("array '{}' truncated", entry.key))?
                    .try_into()
                    .unwrap(),
            ))
            .map_err(|_| anyhow::anyhow!("array '{}' contains a negative value", entry.key))?,
            _ => bail!(
                "array '{}' has type {}; expected a u32 or i32 integer",
                entry.key,
                array.element_type
            ),
        };
        values.push(value);
    }
    Ok(values)
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
        .ok_or_else(|| anyhow::anyhow!("GGUF array missing for '{}'", entry.key))?;
    let element_size = scalar_size(array.element_type).ok_or_else(|| {
        anyhow::anyhow!(
            "GGUF array '{}' has nested elements; length {} cannot be sliced",
            entry.key,
            array.count
        )
    })?;
    let start = usize::try_from(start).map_err(|_| anyhow::anyhow!("invalid array index"))?;
    let count = usize::try_from(count).map_err(|_| anyhow::anyhow!("GGUF array too large"))?;
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
        bail!("array '{}' truncated", entry.key);
    }
    let source = mm
        .get(byte_start..byte_end)
        .ok_or_else(|| anyhow::anyhow!("array '{}' truncated", entry.key))?;

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
        .ok_or_else(|| anyhow::anyhow!("overflow in the layer range"))?;
    if start_layer >= original_block_count || end_layer > original_block_count {
        bail!(
            "layer range {}..{} outside the original count {}",
            start_layer,
            end_layer.saturating_sub(1),
            original_block_count
        );
    }
    if array_count < original_block_count as u64 {
        bail!("per-layer array has length {array_count}; expected at least {original_block_count}");
    }
    Ok((start_layer as usize, end_layer as usize))
}

// Test helper that preserves the proof of per-layer value selection; the
// production path uses `layer_slice_bounds` directly when serializing.
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

fn want_tensor(
    name: &str,
    architecture: &str,
    start: u32,
    end: u32,
    is_last: bool,
    ple_layers: &[u32],
) -> bool {
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
        "per_layer_token_embd.weight"
            if matches!(architecture, "qwen4exp" | "deepseek41")
                && ple_layers
                    .iter()
                    .any(|&layer| (start..=end).contains(&layer)) =>
        {
            true
        }
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
        bail!("invalid layer range L{start_layer}-{end_layer}");
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
    split_impl(src, dst, start_layer, end_layer, is_first, is_last, false, on_progress)
}

/// A header-only slice: the same header `split` writes (same metadata and
/// tensor rules), but each source tensor points at its bytes in the source
/// file(s) through `general.external_data.*`, which the llama.cpp fork's loader
/// reads. Only the injected tensors (the identity `output_norm` of a non-final
/// range) carry data, so the file is a few KB to a few MB (the tokenizer) and
/// takes milliseconds to write. It is valid only while the source files stay
/// where they are and unchanged.
pub fn write_virtual_slice(
    src: &Path,
    dst: &Path,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
) -> Result<WrittenSlice> {
    split_impl(src, dst, start_layer, end_layer, is_first, is_last, true, |_| {})
}

/// Metadata keys of the external tensor data the loader reads.
pub const EXTERNAL_DATA_FILES_KEY: &str = "general.external_data.files";
pub const EXTERNAL_DATA_TENSOR_FILES_KEY: &str = "general.external_data.tensor_files";
pub const EXTERNAL_DATA_TENSOR_OFFSETS_KEY: &str = "general.external_data.tensor_offsets";

#[allow(clippy::too_many_arguments)]
fn split_impl(
    src: &Path,
    dst: &Path,
    start_layer: u32,
    end_layer: u32,
    is_first: bool,
    is_last: bool,
    external_data: bool,
    on_progress: impl Fn(f64),
) -> Result<WrittenSlice> {
    let _ = is_first; // reserved; currently unused (no tensors differ on first vs. middle)
    let n_split = end_layer - start_layer + 1;

    info!(
        "✂️  slicing layers {start_layer}–{end_layer} of {}",
        src.display()
    );

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF without parts"))?
        .map();

    let mut r = R { data: mm, pos: 0 };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("GGUF version {version} not supported (expected 2 or 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("implausible GGUF header ({n_tensors} tensors, {n_kv} kv)");
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
    let ple_key = format!("{arch}.ple.layers");
    let ple_layers = if crate::gguf::is_layer_selector_array(&ple_key, &arch) {
        entries
            .iter()
            .find(|entry| entry.key == ple_key)
            .map(|entry| source_u32_array(mm, entry))
            .transpose()?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let Some(original) = original_block_count {
        if let Some(&layer) = ple_layers.iter().find(|&&layer| layer >= original) {
            bail!("{ple_key} contains layer {layer}, but the source declares only {original} blocks");
        }
    }

    let tensor_names: HashSet<&str> = source
        .tensors()
        .iter()
        .map(|tensor| tensor.name.as_str())
        .collect();
    crate::gguf::validate_slice_tensors(&arch, &tensor_names, is_last)?;
    if !ple_layers.is_empty() && !tensor_names.contains("per_layer_token_embd.weight") {
        bail!("the {arch} source declares PLE, but does not contain per_layer_token_embd.weight");
    }

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
                        "metadata '{key}' counts layers from the start, but is not a non-negative scalar integer that fits in u32"
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
        if arch == "deepseek41" && key == crate::gguf::DEEPSEEK41_CANDIDATE_SOURCE_KEY {
            // llama.cpp reads an absent key as "no candidate selection", which
            // is what a slice ending before this layer has.
            if let Some(layer) = source_scalar_u32(mm, entry) {
                if (start_layer..=end_layer).contains(&layer) {
                    kv_out.push(KvOut::Encoded(encode_kv_like_source(
                        key,
                        entry.vtype,
                        layer - start_layer,
                    )));
                }
                continue;
            }
        }
        if crate::gguf::is_layer_selector_array(key, &arch) {
            let element_type = entry.array.map(|array| array.element_type).unwrap_or(T_U32);
            let local_layers: Vec<u32> = source_u32_array(mm, entry)?
                .into_iter()
                .filter_map(|layer| {
                    (start_layer..=end_layer)
                        .contains(&layer)
                        .then(|| layer - start_layer)
                })
                .collect();
            kv_out.push(KvOut::Encoded(encode_kv_u32_array_with_type(
                key,
                element_type,
                &local_layers,
            )?));
            continue;
        }
        if let Some(array) = entry.array {
            // Only architecture-namespaced arrays are candidates for per-layer
            // slicing. Global arrays such as tokenizer.ggml.tokens must remain
            // byte-for-byte intact, even when their length exceeds block_count.
            // Qwen4Exp PLE table arrays are structural metadata, not arrays
            // indexed by transformer block. `ple.layers` was handled above
            // because it is a selector list that must be filtered and rebased.
            if !is_global_array(key, &arch) && !crate::gguf::is_structural_array(key, &arch) {
                let original = original_block_count.ok_or_else(|| {
                    anyhow::anyhow!(
                        "metadata array '{}' has length {}, but the GGUF does not declare the original block count",
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
                        "metadata array '{}' has length {}; expected 1 or at least {} (original block count)",
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
        .filter(|t| {
            want_tensor(
                &t.name,
                &arch,
                start_layer,
                end_layer,
                is_last,
                &ple_layers,
            )
        })
        .collect();

    if selected.is_empty() {
        bail!("no tensor selected for layers {start_layer}–{end_layer}");
    }
    for t in &selected {
        if t.byte_size == 0 {
            bail!(
                "unknown size for tensor '{}' (ggml type {}): slicing would produce a corrupted slice",
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

    // `qwen4exp` never creates `output_norm.weight` (its final hyper-connection
    // mixer carries that role instead), so injecting the fake identity here
    // would leave a physical tensor the loader never claims via
    // `create_tensor`, breaking its "wrong number of tensors" count check.
    if !is_last && crate::gguf::uses_output_norm(&arch) {
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

    // A header-only slice lists the tensors that carry data first, so their
    // offsets in this file stay small; the rest point at the source files.
    if external_data {
        out_tensors.sort_by_key(|t| matches!(t, TensorOut::Real(_)));
        let mut files = Vec::with_capacity(source.parts().len());
        for part in source.parts() {
            let path = std::fs::canonicalize(&part.path)
                .with_context(|| format!("resolve {}", part.path.display()))?;
            files.push(
                path.to_str()
                    .ok_or_else(|| anyhow::anyhow!("source path is not UTF-8: {}", path.display()))?
                    .to_string(),
            );
        }
        let mut tensor_files = Vec::with_capacity(out_tensors.len());
        let mut tensor_offsets = Vec::with_capacity(out_tensors.len());
        for t in &out_tensors {
            match t {
                TensorOut::Fake { .. } => {
                    tensor_files.push(-1i32);
                    tensor_offsets.push(0u64);
                }
                TensorOut::Real(rec) => {
                    let part = source.parts().get(rec.part_index).ok_or_else(|| {
                        anyhow::anyhow!("part {} missing for '{}'", rec.part_index, rec.name)
                    })?;
                    tensor_files.push(i32::try_from(rec.part_index)?);
                    tensor_offsets.push(part.data_start() as u64 + rec.src_offset);
                }
            }
        }
        kv_out.push(KvOut::Encoded(encode_kv_string_array(EXTERNAL_DATA_FILES_KEY, &files)));
        kv_out.push(KvOut::Encoded(encode_kv_array(
            EXTERNAL_DATA_TENSOR_FILES_KEY,
            T_I32,
            &tensor_files.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(),
        )));
        kv_out.push(KvOut::Encoded(encode_kv_array(
            EXTERNAL_DATA_TENSOR_OFFSETS_KEY,
            T_U64,
            &tensor_offsets.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(),
        )));
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
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    let dst_file = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    // The hash sits outside the buffer: that way it sees exactly the sequence of
    // bytes the `BufWriter` hands to the file, once each.
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
        if external_data && matches!(t, TensorOut::Real(_)) {
            // Its bytes stay in the source file; the injected tensors came first.
            break;
        }

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
                    anyhow::anyhow!("part {} missing for '{}'", rec.part_index, rec.name)
                })?;
                let start = part
                    .data_start()
                    .checked_add(rec.src_offset as usize)
                    .ok_or_else(|| anyhow::anyhow!("tensor offset outside the file"))?;
                let tensor_bytes = part
                    .map()
                    .get(start..start + size as usize)
                    .ok_or_else(|| anyhow::anyhow!("tensor '{}' truncated", rec.name))?;
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
        "slice ready: {} ({:.0} MB, sha256 {})",
        dst.display(),
        final_size as f64 / (1024.0 * 1024.0),
        &sha256[..12]
    );

    Ok(WrittenSlice {
        bytes: final_size,
        sha256,
    })
}

// ── Tensor parallelism: per-tensor shard ─────────────────────────────────────

/// Version of the per-tensor slicing rule, written to `pom.tp_format`.
///
/// **Bump this number whenever what the shard contains changes** - cut axis,
/// rewritten hyperparameter, a tensor that becomes replicated. It is what makes
/// an old shard get regenerated instead of silently reused.
///
/// 1 = replicated output head, `head_count`/`head_count_kv`/
/// `feed_forward_length` divided by `tp_size`. Files written before this
/// stamp do not have the key, and its absence already means "unknown rule".
pub const TP_SHARD_FORMAT: u32 = 1;

/// Role of a tensor in a tensor parallelism shard (Megatron style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TpRole {
    /// Split along the output dimension (ne[1]): Q/K/V and MLP gate/up.
    /// Contiguous slice; the output is born partitioned, with no reduction.
    Column,
    /// Split along the input dimension (ne[0], the contiguous one): attention
    /// output projection and MLP down. Partial sum to be reduced.
    ///
    /// Only valid for a tensor whose **input already arrives partitioned**:
    /// these two consume the output of the `Column` tensors (`attn_q/k/v`,
    /// `ffn_gate/up`) of the same block. A tensor that receives the whole
    /// hidden state cannot be `Row` - see the note on the output head in `Shared`.
    Row,
    /// Replicated identically on every rank: norms, token_embd, rope and the
    /// output head (`output.weight`).
    ///
    /// The head used to be `Row` and that was a defect: its input is the output of
    /// `output_norm.weight`, which is `Shared`, so the hidden state that reaches it
    /// is **whole** (`n_embd`), never partitioned. Cutting it by `ne[0]` produced
    /// a head expecting `n_embd/size` of input, a width that exists nowhere in
    /// the graph, and llama.cpp refused the load:
    /// `check_tensor_dims: tensor 'output.weight' has wrong shape; expected
    /// 4096, 128256, got 2048, 128256` (Llama-3.1-8B-Q4_K_M, 17/08).
    /// `validate_tensor_parallel` could not catch this: it only checks whether the
    /// dimension divides, and 4096/2 divides.
    ///
    /// The classic alternative is to cut the head by **vocabulary** (`ne[1]`) and
    /// join the logits with an all-gather. It does not fit in v1: the fork's
    /// collective is a reduce (sum), not a gather, and llama.cpp checks `output.weight`
    /// against `n_vocab` - cutting there would require rewriting the vocabulary per rank
    /// and would break the tokenizer. Replicating costs memory and is correct;
    /// `token_embd.weight` has the same shape and is already replicated for the same
    /// reason.
    Shared,
    /// Not safely shardable (3D MoE, unrecognized name).
    Unsupported,
}

/// Classifies a tensor's role by its name (what comes after `blk.N.`).
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

/// Dry-run of path C's shard classification.
///
/// Reads the tensor table of `src` and answers whether `shard_tensor_parallel`
/// could split the model into `tp_size` ranks, **without writing anything**. It
/// is what the deploy screen consults before accepting the deploy, to refuse
/// MoE/SSM/weight-tied with an error that names the tensor - instead of letting
/// the job fail halfway through the apply.
///
/// It mirrors exactly the refusals of `shard_tensor_parallel`; the shard calls this
/// function as its entry gate, so keeping the two in sync means this
/// function being the source of the decision.
pub fn validate_tensor_parallel(src: &Path, tp_size: u32) -> Result<()> {
    if tp_size < 2 {
        bail!("tp_size must be >= 2");
    }

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF without parts"))?
        .map();

    let mut r = R { data: mm, pos: 0 };
    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("GGUF version {version} not supported (expected 2 or 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("implausible GGUF header ({n_tensors} tensors, {n_kv} kv)");
    }

    // The hyperparameters the shard rewrites must divide by `tp_size`,
    // otherwise the file comes out with an impossible header. Checking here is what makes
    // the screen refuse before the deploy, instead of the job dying halfway through
    // the apply - the same reason this function exists. Mirrors `shard_tensor_parallel`.
    for _ in 0..n_kv {
        let key = r.string()?;
        let vtype = r.u32()?;
        if key == "general.architecture" && vtype == T_STR {
            let architecture = r.string()?;
            if architecture == "deepseek41" {
                bail!("tensor parallel is not supported for {architecture}: the Engram table and the KV source layers have no rank split");
            }
            continue;
        }
        let scaled = key.ends_with(".attention.head_count")
            || key.ends_with(".attention.head_count_kv")
            || key.ends_with(".feed_forward_length");
        if !scaled {
            r.skip_value(vtype)?;
            continue;
        }
        let full = match vtype {
            T_U32 => u32::from_le_bytes(r.take(4)?.try_into().unwrap()),
            T_U64 => r.u64()? as u32,
            _ => bail!("'{key}' is not an integer in the GGUF; it cannot be divided by {tp_size}"),
        };
        if full % tp_size != 0 {
            bail!("'{key}' = {full} is not divisible by {tp_size} ranks");
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

    // Weight-tied output (output.weight shares an offset with token_embd)
    // would break the shard: one stays whole and the other is split. Refuse.
    let embd_offset = tensors
        .iter()
        .find(|t| t.name == "token_embd.weight")
        .map(|t| t.src_offset);
    if let Some(out) = tensors.iter().find(|t| t.name == "output.weight") {
        if embd_offset == Some(out.src_offset) {
            bail!("output.weight tied to token_embd.weight is not supported in the shard (v1)");
        }
    }

    for t in &tensors {
        if t.byte_size == 0 {
            bail!(
                "unknown size for tensor '{}' (ggml type {})",
                t.name,
                t.dtype
            );
        }
        let role = tp_role(&t.name);
        match role {
            TpRole::Unsupported => {
                bail!("tensor '{}' is not shardable on path C (v1)", t.name);
            }
            TpRole::Shared => {}
            TpRole::Column | TpRole::Row => {
                if t.dims.len() < 2 {
                    bail!("1D tensor '{}' cannot be {role:?}", t.name);
                }
                let ne0 = t.dims[0];
                let ne1 = t.dims[1];
                if t.byte_size % ne1 != 0 {
                    bail!("tensor '{}' has a size not aligned to ne[1]={ne1}", t.name);
                }
                let row_stride = t.byte_size / ne1;
                match role {
                    TpRole::Column => {
                        if ne1 % tp_size as u64 != 0 {
                            bail!("ne[1]={ne1} of '{}' is not divisible by {tp_size}", t.name);
                        }
                    }
                    TpRole::Row => {
                        if row_stride % tp_size as u64 != 0 {
                            bail!(
                                "ne[0]={ne0} of '{}' (ggml type {}) is not divisible by {tp_size} on a block boundary",
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
        .ok_or_else(|| anyhow::anyhow!("GGUF without parts"))?
        .saturating_add(1 << 20);
    let payload_per_rank = source.tensors().iter().try_fold(0u64, |sum, tensor| {
        let bytes = match tp_role(&tensor.name) {
            TpRole::Shared => tensor.byte_size,
            TpRole::Column | TpRole::Row => tensor.byte_size / u64::from(tp_size),
            TpRole::Unsupported => {
                return Err(anyhow::anyhow!(
                    "tensor '{}' is not shardable on path C (v1)",
                    tensor.name
                ));
            }
        };
        sum.checked_add(align_up(bytes))
            .ok_or_else(|| anyhow::anyhow!("overflow while estimating the TP output"))
    })?;
    payload_per_rank
        .checked_add(header_budget)
        .and_then(|per_rank| per_rank.checked_mul(u64::from(tp_size)))
        .ok_or_else(|| anyhow::anyhow!("overflow while estimating the TP output"))
}

/// Shard of a GGUF model by tensor parallelism.
///
/// Produces in `dst` a structurally valid GGUF with the tensors of rank
/// `tp_rank` of a group of `tp_size`: output projections split into
/// contiguous slices (column), input projections split into slices aligned to
/// the ggml type's block (row), and the rest replicated. All layers remain
/// present; there is no name remapping nor change to `block_count`.
///
/// A shard is not a self-sufficient model: today's llama.cpp does not load it
/// on its own. It is the half that the llama.cpp fork (path C) loads together
/// with the all-reduce collective.
pub fn shard_tensor_parallel(
    src: &Path,
    dst: &Path,
    tp_size: u32,
    tp_rank: u32,
    on_progress: impl Fn(f64),
) -> Result<WrittenSlice> {
    if tp_size < 2 {
        bail!("tp_size must be >= 2");
    }
    if tp_rank >= tp_size {
        bail!("tp_rank {tp_rank} outside a group of {tp_size}");
    }
    // The "this model can be split" decision is a single one: the dry-run above is
    // the entry gate, and the checks below (redundant after it) stay
    // as a safety net for the layout.
    validate_tensor_parallel(src, tp_size)?;

    let source = crate::multipart::GgufSource::open(src)?;
    let mm = source
        .parts()
        .first()
        .ok_or_else(|| anyhow::anyhow!("GGUF without parts"))?
        .map();

    let mut r = R { data: mm, pos: 0 };
    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("GGUF version {version} not supported (expected 2 or 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("implausible GGUF header ({n_tensors} tensors, {n_kv} kv)");
    }

    // KV: pass everything through as is, except the pom.* keys (re-added below) and
    // the hyperparameters that describe a partitioned **width**.
    //
    // `block_count` and `embedding_length` really do not change: all layers
    // remain present, and the residual axis (`n_embd`) is the only one that crosses
    // the whole all-reduce. But three keys describe exactly the dimensions
    // the shard cuts, and letting them through untouched was the 17/08 defect: the
    // tensor came out with half the width while the header kept promising the
    // full width, so llama.cpp refused the load with
    // `'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got 4096, 2048`.
    //
    // - `attention.head_count`    -> columns of `attn_q`   (n_head * head_dim)
    // - `attention.head_count_kv` -> columns of `attn_k/v`  (n_head_kv * head_dim)
    // - `feed_forward_length`     -> columns of `ffn_gate/up` and rows of `ffn_down`
    //
    // Dividing the head count (and not `head_dim`) is what keeps each head
    // whole on a single rank: a head split in half would give wrong attention
    // with no shape error to flag it.
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
                    // Unexpected type for a numeric hyperparameter: refusing is
                    // the only honest way out. Passing it on would produce a shard
                    // that loads and answers wrong.
                    bail!(
                        "'{key}' is not an integer in the GGUF; it cannot be divided by {tp_size}"
                    );
                }
            };
            if full % tp_size != 0 {
                bail!("'{key}' = {full} is not divisible by {tp_size} ranks");
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
    // Which **rule** wrote this file. Without this, the reuse in
    // `plan::extract_tp_shard` was "exists and is larger than 1 KB", which cannot
    // tell a current shard from one written by a defective version: on
    // 17/08 ksia kept loading a shard with an undivided header
    // through three fixes in a row, and the log said "reusing".
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

    // Weight-tied output (output.weight shares an offset with token_embd)
    // would break the shard: one stays whole and the other is split. Refuse.
    let embd_offset = all_tensors
        .iter()
        .find(|t| t.name == "token_embd.weight")
        .map(|t| t.src_offset);
    if let Some(out) = all_tensors.iter().find(|t| t.name == "output.weight") {
        if embd_offset == Some(out.src_offset) {
            bail!("output.weight tied to token_embd.weight is not supported in the shard (v1)");
        }
    }

    // Segment of bytes to copy from the source to the destination.
    #[derive(Debug, Clone, Copy)]
    struct Seg {
        part_index: usize,
        src: usize, // offset relative to the part's data section
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
                "unknown size for tensor '{}' (ggml type {})",
                t.name,
                t.dtype
            );
        }
        let role = tp_role(&t.name);
        match role {
            TpRole::Unsupported => {
                bail!("tensor '{}' is not shardable on path C (v1)", t.name);
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
                    bail!("1D tensor '{}' cannot be {role:?}", t.name);
                }
                let ne0 = t.dims[0];
                let ne1 = t.dims[1];
                if t.byte_size % ne1 != 0 {
                    bail!("tensor '{}' has a size not aligned to ne[1]={ne1}", t.name);
                }
                let row_stride = t.byte_size / ne1; // bytes per full row
                let mut dims = t.dims.clone();
                let segs: Vec<Seg> = match role {
                    TpRole::Column => {
                        if ne1 % tp_size as u64 != 0 {
                            bail!("ne[1]={ne1} of '{}' is not divisible by {tp_size}", t.name);
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
                                "ne[0]={ne0} of '{}' (ggml type {}) is not divisible by {tp_size} on a block boundary",
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
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let dst_file = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
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
                anyhow::anyhow!("part {} missing while writing '{}'", seg.part_index, t.name)
            })?;
            let start = part
                .data_start()
                .checked_add(seg.src)
                .ok_or_else(|| anyhow::anyhow!("tensor offset outside the file"))?;
            let bytes = part
                .map()
                .get(start..start + seg.len)
                .ok_or_else(|| anyhow::anyhow!("tensor '{}' truncated", t.name))?;
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
        "tensor shard ready: {} ({:.0} MB, sha256 {})",
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
#[derive(Clone, Copy)]
struct MergedArray {
    element_type: u32,
    count: u64,
    payload_start: usize,
    payload_end: usize,
}

struct MergedKv {
    key: String,
    raw: KvOut,
    value_type: u32,
    value_start: usize,
    array: Option<MergedArray>,
}

struct MergedSrc {
    version: u32,
    arch: String,
    block_count: Option<u32>,
    original_block_count: Option<u32>,
    /// Every KV entry of this file, tagged with its key so the merge can drop
    /// `pom.*` and swap `*.block_count` before re-emitting.
    kv: Vec<MergedKv>,
    tensors: Vec<TensorRecord>,
    data_start: usize,
}

fn merged_scalar_u32(mm: &[u8], start: usize, value_type: u32) -> Option<u32> {
    let bytes = |width| mm.get(start..start.checked_add(width)?);
    match value_type {
        T_U8 => mm.get(start).copied().map(u32::from),
        T_I8 => mm
            .get(start)
            .copied()
            .map(|value| i8::from_le_bytes([value]))
            .and_then(|value| u32::try_from(value).ok()),
        T_U16 => bytes(2)
            .and_then(|value| value.try_into().ok())
            .map(u16::from_le_bytes)
            .map(u32::from),
        T_I16 => bytes(2)
            .and_then(|value| value.try_into().ok())
            .map(i16::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_U32 => bytes(4)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes),
        T_I32 => bytes(4)
            .and_then(|value| value.try_into().ok())
            .map(i32::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_U64 => bytes(8)
            .and_then(|value| value.try_into().ok())
            .map(u64::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        T_I64 => bytes(8)
            .and_then(|value| value.try_into().ok())
            .map(i64::from_le_bytes)
            .and_then(|value| u32::try_from(value).ok()),
        _ => None,
    }
}

fn merged_string(mm: &[u8], start: usize) -> Option<String> {
    let len = usize::try_from(u64::from_le_bytes(
        mm.get(start..start.checked_add(8)?)?.try_into().ok()?,
    ))
    .ok()?;
    let value_start = start.checked_add(8)?;
    let value_end = value_start.checked_add(len)?;
    std::str::from_utf8(mm.get(value_start..value_end)?)
        .ok()
        .map(str::to_owned)
}

fn parse_merged_src(mm: &[u8]) -> Result<MergedSrc> {
    let mut r = R { data: mm, pos: 0 };

    let magic = r.u32()?;
    if magic != GGUF_MAGIC {
        bail!("not a GGUF file (magic={magic:#x})");
    }
    let version = r.u32()?;
    if version != 2 && version != 3 {
        bail!("GGUF version {version} not supported (expected 2 or 3)");
    }
    let n_tensors = r.u64()? as usize;
    let n_kv = r.u64()? as usize;
    if n_tensors > 1_000_000 || n_kv > 100_000 {
        bail!("implausible GGUF header ({n_tensors} tensors, {n_kv} kv)");
    }

    let mut arch = "llama".to_string();
    let mut block_count: Option<u32> = None;
    let mut original_block_count: Option<u32> = None;
    let mut kv: Vec<MergedKv> = Vec::with_capacity(n_kv);
    for _ in 0..n_kv {
        let kv_start = r.pos;
        let key = r.string()?;
        let vtype = r.u32()?;
        let value_start = r.pos;
        let array = if vtype == T_ARR {
            let element_type = r.u32()?;
            let count = r.u64()?;
            let payload_start = r.pos;
            let (_, payload_end) = r.skip_array_payload(element_type, count)?;
            Some(MergedArray {
                element_type,
                count,
                payload_start,
                payload_end,
            })
        } else {
            r.skip_value(vtype)?;
            None
        };

        if key == "general.architecture" && vtype == T_STR {
            if let Some(value) = merged_string(mm, value_start) {
                arch = value;
            }
        } else if key.ends_with(".block_count") {
            block_count = merged_scalar_u32(mm, value_start, vtype);
        } else if key == crate::brand::gguf_key("original_block_count") {
            original_block_count = merged_scalar_u32(mm, value_start, vtype);
        }
        kv.push(MergedKv {
            key,
            raw: KvOut::Raw(kv_start, r.pos),
            value_type: vtype,
            value_start,
            array,
        });
    }

    let mut tensors: Vec<TensorRecord> = Vec::with_capacity(n_tensors);
    for _ in 0..n_tensors {
        let name = r.string()?;
        let n_dims = r.u32()? as usize;
        if n_dims > 8 {
            bail!("implausible rank {n_dims} for tensor '{name}'");
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
        block_count,
        original_block_count,
        kv,
        tensors,
        data_start,
    })
}

/// Rebuild an architecture-namespaced scalar array from the layer coverage of
/// every input slice. The first `block_count` values in each slice belong to
/// its local layers; values after that are a producer-defined special-layer
/// suffix and are retained once, after the joined layers.
fn merge_layer_array(
    key: &str,
    arch: &str,
    parts: &[MergePart],
    parsed: &[MergedSrc],
    mmaps: &[Mmap],
) -> Result<Option<Vec<u8>>> {
    if is_global_array(key, arch) || crate::gguf::is_structural_array(key, arch) {
        return Ok(None);
    }

    let mut element_type = None;
    let mut layer_payload = Vec::new();
    let mut suffix: Option<Vec<u8>> = None;
    let mut broadcast = false;

    for (index, part) in parts.iter().enumerate() {
        let local_count = part
            .end_layer
            .checked_sub(part.start_layer)
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| {
                anyhow::anyhow!("overflow while counting the layers of slice {index}")
            })?;
        if let Some(block_count) = parsed[index].block_count {
            if block_count != local_count {
                bail!(
                    "slice L{}-{} declares {} blocks, expected {}",
                    part.start_layer,
                    part.end_layer,
                    block_count,
                    local_count
                );
            }
        }

        let entry = parsed[index]
            .kv
            .iter()
            .find(|entry| entry.key == key)
            .ok_or_else(|| anyhow::anyhow!("metadata '{}' missing in slice {}", key, index + 1))?;
        let array = entry.array.ok_or_else(|| {
            anyhow::anyhow!("metadata '{}' is not an array in slice {}", key, index + 1)
        })?;
        if let Some(previous) = element_type {
            if previous != array.element_type {
                bail!(
                    "metadata '{}' changes the element type between slices ({} and {})",
                    key,
                    previous,
                    array.element_type
                );
            }
        } else {
            element_type = Some(array.element_type);
        }

        if array.count == 1 {
            if !layer_payload.is_empty() || suffix.is_some() {
                bail!("metadata '{}' mixes broadcast and per-layer arrays", key);
            }
            broadcast = true;
            continue;
        }
        if broadcast {
            bail!("metadata '{}' mixes broadcast and per-layer arrays", key);
        }
        let element_size = scalar_size(array.element_type).ok_or_else(|| {
            anyhow::anyhow!(
                "metadata array '{}' has nested elements and cannot be rebuilt",
                key
            )
        })?;
        let layer_bytes = usize::try_from(local_count)
            .ok()
            .and_then(|count| count.checked_mul(element_size))
            .ok_or_else(|| anyhow::anyhow!("overflow no array metadata '{}'", key))?;
        if array.count < u64::from(local_count) {
            bail!(
                "metadata array '{}' has length {}; expected at least {} in the slice",
                key,
                array.count,
                local_count
            );
        }
        let payload = mmaps[index]
            .get(array.payload_start..array.payload_end)
            .ok_or_else(|| anyhow::anyhow!("metadata array '{}' truncated", key))?;
        let layer = payload
            .get(..layer_bytes)
            .ok_or_else(|| anyhow::anyhow!("metadata array '{}' truncated", key))?;
        layer_payload.extend_from_slice(layer);

        let tail = payload
            .get(layer_bytes..)
            .ok_or_else(|| anyhow::anyhow!("metadata array '{}' truncated", key))?;
        if !tail.is_empty() {
            match &suffix {
                Some(previous) if previous != tail => {
                    bail!("the metadata suffix '{}' differs between slices", key)
                }
                Some(_) => {}
                None => suffix = Some(tail.to_vec()),
            }
        }
    }

    if broadcast {
        return Ok(None);
    }
    let element_type =
        element_type.ok_or_else(|| anyhow::anyhow!("metadata '{}' without entries", key))?;
    if let Some(suffix) = suffix {
        layer_payload.extend_from_slice(&suffix);
    }
    Ok(Some(encode_kv_array(key, element_type, &layer_payload)))
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
        bail!("merging fewer than two slices makes no sense");
    }
    for w in parts.windows(2) {
        if w[1].start_layer != w[0].end_layer + 1 {
            bail!(
                "slices L{}-{} and L{}-{} are not neighbors",
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
        let src_file = File::open(&p.path).with_context(|| format!("open {}", p.path.display()))?;
        let mm = unsafe { Mmap::map(&src_file) }
            .with_context(|| format!("mmap {}", p.path.display()))?;
        parsed.push(parse_merged_src(&mm)?);
        mmaps.push(mm);
    }

    let arch = parsed[0].arch.clone();
    for (i, p) in parsed.iter().enumerate().skip(1) {
        if p.arch != arch {
            bail!(
                "the slices are not from the same model: the architecture of the first is {arch} and that of number {} is {}",
                i + 1,
                p.arch
            );
        }
    }

    let tensor_names: HashSet<&str> = parsed
        .iter()
        .flat_map(|part| part.tensors.iter().map(|tensor| tensor.name.as_str()))
        .collect();
    if arch == "deepseek41" {
        // The merged header inherits the first slice's structural lists, which
        // only name the sources of its own range, so the join would load and
        // answer with the wrong KV sources. The source GGUF is the whole model.
        bail!("merging {arch} slices is not supported: use the source GGUF");
    }
    crate::gguf::validate_slice_tensors(&arch, &tensor_names, true)?;

    let start_first = parts[0].start_layer;
    let end_last = parts[parts.len() - 1].end_layer;
    let total = end_last - start_first + 1;

    // ── Header: the first slice's KVs, with the total and the metadata fixed ──
    let mut kv_out: Vec<KvOut> = Vec::new();
    for entry in &parsed[0].kv {
        let key = &entry.key;
        if key.starts_with(&format!("{}.", crate::brand::slug()))
            || key.ends_with(".block_count")
            || key.ends_with(".attention.recurrent_layers")
        {
            continue; // re-added below
        }
        if let Some(semantics) = crate::gguf::layer_count_semantics(key) {
            let mut value = 0u32;
            for (index, source) in parsed.iter().enumerate() {
                let source_entry = source
                    .kv
                    .iter()
                    .find(|candidate| candidate.key == *key)
                    .ok_or_else(|| {
                        anyhow::anyhow!("metadata '{}' missing in slice {}", key, index + 1)
                    })?;
                let source_value = merged_scalar_u32(
                    mmaps[index].as_ref(),
                    source_entry.value_start,
                    source_entry.value_type,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "metadata '{}' is not a valid scalar integer in slice {}",
                        key,
                        index + 1
                    )
                })?;
                value = match semantics {
                    crate::gguf::LayerCountSemantics::FromStart => {
                        value.saturating_add(source_value).min(total)
                    }
                    crate::gguf::LayerCountSemantics::FromEnd => value.max(source_value),
                };
            }
            kv_out.push(KvOut::Encoded(encode_kv_u32(key, value)));
            continue;
        }
        if entry.array.is_some() {
            if let Some(rebuilt) = merge_layer_array(key, &arch, parts, &parsed, &mmaps)? {
                kv_out.push(KvOut::Encoded(rebuilt));
                continue;
            }
        }
        kv_out.push(entry.raw.clone());
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
                    "unknown size for tensor '{}' (ggml type {}): merging would produce a corrupted file",
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
        bail!("no tensor to merge");
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
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let dst_file = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
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
        "slices merged: {} ({:.0} MB, sha256 {})",
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
        // A genuine `Row` tensor (input already partitioned by the Column tensors of the
        // same block). Without it the fixture does not cover the ne[0] cut: the output
        // head was the only Row here, and it became replicated.
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
        // The three widths that a tensor parallelism shard rewrites. Without
        // them the fixture does not cover the header division, which is half of the
        // fix: a tensor cut with an intact header does not load.
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
    use std::path::PathBuf;

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

    /// The GGUF value type of `want`'s entry.
    fn kv_type(mm: &[u8], want: &str) -> Option<u32> {
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
            if key == want {
                return Some(vtype);
            }
            r.skip_value(vtype).ok()?;
        }
        None
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
                    T_I32 => u32::try_from(i32::from_le_bytes(r.take(4).ok()?.try_into().unwrap())).ok(),
                    T_U64 => Some(r.u64().ok()? as u32),
                    _ => None,
                };
            }
            r.skip_value(vtype).ok()?;
        }
        None
    }

    /// Writes a real and a virtual slice of a real model for a manual load
    /// comparison: `POM_VIRTUAL_SLICE_SRC=<gguf> POM_VIRTUAL_SLICE_OUT=<dir>
    /// POM_VIRTUAL_SLICE_RANGE=<start>-<end>[-last] cargo test -- --ignored
    /// writes_real_and_virtual_slices_of_a_real_model`.
    #[test]
    #[ignore]
    fn writes_real_and_virtual_slices_of_a_real_model() {
        let src = PathBuf::from(std::env::var("POM_VIRTUAL_SLICE_SRC").unwrap());
        let out = PathBuf::from(std::env::var("POM_VIRTUAL_SLICE_OUT").unwrap());
        let range = std::env::var("POM_VIRTUAL_SLICE_RANGE").unwrap();
        let mut parts = range.split('-');
        let start: u32 = parts.next().unwrap().parse().unwrap();
        let end: u32 = parts.next().unwrap().parse().unwrap();
        let is_last = parts.next() == Some("last");
        let real = out.join(format!("real-{range}.gguf"));
        let virt = out.join(format!("virtual-{range}.gguf"));
        let written = split(&src, &real, start, end, start == 0, is_last, |_| {}).unwrap();
        let header = write_virtual_slice(&src, &virt, start, end, start == 0, is_last).unwrap();
        eprintln!("real {} bytes, virtual {} bytes", written.bytes, header.bytes);
    }

    /// Tensor names of a GGUF header, in table order.
    fn tensor_table_names(mm: &[u8]) -> Vec<String> {
        let mut r = R { data: mm, pos: 0 };
        r.u32().unwrap();
        r.u32().unwrap();
        let n_tensors = r.u64().unwrap();
        let n_kv = r.u64().unwrap();
        for _ in 0..n_kv {
            r.string().unwrap();
            let vtype = r.u32().unwrap();
            r.skip_value(vtype).unwrap();
        }
        let mut names = Vec::new();
        for _ in 0..n_tensors {
            names.push(r.string().unwrap());
            let n_dims = r.u32().unwrap();
            for _ in 0..n_dims {
                r.u64().unwrap();
            }
            r.u32().unwrap();
            r.u64().unwrap();
        }
        names
    }

    #[test]
    fn a_virtual_slice_points_at_the_same_bytes_the_real_slice_copies() {
        let dir = std::env::temp_dir().join(format!("pom-virtual-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("model.gguf");
        std::fs::write(&src, test_gguf_bytes(8)).unwrap();
        let source = std::fs::read(&src).unwrap();

        for (start, end, is_last) in [(0u32, 3u32, false), (2, 5, false), (4, 7, true)] {
            let real = dir.join(format!("real-{start}-{end}.gguf"));
            let virt = dir.join(format!("virtual-{start}-{end}.gguf"));
            split(&src, &real, start, end, start == 0, is_last, |_| {}).unwrap();
            write_virtual_slice(&src, &virt, start, end, start == 0, is_last).unwrap();

            let real_bytes = std::fs::read(&real).unwrap();
            let virt_bytes = std::fs::read(&virt).unwrap();
            for key in ["llama.block_count", &crate::brand::gguf_key("start_layer"), &crate::brand::gguf_key("end_layer")] {
                assert_eq!(kv_u32(&virt_bytes, key), kv_u32(&real_bytes, key), "{key}");
            }

            let real_src = crate::multipart::GgufSource::open(&real).unwrap();
            let real_part = &real_src.parts()[0];
            let real_by_name: HashMap<&str, &crate::multipart::GgufTensor> =
                real_src.tensors().iter().map(|t| (t.name.as_str(), t)).collect();

            let names = tensor_table_names(&virt_bytes);
            let files = kv_i32_array(&virt_bytes, EXTERNAL_DATA_TENSOR_FILES_KEY).unwrap();
            let offsets = kv_u64_array(&virt_bytes, EXTERNAL_DATA_TENSOR_OFFSETS_KEY).unwrap();
            assert_eq!(names.len(), real_by_name.len());
            assert_eq!((files.len(), offsets.len()), (names.len(), names.len()));
            // Tensors with data in the header come first, so the file stays small.
            let first_external = files.iter().position(|&f| f >= 0).unwrap();
            assert!(files[first_external..].iter().all(|&f| f == 0));
            assert_eq!(first_external, if is_last { 0 } else { 1 });

            let mut internal_offset = 0usize;
            for (i, name) in names.iter().enumerate() {
                let real_t = real_by_name[name.as_str()];
                let size = real_t.byte_size as usize;
                let start_real = real_part.data_start() + real_t.offset as usize;
                let expected = &real_bytes[start_real..start_real + size];
                let got = if files[i] < 0 {
                    // The only data in the header: the identity output_norm, at the start of its data section.
                    let data_start = virt_bytes.len() - align_up(size as u64) as usize;
                    let bytes = &virt_bytes[data_start + internal_offset..data_start + internal_offset + size];
                    internal_offset += align_up(size as u64) as usize;
                    bytes
                } else {
                    let off = offsets[i] as usize;
                    &source[off..off + size]
                };
                assert_eq!(got, expected, "{name} in range {start}-{end}");
            }
            assert!(virt_bytes.len() < real_bytes.len());
        }
        std::fs::remove_dir_all(&dir).ok();
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

    fn kv_i32_array(mm: &[u8], want: &str) -> Option<Vec<i32>> {
        let mut r = R { data: mm, pos: 0 };
        if r.u32().ok()? != GGUF_MAGIC {
            return None;
        }
        r.u32().ok()?;
        r.u64().ok()?;
        let n_kv = r.u64().ok()?;
        for _ in 0..n_kv {
            let key = r.string().ok()?;
            let vtype = r.u32().ok()?;
            if key == want && vtype == T_ARR {
                if r.u32().ok()? != T_I32 {
                    return None;
                }
                let count = r.u64().ok()? as usize;
                return (0..count)
                    .map(|_| Some(i32::from_le_bytes(r.take(4).ok()?.try_into().unwrap())))
                    .collect();
            }
            r.skip_value(vtype).ok()?;
        }
        None
    }

    fn kv_u64_array(mm: &[u8], want: &str) -> Option<Vec<u64>> {
        let mut r = R { data: mm, pos: 0 };
        if r.u32().ok()? != GGUF_MAGIC {
            return None;
        }
        r.u32().ok()?;
        r.u64().ok()?;
        let n_kv = r.u64().ok()?;
        for _ in 0..n_kv {
            let key = r.string().ok()?;
            let vtype = r.u32().ok()?;
            if key == want && vtype == T_ARR {
                if r.u32().ok()? != T_U64 {
                    return None;
                }
                let count = r.u64().ok()? as usize;
                return (0..count).map(|_| r.u64().ok()).collect();
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

    fn qwen4exp_test_gguf_bytes() -> Vec<u8> {
        let n_blocks = 48u32;
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
        tensors.extend([
            (
                "per_layer_token_embd.weight".into(),
                vec![n, n],
                T_F32,
                f32v(&[0.6; 16]),
            ),
            (
                "output_hc_norm.weight".into(),
                vec![n, n],
                T_F32,
                f32v(&[0.7; 16]),
            ),
            (
                "output_hc_down.weight".into(),
                vec![n, n],
                T_F32,
                f32v(&[0.8; 16]),
            ),
            (
                "output_hc_up.weight".into(),
                vec![n, n],
                T_F32,
                f32v(&[0.9; 16]),
            ),
        ]);
        for layer in 0..n_blocks {
            tensors.push((
                format!("blk.{layer}.attn_q.weight"),
                vec![n, n],
                T_F32,
                f32v(&[layer as f32; 16]),
            ));
        }
        tensors.push(("output.weight".into(), vec![n, n], T_F32, f32v(&[3.0; 16])));

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
        let i32_array_kv = |key: &str, values: &[i32]| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_ARR.to_le_bytes());
            bytes.extend_from_slice(&T_I32.to_le_bytes());
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        };
        let u64_array_kv = |key: &str, values: &[u64]| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_ARR.to_le_bytes());
            bytes.extend_from_slice(&T_U64.to_le_bytes());
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        };

        let compress_ratios: Vec<i32> = (0..n_blocks)
            .map(|layer| if (layer + 1) % 4 == 0 { 4 } else { 0 })
            .collect();
        let head_offsets: Vec<u64> = (0..16).map(|head| head as u64 * 20).collect();
        let head_vocab_sizes: Vec<u64> = (0..16).map(|head| 20 + head as u64).collect();
        let kv = vec![
            string_kv("general.architecture", "qwen4exp"),
            u32_kv("qwen4exp.embedding_length", n as u32),
            u32_kv("qwen4exp.block_count", n_blocks),
            i32_array_kv("qwen4exp.attention.compress_ratios", &compress_ratios),
            i32_array_kv("qwen4exp.ple.layers", &[1]),
            u64_array_kv(
                "qwen4exp.ple.layer_multipliers",
                &[23703573157769, 20109073645365, 8052911324071],
            ),
            u64_array_kv("qwen4exp.ple.head_offsets", &head_offsets),
            u64_array_kv("qwen4exp.ple.head_vocab_sizes", &head_vocab_sizes),
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

    fn qwen4exp_multipart_part_bytes(part_no: u16, tensors: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let string = |value: &str| {
            let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(value.as_bytes());
            bytes
        };
        let u16_kv = |key: &str, value: u16| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_U16.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes
        };
        let i32_array_kv = |key: &str, values: &[i32]| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_ARR.to_le_bytes());
            bytes.extend_from_slice(&T_I32.to_le_bytes());
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        };
        let u64_array_kv = |key: &str, values: &[u64]| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_ARR.to_le_bytes());
            bytes.extend_from_slice(&T_U64.to_le_bytes());
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        };

        let mut kv = vec![
            u16_kv("split.no", part_no),
            u16_kv("split.tensors.count", 10),
            u16_kv("split.count", 3),
        ];
        if part_no == 0 {
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
            kv.extend([
                string_kv("general.architecture", "qwen4exp"),
                u32_kv("qwen4exp.embedding_length", 4),
                u32_kv("qwen4exp.block_count", 4),
                i32_array_kv("qwen4exp.ple.layers", &[1]),
                u64_array_kv("qwen4exp.ple.layer_multipliers", &[3, 5, 7]),
                u64_array_kv("qwen4exp.ple.head_offsets", &[0, 16]),
                u64_array_kv("qwen4exp.ple.head_vocab_sizes", &[16, 16]),
                u32_kv(&crate::brand::gguf_key("original_block_count"), 4),
            ]);
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&GGUF_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(kv.len() as u64).to_le_bytes());
        for entry in &kv {
            bytes.extend_from_slice(entry);
        }

        let mut offset = 0u64;
        for (name, data) in tensors {
            bytes.extend_from_slice(&string(name));
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&T_F32.to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
            offset = align_up(offset + data.len() as u64);
        }
        bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        for (_, data) in tensors {
            bytes.extend_from_slice(data);
            bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        }
        bytes
    }

    fn qwen4exp_multipart_fixture(dir: &Path) -> PathBuf {
        let parts = [
            Vec::new(),
            vec![
                ("token_embd.weight", vec![0x11; 32]),
                ("per_layer_token_embd.weight", vec![0x22; 32]),
                ("output_hc_norm.weight", vec![0x33; 32]),
                ("output_hc_down.weight", vec![0x34; 32]),
                ("output_hc_up.weight", vec![0x35; 32]),
                ("blk.0.attn_q.weight", vec![0x40; 32]),
                ("blk.1.attn_q.weight", vec![0x41; 32]),
            ],
            vec![
                ("blk.2.attn_q.weight", vec![0x42; 32]),
                ("blk.3.attn_q.weight", vec![0x43; 32]),
                ("output.weight", vec![0x44; 32]),
            ],
        ];
        let mut first = None;
        for (part_no, tensors) in parts.into_iter().enumerate() {
            let path = dir.join(format!("qwen4exp-{:05}-of-00003.gguf", part_no + 1));
            std::fs::write(
                &path,
                qwen4exp_multipart_part_bytes(part_no as u16, &tensors),
            )
            .unwrap();
            if part_no == 0 {
                first = Some(path);
            }
        }
        first.unwrap()
    }

    fn multipart_tensor_data(source: &crate::multipart::GgufSource, name: &str) -> Vec<u8> {
        let tensor = source
            .tensors()
            .iter()
            .find(|tensor| tensor.name == name)
            .unwrap_or_else(|| panic!("tensor {name} not found"));
        let part = &source.parts()[tensor.part_index];
        let start = part.data_start() + tensor.offset as usize;
        part.map()[start..start + tensor.byte_size as usize].to_vec()
    }

    fn tensor_data<'a>(mm: &'a [u8], p: &MergedSrc, name: &str) -> &'a [u8] {
        let t = p
            .tensors
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("tensor {name} not found"));
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
            "the total is the union of the two ranges"
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
            "each block returns to its place and the duplicate disappears"
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
        assert!(err.to_string().contains("are not neighbors"), "{err}");
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
            "an intermediate slice does not have the nextn block (MTP): keeping it at 1 would make llama.cpp require 'blk.2.nextn.eh_proj.weight' and abort the load"
        );
        assert_eq!(
            kv_u32(
                &std::fs::read(&last).unwrap(),
                "qwen35.nextn_predict_layers"
            ),
            Some(1),
            "the final slice carries the nextn block and preserves the original value"
        );
    }

    #[test]
    fn merging_slices_keeps_from_end_metadata_from_the_last_slice() {
        let dir = std::env::temp_dir().join(format!("pom-nextn-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, qwen35_test_gguf_bytes(4)).unwrap();

        let first = dir.join("first.gguf");
        split(&whole, &first, 0, 2, true, false, |_| {}).unwrap();
        let last = dir.join("last.gguf");
        split(&whole, &last, 3, 3, false, true, |_| {}).unwrap();

        let merged = dir.join("merged.gguf");
        merge(
            &[
                MergePart {
                    path: first,
                    start_layer: 0,
                    end_layer: 2,
                },
                MergePart {
                    path: last,
                    start_layer: 3,
                    end_layer: 3,
                },
            ],
            &merged,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            kv_u32(
                &std::fs::read(&merged).unwrap(),
                "qwen35.nextn_predict_layers"
            ),
            Some(1),
            "the end-relative metadata must come from the final slice"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merging_slices_sums_from_start_metadata_across_the_union() {
        let dir =
            std::env::temp_dir().join(format!("pom-layer-counts-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, deepseek4_test_gguf_bytes(4, true)).unwrap();

        let first = dir.join("first.gguf");
        split(&whole, &first, 0, 1, true, false, |_| {}).unwrap();
        let last = dir.join("last.gguf");
        split(&whole, &last, 2, 3, false, true, |_| {}).unwrap();

        let merged = dir.join("merged.gguf");
        merge(
            &[
                MergePart {
                    path: first,
                    start_layer: 0,
                    end_layer: 1,
                },
                MergePart {
                    path: last,
                    start_layer: 2,
                    end_layer: 3,
                },
            ],
            &merged,
            |_| {},
        )
        .unwrap();

        let bytes = std::fs::read(&merged).unwrap();
        for key in [
            "deepseek4.hash_layer_count",
            "deepseek4.leading_dense_block_count",
            "deepseek4.n_deepstack_layers",
        ] {
            assert_eq!(kv_u32(&bytes, key), Some(3), "{key}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Slice 0-23 of Qwen3.8-27B-Q3_K_S, with the exact shape of the model that
    /// failed: 65 blocks, `qwen35.nextn_predict_layers = 1` and the `nextn` block
    /// only in `blk.64`. Copying that marker into slice 0-23 makes llama.cpp
    /// derive `blk.23.nextn.eh_proj.weight` from the hyperparameter and abort the
    /// load with "missing tensor" - that is what happened on salem-gpu on
    /// 2026-08-14, while 24-64 (`block_count = 41`, last block = the original
    /// 64) loaded on ksia.
    ///
    /// The smaller test above proves the same rule; this one proves it holds on the
    /// real numbers, which is what the reported defect cited: a non-final slice
    /// declares zero and carries no `nextn` tensor at all; the final one declares 1 and
    /// carries the block, renumbered to `blk.40.*`.
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
            "slice 0-23 does not have the nextn block: declaring 1 makes llama.cpp require 'blk.23.nextn.eh_proj.weight' and abort the load"
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
            "slice 0-23 should carry no nextn tensor at all, it carried {head_nextn:?}"
        );

        assert_eq!(
            kv_u32(&tail_bytes, "qwen35.nextn_predict_layers"),
            Some(1),
            "the final slice really carries the nextn block and preserves the mark"
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
            "the nextn block of the original blk.64 must become blk.40 in slice 24-64, got {tail_nextn:?}"
        );
    }

    /// Slice 35-64 of Qwen3.8-27B-Q3_K_S - the exact shape of the file that
    /// failed with "missing tensor 'blk.0.ssm_conv1d.weight'" on 2026-08-16.
    ///
    /// The model is hybrid: recurrent layers (SSM/linear attention) carry
    /// `ssm_conv1d.weight`, and llama.cpp decides which is which, in the absence of an
    /// explicit array, by the formula `(i+1) % full_attention_interval != 0`
    /// over the layer's ABSOLUTE index. `split` renumbers `blk.35` to
    /// `blk.0`; without the explicit array, the slice's llama.cpp recomputes with
    /// i=0 (instead of 35) and gets wrong which local block needs the tensor.
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
            .expect("a hybrid slice needs the explicit array of recurrent layers");
        assert_eq!(&flags[..4], &[0, 1, 1, 1], "{flags:?}");

        let tail_src = parse_merged_src(&tail_bytes).unwrap();
        let names: std::collections::HashSet<&str> =
            tail_src.tensors.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains("blk.0.ssm_conv1d.weight"),
            "local block 0 (original 35) is full-attention and should not have ssm_conv1d — it is exactly the tensor llama.cpp wrongly required before this fix (deduced by the formula over the LOCAL index instead of the original 35)"
        );
        assert!(
            names.contains("blk.1.ssm_conv1d.weight")
                && names.contains("blk.2.ssm_conv1d.weight")
                && names.contains("blk.3.ssm_conv1d.weight"),
            "local blocks 1-3 (originals 36-38) are recurrent and have the tensor: {names:?}"
        );
    }

    /// With no `ssm_conv1d` tensor in the whole file (an ordinary, non-hybrid model),
    /// `split` must not invent the key - llama.cpp does not even look at it.
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
            "a model without ssm_conv1d must not gain this key"
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
            assert!(names.contains(required), "missing {required}: {names:?}");
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
                "the intermediate slice must copy the real bytes of {copied}"
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
            assert!(names.contains(required), "missing {required}: {names:?}");
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
                "the count from the start must be limited to the slice size: {key}"
            );
        }

        split(&whole, &output, 2, 3, false, true, |_| {}).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        let layout = crate::gguf::read_layout(&whole).unwrap();
        assert_eq!(
            layout.slice_bytes(2, 3, true),
            std::fs::metadata(&output).unwrap().len(),
            "the estimate must follow the reserialization of the rebased metadata"
        );
        for key in [
            "deepseek4.hash_layer_count",
            "deepseek4.leading_dense_block_count",
            "deepseek4.n_deepstack_layers",
        ] {
            assert_eq!(
                kv_u32(&bytes, key),
                Some(1),
                "the count relative to the start must be local to the slice: {key}"
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
            assert!(message.contains(required), "missing {required}: {message}");
        }
        assert!(
            !output.exists(),
            "a preflight refusal does not create the output"
        );
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
            assert!(message.contains(required), "missing {required}: {message}");
        }
        assert!(
            !output.exists(),
            "a preflight refusal does not create the output"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `merge` rebuilds the hybrid array for the 0-based numbering of the merged
    /// file - it cannot just copy the (wrong) array from the first slice.
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

    // ── Tensor parallelism: per-tensor shard ──────────────────────────────

    /// Reads dims and f32 data of a tensor from a GGUF on disk. The test model
    /// is all f32, so the data is interpreted as floats.
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
        assert!(found, "tensor {name} not found in the shard");
        let data_start = align_up(r.pos as u64) as usize;
        // The real size comes from the dims (the test model is f32), not from the offset
        // delta: the delta includes the alignment padding between tensors.
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
        // The output head receives the **whole** hidden state (its input is
        // `output_norm`, which is Shared), so it cannot be Row: cutting it
        // by ne[0] made llama.cpp refuse the load with
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

        // Replicated: whole, identical to the source.
        let (dims, data) = read_shard_tensor(&out, "token_embd.weight");
        assert_eq!(dims, vec![4, 4]);
        assert_eq!(data.len(), 16);
        assert!(data.iter().all(|&v| (v - 0.5).abs() < 1e-6));

        let (dims, data) = read_shard_tensor(&out, "output_norm.weight");
        assert_eq!(dims, vec![4]);
        assert_eq!(data, vec![2.0; 4]);

        // Column: ne[1] 4 -> 2; rank 0 keeps the first two rows.
        let (dims, data) = read_shard_tensor(&out, "blk.0.attn_q.weight");
        assert_eq!(dims, vec![4, 2]);
        assert_eq!(data, (0..8).map(|j| j as f32).collect::<Vec<_>>());

        // The head is replicated whole: the hidden state that reaches it comes from
        // `output_norm` (Shared) and has full width. Cutting ne[0] here made
        // llama.cpp refuse the shard's load.
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

        // Column: rank 1 keeps the last two rows (floats 8..16).
        let (dims, data) = read_shard_tensor(&out, "blk.0.attn_q.weight");
        assert_eq!(dims, vec![4, 2]);
        assert_eq!(data, (8..16).map(|j| j as f32).collect::<Vec<_>>());

        // The head is replicated: every rank computes the logits from the already
        // reduced hidden state, so it is identical to rank 0's and to the source's.
        let (dims, data) = read_shard_tensor(&out, "output.weight");
        assert_eq!(dims, vec![4, 4]);
        assert_eq!(data, vec![3.0; 16]);

        // A genuine Row: input already partitioned by the block's Column tensors.
        let (dims, _) = read_shard_tensor(&out, "blk.0.ffn_down.weight");
        assert_eq!(dims[0], 2, "ffn_down is still cut by ne[0]");
    }

    /// The shard's header must describe the tensor the shard contains.
    ///
    /// Cutting `attn_q` by column without dividing `head_count` leaves the file
    /// promising full width, and llama.cpp refuses the load:
    /// `'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got
    /// 4096, 2048`. `embedding_length` and `block_count` stay intact - the
    /// residual axis and the layer count are not partitioned.
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

        // Not partitioned: the residual crosses the whole all-reduce and every
        // layer remains present on every rank.
        assert_eq!(kv_u32(&bytes, "llama.embedding_length"), Some(4));
        assert_eq!(kv_u32(&bytes, "llama.block_count"), Some(2));
    }

    /// A width that does not divide by the group is a refusal on both sides, and the refusal
    /// names the key - the operator needs to know which hyperparameter blocks the
    /// model, not just that "it did not work".
    #[test]
    fn a_width_that_does_not_divide_is_refused_by_both_paths() {
        let dir = std::env::temp_dir().join(format!("pom-tp-odd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        // head_count_kv = 2 in the fixture: a group of 4 does not divide.
        let error = validate_tensor_parallel(&whole, 4).unwrap_err().to_string();
        assert!(
            error.contains("head_count_kv") && error.contains("4"),
            "the refusal must name the key and the size: {error}"
        );

        let out = dir.join("rank0.gguf");
        assert!(shard_tensor_parallel(&whole, &out, 4, 0, |_| {}).is_err());
        assert!(
            !out.exists(),
            "a refusal must not leave a half-written file"
        );
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
        assert!(
            !out.exists(),
            "a refusal must not leave a half-written file"
        );
    }

    #[test]
    fn validate_tensor_parallel_matches_the_shard_decision() {
        let dir = std::env::temp_dir().join(format!("pom-tp-validate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, test_gguf_bytes(2)).unwrap();

        // The fixture's dense model can be split; the dry-run agrees with the real
        // shard (which calls this same function as its entry gate).
        assert!(validate_tensor_parallel(&whole, 2).is_ok());

        // tp_size < 2 is refused here too.
        assert!(validate_tensor_parallel(&whole, 1).is_err());

        // A non-GGUF is refused by the magic.
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
    fn qwen4exp_ple_structural_arrays_are_preserved_in_slice() {
        let dir = std::env::temp_dir().join(format!("pom-qwen4exp-ple-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        std::fs::write(&whole, qwen4exp_test_gguf_bytes()).unwrap();

        let written = split(&whole, &out, 12, 15, false, false, |_| {}).unwrap();
        let output = std::fs::read(&out).unwrap();
        let layout = crate::gguf::read_layout(&whole).unwrap();

        assert_eq!(layout.slice_bytes(12, 15, false), written.bytes);
        assert_eq!(
            kv_i32_array(&output, "qwen4exp.ple.layers"),
            Some(Vec::new()),
            "PLE layers outside the slice must not be advertised with the source index"
        );
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.layer_multipliers"),
            Some(vec![23703573157769, 20109073645365, 8052911324071])
        );
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.head_offsets"),
            Some((0..16).map(|head| head * 20).collect())
        );
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.head_vocab_sizes"),
            Some((0..16).map(|head| 20 + head).collect())
        );
        assert_eq!(
            kv_i32_array(&output, "qwen4exp.attention.compress_ratios"),
            Some(vec![0, 0, 0, 4])
        );
        let output_source = crate::multipart::GgufSource::open(&out).unwrap();
        assert!(!output_source
            .tensors()
            .iter()
            .any(|tensor| tensor.name == "per_layer_token_embd.weight"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qwen4exp_non_final_slice_does_not_get_a_fake_output_norm() {
        // qwen4exp's final hyper-connection mixer carries the output-norm
        // role (`output_hc_norm.weight`); the loader never calls
        // `create_tensor("output_norm.weight")`. Injecting the synthetic
        // identity norm that every other architecture needs on a non-final
        // slice would leave an unclaimed physical tensor, and llama.cpp's
        // loader refuses to load with "wrong number of tensors".
        let dir =
            std::env::temp_dir().join(format!("pom-qwen4exp-no-fake-norm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let out = dir.join("slice.gguf");
        std::fs::write(&whole, qwen4exp_test_gguf_bytes()).unwrap();

        let written = split(&whole, &out, 0, 15, false, false, |_| {}).unwrap();
        let layout = crate::gguf::read_layout(&whole).unwrap();
        assert_eq!(layout.slice_bytes(0, 15, false), written.bytes);

        let output_source = crate::multipart::GgufSource::open(&out).unwrap();
        assert!(
            !output_source
                .tensors()
                .iter()
                .any(|tensor| tensor.name == "output_norm.weight"),
            "qwen4exp slices must never carry a fake output_norm.weight: \
             the loader would leave it unclaimed and refuse to load"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Eight blocks shaped like the real DeepSeek-V4.1. Its only valid cut is at
    /// layer 4: KV source, index source and candidate source, after both Engram
    /// layers. `head_offsets`, `head_vocab_sizes` and `token_map` are longer than
    /// the block count on purpose: the generic rule would cut them by layer.
    fn deepseek41_test_gguf_bytes() -> Vec<u8> {
        let n_blocks = 8u32;
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
        // The real DeepSeek-V4.1 files write the candidate source as INT32.
        let i32_kv = |key: &str, value: i32| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_I32.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes
        };
        let u32_array_kv = |key: &str, values: &[u32]| {
            let mut bytes = string(key);
            bytes.extend_from_slice(&T_ARR.to_le_bytes());
            bytes.extend_from_slice(&T_U32.to_le_bytes());
            bytes.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        };
        let f32s = |count: usize| {
            std::iter::repeat(0.5f32.to_le_bytes())
                .take(count)
                .flatten()
                .collect::<Vec<u8>>()
        };
        let mut tensors: Vec<(String, Vec<u64>, Vec<u8>)> = vec![
            ("token_embd.weight".into(), vec![4, 4], f32s(16)),
            ("per_layer_token_embd.weight".into(), vec![4, 64], f32s(256)),
            ("output_norm.weight".into(), vec![4], f32s(4)),
            ("output.weight".into(), vec![4, 4], f32s(16)),
        ];
        for layer in 0..n_blocks {
            tensors.push((format!("blk.{layer}.attn_norm.weight"), vec![4], f32s(4)));
        }
        let head_offsets: Vec<u32> = (0..12).map(|head| head * 20).collect();
        let token_map: Vec<u32> = (0..16).collect();
        let kv = vec![
            string_kv("general.architecture", "deepseek41"),
            u32_kv("deepseek41.embedding_length", 4),
            u32_kv("deepseek41.block_count", n_blocks),
            u32_array_kv(
                "deepseek41.attention.compress_ratios",
                &[0, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0],
            ),
            u32_array_kv("deepseek41.attention.kv_source_layers", &[1, 4]),
            u32_array_kv("deepseek41.attention.indexer.source_layers", &[1, 4, 6]),
            i32_kv("deepseek41.attention.indexer.candidate_source_layer", 4),
            u32_array_kv("deepseek41.ple.layers", &[1, 3]),
            u32_array_kv("deepseek41.ple.layer_multipliers", &[11, 13, 17, 19]),
            u32_array_kv("deepseek41.ple.head_offsets", &head_offsets),
            u32_array_kv("deepseek41.ple.head_vocab_sizes", &[20; 12]),
            u32_array_kv("deepseek41.engram.token_map", &token_map),
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
        for (name, dims, data) in &tensors {
            bytes.extend_from_slice(&string(name));
            bytes.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for dim in dims {
                bytes.extend_from_slice(&dim.to_le_bytes());
            }
            bytes.extend_from_slice(&T_F32.to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
            offset = align_up(offset + data.len() as u64);
        }
        bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        for (_, _, data) in tensors {
            bytes.extend_from_slice(&data);
            bytes.resize(align_up(bytes.len() as u64) as usize, 0);
        }
        bytes
    }

    fn deepseek41_split_at_valid_cut(dir: &Path) -> (Vec<u8>, Vec<u8>) {
        std::fs::create_dir_all(dir).unwrap();
        let whole = dir.join("whole.gguf");
        let first = dir.join("first.gguf");
        let second = dir.join("second.gguf");
        std::fs::write(&whole, deepseek41_test_gguf_bytes()).unwrap();
        let layout = crate::gguf::read_layout(&whole).unwrap();

        let first_written = split(&whole, &first, 0, 3, true, false, |_| {}).unwrap();
        let second_written = split(&whole, &second, 4, 7, false, true, |_| {}).unwrap();

        assert_eq!(layout.slice_bytes(0, 3, false), first_written.bytes);
        assert_eq!(layout.slice_bytes(4, 7, true), second_written.bytes);
        (std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap())
    }

    #[test]
    fn deepseek41_first_slice_keeps_engram_and_only_the_sources_in_range() {
        let dir = std::env::temp_dir().join(format!("pom-ds41-first-{}", std::process::id()));
        let (first, _) = deepseek41_split_at_valid_cut(&dir);

        assert_eq!(
            kv_u32_array(&first, "deepseek41.attention.kv_source_layers"),
            Some(vec![1])
        );
        assert_eq!(
            kv_u32_array(&first, "deepseek41.attention.indexer.source_layers"),
            Some(vec![1])
        );
        assert_eq!(kv_u32_array(&first, "deepseek41.ple.layers"), Some(vec![1, 3]));
        assert_eq!(
            kv_u32(&first, "deepseek41.attention.indexer.candidate_source_layer"),
            None,
            "llama.cpp reads an absent key as no candidate selection"
        );
        assert_eq!(
            kv_u32_array(&first, "deepseek41.ple.head_offsets").map(|values| values.len()),
            Some(12),
            "the Engram hash tables do not follow the block count"
        );
        assert_eq!(
            kv_u32_array(&first, "deepseek41.engram.token_map").map(|values| values.len()),
            Some(16)
        );
        assert_eq!(
            kv_u32_array(&first, "deepseek41.attention.compress_ratios"),
            Some(vec![0, 2, 2, 2])
        );
        let path = dir.join("first.gguf");
        let names: Vec<String> = crate::multipart::GgufSource::open(&path)
            .unwrap()
            .tensors()
            .iter()
            .map(|tensor| tensor.name.clone())
            .collect();
        assert!(names.iter().any(|name| name == "per_layer_token_embd.weight"));
        assert!(names.iter().any(|name| name == "output.weight"), "the loader requires it");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek41_tail_slice_renumbers_its_sources_and_drops_the_engram_table() {
        let dir = std::env::temp_dir().join(format!("pom-ds41-tail-{}", std::process::id()));
        let (_, second) = deepseek41_split_at_valid_cut(&dir);

        assert_eq!(
            kv_u32_array(&second, "deepseek41.attention.kv_source_layers"),
            Some(vec![0])
        );
        assert_eq!(
            kv_u32_array(&second, "deepseek41.attention.indexer.source_layers"),
            Some(vec![0, 2])
        );
        assert_eq!(
            kv_u32(&second, "deepseek41.attention.indexer.candidate_source_layer"),
            Some(0)
        );
        assert_eq!(
            kv_type(&second, "deepseek41.attention.indexer.candidate_source_layer"),
            Some(T_I32),
            "llama.cpp reads the candidate source as INT32 and refuses a UINT32"
        );
        assert_eq!(kv_u32_array(&second, "deepseek41.ple.layers"), Some(Vec::new()));
        assert_eq!(
            kv_u32_array(&second, "deepseek41.attention.compress_ratios"),
            Some(vec![1, 1, 1, 1]),
            "per-layer values follow the local blocks"
        );
        let path = dir.join("second.gguf");
        assert!(!crate::multipart::GgufSource::open(&path)
            .unwrap()
            .tensors()
            .iter()
            .any(|tensor| tensor.name == "per_layer_token_embd.weight"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek41_merge_is_refused_instead_of_keeping_the_first_slice_lists() {
        let dir = std::env::temp_dir().join(format!("pom-ds41-merge-{}", std::process::id()));
        deepseek41_split_at_valid_cut(&dir);

        let error = merge(
            &[
                MergePart {
                    path: dir.join("first.gguf"),
                    start_layer: 0,
                    end_layer: 3,
                },
                MergePart {
                    path: dir.join("second.gguf"),
                    start_layer: 4,
                    end_layer: 7,
                },
            ],
            &dir.join("merged.gguf"),
            |_| {},
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("deepseek41"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deepseek41_tensor_parallel_is_refused_by_name() {
        let dir = std::env::temp_dir().join(format!("pom-ds41-tp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        std::fs::write(&whole, deepseek41_test_gguf_bytes()).unwrap();

        let error = validate_tensor_parallel(&whole, 2).unwrap_err().to_string();

        assert!(error.contains("deepseek41"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qwen4exp_ple_layer_and_table_tensor_are_rebased_to_the_local_slice() {
        let dir =
            std::env::temp_dir().join(format!("pom-qwen4exp-ple-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let with_ple = dir.join("with-ple.gguf");
        let without_ple = dir.join("without-ple.gguf");
        std::fs::write(&whole, qwen4exp_test_gguf_bytes()).unwrap();

        let layout = crate::gguf::read_layout(&whole).unwrap();
        let with_ple_written = split(&whole, &with_ple, 1, 1, false, false, |_| {}).unwrap();
        let without_ple_written =
            split(&whole, &without_ple, 12, 15, false, false, |_| {}).unwrap();

        let with_ple_bytes = std::fs::read(&with_ple).unwrap();
        assert_eq!(
            kv_i32_array(&with_ple_bytes, "qwen4exp.ple.layers"),
            Some(vec![0]),
            "an absolute PLE layer must be rebased to the local block number"
        );
        let with_ple_source = crate::multipart::GgufSource::open(&with_ple).unwrap();
        assert!(with_ple_source
            .tensors()
            .iter()
            .any(|tensor| tensor.name == "per_layer_token_embd.weight"));

        let without_ple_bytes = std::fs::read(&without_ple).unwrap();
        assert_eq!(
            kv_i32_array(&without_ple_bytes, "qwen4exp.ple.layers"),
            Some(Vec::new())
        );
        let without_ple_source = crate::multipart::GgufSource::open(&without_ple).unwrap();
        assert!(!without_ple_source
            .tensors()
            .iter()
            .any(|tensor| tensor.name == "per_layer_token_embd.weight"));

        assert_eq!(
            layout.slice_bytes(1, 1, false),
            with_ple_written.bytes,
            "the preview must include the PLE table when its layer is present"
        );
        assert_eq!(
            layout.slice_bytes(12, 15, false),
            without_ple_written.bytes,
            "the preview must omit the PLE table when its layer is outside the slice"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qwen4exp_multipart_slice_keeps_non_block_tensors_from_all_parts() {
        let dir =
            std::env::temp_dir().join(format!("pom-qwen4exp-multipart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source_path = qwen4exp_multipart_fixture(&dir);
        let output_path = dir.join("slice.gguf");

        let source = crate::multipart::GgufSource::open(&source_path).unwrap();
        assert_eq!(source.parts().len(), 3);
        assert_eq!(source.tensors().len(), 10);

        split(&source_path, &output_path, 0, 3, true, true, |_| {}).unwrap();
        let output = crate::multipart::GgufSource::open(&output_path).unwrap();
        assert_eq!(output.tensors().len(), 10);
        assert_eq!(
            multipart_tensor_data(&output, "per_layer_token_embd.weight"),
            vec![0x22; 32]
        );
        assert_eq!(
            multipart_tensor_data(&output, "output_hc_down.weight"),
            vec![0x34; 32]
        );
        assert_eq!(
            multipart_tensor_data(&output, "blk.3.attn_q.weight"),
            vec![0x43; 32]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn qwen4exp_ple_structural_arrays_survive_merge() {
        let dir = std::env::temp_dir().join(format!("pom-qwen4exp-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("whole.gguf");
        let first = dir.join("first.gguf");
        let second = dir.join("second.gguf");
        let merged = dir.join("merged.gguf");
        std::fs::write(&whole, qwen4exp_test_gguf_bytes()).unwrap();

        split(&whole, &first, 0, 23, true, false, |_| {}).unwrap();
        split(&whole, &second, 24, 47, false, true, |_| {}).unwrap();
        merge(
            &[
                MergePart {
                    path: first,
                    start_layer: 0,
                    end_layer: 23,
                },
                MergePart {
                    path: second,
                    start_layer: 24,
                    end_layer: 47,
                },
            ],
            &merged,
            |_| {},
        )
        .unwrap();

        let output = std::fs::read(&merged).unwrap();
        assert_eq!(kv_i32_array(&output, "qwen4exp.ple.layers"), Some(vec![1]));
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.layer_multipliers"),
            Some(vec![23703573157769, 20109073645365, 8052911324071])
        );
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.head_offsets"),
            Some((0..16).map(|head| head * 20).collect())
        );
        assert_eq!(
            kv_u64_array(&output, "qwen4exp.ple.head_vocab_sizes"),
            Some((0..16).map(|head| 20 + head).collect())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "requires the real Gemma4 GGUF and several GB free on disk/RAM"]
    fn real_gemma4_slice_truncates_layer_metadata_and_loads() {
        let source = std::env::var_os("POM_REAL_GEMMA_GGUF")
            .expect("POM_REAL_GEMMA_GGUF must point to the real GGUF");
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

        unsafe { llama_engine::llama_cpp_sys_2::llama_backend_init() };
        let path = std::ffi::CString::new(out.to_string_lossy().as_bytes()).unwrap();
        let mut params = unsafe { llama_engine::llama_cpp_sys_2::llama_model_default_params() };
        params.n_gpu_layers = 0;
        let model = unsafe {
            llama_engine::llama_cpp_sys_2::llama_model_load_from_file(path.as_ptr(), params)
        };
        assert!(!model.is_null(), "the real Gemma4 slice should load");
        unsafe { llama_engine::llama_cpp_sys_2::llama_model_free(model) };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "requires the real DeepSeek4 GGUF and several GB free on disk/RAM"]
    fn real_deepseek4_intermediate_slice_copies_heads_and_loads() {
        let source = std::env::var_os("POM_REAL_DEEPSEEK4_GGUF")
            .expect("POM_REAL_DEEPSEEK4_GGUF must point to the real GGUF");
        let source = Path::new(&source);
        let dir = std::env::temp_dir().join(format!("pom-deepseek4-slice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("deepseek4-15-28.gguf");

        let layout = crate::gguf::read_layout(source).unwrap();
        assert!(layout.layer_count() > 28, "the real GGUF needs layer 28");
        assert!(
            layout.required_head_bytes() > 0,
            "the DeepSeek4 head must be in the source"
        );
        split(source, &out, 15, 28, false, false, |_| {}).unwrap();

        unsafe { llama_engine::llama_cpp_sys_2::llama_backend_init() };
        let path = std::ffi::CString::new(out.to_string_lossy().as_bytes()).unwrap();
        let mut params = unsafe { llama_engine::llama_cpp_sys_2::llama_model_default_params() };
        params.n_gpu_layers = 0;
        let model = unsafe {
            llama_engine::llama_cpp_sys_2::llama_model_load_from_file(path.as_ptr(), params)
        };
        assert!(
            !model.is_null(),
            "an intermediate DeepSeek4 slice should load"
        );
        unsafe { llama_engine::llama_cpp_sys_2::llama_model_free(model) };
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
        assert!(error.contains("GGML dtype 99") && error.contains("token_embd.weight"));
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
                && layout_error.contains("length 3")
        );
        let split_error = split(&whole, &out, 0, 0, true, true, |_| {})
            .unwrap_err()
            .to_string();
        assert!(
            split_error.contains("llama.future_layer_metadata") && split_error.contains("length 3")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
