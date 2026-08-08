"""
Split a GGUF model file into per-peer slices for rdma-p2p pipeline inference.

Each slice is a *valid standalone GGUF* that llama.cpp can load:
  - All slices:        blk.<N>.* for N in [start_layer, end_layer] (remapped to 0..)
                       token_embd.weight  (real — needed for validation + last-node generation)
  - First / Middle:    output_norm.weight (FAKE — all-ones F32, shape [hidden_dim])
                       output.weight      omitted → weight-tied to token_embd by llama.cpp
  - Last slice:        output_norm.weight (real)
                       output.weight      (real, if present)

Hidden-state flow (inference_server.py):
  Node 1  → llama.eval(tokens)          → llama_get_embeddings_ith → hidden_state
  Node N  → llama_batch(embd=hidden)    → llama_get_embeddings_ith → hidden_state
  Node K  → llama_batch(embd=hidden)    → llama_get_logits_ith     → greedy decode

Notes:
  - Pure Python stdlib (struct, pathlib) — no extra deps.
  - KV key <arch>.block_count is updated to reflect layers in the slice.
"""

# `TensorInfo | None` in a signature is evaluated at def time before 3.10, and
# the only interpreter on the captain's Mac is the system 3.9 — without this the
# script dies on import and no slice is ever produced. Annotations only; nothing
# below changes behaviour.
from __future__ import annotations

import argparse
import mmap
import os
import struct
import sys
from pathlib import Path

# ── GGUF constants ─────────────────────────────────────────────────────────────

GGUF_MAGIC     = 0x46554747
GGUF_VERSION_2 = 2
GGUF_VERSION_3 = 3

# dtype → (bytes_per_block, elements_per_block)
#
# The codes are `enum ggml_type` and the sizes are the `blck_size`/`type_size`
# ggml itself reports; both were read straight from the llama.cpp that ships in
# ./venv (ggml.h for the codes, ggml_blck_size/ggml_type_size for the sizes)
# rather than written from memory. `test_split_model_type_table` in src/gguf.rs
# fails the build if this table and the Rust one ever drift apart again.
#
# Codes 4, 5 (Q4_2/Q4_3) and 31..33, 36..38 are types ggml removed, and 40..42
# (NVFP4, Q1_0, Q2_0) are new enough that their block layout is not cited here.
# All are absent on purpose: an unmeasurable file is refused, never guessed.
# K-quant super-block size is always QK_K=256 elements.
_GGML_TYPE_INFO: dict[int, tuple[int, int]] = {
    0:  (4,   1),    # F32
    1:  (2,   1),    # F16
    2:  (18,  32),   # Q4_0  = {half d; u8 qs[16]}
    3:  (20,  32),   # Q4_1  = {half d,m; u8 qs[16]}
    6:  (22,  32),   # Q5_0  = {half d; u8 qh[4]; u8 qs[16]}
    7:  (24,  32),   # Q5_1  = {half d,m; u8 qh[4]; u8 qs[16]}
    8:  (34,  32),   # Q8_0  = {half d; i8 qs[32]}
    9:  (36,  32),   # Q8_1  = {half2 ds; i8 qs[32]}
    10: (84,  256),  # Q2_K  = {u8 scales[16]; u8 qs[64]; half2 dm}
    11: (110, 256),  # Q3_K  = {u8 hmask[32]; u8 qs[64]; u8 scales[12]; half d}
    12: (144, 256),  # Q4_K  = {half2 dm; u8 scales[12]; u8 qs[128]}
    # Q5_K carries qs[QK_K/2]=128, not 64: it was booked as 112 bytes against a
    # real 176, so every Q5_K tensor was measured at 64% of its size.
    13: (176, 256),  # Q5_K  = {half2 dm; u8 scales[12]; u8 qh[32]; u8 qs[128]}
    14: (210, 256),  # Q6_K  = {u8 ql[128]; u8 qh[64]; i8 scales[16]; half d}
    15: (292, 256),  # Q8_K  = {float d; i8 qs[256]; i16 bsums[16]}
    # 16 is IQ2_XXS, *not* BF16 (BF16 is 30). As (2,1) it never raised: an
    # IQ2_XXS tensor was measured at 2 bytes/element instead of 66 bytes/256,
    # so the file was sliced with a silently wrong size instead of refused.
    16: (66,  256),  # IQ2_XXS = {half d; u16 qs[32]}
    17: (74,  256),  # IQ2_XS  = {half d; u16 qs[32]; u8 scales[8]}
    18: (98,  256),  # IQ3_XXS = {half d; u8 qs[96]}
    19: (50,  256),  # IQ1_S   = {half d; u8 qs[32]; u16 qh[16]}
    20: (18,  32),   # IQ4_NL  = {half d; u8 qs[16]}                (QK4_NL=32)
    21: (110, 256),  # IQ3_S   = {half d; u8 qs[64]; u8 qh[8]; u8 signs[32]; u8 scales[4]}
    22: (82,  256),  # IQ2_S   = {half d; u8 qs[64]; u8 qh[8]; u8 scales[8]}
    23: (136, 256),  # IQ4_XS  = {half d; u16 scales_h; u8 scales_l[4]; u8 qs[128]}
    24: (1,   1),    # I8
    25: (2,   1),    # I16
    26: (4,   1),    # I32
    27: (8,   1),    # I64
    28: (8,   1),    # F64
    29: (56,  256),  # IQ1_M   = {u8 qs[32]; u8 qh[16]; u8 scales[8]}  (no d: in scales)
    30: (2,   1),    # BF16
    34: (54,  256),  # TQ1_0 = {u8 qs[48]; u8 qh[4]; half d}
    35: (66,  256),  # TQ2_0 = {u8 qs[64]; half d}
    39: (17,  32),   # MXFP4 = {u8 e; u8 qs[16]}                     (QK_MXFP4=32)
}

# GGUFValueType codes
GGUF_VAL_UINT8   = 0;  GGUF_VAL_INT8    = 1
GGUF_VAL_UINT16  = 2;  GGUF_VAL_INT16   = 3
GGUF_VAL_UINT32  = 4;  GGUF_VAL_INT32   = 5
GGUF_VAL_FLOAT32 = 6;  GGUF_VAL_BOOL    = 7
GGUF_VAL_STRING  = 8;  GGUF_VAL_ARRAY   = 9
GGUF_VAL_UINT64  = 10; GGUF_VAL_INT64   = 11
GGUF_VAL_FLOAT64 = 12

_SCALAR_FMT = {
    GGUF_VAL_UINT8:   ("<B", 1), GGUF_VAL_INT8:    ("<b", 1),
    GGUF_VAL_UINT16:  ("<H", 2), GGUF_VAL_INT16:   ("<h", 2),
    GGUF_VAL_UINT32:  ("<I", 4), GGUF_VAL_INT32:   ("<i", 4),
    GGUF_VAL_FLOAT32: ("<f", 4), GGUF_VAL_BOOL:    ("<B", 1),
    GGUF_VAL_UINT64:  ("<Q", 8), GGUF_VAL_INT64:   ("<q", 8),
    GGUF_VAL_FLOAT64: ("<d", 8),
}

# ── Low-level reader ───────────────────────────────────────────────────────────

class Reader:
    def __init__(self, data):
        self._d = data
        self._p = 0

    @property
    def pos(self) -> int:
        return self._p

    def read(self, n: int) -> bytes:
        c = self._d[self._p:self._p + n]
        if len(c) != n:
            raise EOFError(f"Need {n} bytes at {self._p}, got {len(c)}")
        self._p += n
        return c

    def unpack(self, fmt: str) -> object:
        size = struct.calcsize(fmt)
        return struct.unpack(fmt, self.read(size))[0]

    def read_string(self) -> str:
        length = self.unpack("<Q")
        return self.read(length).decode("utf-8")

    def read_value(self, vtype: int):
        if vtype in _SCALAR_FMT:
            fmt, size = _SCALAR_FMT[vtype]
            return struct.unpack(fmt, self.read(size))[0]
        if vtype == GGUF_VAL_STRING:
            return self.read_string()
        if vtype == GGUF_VAL_ARRAY:
            elem_type = self.unpack("<I")
            count     = self.unpack("<Q")
            items = [self.read_value(elem_type) for _ in range(count)]
            return (elem_type, items)           # tuple so writer can recover elem_type
        raise ValueError(f"Unknown GGUF value type {vtype}")

# ── Data structures ────────────────────────────────────────────────────────────

class KVEntry:
    def __init__(self, key: str, vtype: int, value):
        self.key = key; self.vtype = vtype; self.value = value


class UnsupportedQuant(Exception):
    """A tensor type this script cannot measure.

    Raised instead of guessing a size: a wrong block size does not fail, it
    writes a slice that llama.cpp loads and quietly computes garbage from.
    `main()` prints it without the traceback, so the sentence reaches the
    interface readable.
    """
    def __init__(self, dtype: int, tensor_name: str):
        self.dtype = dtype
        self.tensor_name = tensor_name
        super().__init__(
            f"o arquivo usa uma quantização que este script ainda não conhece: "
            f"tipo ggml {dtype} (no tensor '{tensor_name}'). "
            f"Fatiar sem saber o tamanho do bloco produziria uma fatia corrompida, "
            f"então nada foi gravado. Acrescente o tipo {dtype} à tabela "
            f"_GGML_TYPE_INFO em split_model.py para habilitar este modelo."
        )


class TensorInfo:
    """A tensor that lives in the source GGUF."""
    def __init__(self, name: str, dims: list[int], dtype: int, offset: int):
        self.name = name; self.dims = dims; self.dtype = dtype; self.offset = offset

    def byte_size(self) -> int:
        n = 1
        for d in self.dims:
            n *= d
        if self.dtype not in _GGML_TYPE_INFO:
            raise UnsupportedQuant(self.dtype, self.name)
        bpe, blk = _GGML_TYPE_INFO[self.dtype]
        return ((n + blk - 1) // blk) * bpe


class FakeTensor:
    """A synthetic tensor (not in source GGUF) injected to satisfy llama.cpp validation."""
    def __init__(self, name: str, dims: list[int], dtype: int, data: bytes):
        self.name = name; self.dims = dims; self.dtype = dtype; self._data = data

    def byte_size(self) -> int:
        return len(self._data)


class GGUFFile:
    def __init__(self, path: Path):
        self._file = open(path, "rb")
        self._mm = mmap.mmap(self._file.fileno(), 0, access=mmap.ACCESS_READ)
        r = Reader(self._mm)

        magic   = r.unpack("<I")
        if magic != GGUF_MAGIC:
            raise ValueError(f"Not a GGUF file (magic={magic:#x})")
        version = r.unpack("<I")
        if version not in (GGUF_VERSION_2, GGUF_VERSION_3):
            raise ValueError(f"Unsupported GGUF version {version}")
        self.version = version

        n_tensors = r.unpack("<Q")
        n_kv      = r.unpack("<Q")

        self.kv: list[KVEntry] = []
        for _ in range(n_kv):
            key   = r.read_string()
            vtype = r.unpack("<I")
            value = r.read_value(vtype)
            self.kv.append(KVEntry(key, vtype, value))

        self.tensors: list[TensorInfo] = []
        for _ in range(n_tensors):
            name   = r.read_string()
            n_dims = r.unpack("<I")
            dims   = [r.unpack("<Q") for _ in range(n_dims)]
            dtype  = r.unpack("<I")
            offset = r.unpack("<Q")
            self.tensors.append(TensorInfo(name, dims, dtype, offset))

        alignment = 32
        self.data_offset = (r.pos + alignment - 1) & ~(alignment - 1)

        # Precompute actual aligned tensor sizes from offset differences.
        # More reliable than the type table — handles unknown quant types and
        # catches any table errors. Weight-tied tensors sharing an offset get
        # the same size entry.
        total_data = len(self._mm) - self.data_offset
        unique_offsets = sorted(set(t.offset for t in self.tensors))
        self._offset_sizes: dict[int, int] = {}
        for i, off in enumerate(unique_offsets):
            next_off = unique_offsets[i + 1] if i + 1 < len(unique_offsets) else total_data
            self._offset_sizes[off] = next_off - off

    def close(self):
        self._mm.close()
        self._file.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def tensor_size(self, t: TensorInfo) -> int:
        # The offset delta wins, but `t.byte_size()` is still evaluated on every
        # call — Python computes a `.get` default eagerly — and that is the only
        # thing that refuses an unmeasurable quant, since every tensor's offset
        # is a key here and the fallback never actually returns. Turning this
        # into a lazy lookup would slice unknown types silently. Keep it eager.
        return self._offset_sizes.get(t.offset, t.byte_size())

    def stream_tensor_to(self, f, t: TensorInfo, chunk: int = 8 << 20, on_bytes=None) -> int:
        """Write tensor data directly to f in chunks. Returns bytes written.

        `on_bytes(written_so_far)` is called once per chunk when given. It only
        reports; token_embd.weight alone can be most of a slice, and a bar that
        moves once per tensor sits still through the longest write of the run.
        """
        start = self.data_offset + t.offset
        size = self.tensor_size(t)
        pos, remaining = start, size
        mv = memoryview(self._mm)
        while remaining > 0:
            n = min(chunk, remaining)
            # memoryview slice: zero-copy read — no 8 MB bytes object on the Python heap
            f.write(mv[pos:pos + n])
            # Drop source pages from page cache after reading — prevents RAM buildup
            try:
                self._mm.madvise(mmap.MADV_DONTNEED, pos, n)
            except (AttributeError, OSError):
                pass
            pos += n
            remaining -= n
            if on_bytes is not None:
                on_bytes(size - remaining)
        return size

    def tensor_data(self, t: TensorInfo) -> bytes:
        start = self.data_offset + t.offset
        size = self.tensor_size(t)
        return bytes(self._mm[start:start + size])

    def kv_dict(self) -> dict:
        return {e.key: e.value for e in self.kv}


# ── GGUF writer helpers ────────────────────────────────────────────────────────

def _pack_str(s: str) -> bytes:
    b = s.encode("utf-8")
    return struct.pack("<Q", len(b)) + b

def _pack_value(vtype: int, value) -> bytes:
    if vtype in _SCALAR_FMT:
        fmt, _ = _SCALAR_FMT[vtype]
        return struct.pack(fmt, value)
    if vtype == GGUF_VAL_STRING:
        return _pack_str(value)
    if vtype == GGUF_VAL_ARRAY:
        elem_type, items = value
        out  = struct.pack("<I", elem_type)
        out += struct.pack("<Q", len(items))
        for item in items:
            out += _pack_value(elem_type, item)
        return out
    raise ValueError(f"Cannot pack vtype {vtype}")

def _pack_kv(e: KVEntry) -> bytes:
    return _pack_str(e.key) + struct.pack("<I", e.vtype) + _pack_value(e.vtype, e.value)

def _pack_tensor_header(name: str, dims: list[int], dtype: int, offset: int) -> bytes:
    out  = _pack_str(name)
    out += struct.pack("<I", len(dims))
    for d in dims:
        out += struct.pack("<Q", d)
    out += struct.pack("<I", dtype)
    out += struct.pack("<Q", offset)
    return out

# ── Fake tensor factories ──────────────────────────────────────────────────────

def _get_tensor_info(gguf: GGUFFile, name: str) -> TensorInfo | None:
    for t in gguf.tensors:
        if t.name == name:
            return t
    return None

def _fake_output_norm(gguf: GGUFFile, hidden_dim: int, scale: float = 1.0) -> FakeTensor:
    """Identity RMS-norm weights: all scale (default 1.0)."""
    # Try to match the dtype of the original output_norm if it exists
    orig = _get_tensor_info(gguf, "output_norm.weight")
    dtype = orig.dtype if orig else 0  # default to F32
    
    if dtype == 0: # F32
        data = struct.pack(f"<{hidden_dim}f", *([scale] * hidden_dim))
    elif dtype == 30: # BF16 (was 16 here, which is IQ2_XXS — packed 2-byte
                      # halves into what is really a 256-element block type)
        # To make it easy, we'll just pack the hex for common scales if needed.
        # For scale=1.0, hex is 0x3f80. For scale=100000.0, hex is 0x47c3.
        val = 0x3f80 if scale == 1.0 else 0x47c3 if scale == 100000.0 else 0x0000
        data = struct.pack(f"<{hidden_dim}H", *([val] * hidden_dim))
    elif dtype == 1: # F16
        # 1.0 is 0x3c00. 100000.0 is too large for F16 (overflows).
        # We'll fallback to F32 for large scales.
        if scale > 65504.0:
            data = struct.pack(f"<{hidden_dim}f", *([scale] * hidden_dim))
            dtype = 0
        else:
            # 1.0 in F16 is 0x3c00
            import numpy as np
            val = np.float16(scale).view(np.uint16)
            data = struct.pack(f"<{hidden_dim}H", *([val] * hidden_dim))
    else:
        # Fallback to F32
        data = struct.pack(f"<{hidden_dim}f", *([scale] * hidden_dim))
        dtype = 0

    return FakeTensor("output_norm.weight", [hidden_dim], dtype, data)

def _fake_output_head(gguf: GGUFFile, hidden_dim: int, vocab_size: int) -> FakeTensor:
    """Identity/Zero output head."""
    orig = _get_tensor_info(gguf, "output.weight")
    dtype = orig.dtype if orig else 0
    
    # We don't want to actually allocate a giant vocab*hidden matrix in memory
    # but for a split model it might be necessary if llama.cpp refuses to load.
    # However, many models tie output.weight to token_embd.weight.
    # For now, let's just use a very small one or skip it if not strictly required.
    # If llama.cpp FAILS to load without it, we'll need a better strategy.
    return None

# ── Main split function ────────────────────────────────────────────────────────

def layer_signatures(gguf: "GGUFFile") -> list[frozenset]:
    """Per-layer tensor-name shape, e.g. {'attn_norm.weight', 'ffn_up.weight', ...}.

    Two layers with different signatures are different *kinds* of layer.
    """
    per_layer: dict[int, set] = {}
    for t in gguf.tensors:
        if not t.name.startswith("blk."):
            continue
        parts = t.name.split(".", 2)
        if len(parts) < 3:
            continue
        try:
            idx = int(parts[1])
        except ValueError:
            continue
        per_layer.setdefault(idx, set()).add(parts[2])
    return [frozenset(per_layer[i]) for i in sorted(per_layer)]


def layer_pattern_period(sigs: list[frozenset]) -> int:
    """Smallest p with sigs[i] == sigs[i % p] for all i. 1 == every layer alike."""
    n = len(sigs)
    for p in range(1, n + 1):
        if all(sigs[i] == sigs[i % p] for i in range(n)):
            return p
    return n


def _check_layer_phase(gguf: "GGUFFile", start_layer: int, arch: str) -> None:
    """Refuse a cut that would silently change what each layer *is*.

    Hybrid models — Qwen3.5-DeepSeek-V4-Flash interleaves attention with Gated
    DeltaNet, and Mamba/Jamba-likes do the same — are not a stack of identical
    blocks. llama.cpp decides each layer's kind from its *index*, not from which
    tensors are present. Since a slice renumbers blk.<start+i> to blk.<i>, a
    start_layer that is not a multiple of the repeat period shifts every layer
    onto the wrong kind.

    Sometimes that fails loudly, as `missing tensor 'blk.0.ssm_conv1d.weight'`.
    The dangerous case is when the phase happens to line up on tensor names but
    not on semantics: the slice loads and only the answers are wrong. So check
    the phase up front rather than letting llama.cpp discover it later.
    """
    sigs = layer_signatures(gguf)
    period = layer_pattern_period(sigs)
    if period <= 1:
        return  # homogeneous stack: any cut point is fine
    if start_layer % period == 0:
        return
    valid = [n for n in range(0, len(sigs), period)]
    preview = ", ".join(str(v) for v in valid[:12]) + (" ..." if len(valid) > 12 else "")
    raise UnsupportedQuant(
        f"{arch} repeats a {period}-layer pattern (layer kinds are not all the "
        f"same), so a slice may only start on a multiple of {period} — "
        f"start-layer {start_layer} would renumber every layer onto the wrong "
        f"kind. Valid start layers: {preview}"
    )


def split_model(
    src_path: Path,
    start_layer: int,
    end_layer: int,
    output_path: Path,
    is_first: bool,
    is_last: bool,
    original_block_count: int | None = None,
) -> None:
    print(f"Reading {src_path} ...")
    gguf = GGUFFile(src_path)
    print(f"File size    : {src_path.stat().st_size / (1024**3):.2f} GB (mmap — not loaded into RAM)")

    n_split_layers = end_layer - start_layer + 1

    # ── Architecture params ────────────────────────────────────────────────────
    kv = gguf.kv_dict()
    arch       = kv.get("general.architecture", "llama")

    # Detect original total layers from the source file if not provided
    if original_block_count is None:
        orig_bc = kv.get(f"{arch}.block_count")
        if orig_bc is not None:
            original_block_count = int(orig_bc)
    hidden_dim = int(kv.get(f"{arch}.embedding_length", 4096))
    vocab_size = 0
    if "tokenizer.ggml.tokens" in kv:
        _, tokens = kv["tokenizer.ggml.tokens"]
        vocab_size = len(tokens)
    else:
        # Fallback to some common vocab size if not found
        vocab_size = 128256 # llama3

    print(f"Architecture : {arch}, hidden_dim = {hidden_dim}")
    print(f"Layers       : {start_layer}–{end_layer}  ({'first' if is_first else ''}{'last' if is_last else ''}{'middle' if not is_first and not is_last else ''})")

    _check_layer_phase(gguf, start_layer, arch)

    # ── Tensor selection ───────────────────────────────────────────────────────
    def _want(t: TensorInfo) -> bool:
        name = t.name
        # Assigned layer blocks (remapped below)
        if name.startswith("blk."):
            parts = name.split(".", 2)
            if len(parts) >= 2:
                try:
                    return start_layer <= int(parts[1]) <= end_layer
                except ValueError:
                    pass
            return False
        
        # Token embedding — include for ALL nodes:
        #   • first node  : real lookup used for tokenized input
        #   • middle/last : needed for llama.cpp validation
        if name == "token_embd.weight":
            return True
        
        # Output norm & head — only real tensors on the last node.
        # Non-last nodes get a fake all-ones output_norm injected below.
        if name in ("output_norm.weight", "output_norm.bias", "output.weight"):
            return is_last

        # Positional / RoPE tables — shared across all nodes
        if "rope" in name or "pos_embd" in name:
            return True

        return False

    # ── Tensor selection (Preserving source order) ─────────────────────────────
    real_tensors: list[TensorInfo] = []
    for t in gguf.tensors:
        if _want(t):
            real_tensors.append(t)

    # Non-last nodes need a fake identity output_norm so that llama_get_embeddings_ith
    # returns h_k/RMS(h_k).  When the next peer applies its own pre_attn_norm:
    #   pre_attn_norm(h_k/RMS(h_k), w) = w ⊙ (h_k/RMS(h_k)) / RMS(h_k/RMS(h_k))
    #                                   = w ⊙ (h_k/RMS(h_k))   [since RMS=1]
    # which equals pre_attn_norm(h_k, w) — mathematically identical to the
    # original model.  Using the real output_norm instead would distort the
    # direction of h_k (non-uniform w_out ≠ constant), causing garbage on
    # models where output_norm weights deviate significantly from 1.0.
    extra_tensors: list[FakeTensor] = []
    if not is_last:
        extra_tensors.append(_fake_output_norm(gguf, hidden_dim, scale=1.0))
        print(f"Injecting fake identity output_norm.weight (all-ones, hidden_dim={hidden_dim})")

    all_tensors: list[TensorInfo | FakeTensor] = real_tensors + extra_tensors

    if not all_tensors:
        raise ValueError(f"No tensors selected for layers {start_layer}–{end_layer}")

    print(f"Selected {len(real_tensors)} real + {len(extra_tensors)} fake tensors")

    # ── Remap layer names ──────────────────────────────────────────────────────
    def _remap(name: str) -> str:
        if name.startswith("blk."):
            parts = name.split(".", 2)
            new_idx = int(parts[1]) - start_layer
            suffix  = ("." + parts[2]) if len(parts) > 2 else ""
            return f"blk.{new_idx}{suffix}"
        return name

    # ── Update KV metadata ─────────────────────────────────────────────────────
    new_kv: list[KVEntry] = []
    updated_block_count = False

    for entry in gguf.kv:
        if entry.key.endswith(".block_count"):
            new_kv.append(KVEntry(entry.key, entry.vtype, n_split_layers))
            updated_block_count = True
        else:
            new_kv.append(entry)
    
    # If we didn't find a .block_count key to update, and we know the arch, try to add it
    if not updated_block_count:
        new_kv.append(KVEntry(f"{arch}.block_count", GGUF_VAL_UINT32, n_split_layers))

    # Store original model metadata so rdma-p2p peers can self-describe their
    # layer range and reconstruct total_layers without the original file.
    if original_block_count is not None:
        new_kv.append(KVEntry("rdmap2p.original_block_count", GGUF_VAL_UINT32, original_block_count))
    new_kv.append(KVEntry("rdmap2p.original_block_start", GGUF_VAL_UINT32, start_layer))
    new_kv.append(KVEntry("rdmap2p.original_block_end",   GGUF_VAL_UINT32, end_layer))

    ALIGN = 32

    # ── Pre-calculate tensor sizes and output offsets (no data loaded) ─────────
    tensor_sizes: list[int] = []
    new_offsets: list[int]  = []
    cur_offset = 0

    for t in all_tensors:
        new_offsets.append(cur_offset)
        size = len(t._data) if isinstance(t, FakeTensor) else gguf.tensor_size(t)
        tensor_sizes.append(size)
        cur_offset += (size + ALIGN - 1) & ~(ALIGN - 1)

    # ── Calculate header/metadata size to find data_offset ────────────────────
    meta_size = 4 + 4 + 8 + 8  # magic, version, n_tensors, n_kv
    for entry in new_kv:
        meta_size += len(_pack_kv(entry))
    for t in all_tensors:
        name = _remap(t.name)
        meta_size += 8 + len(name.encode("utf-8"))
        meta_size += 4 + 8 * len(t.dims) + 4 + 8

    data_offset = (meta_size + ALIGN - 1) & ~(ALIGN - 1)

    # ── Assemble header ────────────────────────────────────────────────────────
    header  = struct.pack("<I", GGUF_MAGIC)
    header += struct.pack("<I", gguf.version)
    header += struct.pack("<Q", len(all_tensors))
    header += struct.pack("<Q", len(new_kv))

    for entry in new_kv:
        header += _pack_kv(entry)

    for t, off in zip(all_tensors, new_offsets):
        header += _pack_tensor_header(_remap(t.name), t.dims, t.dtype, off)

    header += b"\x00" * (data_offset - len(header))

    # ── Stream output (one tensor at a time — no full model in RAM) ────────────
    output_path.parent.mkdir(parents=True, exist_ok=True)
    print(f"Writing {output_path} ...")
    written_bytes = 0
    total_data_bytes = sum((s + ALIGN - 1) & ~(ALIGN - 1) for s in tensor_sizes)

    last_reported = -1

    def _report(done: int) -> None:
        # Percentual em linha própria, no stderr, para quem estiver conduzindo
        # este script em vez de assistindo (ver plan::parse_progress). Uma vez a
        # cada ponto percentual: centenas de linhas iguais não ajudam ninguém.
        nonlocal last_reported
        pct = 100 * done / total_data_bytes
        if int(pct) != last_reported:
            last_reported = int(pct)
            print(f"RDMAP2P_PROGRESS {pct:.1f}", file=sys.stderr, flush=True)

    with output_path.open("wb") as f:
        f.write(header)
        for i, (t, size) in enumerate(zip(all_tensors, tensor_sizes)):
            name = _remap(t.name)
            file_pos_before = data_offset + written_bytes
            if isinstance(t, FakeTensor):
                f.write(t._data)
                _report(written_bytes + size)
            else:
                gguf.stream_tensor_to(
                    f, t, on_bytes=lambda done: _report(written_bytes + done))
            pad = (ALIGN - (size % ALIGN)) % ALIGN
            f.write(b"\x00" * pad)
            written_bytes += size + pad
            # Flush Python buffer → OS, then drop dirty output pages from page cache.
            # Without this, the OS accumulates dirty pages for the entire output file
            # (up to several GB) in RAM before writing to disk.
            f.flush()
            try:
                os.posix_fadvise(f.fileno(), file_pos_before, size + pad, os.POSIX_FADV_DONTNEED)
            except (AttributeError, OSError):
                pass
            pct = 100 * written_bytes / total_data_bytes
            print(f"  [{i+1}/{len(all_tensors)}] {name}  {size / (1024**2):.1f} MB"
                  f"  ({pct:.0f}%)", end="\r")
            _report(written_bytes)

    print()
    gguf.close()

    size_mb = output_path.stat().st_size / (1024 * 1024)
    print(f"Done — {output_path}  ({size_mb:.0f} MB)\n")

# ── CLI ────────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="Split a GGUF model into pipeline slices with identity heads"
    )
    parser.add_argument("--model",       required=True)
    parser.add_argument("--start-layer", type=int, required=True)
    parser.add_argument("--end-layer",   type=int, required=True)
    parser.add_argument("--output",      required=True)
    parser.add_argument("--is-first",    action="store_true")
    parser.add_argument("--is-last",     action="store_true")
    args = parser.parse_args()

    src = Path(args.model)
    out = Path(args.output)

    if not src.exists():
        sys.exit(f"Model not found: {src}")
    if args.start_layer > args.end_layer:
        sys.exit("--start-layer must be <= --end-layer")

    try:
        split_model(
            src_path=src,
            start_layer=args.start_layer,
            end_layer=args.end_layer,
            output_path=out,
            is_first=args.is_first,
            is_last=args.is_last,
        )
    except UnsupportedQuant as e:
        # The interface shows the tail of stderr. A traceback there is six lines
        # of our file paths around one useful sentence, so print just the
        # sentence — the condition is the file's, not a bug to report.
        sys.exit(str(e))


if __name__ == "__main__":
    main()
