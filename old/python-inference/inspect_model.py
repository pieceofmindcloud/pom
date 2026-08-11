
import struct
import sys
from pathlib import Path

# Copy classes from split_model.py for reliability
GGUF_MAGIC     = 0x46554747
GGUF_VERSION_2 = 2
GGUF_VERSION_3 = 3

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

class Reader:
    def __init__(self, data: bytes):
        self._d = data
        self._p = 0
    @property
    def pos(self) -> int: return self._p
    def read(self, n: int) -> bytes:
        c = self._d[self._p:self._p + n]
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
        if vtype == GGUF_VAL_STRING: return self.read_string()
        if vtype == GGUF_VAL_ARRAY:
            elem_type = self.unpack("<I")
            count     = self.unpack("<Q")
            return [self.read_value(elem_type) for _ in range(count)]
        return f"unknown({vtype})"

class GGUFFile:
    def __init__(self, path: Path):
        self._raw = path.read_bytes()
        r = Reader(self._raw)
        magic = r.unpack("<I")
        version = r.unpack("<I")
        n_tensors = r.unpack("<Q")
        n_kv = r.unpack("<Q")
        print(f"Magic: {magic:#x}, Version: {version}, Tensors: {n_tensors}, KV: {n_kv}")
        self.kv = {}
        for _ in range(n_kv):
            key = r.read_string()
            vtype = r.unpack("<I")
            self.kv[key] = r.read_value(vtype)
        self.tensors = []
        for _ in range(n_tensors):
            name = r.read_string()
            n_dims = r.unpack("<I")
            dims = [r.unpack("<Q") for _ in range(n_dims)]
            dtype = r.unpack("<I")
            offset = r.unpack("<Q")
            self.tensors.append((name, dims, dtype, offset))
        alignment = 32
        self.data_offset = (r.pos + alignment - 1) & ~(alignment - 1)
        print(f"Header end: {r.pos}, Data offset: {self.data_offset}")

def inspect(path):
    print(f"Inspecting {path}")
    g = GGUFFile(Path(path))
    for name, dims, dtype, offset in g.tensors:
        if "blk.0" in name or "token_embd" in name or "output" in name:
            print(f"  Tensor: {name:30} dims={str(dims):20} dtype={dtype:2} offset={offset:12} abs_offset={g.data_offset + offset:12}")

if __name__ == "__main__":
    inspect(sys.argv[1])
