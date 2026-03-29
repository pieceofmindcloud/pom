
import struct
import sys
from pathlib import Path

GGUF_MAGIC = 0x46554747

def inspect_gguf(path):
    data = Path(path).read_bytes()
    pos = 0
    
    def unpack(fmt):
        nonlocal pos
        size = struct.calcsize(fmt)
        res = struct.unpack(fmt, data[pos:pos+size])[0]
        pos += size
        return res

    magic = unpack("<I")
    if magic != GGUF_MAGIC:
        print(f"Not a GGUF file: {magic:#x}")
        return
    
    version = unpack("<I")
    n_tensors = unpack("<Q")
    n_kv = unpack("<Q")
    
    print(f"GGUF Version: {version}")
    print(f"Tensors: {n_tensors}")
    print(f"KV pairs: {n_kv}")
    
    def read_str():
        nonlocal pos
        length = struct.unpack("<Q", data[pos:pos+8])[0]
        pos += 8
        s = data[pos:pos+length].decode("utf-8")
        pos += length
        return s

    def read_val(vtype):
        nonlocal pos
        if vtype == 0: # UINT8
            val = data[pos]; pos += 1; return val
        if vtype == 4: # UINT32
            val = struct.unpack("<I", data[pos:pos+4])[0]; pos += 4; return val
        if vtype == 5: # INT32
            val = struct.unpack("<i", data[pos:pos+4])[0]; pos += 4; return val
        if vtype == 6: # FLOAT32
            val = struct.unpack("<f", data[pos:pos+4])[0]; pos += 4; return val
        if vtype == 8: # STRING
            return read_str()
        if vtype == 9: # ARRAY
            etype = struct.unpack("<I", data[pos:pos+4])[0]; pos += 4
            count = struct.unpack("<Q", data[pos:pos+8])[0]; pos += 8
            return [read_val(etype) for _ in range(count)]
        # Add more if needed, but these are common
        pos += 4 # Skip unknown
        return f"unknown({vtype})"

    for _ in range(n_kv):
        key = read_str()
        vtype = unpack("<I")
        val = read_val(vtype)
        print(f"KV: {key} = {val}")

    print("\nTensors:")
    for _ in range(n_tensors):
        name = read_str()
        n_dims = unpack("<I")
        dims = [unpack("<Q") for _ in range(n_dims)]
        dtype = unpack("<I")
        offset = unpack("<Q")
        print(f"  {name} ({dims}) dtype={dtype}")

if __name__ == "__main__":
    inspect_gguf(sys.argv[1])
