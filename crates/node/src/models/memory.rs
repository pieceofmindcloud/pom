/// Actual memory loading for assigned model layers.
///
/// Strategy (in order of preference):
/// 1. Parse the Ollama GGUF blob, find tensors for our layer range, mmap them.
/// 2. Proportional mmap fallback: map a fraction of the blob by byte offset.
/// 3. Allocate a zeroed Vec<u8> of the required size (forces OS page commitment).
use anyhow::Result;
use memmap2::Mmap;
use serde_json::Value;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

// ── Public types ──────────────────────────────────────────────────────────────

pub struct LayerBuffer {
    _inner: Inner,
    pub size_bytes: usize,
    pub source: &'static str,
}

// Fields are intentionally held (not read) to keep memory alive.
#[allow(dead_code)]
enum Inner {
    Mapped(Mmap),
    Allocated(Vec<u8>),
}

impl LayerBuffer {
    pub fn size_mb(&self) -> f64 {
        self.size_bytes as f64 / 1_048_576.0
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Load the assigned layers into physical memory.
/// Keeps the buffer alive for the lifetime of the return value.
pub fn load_layer_memory(
    model_name: &str,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    size_mb: f64,
) -> LayerBuffer {
    if let Some(blob) = find_ollama_blob(model_name) {
        // Try GGUF-aware mapping first, fall back to proportional
        match map_gguf_layers(&blob, start_layer, end_layer)
            .or_else(|_| map_proportional(&blob, start_layer, end_layer, total_layers))
        {
            Ok(buf) => {
                info!(
                    "✅ Mapped {:.1}MB from model file (layers {}-{})",
                    buf.size_mb(),
                    start_layer,
                    end_layer
                );
                return buf;
            }
            Err(e) => warn!("mmap failed: {} — falling back to RAM allocation", e),
        }
    } else {
        info!("Model file not found in ~/.ollama — allocating reserved RAM");
    }

    allocate_ram(size_mb)
}

// ── Ollama blob discovery ─────────────────────────────────────────────────────

fn find_ollama_blob(model_name: &str) -> Option<PathBuf> {
    // `catalog::home_dir` já sabe que no Windows quem responde é USERPROFILE.
    let base = crate::catalog::home_dir()?.join(".ollama").join("models");

    let (name, tag) = model_name.split_once(':').unwrap_or((model_name, "latest"));

    let manifest_path = base
        .join("manifests")
        .join("registry.ollama.ai")
        .join("library")
        .join(name)
        .join(tag);

    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest: Value = serde_json::from_str(&content).ok()?;

    // The model blob has mediaType containing "model" but not "config"
    let digest = manifest["layers"].as_array()?.iter().find(|l| {
        l["mediaType"]
            .as_str()
            .map(|t| t.contains("model") && !t.contains("config"))
            .unwrap_or(false)
    })?["digest"]
        .as_str()?
        .replace(':', "-");

    let blob = base.join("blobs").join(&digest);
    blob.exists().then_some(blob)
}

// ── GGUF-aware mmap ───────────────────────────────────────────────────────────

fn map_gguf_layers(path: &Path, start_layer: u32, end_layer: u32) -> Result<LayerBuffer> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };

    if mmap.len() < 8 || &mmap[..4] != b"GGUF" {
        return Err(anyhow::anyhow!("not a GGUF file"));
    }

    let (data_start, off_lo, off_hi) = gguf_layer_range(&mmap, start_layer, end_layer)?;

    let byte_lo = data_start + off_lo as usize;
    let byte_hi = (data_start + off_hi as usize).min(mmap.len());

    if byte_hi <= byte_lo {
        return Err(anyhow::anyhow!("empty layer range in GGUF"));
    }

    // Touch every page to bring it into physical RAM
    let _ = mmap[byte_lo..byte_hi]
        .chunks(4096)
        .fold(0u8, |acc, c| acc ^ c[0]);

    let size_bytes = byte_hi - byte_lo;
    Ok(LayerBuffer {
        _inner: Inner::Mapped(mmap),
        size_bytes,
        source: "mmap/gguf",
    })
}

/// Fallback: divide the file proportionally across total_layers.
fn map_proportional(
    path: &Path,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
) -> Result<LayerBuffer> {
    let file = File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    let len = mmap.len();

    let lo = (len as f64 * start_layer as f64 / total_layers as f64) as usize;
    let hi = (len as f64 * (end_layer + 1) as f64 / total_layers as f64).min(len as f64) as usize;

    let _ = mmap[lo..hi].chunks(4096).fold(0u8, |acc, c| acc ^ c[0]);

    let size_bytes = hi - lo;
    Ok(LayerBuffer {
        _inner: Inner::Mapped(mmap),
        size_bytes,
        source: "mmap/proportional",
    })
}

// ── RAM allocation fallback ───────────────────────────────────────────────────

fn allocate_ram(size_mb: f64) -> LayerBuffer {
    let size_bytes = (size_mb * 1_048_576.0) as usize;
    let data = vec![0; size_bytes];
    info!("💾 Reserved {:.1}MB RAM", size_mb);
    LayerBuffer {
        _inner: Inner::Allocated(data),
        size_bytes,
        source: "allocated/ram",
    }
}

// ── GGUF header parser ────────────────────────────────────────────────────────

/// Returns (data_section_byte_offset, layer_range_start, layer_range_end).
/// Offsets within range are relative to the data section start.
fn gguf_layer_range(raw: &[u8], start_layer: u32, end_layer: u32) -> Result<(usize, u64, u64)> {
    let mut c = Cursor::new(raw);
    c.set_position(4); // skip "GGUF" magic

    let version = read_u32(&mut c)?;
    if version < 2 {
        return Err(anyhow::anyhow!("unsupported GGUF version {}", version));
    }

    let n_tensors = read_u64(&mut c)?;
    let n_kv = read_u64(&mut c)?;

    // Skip all key-value metadata entries
    for _ in 0..n_kv {
        skip_string(&mut c)?; // key
        let vt = read_u32(&mut c)?;
        skip_value(&mut c, vt)?; // value
    }

    // Collect tensor (name, offset) pairs
    let mut tensors: Vec<(String, u64)> = Vec::with_capacity(n_tensors as usize);
    for _ in 0..n_tensors {
        let name = read_string(&mut c)?;
        let n_dims = read_u32(&mut c)? as usize;
        for _ in 0..n_dims {
            read_u64(&mut c)?; // dimension size (not needed here)
        }
        read_u32(&mut c)?; // type_id
        let offset = read_u64(&mut c)?;
        tensors.push((name, offset));
    }

    // Data section starts at the next alignment boundary after the header
    let header_end = c.position() as usize;
    let alignment = 32usize;
    let data_start = header_end.div_ceil(alignment) * alignment;

    // Filter tensors belonging to our layer range ("blk.<N>.*")
    let mut offsets: Vec<u64> = tensors
        .iter()
        .filter_map(|(name, offset)| {
            block_num(name).filter(|&n| n >= start_layer && n <= end_layer)?;
            Some(*offset)
        })
        .collect();

    if offsets.is_empty() {
        return Err(anyhow::anyhow!(
            "no tensors for layers {}-{}",
            start_layer,
            end_layer
        ));
    }

    offsets.sort_unstable();
    let lo = *offsets.first().unwrap();
    let hi_base = *offsets.last().unwrap();

    // Estimate end of last tensor by finding the next tensor after hi_base
    let mut all_offsets: Vec<u64> = tensors.iter().map(|(_, o)| *o).collect();
    all_offsets.sort_unstable();
    let hi = all_offsets
        .windows(2)
        .find(|w| w[0] >= hi_base)
        .map(|w| w[1])
        .unwrap_or(hi_base + 64 * 1024 * 1024); // 64MB fallback

    Ok((data_start, lo, hi))
}

fn block_num(name: &str) -> Option<u32> {
    name.strip_prefix("blk.")?.split('.').next()?.parse().ok()
}

// ── Primitive readers ─────────────────────────────────────────────────────────

fn read_u8(c: &mut Cursor<&[u8]>) -> Result<u8> {
    let mut b = [0u8; 1];
    c.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(c: &mut Cursor<&[u8]>) -> Result<u16> {
    let mut b = [0u8; 2];
    c.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(c: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut b = [0u8; 4];
    c.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(c: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut b = [0u8; 8];
    c.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string(c: &mut Cursor<&[u8]>) -> Result<String> {
    let len = read_u64(c)? as usize;
    let mut buf = vec![0u8; len];
    c.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn skip_string(c: &mut Cursor<&[u8]>) -> Result<()> {
    let len = read_u64(c)?;
    c.set_position(c.position() + len);
    Ok(())
}

fn skip_value(c: &mut Cursor<&[u8]>, vtype: u32) -> Result<()> {
    match vtype {
        0 | 7 => {
            read_u8(c)?;
        } // u8, bool
        1 => {
            read_u8(c)?;
        } // i8 (same byte size)
        2 | 3 => {
            read_u16(c)?;
        } // u16, i16
        4..=6 => {
            read_u32(c)?;
        } // u32, i32, f32
        8 => {
            skip_string(c)?;
        } // string
        9 => {
            // array
            let elem_type = read_u32(c)?;
            let count = read_u64(c)?;
            for _ in 0..count {
                skip_value(c, elem_type)?;
            }
        }
        10..=12 => {
            read_u64(c)?;
        } // u64, i64, f64
        t => return Err(anyhow::anyhow!("unknown GGUF value type {}", t)),
    }
    Ok(())
}
