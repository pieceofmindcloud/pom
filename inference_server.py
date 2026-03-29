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
import threading
import ctypes
import traceback
from typing import Optional

import numpy as np
import uvicorn
from fastapi import FastAPI, HTTPException
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
_lock = threading.Lock()


# ── Request / Response ────────────────────────────────────────────────────────

class InferRequest(BaseModel):
    model: str
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

def _make_token_batch(tokens: list[int], *, pos_offset: int, logits_mode: str) -> "lc.llama_batch":
    assert _llama is not None
    n_tokens = len(tokens)
    batch = lc.llama_batch_init(n_tokens, 0, 1)
    batch.n_tokens = n_tokens  # llama_batch_init leaves fields uninitialized
    batch.embd = None # Ensure we are using tokens

    for i in range(n_tokens):
        batch.token[i] = int(tokens[i])
        batch.pos[i] = int(pos_offset + i)
        batch.n_seq_id[i] = 1
        batch.seq_id[i][0] = 0

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
) -> "lc.llama_batch":
    assert _llama is not None
    embd = np.asarray(embd, dtype=np.float32, order="C")
    if embd.ndim != 2:
        raise ValueError(f"embd must be 2D [n_tokens, hidden_dim], got shape={embd.shape}")

    n_tokens, hidden_dim = int(embd.shape[0]), int(embd.shape[1])
    # The second arg is n_embd (the dimension), NOT a boolean.
    batch = lc.llama_batch_init(n_tokens, hidden_dim, 1)
    batch.n_tokens = n_tokens
    batch.token = None # Signal that we are using batch.embd

    # Copy embeddings into the batch-allocated buffer.
    flat = embd.reshape(-1)
    dest = np.ctypeslib.as_array(batch.embd, shape=(n_tokens * hidden_dim,))
    dest[:] = flat

    for i in range(n_tokens):
        batch.pos[i] = int(pos_offset + i)
        batch.n_seq_id[i] = 1
        batch.seq_id[i][0] = 0

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
    except Exception as e:
        print(f"ERROR in infer: {e}", flush=True)
        traceback.print_exc()
        raise HTTPException(status_code=500, detail=str(e))

def _infer_impl(req: InferRequest) -> InferResponse:
    assert _llama is not None

    # Determine tokens to process at this node
    tokens = None
    if req.prompt is not None:
        print(f"📥 Received prompt (len={len(req.prompt)}) for layers {req.start_layer}–{req.end_layer}")
        tokens = _llama.tokenize(req.prompt.encode("utf-8"), add_bos=True, special=False)
    elif req.tokens is not None:
        print(f"📥 Received tokens {req.tokens} for layers {req.start_layer}–{req.end_layer}")
        tokens = req.tokens

    if tokens is not None:
        if req.is_last:
            # Last node receiving tokens directly (sampling ONE token)
            with _lock:
                if req.reset: 
                    _llama.reset()
                    _llama._distributed_n_past = 0
                
                pos_offset = getattr(_llama, "_distributed_n_past", 0)
                batch = _make_token_batch(tokens, pos_offset=pos_offset, logits_mode="last")
                try:
                    rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0:
                        raise RuntimeError(f"llama_decode failed rc={rc}")

                    # Sample the LAST token in the batch
                    next_id = _argmax_logits_for_output_token(n_tokens - 1)
                    text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")

                    _llama._distributed_n_past = pos_offset + len(tokens)
                    
                    print(f"📤 Sampled token {next_id} ('{text}')")
                    return InferResponse(text=text, token_id=next_id, done=(next_id == int(_llama.token_eos())))
                finally:
                    lc.llama_batch_free(batch)

        if not tokens:
            print("📤 Sending empty response (no tokens)")
            return InferResponse(hidden_state=None, hidden_shape=None, done=False)
        
        n_tokens = len(tokens)
        if n_tokens > int(_args.n_batch):
            raise HTTPException(400, f"Prompt too long for n_batch: {n_tokens} > {_args.n_batch}")

        with _lock:
            if req.reset: 
                _llama.reset()
                _llama._distributed_n_past = 0
            
            pos_offset = getattr(_llama, "_distributed_n_past", 0)
            
            batch = _make_token_batch(tokens, pos_offset=pos_offset, logits_mode="all")
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")
                hidden = _extract_hidden_all_tokens(n_tokens)
                _llama._distributed_n_past = pos_offset + n_tokens
            finally:
                lc.llama_batch_free(batch)

        b64, shape = _encode_hidden(hidden)
        print(f"📤 Sending hidden state {shape} for {n_tokens} tokens")
        return InferResponse(hidden_state=b64, hidden_shape=shape, done=False)

    # Hidden state path
    if req.hidden_state is None or req.hidden_shape is None:
        raise HTTPException(400, "Provide 'prompt', 'tokens' or both 'hidden_state' and 'hidden_shape'")

    hidden = _decode_hidden(req.hidden_state, req.hidden_shape, req.scale)
    n_tokens = int(hidden.shape[0])
    
    # Calculate RMS for diagnostics
    rms = float(np.sqrt(np.mean(np.square(hidden))))
    print(f"📥 Received hidden state {req.hidden_shape} (RMS={rms:.3f}, scale={req.scale}) for layers {req.start_layer}–{req.end_layer}")

    with _lock:
        if req.reset: 
            _llama.reset()
            _llama._distributed_n_past = 0
            
        pos_offset = getattr(_llama, "_distributed_n_past", 0)

        if req.is_last:
            batch = _make_embd_batch(hidden, pos_offset=pos_offset, logits_mode="last")
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0:
                    raise RuntimeError(f"llama_decode failed rc={rc}")

                # Sample the LAST token in the batch
                next_id = _argmax_logits_for_output_token(n_tokens - 1)
                text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                _llama._distributed_n_past = pos_offset + n_tokens

                
                print(f"📤 Sampled token {next_id} ('{text}')")
                return InferResponse(text=text, token_id=next_id, done=(next_id == int(_llama.token_eos())))
            finally:
                lc.llama_batch_free(batch)

        if n_tokens > int(_args.n_batch):
            raise HTTPException(400, f"Hidden sequence too long for n_batch: {n_tokens} > {_args.n_batch}")

        batch = _make_embd_batch(hidden, pos_offset=pos_offset, logits_mode="all")
        try:
            rc = lc.llama_decode(_llama.ctx, batch)
            if rc != 0:
                raise RuntimeError(f"llama_decode failed rc={rc}")
            new_hidden = _extract_hidden_all_tokens(n_tokens)
            _llama._distributed_n_past = pos_offset + n_tokens
        finally:
            lc.llama_batch_free(batch)

    b64, shape = _encode_hidden(new_hidden)
    print(f"📤 Sending hidden state {shape} for {n_tokens} tokens")
    return InferResponse(hidden_state=b64, hidden_shape=shape, done=False)


# ── Health endpoint ───────────────────────────────────────────────────────────

@app.get("/health")
def health():
    hidden_dim = _n_embd
    vocab_size = _n_vocab
    return {
        "status": "ok",
        "model_loaded": _llama is not None,
        "start_layer": _args.start_layer if _args else None,
        "end_layer": _args.end_layer if _args else None,
        "hidden_dim": hidden_dim,
        "vocab_size": vocab_size,
    }


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    global _llama, _args, _n_embd, _n_vocab

    parser = argparse.ArgumentParser(description="rdma-p2p inference node (transformers backend)")
    parser.add_argument("--model",       required=True,
                        help="Path to GGUF model slice")
    parser.add_argument("--start-layer", type=int, default=0)
    parser.add_argument("--end-layer",   type=int, default=31)
    parser.add_argument("--port",        type=int, default=8181)
    parser.add_argument("--host",        default="0.0.0.0")
    parser.add_argument("--n-gpu-layers", type=int, default=0,
                        help="Number of layers to offload to GPU (llama-cpp-python)")
    parser.add_argument("--n-ctx", type=int, default=4096,
                        help="KV cache context length (must fit prompt token count)")
    parser.add_argument("--n-batch", type=int, default=4096,
                        help="Maximum batch size for prompt embeddings injection")
    parser.add_argument("--max-new-tokens", type=int, default=512,
                        help="Greedy decode length for last peer")
    parser.add_argument("--threads", type=int, default=0,
                        help="CPU threads for llama-cpp-python (0 = let library decide)")
    args = parser.parse_args()
    _args = args

    print(f"Loading model : {args.model}", flush=True)
    print(f"Layers        : {args.start_layer}–{args.end_layer}", flush=True)
    print(f"Server        : http://{args.host}:{args.port}", flush=True)
    print(f"n_ctx          : {args.n_ctx}", flush=True)
    print(f"n_batch        : {args.n_batch}", flush=True)
    print(f"max_new_tokens: {args.max_new_tokens}", flush=True)

    n_threads = int(args.threads)
    kwargs = dict(
        n_ctx=int(args.n_ctx),
        n_batch=int(args.n_batch),
        n_gpu_layers=int(args.n_gpu_layers),
        embedding=True,  # required for llama_get_embeddings_ith()
        logits_all=True,
        verbose=False,
    )
    if n_threads > 0:
        kwargs["n_threads"] = n_threads

    print("Initializing Llama model (this may take a few seconds)...", flush=True)
    _llama = Llama(args.model, **kwargs)
    print("Llama model initialized. Resetting...", flush=True)
    _llama.reset()

    # Ensure context outputs embeddings and causal attention is enabled.
    # lc.llama_set_embeddings(_llama.ctx, True)
    # lc.llama_set_causal_attn(_llama.ctx, True)

    _n_embd = int(_llama.n_embd())
    _n_vocab = int(_llama.n_vocab())

    print(f"Model loaded  : hidden_dim={_n_embd} vocab_size={_n_vocab}", flush=True)
    print(f"Starting server on {args.host}:{args.port}...", flush=True)

    try:
        uvicorn.run(app, host=args.host, port=args.port, log_level="warning")
    except Exception as e:
        print(f"FATAL: Failed to start uvicorn server: {e}", flush=True)
        import sys
        sys.exit(1)


if __name__ == "__main__":
    main()
