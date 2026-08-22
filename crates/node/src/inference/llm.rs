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
//! The callback is an `extern "C"` fn with no closure environment, so the
//! capture state travels in llama.cpp's own `cb_eval_user_data`: each context
//! owns a `CaptureSlot` and the callback is handed its address. It used to be
//! two process-global `Mutex`es, safe only while a single decode ran at a time;
//! with two contexts decoding in parallel that became a race in which one
//! request would carry the other's residual - no error, just a wrong answer.

use crate::config::KvCacheType;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr, CString};
use std::ops::Range;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::warn;

use llama_cpp_sys_2 as sys;

// ── Capture state, one per context ────────────────────────────────────────────

/// Estado da captura de um contexto.
///
/// Já foi global de processo, e o comentário do topo deste arquivo dizia que
/// isso era seguro porque um decode rodava por vez. Deixou de ser verdade no
/// momento em que dois contextos passaram a decodificar em paralelo: as duas
/// requisições escreveriam no mesmo buffer e uma levaria o residual da outra -
/// sem erro nenhum, só resposta errada. Agora cada contexto tem o seu, e o
/// endereço viaja no `cb_eval_user_data` do próprio llama.cpp.
struct CaptureSlot {
    want: Option<CString>,
    buf: Option<Vec<f32>>,
    copy_us: u64,
}

unsafe extern "C" fn capture_callback(
    t: *mut sys::ggml_tensor,
    ask: bool,
    user_data: *mut c_void,
) -> bool {
    if user_data.is_null() {
        return true;
    }
    // SAFETY: `user_data` é o ponteiro que `LlmEngine::load` registrou, e ele
    // aponta para o `Box<Mutex<CaptureSlot>>` do mesmo `LlmEngine` que é dono
    // deste contexto. O Box vive enquanto o contexto viver (ver `Drop`).
    let slot = unsafe { &*(user_data as *const Mutex<CaptureSlot>) };
    let mut slot = match slot.lock() {
        Ok(g) => g,
        Err(_) => return true,
    };
    if slot.want.is_none() {
        return true;
    }

    let name = unsafe { sys::ggml_get_name(t) };
    if name.is_null() {
        return true;
    }
    let matches = slot
        .want
        .as_deref()
        .is_some_and(|target| unsafe { CStr::from_ptr(name) } == target);

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
        accumulate_capture(&mut slot, floats);
    }
    slot.copy_us += copy_started.elapsed().as_micros() as u64;
    true
}

/// Junta o `l_out` de mais um microlote ao residual deste decode.
///
/// **Acumula, não substitui.** Um decode não é um disparo: o llama.cpp parte o
/// lote em microlotes de `n_ubatch` e chama a captura uma vez por microlote,
/// cada uma com o `l_out` daquele pedaço.
///
/// Enquanto isto atribuía, o buffer terminava com o **último**
/// microlote e o resto do chunk ia embora em silêncio - com `fit_memory=true`
/// (padrão), que limita `n_ubatch` a 128, um chunk de 512 tokens perdia 3/4 e o
/// peer seguinte recusava a requisição com "got 655360 floats, expected
/// 2621440".
///
/// A ordem sai certa porque `split_simple` do llama.cpp fatia o lote em índices
/// crescentes e contíguos, que é exatamente a ordem das linhas que
/// `BatchLayout` usa para reencontrar cada item. Ver o defeito anotado sobre
/// `max_sessions > 1` no `AGENTS.md` desta pasta: com vários streams o
/// llama.cpp usa `split_equal`, que agrupa por sequência, e aí esta ordem
/// deixa de valer.
fn accumulate_capture(slot: &mut CaptureSlot, floats: Vec<f32>) {
    match slot.buf.as_mut() {
        Some(acumulado) => acumulado.extend_from_slice(&floats),
        None => slot.buf = Some(floats),
    }
}

// ── Ponte de diagnóstico ─────────────────────────────────────────────────────

/// Chama uma função `llama_rs_*_json` e devolve o JSON já em `serde_json`.
///
/// Toda a família devolve `LLAMA_RS_STATUS_OK` mais uma string de `malloc` que
/// **tem que** voltar por `llama_rs_string_free` - `free` de outra alocação, ou
/// um `CString::from_raw`, é liberar com um alocador que não é o dono. Por isso
/// a liberação mora aqui e não em cada chamador.
///
/// # Safety
/// `f` tem que ser uma dessas funções, com os ponteiros do llama.cpp ainda
/// vivos.
unsafe fn json_from_wrapper(
    f: impl FnOnce(*mut *mut std::os::raw::c_char) -> sys::llama_rs_status,
) -> Option<serde_json::Value> {
    let mut raw: *mut std::os::raw::c_char = ptr::null_mut();
    let status = f(&mut raw);
    if status != sys::LLAMA_RS_STATUS_OK || raw.is_null() {
        warn!("diagnóstico do llama.cpp indisponível (status {status})");
        return None;
    }
    let texto = CStr::from_ptr(raw).to_string_lossy().into_owned();
    sys::llama_rs_string_free(raw);
    match serde_json::from_str(&texto) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("diagnóstico do llama.cpp veio com JSON inválido: {e}");
            None
        }
    }
}

// ── Configuration ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Capacidade total do cache KV. O runtime já multiplica a janela pública
    /// por sessão por `max_sessions` antes de montar esta configuração.
    pub n_ctx: u32,
    pub offload_kqv: bool,
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
    pub fit_memory: bool,
    pub n_batch: u32,
    pub n_gpu_layers: i32,
    pub n_threads: i32,
    pub max_sessions: u32,
    /// True for non-last peers that need to hand the residual to the next peer.
    pub need_embeddings: bool,
    /// Ligar a predição multi-token pelo bloco `nextn` do modelo, quando ele
    /// tiver um. Distinto da especulação por lookup (`find_draft`) - ver
    /// `crate::mtp`. Um modelo sem `nextn` ignora o pedido em silêncio: não é
    /// erro de configuração, é a fatia do meio de um pipeline.
    pub mtp: bool,
    /// Quantos snapshots de estado recorrente manter por sequência
    /// (`llama_context_params::n_rs_seq`), para permitir desfazer um
    /// rascunho especulativo recusado num modelo híbrido/recorrente
    /// (Qwen3.5+: `llm_arch_supports_rs_rollback` no llama.cpp). `0` é o
    /// padrão do llama.cpp - sem isto, `llama_memory_recurrent::seq_rm`
    /// nunca aceita um rollback parcial, e vale a pena pedir um valor maior
    /// que a maior largura de lote especulativo configurada (`speculative_k`,
    /// `mtp_draft_tokens + 1`); arquitetura sem suporte simplesmente zera o
    /// pedido de volta (`llama-context.cpp`), então pedir aqui não machuca
    /// modelo comum. Ver `infer_server::should_draft` e
    /// `gateway::rollback_sessions`, que são o resto do mecanismo.
    pub rs_seq_window: u32,
}

pub(crate) fn apply_cache_types(
    cparams: &mut sys::llama_context_params,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
) {
    cparams.type_k = cache_type_to_ggml(cache_type_k);
    cparams.type_v = cache_type_to_ggml(cache_type_v);
}

fn cache_type_to_ggml(cache_type: KvCacheType) -> sys::ggml_type {
    match cache_type {
        KvCacheType::F32 => sys::GGML_TYPE_F32,
        KvCacheType::F16 => sys::GGML_TYPE_F16,
        KvCacheType::Bf16 => sys::GGML_TYPE_BF16,
        KvCacheType::Q8_0 => sys::GGML_TYPE_Q8_0,
        KvCacheType::Q4_0 => sys::GGML_TYPE_Q4_0,
        KvCacheType::Q4_1 => sys::GGML_TYPE_Q4_1,
        KvCacheType::Iq4Nl => sys::GGML_TYPE_IQ4_NL,
        KvCacheType::Q5_0 => sys::GGML_TYPE_Q5_0,
        KvCacheType::Q5_1 => sys::GGML_TYPE_Q5_1,
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Os pesos, carregados uma vez e compartilhados por todos os contextos.
///
/// Separar isto do contexto é o que torna o paralelismo viável sem duplicar
/// memória: o que pesa são os pesos (gigabytes), e o llama.cpp os trata como
/// somente-leitura depois da carga - vários `llama_context` podem apontar para
/// o mesmo `llama_model`. O contexto carrega o cache KV; o `n_ctx` interno já
/// é a capacidade total necessária para todas as sequências.
pub struct LoadedModel {
    model: *mut sys::llama_model,
    vocab: *const sys::llama_vocab,
    pub n_embd: usize,
    pub n_vocab: usize,
}

// SAFETY: depois da carga o `llama_model` é somente-leitura; é o próprio
// llama.cpp que permite vários contextos sobre um modelo.
unsafe impl Send for LoadedModel {}
unsafe impl Sync for LoadedModel {}

const DEFAULT_GPU_LAYERS: i32 = 99;
const FIT_MEMORY_MARGIN_BYTES: usize = 1024 * 1024 * 1024;
const FIT_MIN_CONTEXT: u32 = 4096;
/// The CUDA graph and kernel workspaces scale with the microbatch, while the
/// public `n_batch` can stay large enough to accept a prompt chunk. A smaller
/// ubatch is the safe automatic fallback before llama.cpp reserves those
/// workspaces on a nearly-full GPU.
const FIT_MAX_UBATCH: u32 = 128;

/// Parâmetros do modelo já ajustados pelo `common_fit_params`.
///
/// Os vetores ficam vivos até `llama_model_load_from_file` consumir os
/// ponteiros guardados em `llama_model_params`. Isso é necessário no caso de
/// mais de uma GPU local, em que o fit também escreve `tensor_split` e
/// `tensor_buft_overrides`.
struct FitPlan {
    config: EngineConfig,
    tensor_split: Vec<f32>,
    tensor_buft_overrides: Vec<sys::llama_model_tensor_buft_override>,
    applied: bool,
}

fn fit_can_adjust_layers(cfg: &EngineConfig) -> bool {
    cfg.fit_memory && cfg.n_gpu_layers == DEFAULT_GPU_LAYERS
}

fn load_mtp_weights(cfg: &EngineConfig) -> bool {
    cfg.mtp
}

fn effective_n_ubatch(cfg: &EngineConfig) -> u32 {
    let n_batch = cfg.n_batch.max(1);
    if cfg.fit_memory {
        n_batch.min(FIT_MAX_UBATCH)
    } else {
        n_batch
    }
}

fn fit_plan(model_path: &str, cfg: &EngineConfig) -> Result<FitPlan> {
    if !fit_can_adjust_layers(cfg) {
        if cfg.fit_memory && cfg.n_gpu_layers != DEFAULT_GPU_LAYERS {
            warn!(
                "llama.cpp fit ignorado porque n_gpu_layers foi definido explicitamente como {}",
                cfg.n_gpu_layers
            );
        }
        return Ok(FitPlan {
            config: cfg.clone(),
            tensor_split: Vec::new(),
            tensor_buft_overrides: Vec::new(),
            applied: false,
        });
    }

    let path = CString::new(model_path)
        .with_context(|| format!("invalid model path for llama.cpp fit: {model_path}"))?;
    // `common_fit_params` queries the backend devices and their free memory.
    // The call is idempotent and mirrors `LoadedModel::load`, which also keeps
    // the initialization close to every possible model-loading entry point.
    unsafe { sys::llama_backend_init() };
    let n_devices = unsafe { sys::llama_max_devices() }.max(1);
    let n_overrides = unsafe { sys::llama_max_tensor_buft_overrides() }.max(1);
    let mut tensor_split = vec![0.0_f32; n_devices];
    let mut tensor_buft_overrides = vec![
        sys::llama_model_tensor_buft_override {
            pattern: ptr::null(),
            buft: ptr::null_mut(),
        };
        n_overrides
    ];
    let mut margins = vec![FIT_MEMORY_MARGIN_BYTES; n_devices];

    let mut mparams = unsafe { sys::llama_model_default_params() };
    mparams.n_gpu_layers = -1;
    mparams.load_mtp = load_mtp_weights(cfg);

    let mut cparams = unsafe { sys::llama_context_default_params() };
    cparams.n_ctx = cfg.n_ctx;
    cparams.n_batch = cfg.n_batch;
    cparams.n_ubatch = effective_n_ubatch(cfg);
    cparams.n_seq_max = cfg.max_sessions.max(1);
    cparams.offload_kqv = cfg.offload_kqv;
    apply_cache_types(&mut cparams, cfg.cache_type_k, cfg.cache_type_v);
    cparams.embeddings = cfg.need_embeddings;
    cparams.n_rs_seq = cfg.rs_seq_window;

    let status = unsafe {
        sys::llama_rs_fit_params(
            path.as_ptr(),
            &mut mparams,
            &mut cparams,
            tensor_split.as_mut_ptr(),
            tensor_buft_overrides.as_mut_ptr(),
            margins.as_mut_ptr(),
            FIT_MIN_CONTEXT,
            sys::GGML_LOG_LEVEL_WARN,
        )
    };

    if status != 0 {
        warn!(
            "llama.cpp fit não encontrou uma configuração automática (status {status}); usando os parâmetros solicitados"
        );
        return Ok(FitPlan {
            config: cfg.clone(),
            tensor_split: Vec::new(),
            tensor_buft_overrides: Vec::new(),
            applied: false,
        });
    }

    let mut effective = cfg.clone();
    effective.n_gpu_layers = mparams.n_gpu_layers;
    effective.n_ctx = cparams.n_ctx;

    tracing::info!(
        requested_gpu_layers = cfg.n_gpu_layers,
        effective_gpu_layers = effective.n_gpu_layers,
        requested_context = cfg.n_ctx,
        effective_context = effective.n_ctx,
        "llama.cpp fit ajustou os parâmetros de memória"
    );

    Ok(FitPlan {
        config: effective,
        tensor_split,
        tensor_buft_overrides,
        applied: true,
    })
}

impl LoadedModel {
    /// Devolve os pesos à GPU agora, em vez de esperar o `Drop`.
    ///
    /// Idempotente: zera o ponteiro, então o `Drop` que vier depois não tem o
    /// que liberar. Só `LlmEngine::unload` chama, e só quando é o último dono.
    fn free_now(&mut self) {
        unsafe {
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
                self.model = std::ptr::null_mut();
                self.vocab = std::ptr::null();
            }
        }
    }
}

impl Drop for LoadedModel {
    fn drop(&mut self) {
        self.free_now();
    }
}

pub struct LlmEngine {
    /// Os pesos. `Arc` porque o último contexto a morrer é quem os libera.
    loaded: Arc<LoadedModel>,
    ctx: *mut sys::llama_context,
    pub n_embd: usize,
    pub n_vocab: usize,
    /// Whether the residual capture callback was successfully installed.
    capture_ok: bool,
    /// Alvo e buffer da captura deste contexto. `Box` porque o endereço é
    /// registrado no llama.cpp em `cb_eval_user_data` e não pode mudar.
    capture: Box<Mutex<CaptureSlot>>,
    offload_kqv: bool,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
    fit_memory_applied: bool,
    effective_n_gpu_layers: i32,
    effective_n_ubatch: u32,
    /// A cabeça de rascunho `nextn`, quando o modelo traz uma e o operador
    /// ligou o MTP. `None` é o caso comum: modelo sem MTP, fatia não-final de
    /// um modelo com MTP, ou interruptor desligado.
    mtp: Option<crate::mtp::MtpDrafter>,
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

/// **Ligado por padrão** desde 2026-08-13; `POM_PROFILE_SYNC=0` desliga.
///
/// O `llama_synchronize` é uma barreira de medição, e a suspeita de que ela
/// custasse tempo foi o motivo de nascer desligada. Medido em
/// `logits_access_versus_backend_completion` (fatia final de 16 camadas, Metal,
/// 16 tokens por rodada, duas rodadas intercaladas de cada modo):
///
/// | | total por token | conclusão | acesso aos logits |
/// |---|---|---|---|
/// | desligado | 33,28 / 34,27 ms | 0 | 32,42 / 33,49 ms |
/// | ligado | 33,06 / 32,27 ms | 31,55 / 30,62 ms | 0 |
///
/// O total não muda (o modo ligado ficou até marginalmente mais rápido, dentro
/// do ruído): a barreira **não cria espera, só a nomeia**. Sem ela a espera
/// existia igual, cobrada no primeiro toque de CPU nos logits, e a telemetria
/// dizia que o gargalo era "acesso aos logits" quando era a GPU terminando.
/// Foi essa conta errada que a Mudança 6 do spec de 2026-08-12 pedia para
/// fechar.
fn profile_backend_sync() -> bool {
    std::env::var("POM_PROFILE_SYNC")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

// SAFETY: We serialise all GPU calls behind a `Mutex<LlmEngine>` in the HTTP
// server, so these are never accessed concurrently from multiple threads.
unsafe impl Send for LlmEngine {}
unsafe impl Sync for LlmEngine {}

impl LlmEngine {
    /// Devolve **agora** a VRAM deste motor: contexto, rascunho MTP e, se este
    /// for o último contexto sobre os pesos, os pesos.
    ///
    /// Existe porque esperar o `Drop` amarrava a liberação da memória ao tempo
    /// de vida das conexões: o `Router` do servidor de inferência carrega o
    /// `Arc<Mutex<InferState>>`, e cada conexão viva segura uma cópia dele. Uma
    /// conexão em keep-alive parada — o pool do gateway deixa várias — mantinha
    /// a contagem acima de zero para sempre, e o `undeploy` voltava sem liberar
    /// nada: o modelo seguinte não cabia. Quem chama garante que ninguém mais
    /// vai usar o motor (ver o encerramento em `infer_server::run`).
    ///
    /// Idempotente; depois dela o motor não decodifica mais nada.
    pub fn unload(&mut self) {
        // Primeiro o rascunho: o contexto dele também é VRAM.
        self.mtp = None;
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_free(self.ctx);
                self.ctx = std::ptr::null_mut();
            }
        }
        // Os pesos só saem se este era o último contexto sobre eles; com um
        // irmão vivo, `get_mut` devolve `None` e o `Drop` do último resolve.
        if let Some(loaded) = Arc::get_mut(&mut self.loaded) {
            loaded.free_now();
        }
    }
}

impl Drop for LlmEngine {
    fn drop(&mut self) {
        // Só o contexto: os pesos são de `LoadedModel`, liberados quando o
        // último contexto que os usa sai de cena. Liberar o modelo aqui, com um
        // irmão ainda vivo, seria use-after-free.
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_free(self.ctx);
            }
        }
    }
}

impl LoadedModel {
    /// Lê os pesos do disco. Uma vez por arquivo, não por contexto.
    fn load(model_path: &str, plan: &FitPlan) -> Result<Arc<Self>> {
        // llama_backend_init is idempotent; call it on every load.
        unsafe { sys::llama_backend_init() };

        let path = CString::new(model_path)
            .with_context(|| format!("invalid model path: {model_path}"))?;

        let mut mparams = unsafe { sys::llama_model_default_params() };
        mparams.n_gpu_layers = plan.config.n_gpu_layers;
        mparams.load_mtp = load_mtp_weights(&plan.config);
        if !plan.tensor_split.is_empty() {
            mparams.tensor_split = plan.tensor_split.as_ptr();
        }
        if !plan.tensor_buft_overrides.is_empty() {
            mparams.tensor_buft_overrides = plan.tensor_buft_overrides.as_ptr();
        }

        let model = unsafe { sys::llama_model_load_from_file(path.as_ptr(), mparams) };
        if model.is_null() {
            bail!("llama_model_load_from_file failed for {model_path}");
        }

        let vocab = unsafe { sys::llama_model_get_vocab(model) };
        let n_embd = unsafe { sys::llama_n_embd(model) } as usize;
        let n_vocab = unsafe { sys::llama_n_vocab(vocab) } as usize;

        Ok(Arc::new(LoadedModel {
            model,
            vocab,
            n_embd,
            n_vocab,
        }))
    }
}

impl LlmEngine {
    /// Load a GGUF model slice and create a llama context.
    pub fn load(model_path: &str, cfg: &EngineConfig) -> Result<Self> {
        let plan = fit_plan(model_path, cfg)?;
        let loaded = LoadedModel::load(model_path, &plan)?;
        match Self::with_model(loaded.clone(), &plan.config) {
            Ok(mut engine) => {
                engine.fit_memory_applied = plan.applied;
                engine.effective_n_gpu_layers = plan.config.n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&plan.config);
                Ok(engine)
            }
            Err(gpu_kv_error) => {
                let Some(cpu_kv_cfg) = Self::cpu_kv_fallback(&plan.config) else {
                    return Err(gpu_kv_error);
                };

                warn!(
                    "llama context failed with KV cache on GPU: {gpu_kv_error}; retrying with KV cache in host RAM while keeping {} GPU layers",
                    plan.config.n_gpu_layers
                );
                let mut engine = Self::with_model(loaded, &cpu_kv_cfg).with_context(|| {
                    format!(
                        "llama context failed with GPU KV and host-RAM KV fallback; requested {} GPU layers",
                        plan.config.n_gpu_layers
                    )
                })?;
                engine.fit_memory_applied = plan.applied;
                engine.effective_n_gpu_layers = plan.config.n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&plan.config);
                Ok(engine)
            }
        }
    }

    fn cpu_kv_fallback(cfg: &EngineConfig) -> Option<EngineConfig> {
        cfg.offload_kqv.then(|| EngineConfig {
            offload_kqv: false,
            ..cfg.clone()
        })
    }

    /// N contextos sobre um modelo só, um por sessão simultânea.
    ///
    /// O `EngineConfig.n_ctx` é a capacidade total. Cada contexto recebe a
    /// janela por sessão, `total_n_ctx / n`, e cada sessão mantém o tamanho
    /// informado pelo operador. O que muda é que cada um tem o seu
    /// `llama_context`,
    /// e dois decodes podem correr ao mesmo tempo em vez de se enfileirarem no
    /// mutex - que era 194 ms por token na medição de 2026-08-14.
    ///
    /// Os pesos são lidos do disco uma vez.
    ///
    /// Ainda sem chamador: o servidor de inferência carrega **um** contexto
    /// (`load`). O pool fica aqui, com os testes que sustentam a promessa de
    /// memória, para quando o decode concorrente for ligado.
    #[allow(dead_code)]
    pub fn pool(model_path: &str, cfg: &EngineConfig, n: usize) -> Result<Vec<Self>> {
        let n = n.max(1);
        let plan = fit_plan(model_path, cfg)?;
        let loaded = LoadedModel::load(model_path, &plan)?;
        let por_contexto = per_context_config(&plan.config, n);
        (0..n)
            .map(|_| {
                let mut engine = Self::with_model(loaded.clone(), &por_contexto)?;
                engine.fit_memory_applied = plan.applied;
                engine.effective_n_gpu_layers = plan.config.n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&por_contexto);
                Ok(engine)
            })
            .collect()
    }

    /// Um contexto novo sobre pesos já carregados.
    ///
    /// É por aqui que se monta mais de um contexto sobre o mesmo modelo, que é
    /// o que permite dois decodes ao mesmo tempo neste nó.
    pub fn with_model(loaded: Arc<LoadedModel>, cfg: &EngineConfig) -> Result<Self> {
        let model = loaded.model;
        let n_embd = loaded.n_embd;
        let n_vocab = loaded.n_vocab;

        // ── Context params ───────────────────────────────────────────────────
        let mut cparams = unsafe { sys::llama_context_default_params() };
        cparams.n_ctx = cfg.n_ctx;
        cparams.n_batch = cfg.n_batch;
        cparams.n_ubatch = effective_n_ubatch(cfg);
        cparams.n_seq_max = cfg.max_sessions.max(1);
        cparams.offload_kqv = cfg.offload_kqv;
        apply_cache_types(&mut cparams, cfg.cache_type_k, cfg.cache_type_v);
        cparams.embeddings = cfg.need_embeddings;
        cparams.n_rs_seq = cfg.rs_seq_window;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        // ── Residual capture callback ────────────────────────────────────────
        let capture: Box<Mutex<CaptureSlot>> = Box::new(Mutex::new(CaptureSlot {
            want: None,
            buf: None,
            copy_us: 0,
        }));
        let capture_ok = if cfg.need_embeddings {
            // Target tensor: l_out-<last_layer_index> of this slice.
            let n_layer = unsafe { sys::llama_model_n_layer(model) };
            let tensor_name =
                CString::new(format!("l_out-{}", n_layer - 1)).expect("tensor name is valid ASCII");
            capture.lock().unwrap().want = Some(tensor_name);
            cparams.cb_eval = Some(capture_callback);
            // O endereço do Box, não de uma global: é isto que separa a captura
            // de um contexto da do vizinho quando os dois decodificam juntos.
            cparams.cb_eval_user_data = &*capture as *const Mutex<CaptureSlot> as *mut c_void;
            true
        } else {
            false
        };

        // ── Create context ───────────────────────────────────────────────────
        let ctx = unsafe { sys::llama_new_context_with_model(model, cparams) };
        if ctx.is_null() {
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

        // O contexto de rascunho MTP vem depois do alvo de propósito: ele
        // precisa do `ctx` já criado para ligar a saída do estado oculto nele.
        let mtp = if cfg.mtp {
            match crate::mtp::MtpDrafter::attach(model, ctx, cfg, n_embd, n_vocab) {
                Ok(d) => {
                    if d.is_none() {
                        tracing::info!(
                            "MTP pedido, mas este modelo não traz bloco nextn \
                             (llama_model_n_layer_nextn = 0) — seguindo sem MTP"
                        );
                    }
                    d
                }
                // Falhar aqui derrubaria um nó que serve bem sem MTP. A
                // aceleração é opcional; a inferência não.
                Err(e) => {
                    warn!("MTP indisponível neste nó: {e}");
                    None
                }
            }
        } else {
            None
        };

        Ok(LlmEngine {
            loaded,
            ctx,
            n_embd,
            n_vocab,
            capture_ok,
            capture,
            offload_kqv: cfg.offload_kqv,
            cache_type_k: cfg.cache_type_k,
            cache_type_v: cfg.cache_type_v,
            fit_memory_applied: false,
            effective_n_gpu_layers: cfg.n_gpu_layers,
            effective_n_ubatch: effective_n_ubatch(cfg),
            mtp,
        })
    }

    // ── Predição multi-token (MTP) ────────────────────────────────────────────

    /// Este contexto consegue rascunhar pelo bloco `nextn`?
    ///
    /// É a pergunta que decide se o nó anuncia `speculate/v1-mtp` e se o
    /// gateway pede rascunho MTP. Falsa sempre que o modelo não tem o bloco -
    /// o que inclui **toda fatia não-final** de um pipeline, porque o `nextn`
    /// mora na última camada.
    pub fn mtp_ready(&self) -> bool {
        self.mtp.is_some()
    }

    /// Rascunha até `k` tokens com a cabeça `nextn`. Vazio quando não há
    /// cabeça — o chamador então decodifica só o token confirmado, que é o
    /// comportamento de sempre.
    pub fn mtp_draft(
        &mut self,
        id_last: i32,
        n_past: u32,
        seq_id: u32,
        k: u32,
        p_min: f32,
    ) -> Vec<i32> {
        match self.mtp.as_mut() {
            Some(d) => d.draft(id_last, n_past, seq_id, k, p_min),
            None => Vec::new(),
        }
    }

    /// Diz à cabeça quantos tokens **do rascunho** a verificação aceitou.
    pub fn mtp_accept(&mut self, accepted: usize) {
        if let Some(d) = self.mtp.as_mut() {
            d.accept(accepted);
        }
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

    pub fn cache_type_k(&self) -> KvCacheType {
        self.cache_type_k
    }

    pub fn cache_type_v(&self) -> KvCacheType {
        self.cache_type_v
    }

    /// Quantos blocos esta fatia carregou.
    pub fn n_layer(&self) -> u32 {
        unsafe { sys::llama_model_n_layer(self.loaded.model) as u32 }
    }

    /// Em qual bloco desta fatia a próxima decodificação deve parar.
    ///
    /// `None` volta ao normal: captura no último bloco, grafo inteiro. Com
    /// `Some(k)` a captura passa a mirar `l_out-k`, e o retorno `false` do
    /// callback na fase de compute **abandona o resto do grafo** - é isso que
    /// faz a execução ser de fato parcial, e não o grafo inteiro com o
    /// resultado de um bloco do meio.
    ///
    /// Vale só para a próxima decodificação porque quem a chama já segura o
    /// mesmo lock que serializa as decodificações deste contexto.
    pub fn set_capture_layer(&self, block_index: Option<u32>) -> Result<()> {
        if !self.capture_ok {
            bail!("esta fatia não exporta residual: não há captura para reposicionar");
        }
        let last = self.n_layer().saturating_sub(1);
        let index = block_index.unwrap_or(last);
        if index > last {
            bail!("bloco {index} não existe nesta fatia (último é {last})");
        }
        let name = CString::new(format!("l_out-{index}"))
            .map_err(|e| anyhow::anyhow!("nome de tensor inválido: {e}"))?;
        self.capture.lock().unwrap().want = Some(name);
        Ok(())
    }

    pub fn fit_memory_applied(&self) -> bool {
        self.fit_memory_applied
    }

    pub fn effective_n_gpu_layers(&self) -> i32 {
        self.effective_n_gpu_layers
    }

    pub fn effective_n_ubatch(&self) -> u32 {
        self.effective_n_ubatch
    }

    /// Onde cada camada realmente ficou, do próprio llama.cpp.
    ///
    /// `None` quando a ponte falha, e aí o `/health` omite o campo em vez de
    /// mostrar um mapa inventado: dizer "não sei" é a única resposta honesta
    /// possível, porque nada em `EngineConfig` substitui o buffer real de um
    /// tensor. Ver `llama_rs_model_placement_json` no `wrapper_common.cpp`.
    pub fn placement(&self) -> Option<serde_json::Value> {
        unsafe {
            json_from_wrapper(|out| sys::llama_rs_model_placement_json(self.loaded.model, out))
        }
    }

    /// A memória deste contexto por buffer: pesos, cache KV e cálculo.
    ///
    /// É o que responde onde o cache KV ficou de verdade. `offload_kqv()` é o
    /// que foi pedido; numa GPU cheia o llama.cpp recarrega com ele desligado
    /// (ver `runtime.rs`), e só a repartição por buffer mostra isso.
    pub fn context_memory(&self) -> Option<serde_json::Value> {
        unsafe { json_from_wrapper(|out| sys::llama_rs_context_memory_json(self.ctx, out)) }
    }

    pub fn eos_token(&self) -> i32 {
        unsafe { sys::llama_vocab_eos(self.loaded.vocab) }
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
                self.loaded.vocab,
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
                self.loaded.vocab,
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
                self.loaded.vocab,
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

    // ── Chat template ─────────────────────────────────────────────────────────

    /// The `tokenizer.chat_template` from the model's GGUF metadata, or None.
    pub fn chat_template(&self) -> Option<String> {
        let key = CString::new("tokenizer.chat_template").ok()?;
        let mut buf = vec![0u8; 32768];
        let n = unsafe {
            sys::llama_model_meta_val_str(
                self.loaded.model,
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

    /// Decodes multiple independent token sequences in one llama batch.
    ///
    /// Each descriptor owns a physical KV sequence. The method deliberately
    /// does not sample or mutate session history: the scheduler does that
    /// after it maps logits back to the logical item.
    pub fn decode_tokens_batch(
        &mut self,
        entries: &[TokenBatchEntry],
        need_residual: bool,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        if entries.is_empty() {
            bail!("batch de tokens vazio");
        }
        let descriptors: Vec<BatchEntry> = entries.iter().map(|e| e.descriptor.clone()).collect();
        let layout = BatchLayout::new(&descriptors)?;
        for entry in entries {
            if entry.tokens.len() != entry.descriptor.tokens {
                bail!(
                    "item {} declara {} tokens, recebeu {}",
                    entry.descriptor.item_id,
                    entry.descriptor.tokens,
                    entry.tokens.len()
                );
            }
        }

        let n = i32::try_from(layout.total_tokens())
            .map_err(|_| anyhow::anyhow!("batch excede o limite de tokens do llama.cpp"))?;
        let enqueue_started = Instant::now();
        self.reset_capture_slot();
        let mut batch = unsafe { sys::llama_batch_init(n, 0, 1) };
        batch.n_tokens = n;
        let mut row = 0usize;
        unsafe {
            for entry in entries {
                for (offset, &token) in entry.tokens.iter().enumerate() {
                    *batch.token.add(row) = token;
                    *batch.pos.add(row) = (entry.descriptor.pos + offset as u32) as i32;
                    *batch.n_seq_id.add(row) = 1;
                    *(*batch.seq_id.add(row)).add(0) = entry.descriptor.seq_id as i32;
                    *batch.logits.add(row) =
                        if entry.descriptor.want_logits && offset + 1 == entry.tokens.len() {
                            1
                        } else {
                            0
                        };
                    row += 1;
                }
            }
        }
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (token batch) failed rc={rc}");
        }
        self.finish_batch(layout, need_residual, enqueue_started)
    }

    /// Decodes multiple hidden-state sequences in one llama batch.
    pub fn decode_hidden_batch(
        &mut self,
        entries: &[HiddenBatchEntry],
        need_residual: bool,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        if entries.is_empty() {
            bail!("batch de hidden states vazio");
        }
        let descriptors: Vec<BatchEntry> = entries.iter().map(|e| e.descriptor.clone()).collect();
        let layout = BatchLayout::new(&descriptors)?;
        for entry in entries {
            let expected = entry.descriptor.tokens.saturating_mul(self.n_embd);
            if entry.hidden.len() != expected {
                bail!(
                    "item {} tem hidden state com {} floats, esperado {}",
                    entry.descriptor.item_id,
                    entry.hidden.len(),
                    expected
                );
            }
        }

        let n = i32::try_from(layout.total_tokens())
            .map_err(|_| anyhow::anyhow!("batch excede o limite de tokens do llama.cpp"))?;
        let enqueue_started = Instant::now();
        self.reset_capture_slot();
        let mut batch = unsafe { sys::llama_batch_init(n, self.n_embd as i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut();
        let mut row = 0usize;
        unsafe {
            for entry in entries {
                let start = row * self.n_embd;
                let end = start + entry.hidden.len();
                std::ptr::copy_nonoverlapping(
                    entry.hidden.as_ptr(),
                    batch.embd.add(start),
                    end - start,
                );
                for offset in 0..entry.descriptor.tokens {
                    *batch.pos.add(row) = (entry.descriptor.pos + offset as u32) as i32;
                    *batch.n_seq_id.add(row) = 1;
                    *(*batch.seq_id.add(row)).add(0) = entry.descriptor.seq_id as i32;
                    *batch.logits.add(row) =
                        if entry.descriptor.want_logits && offset + 1 == entry.descriptor.tokens {
                            1
                        } else {
                            0
                        };
                    row += 1;
                }
            }
        }
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden batch) failed rc={rc}");
        }
        self.finish_batch(layout, need_residual, enqueue_started)
    }

    fn reset_capture_slot(&self) {
        let mut capture = self.capture.lock().unwrap();
        capture.buf = None;
        capture.copy_us = 0;
    }

    fn finish_batch(
        &self,
        layout: BatchLayout,
        need_residual: bool,
        enqueue_started: Instant,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        if crate::tp::is_active() {
            let reduces = crate::tp::reduces_in_current_decode();
            let expected = 2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64;
            if reduces != expected {
                bail!("coletiva TP reduziu {reduces} vezes neste batch, esperado {expected}");
            }
            if let Some(motivo) = crate::tp::take_failure() {
                bail!("coletiva de tensor parallelism falhou: {motivo}");
            }
        }
        let residual = if need_residual {
            Some(self.take_residual(layout.total_tokens())?)
        } else {
            None
        };
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        Ok((
            BatchDecodeOutput {
                layout,
                residual,
                n_embd: self.n_embd,
            },
            timings,
        ))
    }

    /// Decode a batch of token IDs. `logits_all=true` means every position
    /// gets logits; `false` means only the last one.
    pub fn decode_tokens(
        &mut self,
        tokens: &[i32],
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
    ) -> Result<DecodeTimings> {
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot();
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

        let tp_antes = crate::tp::reduce_count();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode failed rc={rc}");
        }
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        // Coletiva ligada e nenhum reduce no decode inteiro: o op do fork não
        // entrou no grafo. O rank rodou com meia largura e sem soma nenhuma, e
        // a resposta é falsa - o modo de falha de 18/08, que não tinha sintoma
        // além do texto sem sentido.
        // Quantos reduces este decode disparou. São **2 por camada**: a projeção
        // de saída da atenção e o `ffn_down`, as duas row-parallel. Um modelo de
        // 32 camadas tem que somar 64.
        //
        // Isto é rede de proteção, não curiosidade. O fork tinha o reduce em uma
        // única sobrecarga de `build_attn` (a `attn_no_cache`), então todo
        // modelo com cache KV somava só o FFN: 32 em vez de 64. E o sintoma é o
        // disfarce mais perigoso possível - a resposta sai gramatical e fluente,
        // só o conteúdo é vazio. Não parece erro de cálculo, parece modelo ruim,
        // e custou uma tarde inteira de 18/08 para ser identificado.
        //
        // Aviso, não erro: arquitetura híbrida pode legitimamente ter camada sem
        // atenção ou sem FFN, e recusar a resposta nesse caso seria pior que o
        // aviso. Zero reduces continua sendo erro (logo abaixo).
        if crate::tp::is_active() {
            let reduces = crate::tp::reduces_in_current_decode();
            let esperado = 2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64;
            if reduces != esperado {
                tracing::warn!(
                    "coletiva TP reduziu {reduces} vezes neste decode, esperado {esperado} \
                     (2 por camada): parte do grafo está rodando sem redução, e a \
                     resposta sai fluente e sem sentido"
                );
            } else {
                tracing::debug!("decode fechou com {reduces} reduces (2 por camada)");
            }
        }
        if crate::tp::is_active() && crate::tp::reduce_count() == tp_antes {
            bail!(
                "coletiva de tensor parallelism ligada mas nenhum reduce rodou \
                 neste decode: o op do llama.cpp não entrou no grafo"
            );
        }
        // Coletiva de tensor parallelism: o op roda **dentro** do decode, num
        // callback `extern "C"` que não tem como devolver erro. Quem falhou
        // anotou o motivo, e é aqui que ele vira erro da requisição.
        //
        // Isto não é zelo: um all-reduce que falha não piora a resposta, ele a
        // torna falsa. O rank fica com a própria parcial - metade das cabeças e
        // metade do FFN - e a geração sai plausível e sem sentido, sem nenhum
        // sinal de erro (18/08). Responder errado é pior que não responder.
        if let Some(motivo) = crate::tp::take_failure() {
            bail!("coletiva de tensor parallelism falhou: {motivo}");
        }

        // A cabeça `nextn` tem que ver **todo** lote que o alvo decodifica -
        // prompt incluído -, senão o cache dela fica atrás do tronco e o par
        // `(h_p, x_{p+1})` que ela consome deixa de existir. Ficar aqui, e não
        // em cada chamador, é o que impede um caminho novo de decodificação de
        // esquecer o passo. Um erro aqui derruba o rascunho, não a resposta: a
        // cabeça sai de cena e a geração segue um token por rodada.
        if self.mtp.is_some() {
            let ctx = self.ctx;
            let mut drafter = self.mtp.take();
            if let Some(d) = drafter.as_mut() {
                if let Err(e) = d.observe(ctx, tokens, pos_offset, seq_id) {
                    warn!("MTP desligado nesta sessão: {e}");
                    drafter = None;
                }
            }
            self.mtp = drafter;
        }

        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
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
    ///
    /// `tokens` são os ids do lote que a cauda decodificou — o caller que os
    /// tem entrega; sem eles a cabeça `nextn` não se mantém em dia (ver o
    /// bloco de `observe` no corpo).
    pub fn decode_hidden(
        &mut self,
        hidden: &[f32],
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
        tokens: Option<&[i32]>,
    ) -> Result<DecodeTimings> {
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot();
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
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        // Ver a nota gêmea em `decode_tokens`: a coletiva falha por callback e
        // só pode virar erro aqui fora.
        if let Some(motivo) = crate::tp::take_failure() {
            bail!("coletiva de tensor parallelism falhou: {motivo}");
        }

        // A cabeça `nextn` tem que ver **todo** lote que o alvo decodifica,
        // senão o cache dela fica atrás do tronco e o par `(h_p, x_{p+1})` que
        // ela consome deixa de existir. Ficar aqui, e não em cada chamador, é
        // o que impede um caminho novo de decodificação de esquecer o passo.
        // Um erro aqui derruba o rascunho, não a resposta: a cabeça sai de
        // cena e a geração segue um token por rodada.
        //
        // Diferente do `decode_tokens`, um lote de estado oculto não carrega os
        // ids dos tokens embutidos — quem os tem é o chamador (a cauda de um
        // pipeline, que conhece a composição do lote). `tokens` traz esses ids;
        // quando ninguém os oferece (peer velho que não encaminha o campo), a
        // cabeça não tem como se manter em dia e desligar com aviso é preferível
        // a rascunhar a partir de um cache que ficou para trás.
        if let Some(tokens) = tokens {
            if self.mtp.is_some() {
                let ctx = self.ctx;
                let mut drafter = self.mtp.take();
                if let Some(d) = drafter.as_mut() {
                    if let Err(e) = d.observe(ctx, tokens, pos_offset, seq_id) {
                        warn!("MTP desligado nesta sessão: {e}");
                        drafter = None;
                    }
                }
                self.mtp = drafter;
            }
        } else if self.mtp.is_some() {
            warn!(
                "MTP desligado: este contexto recebeu estado oculto sem os ids dos \
                 tokens (peer velho no pipeline?), e a cabeça nextn precisa deles para \
                 acompanhar o tronco"
            );
            self.mtp = None;
        }
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
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
            let buf = self.capture.lock().unwrap().buf.take();
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

    /// Descarta posições do cache, e diz se conseguiu.
    ///
    /// O booleano não é decoração: num modelo híbrido de atenção linear (o
    /// `gated_delta_net` do Qwen3-Next, o Ornith) o estado recorrente é uma
    /// soma acumulada, não uma tabela de onde se apaga uma linha, e o
    /// llama.cpp responde `false` sem remover nada. Descartar esse retorno era
    /// o que fazia o `n_past` do nó recuar enquanto o cache ficava parado -
    /// e a rodada seguinte morria com "inconsistent sequence positions",
    /// quatro tokens depois de a especulação começar.
    #[must_use]
    pub fn kv_remove(&self, seq_id: u32, from: i32, to: i32) -> bool {
        // Use the newer llama_get_memory + llama_memory_seq_rm API.
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if mem.is_null() {
                return false;
            }
            sys::llama_memory_seq_rm(mem, seq_id as i32, from, to)
        }
    }

    /// `llama_memory_can_shift` - se o llama.cpp consegue reindexar (RoPE) as
    /// posições já cacheadas, o "K-shift" que o llama.cpp de referência usa
    /// para `--ctx-shift`/geração infinita quando o contexto enche. **Não
    /// governa remoção de posições do meio do cache** - isso é `kv_remove`
    /// (`llama_memory_seq_rm`), uma operação diferente que este projeto usa
    /// para desfazer um rascunho especulativo recusado (`handle_rollback`) e
    /// que não depende deste valor. Nada aqui liga `--ctx-shift`; exposto só
    /// como diagnóstico em `/health`.
    pub fn kv_can_shift(&self) -> bool {
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            !mem.is_null() && sys::llama_memory_can_shift(mem)
        }
    }
}

/// Monta peças de token em texto válido entre chamadas.
///
/// O vocabulário devolve bytes crus por token: um token BYTE é um byte só
/// (`llama-vocab.cpp::token_to_piece`, ramo `LLAMA_TOKEN_ATTR_BYTE`) e um
/// token normal pode terminar no meio de um codepoint. Converter cada peça
/// com `from_utf8_lossy` por chamada nascia U+FFFD independente a cada
/// fragmento e rasgava a palavra no delta do chat. O acumulador guarda o
/// rabicho incompleto e só emite caractere completo; `flush` drena o resto
/// no fim do stream.
pub(crate) struct Utf8Assembler {
    pending: Vec<u8>,
}

impl Utf8Assembler {
    pub(crate) fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Acrescenta os bytes da peça e devolve o texto completo até aqui.
    ///
    /// Sequência incompleta no fim fica pendente para a peça seguinte;
    /// sequência inválida de verdade vira um único U+FFFD na hora, a mesma
    /// semântica de um `from_utf8_lossy` sobre o stream inteiro.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        out.push_str(
                            std::str::from_utf8(&self.pending[..valid])
                                .expect("prefixo validado por from_utf8"),
                        );
                    }
                    match e.error_len() {
                        // Incompleta: o caractere pode completar na próxima peça.
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                        // Inválida: não vai completar nunca; repõe e segue.
                        Some(n) => {
                            self.pending.drain(..valid + n);
                            out.push('\u{fffd}');
                        }
                    }
                }
            }
        }
        out
    }

    /// Fim do stream: drena o rabicho que não virou caractere completo.
    pub(crate) fn flush(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

/// A configuração de um contexto do pool.
///
/// Separada para poder ser conferida sem GPU: é ela que sustenta a promessa de
/// que o pool não custa memória a mais. Como `cfg.n_ctx` é a capacidade total,
/// cada contexto recebe `cfg.n_ctx / n`, e `max_sessions = 1` porque agora cada
/// contexto atende uma sessão.
fn per_context_config(cfg: &EngineConfig, n: usize) -> EngineConfig {
    EngineConfig {
        n_ctx: cfg.n_ctx / n.max(1) as u32,
        max_sessions: 1,
        ..cfg.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEntry {
    pub item_id: String,
    pub seq_id: u32,
    pub pos: u32,
    pub tokens: usize,
    pub want_logits: bool,
}

impl BatchEntry {
    #[cfg(test)]
    fn test(item_id: &str, seq_id: u32, pos: u32, tokens: usize, want_logits: bool) -> Self {
        Self {
            item_id: item_id.into(),
            seq_id,
            pos,
            tokens,
            want_logits,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenBatchEntry {
    pub descriptor: BatchEntry,
    pub tokens: Vec<i32>,
}

#[derive(Debug, Clone)]
pub struct HiddenBatchEntry {
    pub descriptor: BatchEntry,
    pub hidden: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRange {
    pub rows: Range<usize>,
    pub logits_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLayout {
    ranges: HashMap<String, BatchRange>,
    total_tokens: usize,
}

impl BatchLayout {
    pub fn new(entries: &[BatchEntry]) -> Result<Self> {
        if entries.is_empty() {
            bail!("batch sem entradas");
        }

        let mut ranges = HashMap::with_capacity(entries.len());
        let mut seq_ids = HashSet::with_capacity(entries.len());
        let mut cursor = 0usize;
        for entry in entries {
            if entry.item_id.is_empty() {
                bail!("batch item sem item_id");
            }
            if ranges.contains_key(&entry.item_id) {
                bail!("item_id duplicado: {}", entry.item_id);
            }
            if !seq_ids.insert(entry.seq_id) {
                bail!("seq_id duplicado: {}", entry.seq_id);
            }
            if entry.tokens == 0 {
                bail!("item {} sem tokens", entry.item_id);
            }
            let end = cursor
                .checked_add(entry.tokens)
                .ok_or_else(|| anyhow::anyhow!("batch excede o limite numérico"))?;
            let logits_index = entry.want_logits.then_some(end - 1);
            ranges.insert(
                entry.item_id.clone(),
                BatchRange {
                    rows: cursor..end,
                    logits_index,
                },
            );
            cursor = end;
        }

        Ok(Self {
            ranges,
            total_tokens: cursor,
        })
    }

    pub fn total_tokens(&self) -> usize {
        self.total_tokens
    }

    pub fn range(&self, item_id: &str) -> Option<&BatchRange> {
        self.ranges.get(item_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchDecodeOutput {
    pub layout: BatchLayout,
    residual: Option<Vec<f32>>,
    n_embd: usize,
}

impl BatchDecodeOutput {
    #[cfg(test)]
    fn test(layout: BatchLayout, residual: Vec<f32>, n_embd: usize) -> Self {
        Self {
            layout,
            residual: Some(residual),
            n_embd,
        }
    }

    pub fn residual_for(&self, item_id: &str, n_embd: usize) -> Result<Vec<f32>> {
        let range = self
            .layout
            .range(item_id)
            .ok_or_else(|| anyhow::anyhow!("item_id ausente no resultado: {item_id}"))?;
        let residual = self
            .residual
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("batch não capturou residual"))?;
        if n_embd != self.n_embd {
            bail!(
                "n_embd solicitado {} diverge do batch {}",
                n_embd,
                self.n_embd
            );
        }
        let start = range
            .rows
            .start
            .checked_mul(n_embd)
            .ok_or_else(|| anyhow::anyhow!("offset de residual excede o limite numérico"))?;
        let end = range
            .rows
            .end
            .checked_mul(n_embd)
            .ok_or_else(|| anyhow::anyhow!("fim de residual excede o limite numérico"))?;
        residual
            .get(start..end)
            .map(|values| values.to_vec())
            .ok_or_else(|| anyhow::anyhow!("residual incompleto para item {item_id}"))
    }
}

#[cfg(test)]
mod batch_layout_tests {
    use super::*;

    #[test]
    fn layout_marca_a_posicao_global_de_logits_de_cada_item() {
        let entries = vec![
            BatchEntry::test("a", 0, 10, 3, true),
            BatchEntry::test("b", 1, 4, 2, true),
            BatchEntry::test("c", 2, 8, 1, false),
        ];
        let layout = BatchLayout::new(&entries).unwrap();

        assert_eq!(layout.total_tokens(), 6);
        assert_eq!(layout.range("a").unwrap().rows, 0..3);
        assert_eq!(layout.range("a").unwrap().logits_index, Some(2));
        assert_eq!(layout.range("b").unwrap().rows, 3..5);
        assert_eq!(layout.range("b").unwrap().logits_index, Some(4));
        assert_eq!(layout.range("c").unwrap().rows, 5..6);
        assert_eq!(layout.range("c").unwrap().logits_index, None);
    }

    #[test]
    fn layout_recusa_item_duplicado_ou_sem_tokens() {
        let duplicate = vec![
            BatchEntry::test("a", 0, 0, 1, false),
            BatchEntry::test("a", 1, 0, 1, false),
        ];
        assert!(BatchLayout::new(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("item_id duplicado"));

        let empty = vec![BatchEntry::test("empty", 0, 0, 0, false)];
        assert!(BatchLayout::new(&empty)
            .unwrap_err()
            .to_string()
            .contains("sem tokens"));
    }

    #[test]
    fn saida_do_batch_devolve_residual_somente_do_item_pedido() {
        let entries = vec![
            BatchEntry::test("a", 0, 0, 2, false),
            BatchEntry::test("b", 1, 0, 1, false),
        ];
        let layout = BatchLayout::new(&entries).unwrap();
        let output = BatchDecodeOutput::test(layout, vec![1.0, 2.0, 3.0], 1);

        assert_eq!(output.residual_for("a", 1).unwrap(), vec![1.0, 2.0]);
        assert_eq!(output.residual_for("b", 1).unwrap(), vec![3.0]);
        assert!(output.residual_for("missing", 1).is_err());
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn cfg() -> EngineConfig {
        EngineConfig {
            n_ctx: 120_320,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: true,
            n_batch: 512,
            n_gpu_layers: 99,
            n_threads: 0,
            max_sessions: 2,
            need_embeddings: true,
            mtp: false,
            rs_seq_window: 0,
        }
    }

    #[test]
    fn gpu_kv_failure_fallback_keeps_the_requested_gpu_layers() {
        let cfg = cfg();
        let fallback = LlmEngine::cpu_kv_fallback(&cfg).expect("GPU KV was enabled");

        assert!(!fallback.offload_kqv);
        assert_eq!(fallback.n_gpu_layers, cfg.n_gpu_layers);
        assert_eq!(fallback.n_ctx, cfg.n_ctx);
    }

    #[test]
    fn cpu_kv_configuration_does_not_retry() {
        let mut cfg = cfg();
        cfg.offload_kqv = false;

        assert!(LlmEngine::cpu_kv_fallback(&cfg).is_none());
    }

    #[test]
    fn fit_only_auto_adjusts_the_default_gpu_layer_setting() {
        assert!(fit_can_adjust_layers(&cfg()));

        let mut explicit_layers = cfg();
        explicit_layers.n_gpu_layers = 20;
        assert!(!fit_can_adjust_layers(&explicit_layers));

        let mut disabled = cfg();
        disabled.fit_memory = false;
        assert!(!fit_can_adjust_layers(&disabled));
    }

    /// A regressão de 2026-08-21: prompt vindo de um cliente OpenAI externo
    /// morria com "got 655360 floats, expected 2621440 (512x5120)".
    ///
    /// 655360 = 128 x 5120, e 128 e o `FIT_MAX_UBATCH`. O chunk de 512 tokens
    /// virava quatro microlotes, a captura substituia a cada um, e sobrava so o
    /// ultimo quarto. Pela UI passava despercebido porque prompt curto cabe num
    /// microlote so - e por isso o teste abaixo usa mais de um.
    #[test]
    fn capture_accumulates_every_microbatch_of_one_decode() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            copy_us: 0,
        };

        let n_embd = 4;
        let ubatch = 2;
        let tokens = 6; // tres microlotes, como um chunk maior que `n_ubatch`
        for micro in 0..(tokens / ubatch) {
            let inicio = (micro * ubatch * n_embd) as f32;
            let pedaco: Vec<f32> = (0..(ubatch * n_embd)).map(|i| inicio + i as f32).collect();
            accumulate_capture(&mut slot, pedaco);
        }

        let capturado = slot.buf.expect("a captura tem que ter disparado");
        assert_eq!(
            capturado.len(),
            tokens * n_embd,
            "o residual do chunk inteiro, nao o do ultimo microlote"
        );
        let esperado: Vec<f32> = (0..(tokens * n_embd)).map(|i| i as f32).collect();
        assert_eq!(
            capturado, esperado,
            "os microlotes entram na ordem em que o split_simple os produz"
        );
    }

    /// Sem isto, o residual de uma requisicao entraria concatenado no da
    /// proxima - o modo de falha que a acumulacao introduziria se o buffer
    /// nao fosse zerado a cada decode.
    #[test]
    fn a_new_decode_starts_from_an_empty_capture() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            copy_us: 0,
        };
        accumulate_capture(&mut slot, vec![1.0, 2.0]);
        slot.buf = None; // o que `reset_capture_slot` faz antes de cada decode
        accumulate_capture(&mut slot, vec![3.0, 4.0]);
        assert_eq!(slot.buf.expect("captura"), vec![3.0, 4.0]);
    }

    #[test]
    fn fit_mode_caps_compute_microbatch_without_changing_prompt_batch() {
        let cfg = cfg();
        assert_eq!(cfg.n_batch, 512);
        assert_eq!(effective_n_ubatch(&cfg), 128);

        let mut no_fit = cfg;
        no_fit.fit_memory = false;
        assert_eq!(effective_n_ubatch(&no_fit), 512);

        let mut small_batch = no_fit;
        small_batch.n_batch = 64;
        small_batch.fit_memory = true;
        assert_eq!(effective_n_ubatch(&small_batch), 64);
    }

    #[test]
    fn mtp_configuration_requests_loading_nextn_weights() {
        let mut cfg = cfg();
        assert!(!load_mtp_weights(&cfg));

        cfg.mtp = true;
        assert!(load_mtp_weights(&cfg));
    }

    /// O pool nao pode custar memoria a mais: o cache de cada contexto e o
    /// mesmo pedaco que a sessao ja tinha. Numeros da ksia em 2026-08-14, onde
    /// um contexto de 120.320 atendia 2 sessoes de 60.160.
    #[test]
    fn each_context_gets_the_slice_a_session_already_had() {
        let por_contexto = per_context_config(&cfg(), 2);
        assert_eq!(por_contexto.n_ctx, 60_160);
        assert_eq!(por_contexto.n_ctx * 2, cfg().n_ctx, "o total nao cresce");
    }

    /// Cada contexto atende uma sessao; quem divide agora e o pool.
    #[test]
    fn a_pooled_context_serves_one_session() {
        assert_eq!(per_context_config(&cfg(), 4).max_sessions, 1);
    }

    /// O resto da configuracao e do no, nao do contexto, e nao pode divergir
    /// entre irmaos.
    #[test]
    fn everything_else_is_carried_over_unchanged() {
        let p = per_context_config(&cfg(), 3);
        assert_eq!(p.n_batch, 512);
        assert_eq!(p.n_gpu_layers, 99);
        assert!(p.offload_kqv);
        assert!(p.need_embeddings);
    }

    /// Zero contextos nao existe, e dividir por zero menos ainda.
    #[test]
    fn a_pool_of_zero_is_treated_as_one() {
        assert_eq!(per_context_config(&cfg(), 0).n_ctx, cfg().n_ctx);
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
/// O acesso aos logits e a conversão do token em texto ficam de fora: quem os
/// mede é o chamador (`infer_server::NodeTimings`), porque acontecem antes e
/// depois desta função. Campo que ninguém preenche vira zero na tela e some da
/// soma sem avisar.
#[derive(Debug, Clone, Default)]
pub struct SamplingTimings {
    pub logits_clone_us: u64,
    pub repetition_penalty_us: u64,
    pub softmax_us: u64,
    pub top_k_us: u64,
    pub top_p_us: u64,
    pub selection_us: u64,
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

/// Atalho sem a decomposição de tempos. A produção chama
/// `sample_token_profiled` direto, porque alimenta a telemetria por hop; este
/// embrulho existe para os testes que só querem o token.
#[cfg(test)]
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

    #[test]
    fn cache_types_map_to_the_llama_context_types() {
        assert_eq!(cache_type_to_ggml(KvCacheType::F16), sys::GGML_TYPE_F16);
        assert_eq!(cache_type_to_ggml(KvCacheType::Q8_0), sys::GGML_TYPE_Q8_0);
        assert_eq!(
            cache_type_to_ggml(KvCacheType::Iq4Nl),
            sys::GGML_TYPE_IQ4_NL
        );
    }

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

    /// The two boundaries that belong to the *caller* stay at zero here.
    ///
    /// Reading the logits out of llama.cpp and turning a token id into text are
    /// measured by `infer_server`, on either side of this call. If the sampler
    /// ever started reporting them, a cost would be counted twice.
    /// The capture target and buffer are process-global — the module doc
    /// explains why that is sound in production, where one engine exists per
    /// process. Tests run in parallel threads, so two engines would exist at
    /// once and whichever loaded last would decide what the other captures.
    /// This lock restores the one-engine-at-a-time property the design assumes.
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// How many layers the tests offload. Real peers run with everything on the
    /// GPU, and the capture behaves differently there, so this has to be a knob.
    fn test_gpu_layers() -> i32 {
        std::env::var("POM_TEST_GPU_LAYERS")
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
    /// graph. Set `POM_TEST_GPU_LAYERS=99` to run it with the weights
    /// offloaded, which is how every real peer runs it.
    #[test]
    fn residual_capture_fires_for_an_injected_hidden_state() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
            mtp: false,
            rs_seq_window: 0,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");

        let n_tokens = 4;
        // Values in the range a real residual carries; zeros would let a graph
        // that never ran look identical to one that did.
        let hidden: Vec<f32> = (0..n_tokens * engine.n_embd)
            .map(|i| ((i % 97) as f32 - 48.0) / 48.0)
            .collect();
        engine
            .decode_hidden(&hidden, n_tokens, 0, 0, true, None)
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
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
            mtp: false,
            rs_seq_window: 0,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let toks = engine.tokenize("Explique inferencia distribuida em uma frase.", true, true);
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

    /// Mudança 6 do spec de 2026-08-12: `node_logits_access_ms` marcava ~4,9 ms
    /// por token no nó da LM head, com `backend_completion_ms` em zero porque o
    /// `llama_synchronize` de diagnóstico está desligado por padrão. A hipótese
    /// é que esses milissegundos são a GPU terminando o trabalho, cobrados na
    /// etapa errada — o primeiro toque de CPU nos logits é quem espera.
    ///
    /// Este teste decide isso sem precisar de duas máquinas: decodifica um
    /// token na mesma fatia duas vezes, uma com `POM_PROFILE_SYNC`
    /// desligado e outra com ele ligado, e imprime para onde o tempo foi.
    ///
    /// ```text
    /// POM_TEST_GGUF=<fatia final>.gguf POM_TEST_GPU_LAYERS=99 \
    ///   cargo test -p node --release --features metal logits_access_versus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn logits_access_versus_backend_completion() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 0,
            max_sessions: 1,
            // O nó da LM head não exporta residual — é o ponto da Mudança 5.
            need_embeddings: false,
            mtp: false,
            rs_seq_window: 0,
        };
        let mut engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let tokens = engine.tokenize(
            "Explique o que e inferencia distribuida em uma frase.",
            true,
            true,
        );

        for sync in [false, true, false, true] {
            std::env::set_var("POM_PROFILE_SYNC", if sync { "1" } else { "0" });
            // Cache limpo entre as rodadas; o `false` aqui seria o cache que
            // não desloca, e este teste não depende disso.
            let _ = engine.kv_remove(0, 0, -1);
            let mut pos = 0u32;
            engine
                .decode_tokens(&tokens, pos, 0, false)
                .expect("prefill decodes");
            let _ = engine.logits_ith(tokens.len() as i32 - 1)[0];
            pos += tokens.len() as u32;

            let mut access_us = 0u64;
            let mut completion_us = 0u64;
            let mut enqueue_us = 0u64;
            let reps = 16;
            let wall_started = Instant::now();
            for _ in 0..reps {
                let t = engine.decode_tokens(&[1i32], pos, 0, false).unwrap();
                let started = Instant::now();
                let logits = engine.logits_ith(0);
                let _touch = logits[0] + logits[logits.len() - 1];
                access_us += started.elapsed().as_micros() as u64;
                completion_us += t.backend_completion_us;
                enqueue_us += t.enqueue_us;
                pos += 1;
            }
            let wall = wall_started.elapsed().as_secs_f64() * 1000.0 / reps as f64;
            println!(
                "PROFILE_SYNC={}: TOTAL {wall:.2} ms/token | enfileirar {:.2} ms | conclusão {:.2} ms | acesso aos logits {:.2} ms",
                sync as u8,
                enqueue_us as f64 / reps as f64 / 1000.0,
                completion_us as f64 / reps as f64 / 1000.0,
                access_us as f64 / reps as f64 / 1000.0,
            );
        }
        std::env::remove_var("POM_PROFILE_SYNC");
    }

    #[test]
    fn tokenize_returns_tokens_for_a_real_vocab() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
            mtp: false,
            rs_seq_window: 0,
        };
        let engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let tokens = engine.tokenize("oi", true, true);
        assert!(!tokens.is_empty(), "a two-character prompt must tokenize");
        assert!(
            engine
                .tokenize(
                    "Explique o que e inferencia distribuida em uma frase.",
                    true,
                    true
                )
                .len()
                > tokens.len(),
            "a longer prompt must produce more tokens"
        );
    }

    #[test]
    fn chat_template_wraps_the_user_prompt_and_opens_assistant_turn() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
            mtp: false,
            rs_seq_window: 0,
        };
        let engine = LlmEngine::load(&path, &cfg).expect("model loads");
        let formatted = engine.apply_chat_template(&[("user", "Oi, teste")], true, false);
        assert!(formatted.contains("Oi, teste"));
        assert!(!formatted.starts_with("Oi, teste"));
        assert!(formatted.to_ascii_lowercase().contains("assistant"));
        assert!(
            engine.tokenize(&formatted, false, true).len()
                > engine.tokenize("Oi, teste", true, true).len()
        );
    }
}

#[cfg(test)]
mod utf8_assembler_tests {
    use super::*;

    /// Um token pode terminar no meio de um codepoint (byte-fallback) e o
    /// pedaço seguinte completa a sequência. Converter cada chamada com
    /// `from_utf8_lossy` nascia U+FFFD no meio da palavra - este é o
    /// contrato de regressão: só caractere completo sai, e o rabinho fica.
    #[test]
    fn assembler_emite_so_caractere_completo_e_guarda_o_rabicho() {
        let mut asm = Utf8Assembler::new();
        // "ão" = c3 a3, quebrado entre dois tokens depois de "Jo".
        assert_eq!(asm.feed(b"Jo\xc3"), "Jo");
        assert_eq!(asm.feed(b"\xa3o "), "\u{e3}o ");
        // Emoji de 4 bytes (🧠 = f0 9f a7 a0) quebrado em três feeds.
        assert_eq!(asm.feed(&[0xf0]), "");
        assert_eq!(asm.feed(&[0x9f, 0xa7]), "");
        assert_eq!(asm.feed(&[0xa0, b'!']), "\u{1f9e0}!");
        assert_eq!(asm.flush(), "");
    }

    #[test]
    fn assembler_passa_ascii_inteiro_numa_so_chamada() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(asm.feed(b"Ol\xc3\xa1, a\xc3\xa7\xc3\xa3o, cora\xc3\xa7\xc3\xa3o"), "Ol\u{e1}, a\u{e7}\u{e3}o, cora\u{e7}\u{e3}o");
    }

    /// Byte inválido de verdade (não é prefixo de nada): vira um U+FFFD na
    /// hora e o stream continua - mesma semântica do from_utf8_lossy global,
    /// sem esperar bytes que nunca vão completar sequência alguma.
    #[test]
    fn assembler_repassa_byte_invalido_como_um_fffd_e_segue() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(asm.feed(b"a\xffb"), "a\u{fffd}b");
    }

    /// Fim do stream com sequência truncada: o flush drena o resto como um
    /// único replacement, e chamar de novo devolve vazio.
    #[test]
    fn flush_drena_rabicho_truncado_uma_vez() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(asm.feed(b"ok\xc3"), "ok");
        assert_eq!(asm.flush(), "\u{fffd}");
        assert_eq!(asm.flush(), "");
    }
}
