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
use std::time::Instant;
use tracing::warn;

use llama_cpp_sys_2 as sys;

// ── Capture globals ───────────────────────────────────────────────────────────

static CAPTURE_WANT: Mutex<Option<CString>> = Mutex::new(None);
static CAPTURE_BUF: Mutex<Option<Vec<f32>>> = Mutex::new(None);
static CAPTURE_COPY_US: Mutex<u64> = Mutex::new(0);

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
    let copy_started = Instant::now();
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
    if let Ok(mut elapsed) = CAPTURE_COPY_US.lock() {
        *elapsed += copy_started.elapsed().as_micros() as u64;
    }
    true
}

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub n_ctx: u32,
    pub offload_kqv: bool,
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
    offload_kqv: bool,
}

/// A diagnostic boundary around `llama_decode`. `backend_completion_us` is
/// measured with the public `llama_synchronize` API and therefore includes all
/// queued backend work and output transfers that must complete. It is not
/// labelled as pure kernel time.
#[derive(Debug, Clone, Default)]
pub struct DecodeTimings {
    pub enqueue_us: u64,
    pub backend_completion_us: u64,
    pub residual_capture_d2h_sync_us: u64,
}

/// Off by default: the extra `llama_synchronize` is a measurement barrier, and
/// leaving it on would charge every request for a diagnostic nobody asked for.
fn profile_backend_sync() -> bool {
    std::env::var("RDMA_P2P_PROFILE_SYNC")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
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
        cparams.offload_kqv = cfg.offload_kqv;
        cparams.embeddings = cfg.need_embeddings;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        // ── Residual capture callback ────────────────────────────────────────
        let capture_ok = if cfg.need_embeddings {
            // Target tensor: l_out-<last_layer_index> of this slice.
            let n_layer = unsafe { sys::llama_model_n_layer(model) };
            let tensor_name =
                CString::new(format!("l_out-{}", n_layer - 1)).expect("tensor name is valid ASCII");
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
            bail!(
                "llama_new_context_with_model failed (n_seq_max={})",
                cfg.max_sessions
            );
        }

        // Ensure causal attention is on — needed when embeddings mode is active.
        unsafe { sys::llama_set_causal_attn(ctx, true) };

        let actual_n_ctx = unsafe { sys::llama_n_ctx(ctx) };
        let n_ctx_seq = actual_n_ctx / cfg.max_sessions.max(1);
        tracing::info!(
            "llama context ready: n_seq_max={} n_ctx={} n_ctx_seq={} capture={}",
            cfg.max_sessions,
            actual_n_ctx,
            n_ctx_seq,
            if capture_ok {
                format!("l_out-{}", unsafe { sys::llama_model_n_layer(model) } - 1)
            } else {
                "disabled".to_string()
            }
        );

        Ok(LlmEngine {
            model,
            ctx,
            vocab,
            n_embd,
            n_vocab,
            capture_ok,
            offload_kqv: cfg.offload_kqv,
        })
    }

    pub fn n_ctx(&self) -> u32 {
        unsafe { sys::llama_n_ctx(self.ctx) }
    }

    pub fn offload_kqv(&self) -> bool {
        // The setting is immutable for this context and carried in the engine
        // configuration only during construction. Keep a copy visible through
        // `/health` rather than guessing from GPU utilisation.
        self.offload_kqv
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
        tokens
            .iter()
            .map(|&t| self.token_to_piece(t, false))
            .collect()
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
    ) -> Result<DecodeTimings> {
        *CAPTURE_COPY_US.lock().unwrap() = 0;
        let enqueue_started = Instant::now();
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
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: *CAPTURE_COPY_US.lock().unwrap(),
            ..Default::default()
        };
        if profile_backend_sync() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        Ok(timings)
    }

    /// Decode a batch of embedding vectors (hidden states from the previous peer).
    pub fn decode_hidden(
        &mut self,
        hidden: &[f32],
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
    ) -> Result<DecodeTimings> {
        *CAPTURE_COPY_US.lock().unwrap() = 0;
        let enqueue_started = Instant::now();
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
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: *CAPTURE_COPY_US.lock().unwrap(),
            ..Default::default()
        };
        if profile_backend_sync() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        Ok(timings)
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
                let ptr = unsafe { sys::llama_get_embeddings_ith(self.ctx, i as i32) };
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

/// CPU-only sampling boundaries. GPU/backend completion is measured before
/// this function, so a long CPU sampler cannot hide a pending CUDA stream.
///
/// Measured in **microseconds**. Milliseconds truncated every stage that cost
/// less than 1 ms to zero, which after this rewrite is every stage — the whole
/// sampler now costs less than one of the old millisecond ticks.
#[derive(Debug, Clone, Default)]
pub struct SamplingTimings {
    pub logits_access_us: u64,
    pub logits_clone_us: u64,
    pub repetition_penalty_us: u64,
    pub softmax_us: u64,
    pub top_k_us: u64,
    pub top_p_us: u64,
    pub selection_us: u64,
    pub token_text_us: u64,
}

/// Temperature + top-k + top-p + repetition-penalty sampling.
///
/// Returns the sampled token id. When `temperature <= 0` this is greedy argmax.
pub fn sample_token_profiled(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
) -> (i32, SamplingTimings) {
    let mut timings = SamplingTimings::default();
    if logits.is_empty() {
        return (0, timings);
    }
    let clone_started = Instant::now();
    let mut logits = logits.to_vec();
    timings.logits_clone_us = clone_started.elapsed().as_micros() as u64;

    // Repetition penalty.
    let repetition_started = Instant::now();
    if cfg.repeat_penalty != 1.0 && !recent.is_empty() {
        let window_start = recent.len().saturating_sub(cfg.repeat_last_n);
        let window: std::collections::HashSet<i32> =
            recent[window_start..].iter().copied().collect();
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
    timings.repetition_penalty_us = repetition_started.elapsed().as_micros() as u64;

    if cfg.temperature <= 0.0 {
        let selection_started = Instant::now();
        let token = argmax(&logits);
        timings.selection_us = selection_started.elapsed().as_micros() as u64;
        return (token, timings);
    }

    // ── Top-K, on the raw logits ─────────────────────────────────────────────
    //
    // `exp` is strictly increasing, so the k largest logits are the k largest
    // probabilities — the cut can be made before the softmax and without
    // sorting. `select_nth_unstable_by` partitions in O(V) instead of the
    // O(V log V) full sort this used to do, and everything downstream then
    // works on k values instead of the whole vocabulary.
    //
    // On a 152k-token vocabulary with the default k of 40, that is the
    // difference between ~30 ms and a few microseconds per token.
    let top_k_started = Instant::now();
    let kept_ids: Vec<usize> = if cfg.top_k > 0 && (cfg.top_k as usize) < logits.len() {
        let k = cfg.top_k as usize;
        let mut scratch = logits.clone();
        let (_, kth, _) = scratch.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
        let threshold = *kth;
        // `>=` keeps ties at the cut, exactly as the old `p < threshold` did.
        (0..logits.len())
            .filter(|&i| logits[i] >= threshold)
            .collect()
    } else {
        (0..logits.len()).collect()
    };
    timings.top_k_us = top_k_started.elapsed().as_micros() as u64;

    // ── Softmax, over the survivors only ─────────────────────────────────────
    //
    // The shift uses the largest surviving logit, which is the largest logit
    // overall — the argmax always survives a top-k. So the values here are bit
    // for bit what a softmax over the whole vocabulary produced.
    let softmax_started = Instant::now();
    let max_l = kept_ids
        .iter()
        .map(|&i| logits[i])
        .fold(f32::NEG_INFINITY, f32::max);
    let mut kept: Vec<(usize, f32)> = kept_ids
        .into_iter()
        .map(|i| (i, ((logits[i] - max_l) / cfg.temperature).exp()))
        .collect();
    timings.softmax_us = softmax_started.elapsed().as_micros() as u64;

    // ── Top-P (nucleus) ──────────────────────────────────────────────────────
    //
    // Sorting is unavoidable here, but it now runs over k elements. Note that
    // the walk is over UNNORMALISED probabilities, which is a defect kept
    // deliberately: the largest is always `exp(0) == 1.0`, so any `top_p <= 1`
    // is met by the first element and the draw collapses to greedy. Fixing it
    // changes what the cluster generates, so it is a separate decision — see
    // `top_p_below_one_currently_collapses_to_greedy`.
    let top_p_started = Instant::now();
    if cfg.top_p < 1.0 {
        let mut by_prob: Vec<f32> = kept.iter().map(|(_, p)| *p).collect();
        by_prob.sort_unstable_by(|a, b| b.total_cmp(a));
        let mut cumsum = 0.0f32;
        let mut cutoff = f32::NEG_INFINITY;
        for p in &by_prob {
            cumsum += p;
            if cumsum >= cfg.top_p {
                cutoff = *p;
                break;
            }
        }
        kept.retain(|(_, p)| *p >= cutoff);
    }
    timings.top_p_us = top_p_started.elapsed().as_micros() as u64;

    // ── Multinomial draw ─────────────────────────────────────────────────────
    //
    // `kept` is still in ascending index order, and the entries left out are
    // exactly the ones that used to hold 0.0, so this walks the same cumulative
    // sums as the old full-vocabulary loop and lands on the same token.
    let selection_started = Instant::now();
    let total: f32 = kept.iter().map(|(_, p)| *p).sum();
    if total <= 0.0 {
        let token = argmax(&logits);
        timings.selection_us = selection_started.elapsed().as_micros() as u64;
        return (token, timings);
    }

    let rng_val: f32 = {
        // A simple LCG, to avoid pulling in `rand`.
        static RNG: Mutex<u64> = Mutex::new(0x853c49e6748fea9b);
        let mut r = RNG.lock().unwrap();
        *r = r
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bits = ((*r >> 33) as u32) | 0x3f800000;
        f32::from_bits(bits) - 1.0
    };

    let mut cumsum = 0.0f32;
    let mut best = (0usize, f32::NEG_INFINITY);
    for (i, p) in &kept {
        let normalised = *p / total;
        cumsum += normalised;
        if rng_val <= cumsum {
            timings.selection_us = selection_started.elapsed().as_micros() as u64;
            return (*i as i32, timings);
        }
        // Rounding can leave `rng_val` just past the final cumulative sum; the
        // most likely token is the answer then, matching the old fallback.
        if normalised >= best.1 {
            best = (*i, normalised);
        }
    }
    timings.selection_us = selection_started.elapsed().as_micros() as u64;
    (best.0 as i32, timings)
}

/// Index of the largest value, matching `max_by`'s "last one wins" on ties.
fn argmax(values: &[f32]) -> i32 {
    values
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i as i32)
        .unwrap_or(0)
}

/// Compatibility wrapper for callers that do not need a diagnostic breakdown.
pub fn sample_token(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> i32 {
    sample_token_profiled(logits, recent, cfg).0
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

    /// Characterisation tests: they pin the sampler's *current* choices so the
    /// O(V log V) → O(V) rewrite below can be proven not to change which token
    /// comes out. They are deliberately about behaviour, not speed.
    mod sampler_behaviour {
        use super::*;

        fn cfg(temperature: f32, top_k: i32, top_p: f32) -> SamplingConfig {
            SamplingConfig {
                temperature,
                top_p,
                top_k,
                repeat_penalty: 1.0,
                repeat_last_n: 0,
            }
        }

        /// Zero temperature is argmax, and argmax is the highest logit.
        #[test]
        fn zero_temperature_is_argmax() {
            let logits = [0.1, 9.0, 0.3, 2.0];
            assert_eq!(sample_token(&logits, &[], &cfg(0.0, 40, 1.0)), 1);
        }

        /// With top-p disabled, top-k has to be the only thing narrowing the
        /// field: no token outside the k highest logits may ever be drawn.
        #[test]
        fn top_k_confines_the_draw_to_the_k_highest_logits() {
            // Indices 3 and 1 hold the two highest logits.
            let logits = [0.0, 5.0, 1.0, 9.0, 2.0, 0.5];
            let c = cfg(1.0, 2, 1.0);

            for _ in 0..500 {
                let picked = sample_token(&logits, &[], &c);
                assert!(picked == 1 || picked == 3, "saiu fora do top-2: {picked}");
            }
        }

        /// Ties at the cut are kept rather than broken arbitrarily, so a k of 1
        /// over three equal maxima can still draw any of them.
        #[test]
        fn a_tie_at_the_cut_survives() {
            let logits = [7.0, 7.0, 7.0, 1.0];
            let c = cfg(1.0, 1, 1.0);

            let mut seen = [false; 3];
            for _ in 0..500 {
                let picked = sample_token(&logits, &[], &c);
                assert!(picked != 3, "o logit baixo nunca deveria sair");
                seen[picked as usize] = true;
            }
            assert!(seen.iter().all(|s| *s), "os três empatados têm que sair");
        }

        /// The repetition penalty divides positive logits, which can move a
        /// token out of the running entirely.
        #[test]
        fn the_repetition_penalty_can_push_a_token_out_of_the_top_k() {
            let logits = [10.0, 9.0, 0.0];
            let penalised = SamplingConfig {
                repeat_penalty: 4.0,
                repeat_last_n: 4,
                ..cfg(0.0, 40, 1.0)
            };
            // Index 0 falls to 2.5 and index 1 wins.
            assert_eq!(sample_token(&logits, &[0], &penalised), 1);
        }

        /// The measurement behind the rewrite, on a real Qwen vocabulary.
        ///
        /// Ignored because a wall-clock assertion belongs nowhere near CI, and
        /// because the debug profile makes the number meaningless. Run it the
        /// way the claim was made:
        ///
        /// ```text
        /// cargo test -p node --release sampler_cost_on_a_real_vocabulary -- --ignored --nocapture
        /// ```
        #[test]
        #[ignore = "medição de tempo: rode à mão, em --release"]
        fn sampler_cost_on_a_real_vocabulary() {
            const V: usize = 151_936; // Qwen3
            let logits: Vec<f32> = (0..V).map(|i| ((i % 977) as f32) * 0.01).collect();
            let c = cfg(0.7, 40, 1.0);

            let started = Instant::now();
            const ROUNDS: u32 = 50;
            for _ in 0..ROUNDS {
                std::hint::black_box(sample_token(&logits, &[], &c));
            }
            let per_token = started.elapsed() / ROUNDS;
            println!("amostragem: {per_token:?}/token com vocabulário de {V}");

            // The full-sort version cost ~30 ms/token here. Anything in that
            // range means the O(V) selection is not being taken.
            assert!(
                per_token < std::time::Duration::from_millis(5),
                "amostragem em {per_token:?}/token — o sort completo voltou?"
            );
        }

        /// Documents a real defect, so the rewrite preserves it instead of
        /// silently changing what the cluster generates. `probs` are NOT
        /// normalised when top-p walks them: the largest is always exactly
        /// `exp(0) == 1.0`, so any `top_p <= 1.0` is satisfied by the first
        /// element alone and everything else is cut. Every request with the
        /// default `top_p` of 0.9 is therefore greedy, and temperature and
        /// top_k have no effect at all.
        #[test]
        fn top_p_below_one_currently_collapses_to_greedy() {
            let logits = [1.0, 8.0, 7.9, 7.8];
            let c = cfg(1.0, 40, 0.9);

            for _ in 0..200 {
                assert_eq!(
                    sample_token(&logits, &[], &c),
                    1,
                    "top_p < 1.0 hoje só deixa passar o argmax"
                );
            }
        }
    }

    #[test]
    fn profiled_sampler_separates_cpu_work_from_logit_access() {
        let cfg = SamplingConfig {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 2,
            repeat_penalty: 1.0,
            repeat_last_n: 0,
        };

        let (_token, timings) = sample_token_profiled(&[0.1, 0.3, 0.8], &[], &cfg);

        assert_eq!(timings.logits_access_us, 0);
        assert_eq!(timings.logits_clone_us, 0);
        assert_eq!(timings.token_text_us, 0);
    }

    /// The capture target and buffer are process-global — the module doc
    /// explains why that is sound in production, where one engine exists per
    /// process. Tests run in parallel threads, so two engines would exist at
    /// once and whichever loaded last would decide what the other captures.
    /// This lock restores the one-engine-at-a-time property the design assumes.
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// How many layers the tests offload. Real peers run with everything on the
    /// GPU, and the capture behaves differently there, so this has to be a knob.
    fn test_gpu_layers() -> i32 {
        std::env::var("RDMA_P2P_TEST_GPU_LAYERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

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

    /// The middle-peer path: a hidden state comes in, the residual must come
    /// out. This is the one the pipeline actually breaks on and the one the
    /// token test above cannot reach — `decode_hidden` builds a different batch
    /// (`batch.token = NULL`, data in `batch.embd`) and therefore a different
    /// graph. Set `RDMA_P2P_TEST_GPU_LAYERS=99` to run it with the weights
    /// offloaded, which is how every real peer runs it.
    #[test]
    fn residual_capture_fires_for_an_injected_hidden_state() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("RDMA_P2P_TEST_GGUF") else {
            eprintln!("skipping: set RDMA_P2P_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");

        let n_tokens = 4;
        // Values in the range a real residual carries; zeros would let a graph
        // that never ran look identical to one that did.
        let hidden: Vec<f32> = (0..n_tokens * engine.n_embd)
            .map(|i| ((i % 97) as f32 - 48.0) / 48.0)
            .collect();
        engine
            .decode_hidden(&hidden, n_tokens, 0, 0, true)
            .expect("decode of an injected hidden state succeeds");

        let residual = engine
            .take_residual(n_tokens)
            .expect("the capture callback must fire on the hidden-state path too");
        assert_eq!(residual.len(), n_tokens * engine.n_embd);
        assert!(
            residual.iter().any(|v| *v != 0.0 && v.is_finite()),
            "residual is all zeros or non-finite — the graph did not run"
        );
    }

    #[test]
    fn residual_capture_fires_with_one_row_per_token() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("RDMA_P2P_TEST_GGUF") else {
            eprintln!("skipping: set RDMA_P2P_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let toks = engine.tokenize("Explique RDMA em uma frase.", true, true);
        assert!(!toks.is_empty(), "prompt must tokenize");
        engine
            .decode_tokens(&toks, 0, 0, true)
            .expect("decode succeeds");

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
            offload_kqv: true,
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
            engine
                .tokenize("Explique o que e RDMA em uma frase.", true, true)
                .len()
                > tokens.len(),
            "a longer prompt must produce more tokens"
        );
    }
}
