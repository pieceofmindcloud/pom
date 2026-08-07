"""
Per-node inference server for rdma-p2p distributed pipeline.

Uses llama-cpp-python (GGUF backend) for partial forward passes via
hidden-state threading.

Each peer runs this server alongside the rdma-p2p binary.
  - First node:   prompt text -> token ids -> layers -> hidden state
  - Middle nodes: hidden state -> layers -> hidden state
  - Last node:    hidden state -> layers -> lm_head -> text

Usage:
    pip install fastapi uvicorn llama-cpp-python numpy
    python inference_server.py \\
        --model models/gemma2_0_20.gguf \\
        --start-layer 0 --end-layer 20 --port 8181
"""

import argparse
import base64
import json
import faulthandler
import struct
import threading
import time
import ctypes
import traceback
from typing import Optional

faulthandler.enable()  # print C-level traceback on SIGSEGV/SIGABRT

import numpy as np
import uvicorn
from fastapi import FastAPI, HTTPException, Request, Response
from pydantic import BaseModel

try:
    from llama_cpp import Llama
    import llama_cpp.llama_cpp as lc
except ImportError:
    raise SystemExit(
        "Missing dependencies. Install with:\n"
        "  pip install fastapi uvicorn llama-cpp-python numpy"
    )

# ── App & shared state ────────────────────────────────────────────────────────

app = FastAPI(title="rdma-p2p inference node (llama-cpp-python / GGUF)")

_llama: Optional[Llama] = None
_args = None
_n_embd: Optional[int] = None
_n_vocab: Optional[int] = None
_lock = threading.Lock()   # serializes individual GPU decode steps (one at a time)

# ── Minimal GGUF KV reader (no deps, reads metadata before model load) ────────

def _skip_gguf_value(f, vtype: int) -> None:
    """Skip one GGUF value in the binary stream."""
    if vtype in (0, 1, 7):   f.read(1)           # UINT8/INT8/BOOL
    elif vtype in (2, 3):    f.read(2)            # UINT16/INT16
    elif vtype in (4, 5, 6): f.read(4)            # UINT32/INT32/FLOAT32
    elif vtype in (10, 11, 12): f.read(8)         # UINT64/INT64/FLOAT64
    elif vtype == 8:                               # STRING
        f.read(struct.unpack("<Q", f.read(8))[0])
    elif vtype == 9:                               # ARRAY
        elem_type = struct.unpack("<I", f.read(4))[0]
        count     = struct.unpack("<Q", f.read(8))[0]
        for _ in range(count):
            _skip_gguf_value(f, elem_type)
    else:
        raise ValueError(f"Unknown GGUF vtype {vtype}")


def _read_gguf_rdmap2p_meta(model_path: str) -> dict:
    """Parse rdmap2p.* UINT32 KV entries from a GGUF file without loading tensors.

    Returns a dict with keys like 'rdmap2p.original_block_count'. Called before
    Llama() so the values are available immediately in /health.
    """
    result: dict = {}
    try:
        with open(model_path, "rb") as f:
            magic = struct.unpack("<I", f.read(4))[0]
            if magic != 0x46554747:
                return result
            f.read(4)  # version
            f.read(8)  # n_tensors
            n_kv = struct.unpack("<Q", f.read(8))[0]
            for _ in range(n_kv):
                key_len = struct.unpack("<Q", f.read(8))[0]
                key = f.read(key_len).decode("utf-8")
                vtype = struct.unpack("<I", f.read(4))[0]
                if vtype == 4 and key.startswith("rdmap2p."):  # UINT32
                    result[key] = struct.unpack("<I", f.read(4))[0]
                else:
                    _skip_gguf_value(f, vtype)
    except Exception as e:
        print(f"WARNING: _read_gguf_rdmap2p_meta: {e}", flush=True)
    return result


# ── Per-session state ─────────────────────────────────────────────────────────
# Each gateway request gets a unique session_id. Sessions track their own
# seq_id (KV cache slot) and generation position so parallel requests don't
# corrupt each other's state. The GPU is still time-shared via _lock.

# How many generations may be in flight at once. The context is split evenly
# between the slots, so this is also the divisor of the usable window:
# n_ctx_seq = n_ctx / _MAX_SESSIONS. Set with --max-sessions.
_MAX_SESSIONS = 8

# A session whose last use is older than this is reclaimed. Sessions are freed
# explicitly by the gateway; this only covers a gateway that died mid-request,
# so that a peer can never be wedged forever by a crash.
_SESSION_IDLE_TIMEOUT_S = 900.0

class _Session:
    __slots__ = ("seq_id", "n_past", "recent_tokens", "last_used", "prompt_tokens")
    def __init__(self, seq_id: int):
        self.seq_id = seq_id
        self.n_past = 0
        self.recent_tokens: list[int] = []
        self.last_used = time.time()
        # Tokens do prompt, guardados pelo primeiro peer para alimentar em
        # passadas sucessivas sem retokenizar a cada pedaco.
        self.prompt_tokens: list[int] = []

# Context actually allocated by llama.cpp, filled in once the context exists.
_n_ctx_total = 0

_sessions: dict[str, _Session] = {}
_sessions_lock = threading.Lock()
_seq_pool: list[int] = []
_seq_pool_lock = threading.Lock()


def _init_seq_pool() -> None:
    global _seq_pool
    with _seq_pool_lock:
        _seq_pool = list(range(_MAX_SESSIONS))


def _alloc_seq() -> int:
    with _seq_pool_lock:
        if not _seq_pool:
            raise RuntimeError(f"Seq pool exhausted (max {_MAX_SESSIONS} concurrent sessions)")
        return _seq_pool.pop(0)


def _reap_idle_sessions_locked() -> int:
    """Reclaim sessions abandoned by a gateway that never released them.

    Caller must hold _sessions_lock.
    """
    cutoff = time.time() - _SESSION_IDLE_TIMEOUT_S
    stale = [sid for sid, s in _sessions.items() if s.last_used < cutoff]
    for sid in stale:
        session = _sessions.pop(sid)
        print(f"Reclaiming idle session {sid[:8]} (seq={session.seq_id})", flush=True)
        _clear_seq_kv(session.seq_id)
        _free_seq(session.seq_id)
    return len(stale)


def _free_seq(seq_id: int) -> None:
    with _seq_pool_lock:
        if seq_id not in _seq_pool:
            _seq_pool.append(seq_id)


def _clear_seq_kv(seq_id: int) -> None:
    if _llama is None:
        return
    try:
        if hasattr(lc, "llama_kv_cache_seq_rm"):
            lc.llama_kv_cache_seq_rm(_llama.ctx, seq_id, 0, -1)
        elif hasattr(lc, "llama_get_memory") and hasattr(lc, "llama_memory_seq_rm"):
            mem = lc.llama_get_memory(_llama.ctx)
            lc.llama_memory_seq_rm(mem, seq_id, 0, -1)
    except Exception as e:
        print(f"WARNING: _clear_seq_kv(seq={seq_id}): {e}", flush=True)


def _rollback_seq_kv(seq_id: int, from_pos: int) -> None:
    """Drop this sequence's KV from `from_pos` on.

    Makes a retried chunk idempotent: if the answer to chunk k was lost on the
    way back, the sender repeats it and we redo the work from exactly the same
    position instead of appending the same tokens twice.
    """
    if _llama is None:
        return
    try:
        if hasattr(lc, "llama_kv_cache_seq_rm"):
            lc.llama_kv_cache_seq_rm(_llama.ctx, seq_id, from_pos, -1)
        elif hasattr(lc, "llama_get_memory") and hasattr(lc, "llama_memory_seq_rm"):
            mem = lc.llama_get_memory(_llama.ctx)
            lc.llama_memory_seq_rm(mem, seq_id, from_pos, -1)
    except Exception as e:
        print(f"WARNING: _rollback_seq_kv(seq={seq_id}, pos={from_pos}): {e}", flush=True)


def _recreate_ctx_with_seq_max(llama_obj: Llama) -> None:
    """Replace the llama context so n_seq_max == _MAX_SESSIONS.

    llama.cpp / the model metadata often sets n_seq_max to 256 which limits
    each KV-cache sequence to only n_ctx/256 tokens.  By recreating the context
    with n_seq_max = _MAX_SESSIONS we get n_ctx / _MAX_SESSIONS tokens per
    session — enough for long generations with a small number of parallel requests.
    """
    assert _args is not None
    need_embeddings = not _args.single_peer

    ctx_params = lc.llama_context_default_params()
    ctx_params.n_ctx     = int(_args.n_ctx)
    ctx_params.n_batch   = int(_args.n_batch)
    ctx_params.n_ubatch  = int(_args.n_batch)
    ctx_params.n_seq_max = _MAX_SESSIONS
    ctx_params.offload_kqv = True
    ctx_params.embeddings  = need_embeddings
    if int(_args.threads) > 0:
        ctx_params.n_threads       = int(_args.threads)
        ctx_params.n_threads_batch = int(_args.threads)

    lc.llama_free(llama_obj._ctx.ctx)
    new_ctx = lc.llama_new_context_with_model(llama_obj._model.model, ctx_params)
    if not new_ctx:
        raise RuntimeError(f"llama_new_context_with_model failed (n_seq_max={_MAX_SESSIONS})")
    llama_obj._ctx.ctx = new_ctx
    lc.llama_set_causal_attn(new_ctx, True)

    global _n_ctx_total
    actual_n_ctx = int(lc.llama_n_ctx(new_ctx))
    _n_ctx_total = actual_n_ctx
    n_ctx_seq = actual_n_ctx // _MAX_SESSIONS
    print(f"Context recreated: n_seq_max={_MAX_SESSIONS} n_ctx={actual_n_ctx} n_ctx_seq={n_ctx_seq}", flush=True)


def _get_or_create_session(session_id: str, reset: bool) -> "_Session":
    with _sessions_lock:
        if reset:
            if session_id in _sessions:
                old = _sessions.pop(session_id)
                _clear_seq_kv(old.seq_id)
                _free_seq(old.seq_id)
            try:
                seq = _alloc_seq()
            except RuntimeError:
                # Last resort before refusing: take back anything abandoned.
                if _reap_idle_sessions_locked() == 0:
                    raise
                seq = _alloc_seq()
            session = _Session(seq)
            _sessions[session_id] = session
            return session
        session = _sessions.get(session_id)
        if session is None:
            raise HTTPException(400, f"Unknown session '{session_id}' — send reset=true first")
        session.last_used = time.time()
        return session


def _finish_session(session_id: str) -> None:
    with _sessions_lock:
        session = _sessions.pop(session_id, None)
        if session is not None:
            _clear_seq_kv(session.seq_id)
            _free_seq(session.seq_id)


# ── Request / Response ────────────────────────────────────────────────────────

class InferRequest(BaseModel):
    model: str
    session_id: str = ""                       # unique per gateway request; empty = legacy
    prompt: Optional[str] = None
    tokens: Optional[list[int]] = None        # token IDs for continuing generation
    hidden_state: Optional[str] = None        # base64 float32
    hidden_shape: Optional[list[int]] = None  # [n_tokens, hidden_dim]
    start_layer: int = 0
    end_layer: int = 31
    is_last: bool = False
    reset: bool = False                       # Whether to clear KV cache before this step
    scale: float = 1.0                        # Scale factor for input hidden states


class InferResponse(BaseModel):
    hidden_state: Optional[str] = None
    hidden_shape: Optional[list[int]] = None
    text: Optional[str] = None
    token_id: Optional[int] = None
    done: bool = False


# ── Formato de fio do estado oculto ──────────────────────────────────────────
#
# Um pedaco de estado oculto atravessa como:
#
#     [4 bytes big-endian: tamanho do cabecalho JSON][cabecalho JSON][tensor cru]
#
# O cabecalho diz `dtype` e `shape`, entao a precisao e a codificacao sao
# PARAMETRO, nao premissa: trocar f16 por outra coisa mexe so em
# `_pack_hidden`/`_unpack_hidden`, e nao no caminho todo. O corpo e binario
# porque base64 inflava 33% dos bytes, e sob um alvo de internet os bytes na
# rede e que mandam no tempo ate a primeira palavra.

WIRE_DTYPES = {"f32": np.float32, "f16": np.float16}


def _pack_hidden(meta: dict, hidden: "np.ndarray | None", dtype: str) -> bytes:
    """Frame `meta` and, when present, `hidden` converted to `dtype`."""
    if hidden is not None:
        np_dtype = WIRE_DTYPES.get(dtype)
        if np_dtype is None:
            raise ValueError(f"dtype de fio desconhecido: {dtype}")
        arr = np.ascontiguousarray(hidden, dtype=np_dtype)
        meta = {**meta, "dtype": dtype, "shape": [int(arr.shape[0]), int(arr.shape[1])]}
        payload = arr.tobytes()
    else:
        payload = b""
    head = json.dumps(meta).encode("utf-8")
    return struct.pack(">I", len(head)) + head + payload


def _unpack_hidden(buf: bytes) -> tuple[dict, "np.ndarray | None"]:
    """Inverse of `_pack_hidden`. Always returns float32 for the maths."""
    if len(buf) < 4:
        raise ValueError("quadro curto demais")
    (head_len,) = struct.unpack(">I", buf[:4])
    meta = json.loads(buf[4 : 4 + head_len].decode("utf-8"))
    payload = buf[4 + head_len :]
    if not payload:
        return meta, None
    dtype = meta.get("dtype", "f32")
    np_dtype = WIRE_DTYPES.get(dtype)
    if np_dtype is None:
        raise ValueError(f"dtype de fio desconhecido: {dtype}")
    shape = meta.get("shape")
    if not shape or len(shape) != 2:
        raise ValueError(f"shape invalido: {shape}")
    arr = np.frombuffer(payload, dtype=np_dtype)
    expected = int(shape[0]) * int(shape[1])
    if arr.size != expected:
        raise ValueError(f"tensor com {arr.size} valores, esperado {expected}")
    # O calculo e sempre em f32; a reducao de precisao vale so no fio.
    return meta, arr.reshape(int(shape[0]), int(shape[1])).astype(np.float32)


# ── Hidden tensor helpers ────────────────────────────────────────────────────

def _encode_hidden(hidden: np.ndarray) -> tuple[str, list[int]]:
    arr = np.asarray(hidden, dtype=np.float32, order="C")
    if arr.ndim != 2:
        raise ValueError(f"hidden must be 2D [n_tokens, hidden_dim], got shape={arr.shape}")
    shape = [int(arr.shape[0]), int(arr.shape[1])]
    return base64.b64encode(arr.tobytes()).decode("ascii"), shape


def _decode_hidden(b64: str, shape: list[int], scale: float = 1.0) -> np.ndarray:
    if len(shape) != 2:
        raise HTTPException(400, f"hidden_shape must be [n_tokens, hidden_dim], got: {shape}")
    n_tokens, hidden_dim = int(shape[0]), int(shape[1])
    arr = np.frombuffer(base64.b64decode(b64), dtype=np.float32).copy()
    if arr.size != n_tokens * hidden_dim:
        raise HTTPException(
            400,
            f"hidden_state size mismatch: got {arr.size} floats, expected {n_tokens*hidden_dim}",
        )
    arr = arr.reshape((n_tokens, hidden_dim))
    if scale != 1.0:
        arr *= scale
    return arr


# ── Low-level llama helpers (llama_batch + decode) ───────────────────────────

def _make_token_batch(tokens: list[int], *, pos_offset: int, logits_mode: str, seq_id: int = 0) -> "lc.llama_batch":
    assert _llama is not None
    n_tokens = len(tokens)
    batch = lc.llama_batch_init(n_tokens, 0, 1)
    batch.n_tokens = n_tokens
    batch.embd = None

    for i in range(n_tokens):
        batch.token[i] = int(tokens[i])
        batch.pos[i] = int(pos_offset + i)
        batch.n_seq_id[i] = 1
        batch.seq_id[i][0] = seq_id

        if logits_mode == "all":
            batch.logits[i] = 1
        elif logits_mode == "last":
            batch.logits[i] = 1 if i == (n_tokens - 1) else 0
        else:
            raise ValueError(f"Unknown logits_mode: {logits_mode}")

    return batch


def _make_embd_batch(
    embd: np.ndarray,
    *,
    pos_offset: int,
    logits_mode: str,
    seq_id: int = 0,
) -> "lc.llama_batch":
    assert _llama is not None
    embd = np.asarray(embd, dtype=np.float32, order="C")
    if embd.ndim != 2:
        raise ValueError(f"embd must be 2D [n_tokens, hidden_dim], got shape={embd.shape}")

    n_tokens, hidden_dim = int(embd.shape[0]), int(embd.shape[1])
    batch = lc.llama_batch_init(n_tokens, hidden_dim, 1)
    batch.n_tokens = n_tokens
    batch.token = None

    flat = embd.reshape(-1)
    dest = np.ctypeslib.as_array(batch.embd, shape=(n_tokens * hidden_dim,))
    dest[:] = flat

    for i in range(n_tokens):
        batch.pos[i] = int(pos_offset + i)
        batch.n_seq_id[i] = 1
        batch.seq_id[i][0] = seq_id

        if logits_mode == "all":
            batch.logits[i] = 1
        elif logits_mode == "last":
            batch.logits[i] = 1 if i == (n_tokens - 1) else 0
        else:
            raise ValueError(f"Unknown logits_mode: {logits_mode}")

    return batch


def _extract_hidden_all_tokens(n_tokens: int) -> np.ndarray:
    assert _llama is not None
    assert _n_embd is not None

    hidden = np.empty((n_tokens, _n_embd), dtype=np.float32)
    ctx = _llama.ctx
    for i in range(n_tokens):
        ptr = lc.llama_get_embeddings_ith(ctx, i)
        if ptr is None:
            raise RuntimeError(f"llama_get_embeddings_ith returned NULL for i={i}")
        hidden[i, :] = np.ctypeslib.as_array(ptr, shape=(_n_embd,))
    return hidden


def _argmax_logits_for_output_token(idx: int = 0) -> int:
    assert _llama is not None
    assert _n_vocab is not None
    ptr = lc.llama_get_logits_ith(_llama.ctx, idx)
    if ptr is None:
        raise RuntimeError(f"llama_get_logits_ith returned NULL for index {idx}")
    logits = np.ctypeslib.as_array(ptr, shape=(_n_vocab,))
    return int(np.argmax(logits))


def _sample_next_token(logit_idx: int, recent_tokens: list) -> int:
    """Temperature + top-k + top-p + repetition-penalty sampling."""
    assert _llama is not None and _n_vocab is not None and _args is not None
    ptr = lc.llama_get_logits_ith(_llama.ctx, logit_idx)
    if ptr is None:
        raise RuntimeError(f"llama_get_logits_ith returned NULL for index {logit_idx}")
    logits = np.ctypeslib.as_array(ptr, shape=(_n_vocab,)).copy().astype(np.float32)

    # Repetition penalty
    rp = float(_args.repeat_penalty)
    if rp != 1.0 and recent_tokens:
        for tok in set(recent_tokens[-int(_args.repeat_last_n):]):
            if 0 <= tok < len(logits):
                logits[tok] = logits[tok] / rp if logits[tok] > 0 else logits[tok] * rp

    temp = float(_args.temperature)
    if temp <= 0.0:
        return int(np.argmax(logits))

    logits = logits / temp
    logits -= logits.max()
    probs = np.exp(logits)

    # Top-K
    top_k = int(_args.top_k)
    if 0 < top_k < len(probs):
        kth = np.partition(probs, -top_k)[-top_k]
        probs[probs < kth] = 0.0

    # Top-P (nucleus)
    top_p = float(_args.top_p)
    if top_p < 1.0:
        sorted_desc = np.sort(probs)[::-1]
        cumsum = np.cumsum(sorted_desc)
        cutoff_idx = np.searchsorted(cumsum, top_p)
        if cutoff_idx < len(sorted_desc):
            probs[probs < sorted_desc[cutoff_idx]] = 0.0

    total = probs.sum()
    if total <= 0.0:
        return int(np.argmax(logits))
    probs /= total

    return int(np.random.choice(len(probs), p=probs))


def _generate_greedy_from_prompt(prompt: str) -> str:
    """
    Single-node pipeline (this peer is both FIRST and LAST):
    prompt -> token ids prefill -> greedy decode.
    """
    assert _llama is not None
    assert _n_vocab is not None

    tokens = _llama.tokenize(prompt.encode("utf-8"), add_bos=True, special=False)
    if not tokens:
        return ""

    max_new_tokens = int(_args.max_new_tokens)
    eos_id = int(_llama.token_eos())

    with _lock:
        _llama.reset()

        if len(tokens) > int(_args.n_batch):
            raise HTTPException(400, f"Prompt too long for n_batch: {len(tokens)} > {_args.n_batch}")

        batch = _make_token_batch(tokens, pos_offset=0, logits_mode="last")
        try:
            rc = lc.llama_decode(_llama.ctx, batch)
            if rc != 0:
                raise RuntimeError(f"llama_decode failed rc={rc}")
        finally:
            lc.llama_batch_free(batch)

        next_id = _argmax_logits_for_output_token()

        generated: list[int] = []
        for step in range(max_new_tokens):
            if next_id == eos_id:
                break
            generated.append(next_id)

            # Decode the newly sampled token to advance KV cache.
            tok_batch = _make_token_batch(
                [next_id],
                pos_offset=len(tokens) + step,
                logits_mode="last",
            )
            try:
                rc = lc.llama_decode(_llama.ctx, tok_batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")
            finally:
                lc.llama_batch_free(tok_batch)

            next_id = _argmax_logits_for_output_token()

        out = _llama.detokenize(generated, special=False).decode("utf-8", errors="ignore")
        return out


def _generate_greedy_from_hidden(hidden: np.ndarray) -> str:
    """
    Distributed pipeline generation:
    hidden_state (token embeddings) prefill -> greedy decode.
    """
    assert _llama is not None
    assert _n_vocab is not None
    assert _n_embd is not None

    if hidden.ndim != 2 or hidden.shape[1] != _n_embd:
        raise HTTPException(400, f"hidden_state must be [n_tokens, {_n_embd}], got {hidden.shape}")

    prefill_len = int(hidden.shape[0])
    if prefill_len == 0:
        return ""

    if prefill_len > int(_args.n_batch):
        raise HTTPException(400, f"Hidden sequence too long for n_batch: {prefill_len} > {_args.n_batch}")

    max_new_tokens = int(_args.max_new_tokens)
    eos_id = int(_llama.token_eos())

    with _lock:
        _llama.reset()

        batch = _make_embd_batch(hidden, pos_offset=0, logits_mode="last")
        try:
            rc = lc.llama_decode(_llama.ctx, batch)
            if rc != 0:
                raise RuntimeError(f"llama_decode failed rc={rc}")
        finally:
            lc.llama_batch_free(batch)

        next_id = _argmax_logits_for_output_token(prefill_len - 1)

        generated: list[int] = []
        for step in range(max_new_tokens):
            if next_id == eos_id:
                break
            generated.append(next_id)

            tok_batch = _make_token_batch(
                [next_id],
                pos_offset=prefill_len + step,
                logits_mode="last",
            )
            try:
                rc = lc.llama_decode(_llama.ctx, tok_batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")
            finally:
                lc.llama_batch_free(tok_batch)

            next_id = _argmax_logits_for_output_token(0)

        out = _llama.detokenize(generated, special=False).decode("utf-8", errors="ignore")
        return out


# ── Inference endpoint ────────────────────────────────────────────────────────

@app.post("/v1/infer", response_model=InferResponse)
def infer(req: InferRequest) -> InferResponse:
    try:
        return _infer_impl(req)
    except HTTPException:
        raise
    except Exception as e:
        traceback.print_exc()
        raise HTTPException(status_code=500, detail=str(e))


def _reinit_llama() -> None:
    """Recreate Llama context when GPU state is unrecoverable. Frees VRAM first."""
    global _llama, _n_embd, _n_vocab
    assert _args is not None
    import gc
    print("Reinitializing Llama context (freeing VRAM first)...", flush=True)
    old = _llama
    _llama = None
    try:
        del old
    except Exception:
        pass
    gc.collect()

    need_embeddings = not _args.single_peer
    kwargs = dict(
        n_ctx=int(_args.n_ctx),
        n_batch=int(_args.n_batch),
        n_gpu_layers=int(_args.n_gpu_layers),
        embedding=need_embeddings,
        logits_all=False,
        offload_kqv=False,
        verbose=False,
    )
    if int(_args.threads) > 0:
        kwargs["n_threads"] = int(_args.threads)
    _llama = Llama(_args.model, **kwargs)
    _llama.reset()
    _recreate_ctx_with_seq_max(_llama)
    _n_embd = int(_llama.n_embd())
    _n_vocab = int(_llama.n_vocab())
    print("Llama context reinitialized.", flush=True)


def _apply_chat_template(prompt: str) -> str:
    """Wrap raw prompt in Qwen3.5 chat template. With no_thinking, forces empty <think> block."""
    assert _args is not None
    system = "You are a helpful assistant."
    out = (
        f"<|im_start|>system\n{system}<|im_end|>\n"
        f"<|im_start|>user\n{prompt}<|im_end|>\n"
        "<|im_start|>assistant\n"
    )
    if _args.no_thinking:
        out += "<think>\n\n</think>\n"
    return out


def _infer_impl(req: InferRequest) -> InferResponse:
    assert _llama is not None

    # Resolve session (creates/resets if req.reset=True, else looks up existing).
    # Falls back to a fixed legacy session_id when the caller doesn't send one.
    sid_key = req.session_id if req.session_id else "__legacy__"
    session = _get_or_create_session(sid_key, req.reset)
    n_past = session.n_past
    sid = session.seq_id

    # Determine tokens/embeddings to process
    tokens = None
    if req.prompt is not None:
        use_template = bool(_args and _args.chat_template)
        text = _apply_chat_template(req.prompt) if use_template else req.prompt
        tokens = _llama.tokenize(text.encode("utf-8"), add_bos=not use_template, special=True)
    elif req.tokens is not None:
        tokens = req.tokens

    if tokens is not None:
        n_tokens = len(tokens)
        if n_tokens == 0:
            return InferResponse(done=False)

        with _lock:
            if req.is_last:
                batch = _make_token_batch(tokens, pos_offset=n_past, logits_mode="last", seq_id=sid)
                try:
                    rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0 and n_past == 0:
                        lc.llama_batch_free(batch)
                        _reinit_llama()
                        batch = _make_token_batch(tokens, pos_offset=0, logits_mode="last", seq_id=sid)
                        rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0:
                        raise RuntimeError(f"llama_decode failed rc={rc}")
                    next_id = _sample_next_token(n_tokens - 1, session.recent_tokens)
                    text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                    session.n_past += n_tokens
                    session.recent_tokens.extend(tokens)
                    session.recent_tokens.append(next_id)
                    done = next_id == int(_llama.token_eos())
                    print(f"SINGLE seq={sid} sess={sid_key[:8]} (n={n_tokens}, pos={n_past}) -> '{text}'")
                    if done:
                        _finish_session(sid_key)
                    return InferResponse(text=text, token_id=next_id, done=done)
                finally:
                    lc.llama_batch_free(batch)
            else:
                batch = _make_token_batch(tokens, pos_offset=n_past, logits_mode="all", seq_id=sid)
                try:
                    rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0:
                        raise RuntimeError(f"llama_decode failed rc={rc}")
                    hidden = _extract_hidden_all_tokens(n_tokens)
                    session.n_past += n_tokens
                    rms = float(np.sqrt(np.mean(np.square(hidden))))
                    print(f"PROMPT seq={sid} sess={sid_key[:8]} (n={n_tokens}, pos={n_past}, RMS={rms:.2f})")
                finally:
                    lc.llama_batch_free(batch)

        b64, shape = _encode_hidden(hidden)
        return InferResponse(hidden_state=b64, hidden_shape=shape, done=False)

    # Hidden state path
    if req.hidden_state is None or req.hidden_shape is None:
        raise HTTPException(400, "Provide 'prompt', 'tokens' or both 'hidden_state' and 'hidden_shape'")

    hidden = _decode_hidden(req.hidden_state, req.hidden_shape, req.scale)
    n_tokens = int(hidden.shape[0])

    with _lock:
        if req.is_last:
            batch = _make_embd_batch(hidden, pos_offset=n_past, logits_mode="last", seq_id=sid)
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")
                next_id = _sample_next_token(n_tokens - 1, session.recent_tokens)
                text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                session.n_past += n_tokens
                session.recent_tokens.append(next_id)
                rms = float(np.sqrt(np.mean(np.square(hidden))))
                done = next_id == int(_llama.token_eos())
                print(f"[Hidden] seq={sid} sess={sid_key[:8]} (n={n_tokens}, pos={n_past}, RMS={rms:.2f}) -> '{text}'")
                if done:
                    _finish_session(sid_key)
                return InferResponse(text=text, token_id=next_id, done=done)
            finally:
                lc.llama_batch_free(batch)
        else:
            batch = _make_embd_batch(hidden, pos_offset=n_past, logits_mode="all", seq_id=sid)
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")
                new_hidden = _extract_hidden_all_tokens(n_tokens)
                session.n_past += n_tokens
                rms_in = float(np.sqrt(np.mean(np.square(hidden))))
                rms_out = float(np.sqrt(np.mean(np.square(new_hidden))))
                print(f"[Hidden] seq={sid} sess={sid_key[:8]} MID (n={n_tokens}, pos={n_past}, RMS {rms_in:.2f}->{rms_out:.2f})")
                b64, shape = _encode_hidden(new_hidden)
                return InferResponse(hidden_state=b64, hidden_shape=shape, done=False)
            finally:
                lc.llama_batch_free(batch)



# ── Health endpoint ───────────────────────────────────────────────────────────

# ── Prefill em pedacos ───────────────────────────────────────────────────────


@app.post("/v1/prefill")
async def prefill(request: Request):
    """Ingest one chunk of the prompt.

    The prompt used to go into a single `llama_batch`, which forced it to fit in
    `n_batch` and made a multi-gigabyte hidden state cross the wire as one
    block — on an internet link that is a request that dies. Here each chunk is
    decoded at its own position, so memory stops scaling with the prompt and a
    lost chunk costs one chunk, not the whole request.

    Body is the framed format of `_pack_hidden`.
    """
    raw = await request.body()
    try:
        meta, hidden = _unpack_hidden(raw)
    except Exception as e:
        raise HTTPException(400, f"quadro invalido: {e}")

    sid_key = meta.get("session_id") or "__legacy__"
    chunk_index = int(meta.get("chunk_index", 0))
    chunk_size = int(meta.get("chunk_size", 512))
    is_last = bool(meta.get("is_last", False))
    is_final_chunk = bool(meta.get("is_final_chunk", False))
    wire_dtype = meta.get("dtype", "f16")
    scale = float(meta.get("scale", 1.0))

    with _lock:
        # Chunk 0 opens the session; later chunks must find it.
        session = _get_or_create_session(sid_key, reset=(chunk_index == 0))
        sid = session.seq_id

        # First peer: tokenize once on chunk 0 and keep the tokens.
        prompt = meta.get("prompt")
        if prompt is not None and chunk_index == 0:
            use_template = bool(_args and _args.chat_template)
            text = _apply_chat_template(prompt) if use_template else prompt
            session.prompt_tokens = list(
                _llama.tokenize(text.encode("utf-8"), add_bos=not use_template, special=True)
            )

        expected_pos = chunk_index * chunk_size
        if session.n_past > expected_pos:
            # The answer to this chunk was lost on the way back and the sender
            # is repeating it. Rewind and redo — never append twice.
            print(f"Refazendo pedaco {chunk_index} da sessao {sid_key[:8]} (pos {expected_pos})", flush=True)
            _rollback_seq_kv(sid, expected_pos)
            session.n_past = expected_pos
        elif session.n_past < expected_pos:
            raise HTTPException(
                400,
                f"pedaco fora de ordem: recebi {chunk_index} (pos {expected_pos}) "
                f"mas a sessao esta em {session.n_past}",
            )

        n_past = session.n_past

        if hidden is not None:
            if scale != 1.0:
                hidden = hidden * scale
            n_tokens = hidden.shape[0]
            batch = _make_embd_batch(
                hidden, pos_offset=n_past, logits_mode="last" if is_last else "all", seq_id=sid
            )
        else:
            tokens = session.prompt_tokens[expected_pos : expected_pos + chunk_size]
            if not tokens:
                raise HTTPException(400, f"pedaco {chunk_index} nao tem tokens")
            n_tokens = len(tokens)
            batch = _make_token_batch(
                tokens, pos_offset=n_past, logits_mode="last" if is_last else "all", seq_id=sid
            )

        try:
            rc = lc.llama_decode(_llama.ctx, batch)
            if rc != 0:
                raise HTTPException(500, f"llama_decode falhou rc={rc}")
            session.n_past += n_tokens
            session.last_used = time.time()

            total_tokens = len(session.prompt_tokens)
            more = (hidden is None) and (session.n_past < total_tokens)

            if is_last:
                if not is_final_chunk:
                    # Middle of the prompt: just absorb it, nothing to sample yet.
                    return Response(
                        content=_pack_hidden({"done": False, "more_chunks": more}, None, wire_dtype),
                        media_type="application/octet-stream",
                    )
                next_id = _sample_next_token(n_tokens - 1, session.recent_tokens)
                text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                session.recent_tokens.append(next_id)
                done = next_id == int(_llama.token_eos())
                return Response(
                    content=_pack_hidden(
                        {"done": done, "token_id": int(next_id), "text": text, "more_chunks": False},
                        None,
                        wire_dtype,
                    ),
                    media_type="application/octet-stream",
                )

            out = _extract_hidden_all_tokens(n_tokens)
            return Response(
                content=_pack_hidden(
                    {"done": False, "more_chunks": more, "n_prompt_tokens": len(session.prompt_tokens)},
                    out,
                    wire_dtype,
                ),
                media_type="application/octet-stream",
            )
        finally:
            lc.llama_batch_free(batch)


class ReleaseRequest(BaseModel):
    session_id: str


@app.post("/v1/session/release")
def release_session(req: ReleaseRequest):
    """Give a session's KV slot back.

    The gateway calls this when a generation ends, however it ends — EOS, token
    limit, client disconnect or error. Without it the first and middle peers
    never released anything (only the sampling peer freed, and only on EOS), so
    eight requests exhausted the pool and the whole network stopped answering.
    """
    with _sessions_lock:
        existed = req.session_id in _sessions
    _finish_session(req.session_id)
    with _seq_pool_lock:
        free = len(_seq_pool)
    return {"released": existed, "free_slots": free, "max_sessions": _MAX_SESSIONS}


@app.get("/health")
def health():
    return {
        "status": "ok",
        "model_loaded": _llama is not None,
        "start_layer": _args.start_layer if _args else None,
        "end_layer": _args.end_layer if _args else None,
        "original_block_count": getattr(_args, "original_block_count", None) if _args else None,
        "hidden_dim": _n_embd,
        "vocab_size": _n_vocab,
        "max_sessions": _MAX_SESSIONS,
        "sessions_in_use": len(_sessions),
        "free_slots": len(_seq_pool),
        # The window a single request actually gets: the context is split
        # evenly across the session slots.
        "n_ctx_total": _n_ctx_total,
        "n_ctx_per_session": (_n_ctx_total // _MAX_SESSIONS) if _n_ctx_total else None,
    }


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    global _llama, _args, _n_embd, _n_vocab

    parser = argparse.ArgumentParser(description="rdma-p2p inference node (transformers backend)")
    parser.add_argument("--model",       required=True,
                        help="Path to GGUF model slice")
    parser.add_argument("--start-layer", type=int, default=None,
                        help="First layer this node owns (auto-detected from GGUF if omitted)")
    parser.add_argument("--end-layer",   type=int, default=None,
                        help="Last layer this node owns (auto-detected from GGUF if omitted)")
    parser.add_argument("--port",        type=int, default=8181)
    parser.add_argument("--host",        default="0.0.0.0")
    parser.add_argument("--n-gpu-layers", type=int, default=0,
                        help="Number of layers to offload to GPU (llama-cpp-python)")
    parser.add_argument("--n-ctx", type=int, default=4096,
                        help="KV cache context length (must fit prompt token count)")
    parser.add_argument("--max-sessions", type=int, default=8,
                        help="concurrent generations; the context is split between "
                             "them, so the usable window is n_ctx / max_sessions")
    parser.add_argument("--n-batch", type=int, default=4096,
                        help="Maximum batch size for prompt embeddings injection")
    parser.add_argument("--max-new-tokens", type=int, default=512,
                        help="Greedy decode length for last peer")
    parser.add_argument("--threads", type=int, default=0,
                        help="CPU threads for llama-cpp-python (0 = let library decide)")
    parser.add_argument("--single-peer", action="store_true",
                        help="Single-peer mode: loads WITHOUT embedding=True so causal attention works correctly. "
                             "Use this when one node covers all layers (no hidden-state passing needed).")
    parser.add_argument("--chat-template", action="store_true",
                        help="Wrap prompts in Qwen3.5 chat template (<|im_start|> format)")
    parser.add_argument("--no-thinking", action="store_true",
                        help="Disable Qwen3.5 thinking mode by injecting empty <think></think> block. Implies --chat-template.")
    parser.add_argument("--temperature", type=float, default=0.7,
                        help="Sampling temperature (0 = greedy/argmax, recommended: 0.6-0.8)")
    parser.add_argument("--top-p", type=float, default=0.9,
                        help="Top-p (nucleus) sampling cutoff")
    parser.add_argument("--top-k", type=int, default=40,
                        help="Top-k sampling (0 = disabled)")
    parser.add_argument("--repeat-penalty", type=float, default=1.15,
                        help="Penalty for recently generated tokens (1.0 = off)")
    parser.add_argument("--repeat-last-n", type=int, default=64,
                        help="Number of recent tokens to apply repeat penalty to")
    args = parser.parse_args()
    if args.no_thinking:
        args.chat_template = True

    # Read rdmap2p.* metadata from the GGUF file BEFORE the heavy Llama() load
    # so /health returns original_block_count immediately (Rust polls after 5 s).
    gguf_meta = _read_gguf_rdmap2p_meta(args.model)
    if args.start_layer is None:
        v = gguf_meta.get("rdmap2p.original_block_start")
        args.start_layer = int(v) if v is not None else 0
    if args.end_layer is None:
        v = gguf_meta.get("rdmap2p.original_block_end")
        args.end_layer = int(v) if v is not None else 31
    args.original_block_count = None
    v = gguf_meta.get("rdmap2p.original_block_count")
    if v is not None:
        args.original_block_count = int(v)

    _args = args

    print(f"Loading model : {args.model}", flush=True)
    print(f"Layers        : {args.start_layer}–{args.end_layer}"
          + (f" of {args.original_block_count}" if args.original_block_count else ""), flush=True)
    print(f"Server        : http://{args.host}:{args.port}", flush=True)
    print(f"n_ctx (req)    : {args.n_ctx}", flush=True)
    print(f"n_batch        : {args.n_batch}", flush=True)
    print(f"max_new_tokens: {args.max_new_tokens}", flush=True)
    print(f"chat_template  : {args.chat_template}", flush=True)
    print(f"no_thinking    : {args.no_thinking}", flush=True)

    n_threads = int(args.threads)
    # embedding=True disables causal attention in recent llama.cpp builds, breaking generation.
    # Only enable it when multi-peer distributed mode needs hidden-state extraction.
    need_embeddings = not args.single_peer
    kwargs = dict(
        n_ctx=int(args.n_ctx),
        n_batch=int(args.n_batch),
        n_gpu_layers=int(args.n_gpu_layers),
        embedding=need_embeddings,
        logits_all=False,  # per-token logit flags in the batch are sufficient; True wastes n_batch*vocab*4 bytes
        offload_kqv=False, # keep KV cache in CPU RAM; embedding=True forces large n_ctx on Metal, this avoids GPU allocation
        verbose=False,
    )
    if n_threads > 0:
        kwargs["n_threads"] = n_threads

    global _MAX_SESSIONS
    _MAX_SESSIONS = max(1, int(args.max_sessions))
    _init_seq_pool()
    print(f"Session slots : {_MAX_SESSIONS} (janela por requisicao = n_ctx / {_MAX_SESSIONS})", flush=True)

    print("Initializing Llama model (this may take a few seconds)...", flush=True)
    _llama = Llama(args.model, **kwargs)
    print("Llama model initialized. Recreating context with n_seq_max...", flush=True)
    _llama.reset()
    _recreate_ctx_with_seq_max(_llama)

    _n_embd = int(_llama.n_embd())
    _n_vocab = int(_llama.n_vocab())
    actual_n_ctx = int(lc.llama_n_ctx(_llama.ctx))

    print(f"Mode          : {'single-peer (causal gen)' if args.single_peer else 'multi-peer (embedding extraction)'}", flush=True)
    print(f"Model loaded  : hidden_dim={_n_embd} vocab_size={_n_vocab}", flush=True)
    print(f"Layers (final): {args.start_layer}–{args.end_layer}"
          + (f" of {args.original_block_count}" if args.original_block_count else ""), flush=True)
    print(f"n_ctx (actual): {actual_n_ctx} tokens (llama.cpp allocated)", flush=True)
    if actual_n_ctx < args.n_ctx:
        print(f"WARNING: llama.cpp reduced n_ctx {args.n_ctx}→{actual_n_ctx} (memory constraint)", flush=True)
        print(f"  Max response ≈ {max(0, actual_n_ctx - 100)} tokens", flush=True)
    print(f"Starting server on {args.host}:{args.port}...", flush=True)

    try:
        uvicorn.run(app, host=args.host, port=args.port, log_level="warning")
    except Exception as e:
        print(f"FATAL: Failed to start uvicorn server: {e}", flush=True)
        import sys
        sys.exit(1)


if __name__ == "__main__":
    main()
