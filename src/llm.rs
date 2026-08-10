//! Low-level llama.cpp inference engine.
//!
//! Wraps `llama-cpp-sys-2` raw FFI to give the inference server a safe,
//! typed surface. All GPU/CPU calls are here; the rest of the codebase is pure
//! Rust.
//!
//! # Residual capture
//!
//! A non-last peer must hand the *residual stream* (the un-normalised `h` that
//! enters the next block's attention) to its successor. `llama_get_embeddings_ith`
//! returns the post-output_norm value instead, so we install a
//! `ggml_backend_sched_eval_callback` that fires when the last block's output
//! tensor (`l_out-<N>`) is computed and copies it before output_norm sees it.
//!
//! The callback is an `extern "C"` fn with no closure environment. We store the
//! capture state in two process-global `Mutex`es. Because only one decode runs
//! at a time (serialised by the HTTP server's per-engine mutex), this is safe.

use anyhow::{bail, Context, Result};
use std::ffi::{c_void, CStr, CString};
use std::ptr;
use std::sync::Mutex;
use tracing::warn;

use llama_cpp_sys_2 as sys;

// ── Capture globals ───────────────────────────────────────────────────────────

static CAPTURE_WANT: Mutex<Option<CString>> = Mutex::new(None);
static CAPTURE_BUF: Mutex<Option<Vec<f32>>> = Mutex::new(None);

unsafe extern "C" fn capture_callback(
    t: *mut sys::ggml_tensor,
    ask: bool,
    _user_data: *mut c_void,
) -> bool {
    let want = match CAPTURE_WANT.lock() {
        Ok(g) => g,
        Err(_) => return true,
    };
    let Some(ref target) = *want else { return true };

    let name = unsafe { sys::ggml_get_name(t) };
    if name.is_null() {
        return true;
    }
    let matches = unsafe { CStr::from_ptr(name) } == target.as_c_str();

    // The two phases mean different things, and the return value means something
    // different in each — see `ggml_backend_sched_compute_splits`:
    //
    //   ask = true   "will you want this tensor?" `false` lets ggml batch this
    //                node with the following ones, which is what we want for
    //                every node but ours.
    //   ask = false  the tensor is ready. Here `false` does NOT mean "not mine":
    //                it BREAKS the evaluation loop and abandons the rest of the
    //                graph.
    //
    // Returning `ask` for a non-matching tensor got both wrong at once: it
    // claimed every node, forcing them to be computed one at a time, and then
    // returned false on the first one — so the forward pass stopped at the very
    // first node while `llama_decode` still reported success. Nothing after the
    // input embedding was ever computed.
    if ask {
        return matches;
    }
    if !matches {
        return true; // keep going; this node was simply not ours
    }

    // Compute phase: copy the tensor bytes.
    let n_bytes = unsafe { sys::ggml_nbytes(t) } as usize;
    let n_floats = n_bytes / 4;
    let mut raw = vec![0u8; n_bytes];
    unsafe {
        sys::ggml_backend_tensor_get(t, raw.as_mut_ptr() as *mut c_void, 0, n_bytes);
    }
    let floats: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    if floats.len() == n_floats {
        if let Ok(mut buf) = CAPTURE_BUF.lock() {
            *buf = Some(floats);
        }
    }
    true
}

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub n_ctx: u32,
    pub n_batch: u32,
    pub n_gpu_layers: i32,
    pub n_threads: i32,
    pub max_sessions: u32,
    /// True for non-last peers that need to hand the residual to the next peer.
    pub need_embeddings: bool,
}

// ── Engine ────────────────────────────────────────────────────────────────────

pub struct LlmEngine {
    model: *mut sys::llama_model,
    ctx: *mut sys::llama_context,
    vocab: *const sys::llama_vocab,
    pub n_embd: usize,
    pub n_vocab: usize,
    /// Whether the residual capture callback was successfully installed.
    capture_ok: bool,
}

// SAFETY: We serialise all GPU calls behind a `Mutex<LlmEngine>` in the HTTP
// server, so these are never accessed concurrently from multiple threads.
unsafe impl Send for LlmEngine {}
unsafe impl Sync for LlmEngine {}

impl Drop for LlmEngine {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_free(self.ctx);
            }
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
            }
        }
    }
}

impl LlmEngine {
    /// Load a GGUF model slice and create a llama context.
    pub fn load(model_path: &str, cfg: &EngineConfig) -> Result<Self> {
        // llama_backend_init is idempotent; call it on every load.
        unsafe { sys::llama_backend_init() };

        let path = CString::new(model_path)
            .with_context(|| format!("invalid model path: {model_path}"))?;

        // ── Model ────────────────────────────────────────────────────────────
        let mut mparams = unsafe { sys::llama_model_default_params() };
        mparams.n_gpu_layers = cfg.n_gpu_layers;

        let model = unsafe { sys::llama_model_load_from_file(path.as_ptr(), mparams) };
        if model.is_null() {
            bail!("llama_model_load_from_file failed for {model_path}");
        }

        let vocab = unsafe { sys::llama_model_get_vocab(model) };
        let n_embd = unsafe { sys::llama_n_embd(model) } as usize;
        let n_vocab = unsafe { sys::llama_n_vocab(vocab) } as usize;

        // ── Context params ───────────────────────────────────────────────────
        let mut cparams = unsafe { sys::llama_context_default_params() };
        cparams.n_ctx = cfg.n_ctx;
        cparams.n_batch = cfg.n_batch;
        cparams.n_ubatch = cfg.n_batch;
        cparams.n_seq_max = cfg.max_sessions;
        cparams.offload_kqv = true;
        cparams.embeddings = cfg.need_embeddings;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        // ── Residual capture callback ────────────────────────────────────────
        let capture_ok = if cfg.need_embeddings {
            // Target tensor: l_out-<last_layer_index> of this slice.
            let n_layer = unsafe { sys::llama_model_n_layer(model) };
            let tensor_name = CString::new(format!("l_out-{}", n_layer - 1))
                .expect("tensor name is valid ASCII");
            *CAPTURE_WANT.lock().unwrap() = Some(tensor_name);
            *CAPTURE_BUF.lock().unwrap() = None;
            cparams.cb_eval = Some(capture_callback);
            cparams.cb_eval_user_data = ptr::null_mut();
            true
        } else {
            *CAPTURE_WANT.lock().unwrap() = None;
            false
        };

        // ── Create context ───────────────────────────────────────────────────
        let ctx = unsafe { sys::llama_new_context_with_model(model, cparams) };
        if ctx.is_null() {
            unsafe { sys::llama_model_free(model) };
            bail!("llama_new_context_with_model failed (n_seq_max={})", cfg.max_sessions);
        }

        // Ensure causal attention is on — needed when embeddings mode is active.
        unsafe { sys::llama_set_causal_attn(ctx, true) };

        let actual_n_ctx = unsafe { sys::llama_n_ctx(ctx) };
        let n_ctx_seq = actual_n_ctx / cfg.max_sessions.max(1);
        tracing::info!(
            "llama context ready: n_seq_max={} n_ctx={} n_ctx_seq={} capture={}",
            cfg.max_sessions, actual_n_ctx, n_ctx_seq,
            if capture_ok { format!("l_out-{}", unsafe { sys::llama_model_n_layer(model) } - 1) }
            else { "disabled".to_string() }
        );

        Ok(LlmEngine { model, ctx, vocab, n_embd, n_vocab, capture_ok })
    }

    pub fn n_ctx(&self) -> u32 {
        unsafe { sys::llama_n_ctx(self.ctx) }
    }

    pub fn eos_token(&self) -> i32 {
        unsafe { sys::llama_vocab_eos(self.vocab) }
    }

    pub fn bos_token(&self) -> i32 {
        unsafe { sys::llama_vocab_bos(self.vocab) }
    }

    // ── Tokenization ─────────────────────────────────────────────────────────

    pub fn tokenize(&self, text: &str, add_special: bool, parse_special: bool) -> Vec<i32> {
        let c_text = match CString::new(text) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        // Sizing call. With `n_tokens_max = 0` llama.cpp cannot write anything,
        // so it reports the space it *would* need as a NEGATIVE number - the
        // documented contract in llama.h ("returns a negative number on failure
        // - the number of tokens that would have been returned"). Reading that
        // as an error made every prompt tokenize to nothing, and the head peer
        // rejected chunk 0 with "não tem tokens" before any hidden state ever
        // reached the second peer.
        let n = unsafe {
            sys::llama_tokenize(
                self.vocab,
                c_text.as_ptr(),
                text.len() as i32,
                ptr::null_mut(),
                0,
                add_special,
                parse_special,
            )
        };
        // INT32_MIN means the count overflowed; anything else negative is the
        // capacity we have to allocate.
        if n == 0 || n == i32::MIN {
            return vec![];
        }
        let cap = n.unsigned_abs() as usize;
        let mut tokens = vec![0i32; cap];
        let written = unsafe {
            sys::llama_tokenize(
                self.vocab,
                c_text.as_ptr(),
                text.len() as i32,
                tokens.as_mut_ptr(),
                cap as i32,
                add_special,
                parse_special,
            )
        };
        if written <= 0 {
            return vec![];
        }
        tokens.truncate(written as usize);
        tokens
    }

    pub fn token_to_piece(&self, token: i32, special: bool) -> String {
        let mut buf = vec![0u8; 256];
        let n = unsafe {
            sys::llama_token_to_piece(
                self.vocab,
                token,
                buf.as_mut_ptr() as *mut i8,
                buf.len() as i32,
                0,
                special,
            )
        };
        if n <= 0 {
            return String::new();
        }
        buf.truncate(n as usize);
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn tokens_to_string(&self, tokens: &[i32]) -> String {
        tokens.iter().map(|&t| self.token_to_piece(t, false)).collect()
    }

    // ── Chat template ─────────────────────────────────────────────────────────

    /// The `tokenizer.chat_template` from the model's GGUF metadata, or None.
    pub fn chat_template(&self) -> Option<String> {
        let key = CString::new("tokenizer.chat_template").ok()?;
        let mut buf = vec![0u8; 32768];
        let n = unsafe {
            sys::llama_model_meta_val_str(
                self.model,
                key.as_ptr(),
                buf.as_mut_ptr() as *mut i8,
                buf.len(),
            )
        };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Apply the model's built-in Jinja2 chat template.
    ///
    /// Returns the formatted prompt string. On failure falls back to a simple
    /// Qwen-style `<|im_start|>` wrapper — same as the Python server's last resort.
    pub fn apply_chat_template(
        &self,
        messages: &[(&str, &str)],
        add_generation_prompt: bool,
        no_thinking: bool,
    ) -> String {
        let sys_msg = "You are a helpful assistant.";
        let full_messages: Vec<(&str, &str)> = std::iter::once(("system", sys_msg))
            .chain(messages.iter().copied())
            .collect();

        let tmpl = self.chat_template();

        // Build the llama_chat_message array.
        let c_msgs: Vec<(CString, CString)> = full_messages
            .iter()
            .filter_map(|(role, content)| {
                Some((CString::new(*role).ok()?, CString::new(*content).ok()?))
            })
            .collect();
        let raw_msgs: Vec<sys::llama_chat_message> = c_msgs
            .iter()
            .map(|(role, content)| sys::llama_chat_message {
                role: role.as_ptr(),
                content: content.as_ptr(),
            })
            .collect();

        let tmpl_ptr = tmpl
            .as_ref()
            .and_then(|s| CString::new(s.as_str()).ok())
            .map(|c| c.into_raw() as *const i8)
            .unwrap_or(ptr::null());

        let mut out_buf = vec![0u8; 65536];
        let n = unsafe {
            sys::llama_chat_apply_template(
                tmpl_ptr,
                raw_msgs.as_ptr(),
                raw_msgs.len(),
                add_generation_prompt,
                out_buf.as_mut_ptr() as *mut i8,
                out_buf.len() as i32,
            )
        };

        // Reclaim tmpl_ptr if we allocated one.
        if !tmpl_ptr.is_null() {
            unsafe { drop(CString::from_raw(tmpl_ptr as *mut i8)) };
        }

        let mut text = if n > 0 {
            out_buf.truncate(n as usize);
            String::from_utf8_lossy(&out_buf).into_owned()
        } else {
            warn!("llama_chat_apply_template returned {n}; using Qwen fallback");
            qwen_fallback(messages, sys_msg, no_thinking)
        };

        if no_thinking {
            if let Some(ref tmpl_str) = tmpl {
                if tmpl_str.contains("<think>") {
                    text.push_str("<think>\n\n</think>\n");
                }
            }
        }
        text
    }

    // ── Batch decode ─────────────────────────────────────────────────────────

    /// Decode a batch of token IDs. `logits_all=true` means every position
    /// gets logits; `false` means only the last one.
    pub fn decode_tokens(
        &mut self,
        tokens: &[i32],
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
    ) -> Result<()> {
        let n = tokens.len() as i32;
        let mut batch = unsafe { sys::llama_batch_init(n, 0, 1) };
        batch.n_tokens = n;

        unsafe {
            for (i, &tok) in tokens.iter().enumerate() {
                *batch.token.add(i) = tok;
                *batch.pos.add(i) = (pos_offset as i32) + i as i32;
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = if logits_all || i == tokens.len() - 1 {
                    1
                } else {
                    0
                };
            }
        }

        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode failed rc={rc}");
        }
        Ok(())
    }

    /// Decode a batch of embedding vectors (hidden states from the previous peer).
    pub fn decode_hidden(
        &mut self,
        hidden: &[f32],
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
    ) -> Result<()> {
        if hidden.len() != n_tokens * self.n_embd {
            bail!(
                "hidden state length {} != n_tokens {} × n_embd {}",
                hidden.len(),
                n_tokens,
                self.n_embd
            );
        }
        let n = n_tokens as i32;
        let mut batch = unsafe { sys::llama_batch_init(n, self.n_embd as i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut(); // signals embedding mode

        unsafe {
            // Copy the flat embedding data into the batch's embd buffer.
            let embd_ptr = batch.embd;
            std::ptr::copy_nonoverlapping(hidden.as_ptr(), embd_ptr, hidden.len());

            for i in 0..n_tokens {
                *batch.pos.add(i) = (pos_offset as i32) + i as i32;
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = if logits_all || i == n_tokens - 1 {
                    1
                } else {
                    0
                };
            }
        }

        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden) failed rc={rc}");
        }
        Ok(())
    }

    // ── Residual extraction ───────────────────────────────────────────────────

    /// Consume the residual captured during the last `decode_*` call.
    ///
    /// Must be called exactly once per decode when `need_embeddings` is true.
    pub fn take_residual(&self, n_tokens: usize) -> Result<Vec<f32>> {
        if self.capture_ok {
            let buf = CAPTURE_BUF.lock().unwrap().take();
            match buf {
                Some(v) if v.len() == n_tokens * self.n_embd => Ok(v),
                Some(v) => bail!(
                    "residual capture: got {} floats, expected {} ({}×{})",
                    v.len(),
                    n_tokens * self.n_embd,
                    n_tokens,
                    self.n_embd
                ),
                None => bail!(
                    "residual capture did not fire — refusing to fall back to the post-norm state"
                ),
            }
        } else {
            // Fallback: post-output_norm embeddings. Only correct when the
            // slice's output_norm is all-ones (shallow cuts are wrong with this).
            let mut out = Vec::with_capacity(n_tokens * self.n_embd);
            for i in 0..n_tokens {
                let ptr =
                    unsafe { sys::llama_get_embeddings_ith(self.ctx, i as i32) };
                if ptr.is_null() {
                    bail!("llama_get_embeddings_ith returned NULL for i={i}");
                }
                unsafe {
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, self.n_embd));
                }
            }
            Ok(out)
        }
    }

    // ── Logits ───────────────────────────────────────────────────────────────

    pub fn logits_ith(&self, i: i32) -> &[f32] {
        let ptr = unsafe { sys::llama_get_logits_ith(self.ctx, i) };
        if ptr.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, self.n_vocab) }
    }

    // ── KV cache management ───────────────────────────────────────────────────

    pub fn kv_remove(&self, seq_id: u32, from: i32, to: i32) {
        // Use the newer llama_get_memory + llama_memory_seq_rm API.
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if !mem.is_null() {
                sys::llama_memory_seq_rm(mem, seq_id as i32, from, to);
            }
        }
    }
}

// ── Sampling ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub repeat_penalty: f32,
    pub repeat_last_n: usize,
}

/// Temperature + top-k + top-p + repetition-penalty sampling.
///
/// Returns the sampled token id. When `temperature <= 0` this is greedy argmax.
pub fn sample_token(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> i32 {
    if logits.is_empty() {
        return 0;
    }
    let mut logits = logits.to_vec();

    // Repetition penalty.
    if cfg.repeat_penalty != 1.0 && !recent.is_empty() {
        let window_start = recent.len().saturating_sub(cfg.repeat_last_n);
        let window: std::collections::HashSet<i32> = recent[window_start..].iter().copied().collect();
        for &tok in &window {
            let t = tok as usize;
            if t < logits.len() {
                logits[t] = if logits[t] > 0.0 {
                    logits[t] / cfg.repeat_penalty
                } else {
                    logits[t] * cfg.repeat_penalty
                };
            }
        }
    }

    if cfg.temperature <= 0.0 {
        return logits
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i as i32)
            .unwrap_or(0);
    }

    // Apply temperature.
    let max_l = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut probs: Vec<f32> = logits
        .iter()
        .map(|&l| ((l - max_l) / cfg.temperature).exp())
        .collect();

    // Top-K filter.
    if cfg.top_k > 0 && (cfg.top_k as usize) < probs.len() {
        let k = cfg.top_k as usize;
        let mut sorted = probs.clone();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let threshold = sorted[k - 1];
        probs.iter_mut().for_each(|p| {
            if *p < threshold {
                *p = 0.0;
            }
        });
    }

    // Top-P (nucleus) filter.
    if cfg.top_p < 1.0 {
        let mut indexed: Vec<(usize, f32)> = probs.iter().copied().enumerate().collect();
        indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let mut cumsum = 0.0f32;
        let mut cutoff = f32::NEG_INFINITY;
        for (_, p) in &indexed {
            cumsum += p;
            if cumsum >= cfg.top_p {
                cutoff = *p;
                break;
            }
        }
        probs.iter_mut().for_each(|p| {
            if *p < cutoff {
                *p = 0.0;
            }
        });
    }

    let total: f32 = probs.iter().sum();
    if total <= 0.0 {
        return logits
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i as i32)
            .unwrap_or(0);
    }
    probs.iter_mut().for_each(|p| *p /= total);

    // Multinomial sample.
    let mut rng_val: f32 = unsafe {
        // Use a simple LCG seeded from the system clock to avoid pulling in `rand`.
        static RNG: Mutex<u64> = Mutex::new(0x853c49e6748fea9b);
        let mut r = RNG.lock().unwrap();
        *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bits = ((*r >> 33) as u32) | 0x3f800000;
        f32::from_bits(bits) - 1.0
    };
    let mut cumsum = 0.0f32;
    for (i, &p) in probs.iter().enumerate() {
        cumsum += p;
        if rng_val <= cumsum {
            return i as i32;
        }
    }
    probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i as i32)
        .unwrap_or(0)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn qwen_fallback(messages: &[(&str, &str)], system: &str, no_thinking: bool) -> String {
    let mut out = format!("<|im_start|>system\n{system}<|im_end|>\n");
    for (role, content) in messages {
        out.push_str(&format!("<|im_start|>{role}\n{content}<|im_end|>\n"));
    }
    out.push_str("<|im_start|>assistant\n");
    if no_thinking {
        out.push_str("<think>\n\n</think>\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture target and buffer are process-global — the module doc
    /// explains why that is sound in production, where one engine exists per
    /// process. Tests run in parallel threads, so two engines would exist at
    /// once and whichever loaded last would decide what the other captures.
    /// This lock restores the one-engine-at-a-time property the design assumes.
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// `tokenize` sizes its buffer from a call with `n_tokens_max = 0`, whose
    /// documented answer is the NEGATIVE of the count. Reading that as an error
    /// made every prompt come back empty, and the head peer of the pipeline
    /// rejected chunk 0 with "não tem tokens" before the second peer was ever
    /// contacted. Only a real vocab exercises the contract, so the test points
    /// at a GGUF given by the environment and skips when there is none.

    /// The residual capture has to actually fire, and the tensor has to have one
    /// row per token. This is the regression test for the eval-callback
    /// contract: returning `false` in the compute phase does not mean "not my
    /// tensor", it breaks ggml's evaluation loop and abandons the rest of the
    /// graph. With that inverted, the forward pass stopped at the input
    /// embedding, `llama_decode` still returned success, and the peer answered
    /// "residual capture did not fire".
    #[test]
    fn residual_capture_fires_with_one_row_per_token() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("RDMA_P2P_TEST_GGUF") else {
            eprintln!("skipping: set RDMA_P2P_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let toks = engine.tokenize("Explique RDMA em uma frase.", true, true);
        assert!(!toks.is_empty(), "prompt must tokenize");
        engine.decode_tokens(&toks, 0, 0, true).expect("decode succeeds");

        let residual = engine
            .take_residual(toks.len())
            .expect("the capture callback must have fired");
        assert_eq!(residual.len(), toks.len() * engine.n_embd);
        // An abandoned graph leaves the buffer zeroed; a real residual does not.
        assert!(
            residual.iter().any(|v| *v != 0.0 && v.is_finite()),
            "residual is all zeros or non-finite — the graph did not run"
        );
    }

    #[test]
    fn tokenize_returns_tokens_for_a_real_vocab() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("RDMA_P2P_TEST_GGUF") else {
            eprintln!("skipping: set RDMA_P2P_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
        };
        let engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let tokens = engine.tokenize("oi", true, true);
        assert!(!tokens.is_empty(), "a two-character prompt must tokenize");
        assert!(
            engine.tokenize("Explique o que e RDMA em uma frase.", true, true).len() > tokens.len(),
            "a longer prompt must produce more tokens"
        );
    }
}
