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
import faulthandler
import threading
import ctypes
import traceback
from typing import Optional

faulthandler.enable()  # print C-level traceback on SIGSEGV/SIGABRT

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
_seq_id: int = 0  # rotates on each reset to avoid stale SSM state in VRAM


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
    except Exception as e:
        # Only print full traceback for real errors
        if not isinstance(e, HTTPException):
            traceback.print_exc()
        raise HTTPException(status_code=500, detail=str(e))

def _do_reset() -> None:
    """Rotate seq_id and remove old sequence to get fresh SSM state in VRAM."""
    global _seq_id
    assert _llama is not None
    old_seq = _seq_id
    _seq_id = (old_seq + 1) % 32768
    try:
        if hasattr(lc, 'llama_kv_cache_seq_rm'):
            # llama.cpp < b4000: context pointer goes directly
            lc.llama_kv_cache_seq_rm(_llama.ctx, old_seq, 0, -1)
        elif hasattr(lc, 'llama_get_memory') and hasattr(lc, 'llama_memory_seq_rm'):
            # llama.cpp >= b4000: first arg must be llama_memory_t*, not llama_context*
            mem = lc.llama_get_memory(_llama.ctx)
            lc.llama_memory_seq_rm(mem, old_seq, 0, -1)
        else:
            _llama.reset()
    except Exception as e:
        print(f"WARNING: seq_rm(seq={old_seq}) failed: {e} — using reset()", flush=True)
        try:
            _llama.reset()
        except Exception as e2:
            print(f"WARNING: reset() also failed: {e2}", flush=True)
    _llama._distributed_n_past = 0
    _llama._recent_tokens = []


def _reinit_llama() -> None:
    """Recreate Llama context when GPU state is unrecoverable. Frees VRAM first."""
    global _llama, _n_embd, _n_vocab, _seq_id
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

    _seq_id = 0
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
    lc.llama_set_causal_attn(_llama.ctx, True)
    _llama._distributed_n_past = 0
    _llama._recent_tokens = []
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

    # Determine tokens/embeddings to process
    tokens = None
    if req.prompt is not None:
        use_template = bool(_args and _args.chat_template)
        text = _apply_chat_template(req.prompt) if use_template else req.prompt
        # Chat template already starts with <|im_start|> — adding BOS confuses the model.
        tokens = _llama.tokenize(text.encode("utf-8"), add_bos=not use_template, special=True)
    elif req.tokens is not None:
        tokens = req.tokens

    # Session state: track n_past and recent tokens for repeat penalty
    if not hasattr(_llama, "_distributed_n_past"):
        _llama._distributed_n_past = 0
    if not hasattr(_llama, "_recent_tokens"):
        _llama._recent_tokens = []

    if req.reset:
        _do_reset()

    n_past = _llama._distributed_n_past
    sid = _seq_id  # snapshot for this call; _seq_id only changes on reset

    if tokens is not None:
        n_tokens = len(tokens)
        if n_tokens == 0:
            return InferResponse(done=False)

        with _lock:
            if req.is_last:
                # Single-peer mode (this node is first AND last): prefill then sample.
                batch = _make_token_batch(tokens, pos_offset=n_past, logits_mode="last", seq_id=sid)
                try:
                    rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0 and n_past == 0:
                        # SSM state still dirty — reinit and retry once.
                        lc.llama_batch_free(batch)
                        _reinit_llama()
                        batch = _make_token_batch(tokens, pos_offset=0, logits_mode="last", seq_id=_seq_id)
                        rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0: raise RuntimeError(f"llama_decode failed rc={rc}")

                    next_id = _sample_next_token(n_tokens - 1, _llama._recent_tokens)
                    text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                    _llama._distributed_n_past += n_tokens
                    _llama._recent_tokens.extend(tokens)
                    _llama._recent_tokens.append(next_id)
                    print(f"SINGLE seq={sid} (n={n_tokens}, pos={n_past}) -> '{text}'")
                    return InferResponse(
                        text=text,
                        token_id=next_id,
                        done=(next_id == int(_llama.token_eos())),
                    )
                finally:
                    lc.llama_batch_free(batch)
            else:
                # First/middle peer: extract hidden state to pass to next peer.
                batch = _make_token_batch(tokens, pos_offset=n_past, logits_mode="all", seq_id=sid)
                try:
                    rc = lc.llama_decode(_llama.ctx, batch)
                    if rc != 0: raise RuntimeError(f"llama_decode failed rc={rc}")

                    hidden = _extract_hidden_all_tokens(n_tokens)
                    _llama._distributed_n_past += n_tokens

                    rms = float(np.sqrt(np.mean(np.square(hidden))))
                    print(f"PROMPT seq={sid} (n={n_tokens}, pos={n_past}, RMS={rms:.2f}) -> [Activations]")
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
            # Last peer: inject hidden state and sample one token.
            batch = _make_embd_batch(hidden, pos_offset=n_past, logits_mode="last", seq_id=sid)
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0: raise RuntimeError(f"llama_decode failed rc={rc}")

                next_id = _sample_next_token(n_tokens - 1, _llama._recent_tokens)
                text = _llama.detokenize([next_id], special=False).decode("utf-8", errors="ignore")
                _llama._distributed_n_past += n_tokens
                _llama._recent_tokens.append(next_id)

                rms = float(np.sqrt(np.mean(np.square(hidden))))
                print(f"[Hidden] seq={sid} (n={n_tokens}, pos={n_past}, RMS={rms:.2f}) -> '{text}'")

                return InferResponse(text=text, token_id=next_id, done=(next_id == int(_llama.token_eos())))
            finally:
                lc.llama_batch_free(batch)
        else:
            # Middle peer: pass hidden state through and return new hidden state.
            batch = _make_embd_batch(hidden, pos_offset=n_past, logits_mode="all", seq_id=sid)
            try:
                rc = lc.llama_decode(_llama.ctx, batch)
                if rc != 0: raise RuntimeError(f"llama_decode failed rc={rc}")

                new_hidden = _extract_hidden_all_tokens(n_tokens)
                _llama._distributed_n_past += n_tokens

                rms_in = float(np.sqrt(np.mean(np.square(hidden))))
                rms_out = float(np.sqrt(np.mean(np.square(new_hidden))))
                print(f"[Hidden] seq={sid} MID (n={n_tokens}, pos={n_past}, RMS {rms_in:.2f}->{rms_out:.2f})")

                b64, shape = _encode_hidden(new_hidden)
                return InferResponse(hidden_state=b64, hidden_shape=shape, done=False)
            finally:
                lc.llama_batch_free(batch)



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
    _args = args

    print(f"Loading model : {args.model}", flush=True)
    print(f"Layers        : {args.start_layer}–{args.end_layer}", flush=True)
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

    print("Initializing Llama model (this may take a few seconds)...", flush=True)
    _llama = Llama(args.model, **kwargs)
    print("Llama model initialized. Resetting...", flush=True)
    _llama.reset()

    # Always ensure causal attention is on for correct autoregressive generation.
    lc.llama_set_causal_attn(_llama.ctx, True)

    _n_embd = int(_llama.n_embd())
    _n_vocab = int(_llama.n_vocab())
    actual_n_ctx = int(lc.llama_n_ctx(_llama.ctx))

    print(f"Mode          : {'single-peer (causal gen)' if args.single_peer else 'multi-peer (embedding extraction)'}", flush=True)
    print(f"Model loaded  : hidden_dim={_n_embd} vocab_size={_n_vocab}", flush=True)
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
