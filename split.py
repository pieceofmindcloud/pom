"""
split.py — Detect layer count from a GGUF model and split it into N slices.

Usage:
    python split.py \\
        --model <path-to.gguf> \\
        --qtd <number-of-splits> \\
        --output <base-output-path.gguf>

Example:
    python split.py \\
        --model ~/.ollama/models/blobs/sha256-<hash> \\
        --qtd 3 \\
        --output ./models/gemma2.gguf

The base output name has the layer range inserted before the extension:
    ./models/gemma2_0_13.gguf
    ./models/gemma2_14_27.gguf
    ./models/gemma2_28_41.gguf
"""

import argparse
import struct
import subprocess
import sys
from pathlib import Path

# ── Minimal GGUF KV reader (no extra deps) ────────────────────────────────────

GGUF_MAGIC = 0x46554747

GGUF_VAL_UINT8   = 0
GGUF_VAL_INT8    = 1
GGUF_VAL_UINT16  = 2
GGUF_VAL_INT16   = 3
GGUF_VAL_UINT32  = 4
GGUF_VAL_INT32   = 5
GGUF_VAL_FLOAT32 = 6
GGUF_VAL_BOOL    = 7
GGUF_VAL_STRING  = 8
GGUF_VAL_ARRAY   = 9
GGUF_VAL_UINT64  = 10
GGUF_VAL_INT64   = 11
GGUF_VAL_FLOAT64 = 12

_SCALAR_FMT = {
    GGUF_VAL_UINT8:   ("<B", 1),
    GGUF_VAL_INT8:    ("<b", 1),
    GGUF_VAL_UINT16:  ("<H", 2),
    GGUF_VAL_INT16:   ("<h", 2),
    GGUF_VAL_UINT32:  ("<I", 4),
    GGUF_VAL_INT32:   ("<i", 4),
    GGUF_VAL_FLOAT32: ("<f", 4),
    GGUF_VAL_BOOL:    ("<B", 1),
    GGUF_VAL_UINT64:  ("<Q", 8),
    GGUF_VAL_INT64:   ("<q", 8),
    GGUF_VAL_FLOAT64: ("<d", 8),
}


def _read_gguf_kv(path: Path) -> dict[str, object]:
    """Read only the KV metadata section of a GGUF file (sequential, no full-file load)."""
    with path.open("rb") as f:

        def read(n):
            data = f.read(n)
            if len(data) != n:
                raise EOFError(f"Need {n} bytes at offset {f.tell() - len(data)}, got {len(data)}")
            return data

        def unpack(fmt, size):
            return struct.unpack(fmt, read(size))[0]

        def read_string():
            length = unpack("<Q", 8)
            return read(length).decode("utf-8")

        def skip_value(vtype):
            if vtype in _SCALAR_FMT:
                _, size = _SCALAR_FMT[vtype]
                f.seek(size, 1)
            elif vtype == GGUF_VAL_STRING:
                f.seek(unpack("<Q", 8), 1)
            elif vtype == GGUF_VAL_ARRAY:
                elem_type = unpack("<I", 4)
                count = unpack("<Q", 8)
                if elem_type in _SCALAR_FMT:
                    _, elem_size = _SCALAR_FMT[elem_type]
                    f.seek(count * elem_size, 1)
                else:
                    for _ in range(count):
                        skip_value(elem_type)
            else:
                raise ValueError(f"Unknown GGUF value type {vtype}")

        def read_value(vtype):
            if vtype in _SCALAR_FMT:
                fmt, size = _SCALAR_FMT[vtype]
                return unpack(fmt, size)
            elif vtype == GGUF_VAL_STRING:
                return read_string()
            elif vtype == GGUF_VAL_ARRAY:
                elem_type = unpack("<I", 4)
                count = unpack("<Q", 8)
                return [read_value(elem_type) for _ in range(count)]
            else:
                raise ValueError(f"Unknown GGUF value type {vtype}")

        magic = unpack("<I", 4)
        if magic != GGUF_MAGIC:
            raise ValueError(f"Not a GGUF file (magic={magic:#x})")
        unpack("<I", 4)   # version
        unpack("<Q", 8)   # n_tensors
        n_kv = unpack("<Q", 8)

        kv: dict[str, object] = {}
        for _ in range(n_kv):
            key   = read_string()
            vtype = unpack("<I", 4)
            value = read_value(vtype)
            kv[key] = value

        return kv


def detect_layers(kv: dict) -> int:
    """Extract layer count from an already-parsed KV dict (avoids re-reading the file)."""
    arch = kv.get("general.architecture", "llama")
    block_count = kv.get(f"{arch}.block_count")
    if block_count is None:
        for k, v in kv.items():
            if k.endswith(".block_count"):
                block_count = v
                break
    if block_count is None:
        raise ValueError(
            "Could not find block_count in model metadata. "
            "Keys available: " + ", ".join(kv.keys())
        )
    return int(block_count)


# ── Chunk calculation ─────────────────────────────────────────────────────────

def compute_chunks(total_layers: int, qtd: int, is_gemma2: bool = False) -> list[tuple[int, int]]:
    """Divide [0, total_layers-1] into qtd roughly-equal chunks."""
    if qtd > total_layers:
        raise ValueError(f"Cannot split {total_layers} layers into {qtd} chunks")
    
    base = total_layers // qtd
    remainder = total_layers % qtd
    chunks = []
    start = 0
    for i in range(qtd):
        if i == qtd - 1:
            end = total_layers - 1
        else:
            # Distribute the remainder across the first chunks
            size = base + (1 if i < remainder else 0)
            end = start + size - 1
            
            # CRITICAL for Gemma 2: Preserve attention parity (Even=SWA, Odd=Global)
            # If we remap, the NEXT slice's start index (which will become its layer 0)
            # MUST have the same parity as the original start index.
            # Since we remap original layer L to new layer 0, parity(L) MUST be Even (0).
            # So the next slice MUST start on an EVEN layer.
            if is_gemma2 and (end + 1) % 2 != 0:
                # If next start is odd, extend this slice by 1 layer
                end += 1
        
        chunks.append((start, end))
        start = end + 1
    return chunks


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Auto-split a GGUF model for pipeline inference")
    parser.add_argument("--model",  required=True, help="Source GGUF model path")
    parser.add_argument("--qtd",    required=True, type=int, help="Number of splits")
    parser.add_argument("--output", required=True, help="Base output path (e.g. ./models/gemma2.gguf)")
    args = parser.parse_args()

    model_path  = Path(args.model).expanduser()
    output_base = Path(args.output)

    if not model_path.exists():
        sys.exit(f"Model not found: {model_path}")
    if args.qtd < 1:
        sys.exit("--qtd must be >= 1")

    output_base.parent.mkdir(parents=True, exist_ok=True)
    stem = output_base.stem   # e.g. "gemma2"
    ext  = output_base.suffix or ".gguf"

    # ── Detect layer count & arch ─────────────────────────────────────────────
    print(f"Reading model metadata: {model_path}")
    kv = _read_gguf_kv(model_path)
    arch = str(kv.get("general.architecture", "llama")).lower()
    is_gemma2 = "gemma2" in arch
    total_layers = detect_layers(kv)  # reuse already-parsed KV — don't re-read the file
    
    if is_gemma2:
        print("Gemma 2 detected — ensuring even layer boundaries for parity.")
        
    # ── Compute layer ranges ──────────────────────────────────────────────────
    chunks = compute_chunks(total_layers, args.qtd, is_gemma2=is_gemma2)
    print(f"Detected {total_layers} layers — splitting into {len(chunks)} slice(s)")
    print()
    n = len(chunks)

    # ── Run split_model.py for each chunk ─────────────────────────────────────
    split_script = Path(__file__).parent / "split_model.py"

    for i, (start, end) in enumerate(chunks):
        is_first = (i == 0)
        is_last  = (i == n - 1)

        out_file = output_base.parent / f"{stem}_{start}_{end}{ext}"

        cmd = [
            sys.executable, str(split_script),
            "--model",       str(model_path),
            "--start-layer", str(start),
            "--end-layer",   str(end),
            "--output",      str(out_file),
        ]
        if is_first:
            cmd.append("--is-first")
        if is_last:
            cmd.append("--is-last")

        tag = " ".join(
            filter(None, ["[first]" if is_first else None, "[last]" if is_last else None])
        )
        print(f"─── Slice {i+1}/{n}: layers {start}–{end}  {tag}")
        print("  " + " ".join(cmd))
        print()

        result = subprocess.run(cmd)
        if result.returncode != 0:
            sys.exit(f"split_model.py failed for layers {start}–{end}")

    print("All slices written:")
    for start, end in chunks:
        out_file = output_base.parent / f"{stem}_{start}_{end}{ext}"
        if out_file.exists():
            size_mb = out_file.stat().st_size / (1024 * 1024)
            print(f"  {out_file}  ({size_mb:.0f} MB)")

    print()
    print("Start inference servers with:")
    for i, (start, end) in enumerate(chunks):
        out_file = output_base.parent / f"{stem}_{start}_{end}{ext}"
        print(f"  python inference_server.py --model {out_file} --start-layer {start} --end-layer {end} --port {8181 + i}")


if __name__ == "__main__":
    main()
