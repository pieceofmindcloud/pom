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

import argparse
import struct
import sys
from pathlib import Path

# ── GGUF constants ─────────────────────────────────────────────────────────────

GGUF_MAGIC     = 0x46554747
GGUF_VERSION_2 = 2
GGUF_VERSION_3 = 3

# dtype → (bytes_per_element_or_block, elements_per_block)
_GGML_TYPE_INFO: dict[int, tuple[int, int]] = {
    0:  (4,  1),    # F32
    1:  (2,  1),    # F16
    2:  (18, 32),   # Q4_0
    3:  (20, 32),   # Q4_1
    6:  (18, 32),   # Q5_0
    7:  (20, 32),   # Q5_1
    8:  (34, 32),   # Q8_0
    10: (18, 256),  # Q2_K
    11: (12, 256),  # Q3_K_S
    12: (52, 256),  # Q4_K_S / Q4_K_M
    13: (36, 256),  # Q5_K_S / Q5_K_M
    14: (210, 256), # Q6_K
    15: (34, 256),  # Q8_K
    16: (2,  1),    # BF16
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
    def __init__(self, data: bytes):
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


class TensorInfo:
    """A tensor that lives in the source GGUF."""
    def __init__(self, name: str, dims: list[int], dtype: int, offset: int):
        self.name = name; self.dims = dims; self.dtype = dtype; self.offset = offset

    def byte_size(self) -> int:
        n = 1
        for d in self.dims:
            n *= d
        if self.dtype not in _GGML_TYPE_INFO:
            raise ValueError(f"Unknown dtype {self.dtype} for '{self.name}'")
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
        self._raw = path.read_bytes()
        r = Reader(self._raw)

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

    def tensor_data(self, t: TensorInfo) -> bytes:
        start = self.data_offset + t.offset
        return self._raw[start:start + t.byte_size()]

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
    elif dtype == 16: # BF16
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

def split_model(
    src_path: Path,
    start_layer: int,
    end_layer: int,
    output_path: Path,
    is_first: bool,
    is_last: bool,
) -> None:
    print(f"Reading {src_path} ...")
    gguf = GGUFFile(src_path)

    n_split_layers = end_layer - start_layer + 1

    # ── Architecture params ────────────────────────────────────────────────────
    kv = gguf.kv_dict()
    arch       = kv.get("general.architecture", "llama")
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
        
        # Output norm & head
        if name in ("output_norm.weight", "output_norm.bias", "output.weight"):
            # Only include REAL tensors for the last node.
            # For other nodes, we'll inject fake ones in extra_tensors.
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

    # ── Fake tensors for non-last nodes ────────────────────────────────────────
    extra_tensors: list[FakeTensor] = []
    if not is_last:
        scale = 100000.0 # sqrt(BYPASS_EPS)
        extra_tensors.append(_fake_output_norm(gguf, hidden_dim, scale=scale))
        print(f"Injecting BYPASS fake output_norm.weight (scale={scale})")

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
    
    # We use a huge epsilon for non-last nodes to make RMSNorm an identity.
    # output = input * weight / sqrt(RMS(input)^2 + eps)
    # If eps=1e10 and weight=100000.0, output = input * 100000 / 100000 = input.
    BYPASS_EPS = 1e10
    BYPASS_SCALE = 100000.0

    for entry in gguf.kv:
        if entry.key.endswith(".block_count"):
            new_kv.append(KVEntry(entry.key, entry.vtype, n_split_layers))
            updated_block_count = True
        elif not is_last and entry.key == f"{arch}.attention.layer_norm_rms_epsilon":
            new_kv.append(KVEntry(entry.key, entry.vtype, BYPASS_EPS))
        else:
            new_kv.append(entry)
    
    # If we didn't find a .block_count key to update, and we know the arch, try to add it
    if not updated_block_count:
        new_kv.append(KVEntry(f"{arch}.block_count", GGUF_VAL_UINT32, n_split_layers))

    # ── Pre-calculate header size to find data_offset ──────────────────────────
    # ... rest of logic remains same, but we use BYPASS_SCALE for fake norm ...
    ALIGN = 32
    
    # Calculate metadata size
    meta_size = 4 + 4 + 8 + 8 # magic, version, n_tensors, n_kv
    for entry in new_kv:
        meta_size += len(_pack_kv(entry))
    
    # Calculate tensor info size
    for t in all_tensors:
        # name, n_dims, dims, dtype, offset
        name = _remap(t.name)
        meta_size += 8 + len(name.encode("utf-8")) # string length + bytes
        meta_size += 4 # n_dims
        meta_size += 8 * len(t.dims)
        meta_size += 4 # dtype
        meta_size += 8 # offset
    
    data_offset = (meta_size + ALIGN - 1) & ~(ALIGN - 1)

    # ── Build data section (32-byte aligned) ───────────────────────────────────
    data_parts: list[bytes] = []
    new_offsets: list[int]  = []
    cur_offset = 0

    for t in all_tensors:
        new_offsets.append(cur_offset)
        if isinstance(t, FakeTensor):
            blob = t._data
        else:
            blob = gguf.tensor_data(t)
        data_parts.append(blob)
        pad = (ALIGN - (len(blob) % ALIGN)) % ALIGN
        data_parts.append(b"\x00" * pad)
        cur_offset += len(blob) + pad

    # ── Assemble header ────────────────────────────────────────────────────────
    header  = struct.pack("<I", GGUF_MAGIC)
    header += struct.pack("<I", gguf.version)
    header += struct.pack("<Q", len(all_tensors))
    header += struct.pack("<Q", len(new_kv))

    for entry in new_kv:
        header += _pack_kv(entry)

    for t, off in zip(all_tensors, new_offsets):
        header += _pack_tensor_header(_remap(t.name), t.dims, t.dtype, off)

    pad_len  = data_offset - len(header)
    header  += b"\x00" * pad_len

    # ── Write output ───────────────────────────────────────────────────────────
    output_path.parent.mkdir(parents=True, exist_ok=True)
    print(f"Writing {output_path} ...")
    with output_path.open("wb") as f:
        f.write(header)
        for part in data_parts:
            f.write(part)

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

    split_model(
        src_path=src,
        start_layer=args.start_layer,
        end_layer=args.end_layer,
        output_path=out,
        is_first=args.is_first,
        is_last=args.is_last,
    )


if __name__ == "__main__":
    main()
