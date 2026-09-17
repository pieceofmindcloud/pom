use super::*;

impl LoadedModel {
    pub fn model_ptr(&self) -> *mut sys::llama_model {
        self.model
    }

    pub fn has_nextn_weights(&self) -> bool {
        self.mtp_weights_loaded
    }

    pub fn model_architecture(&self) -> Option<String> {
        model_metadata_string(self.model, "general.architecture")
    }

    /// Devolve os pesos à GPU agora, em vez de esperar o `Drop`.
    ///
    /// Idempotente: zera o ponteiro, então o `Drop` que vier depois não tem o
    /// que liberar. Só `LlmEngine::unload` chama, e só quando é o último dono.
    pub(super) fn free_now(&mut self) {
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

/// Os parâmetros de contexto que **não** têm getter no `llama.h`.
///
/// Tudo o mais (`n_ctx`, `n_batch`, `n_ubatch`, `n_seq_max`, threads, Flash
/// Attention) é lido do contexto vivo, porque o contexto é a verdade: o
/// caminho de fallback de KV em RAM reconstrói o contexto com outra
/// configuração, e ecoar o `EngineConfig` mostraria parâmetros que este nó não
/// está usando.
#[derive(Debug, Clone, Copy)]
pub(super) struct ContextParamsSnapshot {
    pub(super) kv_unified: bool,
    pub(super) embeddings: bool,
    pub(super) n_rs_seq: u32,
}

/// Contexto `mtmd` correspondente ao argumento `--mmproj` do llama.cpp.
///
/// O projector depende dos pesos do `llama_model`, por isso vive no mesmo
/// `LlmEngine` que o contexto de texto e é destruído antes dele.
pub(super) struct VisionContext {
    pub(super) ctx: *mut sys::mtmd_context,
}

unsafe impl Send for VisionContext {}
unsafe impl Sync for VisionContext {}

impl VisionContext {
    pub(super) fn load(
        mmproj_path: &str,
        model: *const sys::llama_model,
        cfg: &EngineConfig,
    ) -> Result<Self> {
        let path = CString::new(mmproj_path)
            .map_err(|_| anyhow::anyhow!("caminho mmproj contém NUL: {mmproj_path}"))?;
        let mut params = unsafe { sys::mtmd_context_params_default() };
        params.use_gpu = cfg.n_gpu_layers != 0;
        params.batch_max_tokens = i32::try_from(cfg.n_batch)
            .map_err(|_| anyhow::anyhow!("n_batch excede o limite do mtmd"))?;
        let ctx = unsafe { sys::mtmd_init_from_file(path.as_ptr(), model, params) };
        if ctx.is_null() {
            bail!("não foi possível carregar o mmproj: {mmproj_path}");
        }
        if !unsafe { sys::mtmd_support_vision(ctx) } {
            unsafe { sys::mtmd_free(ctx) };
            bail!("o mmproj não oferece entrada de imagem: {mmproj_path}");
        }
        Ok(Self { ctx })
    }
}

impl Drop for VisionContext {
    fn drop(&mut self) {
        unsafe {
            if !self.ctx.is_null() {
                sys::mtmd_free(self.ctx);
                self.ctx = ptr::null_mut();
            }
        }
    }
}

pub struct LlmEngine {
    /// Os pesos. `Arc` porque o último contexto a morrer é quem os libera.
    pub(super) loaded: Arc<LoadedModel>,
    pub(super) ctx: *mut sys::llama_context,
    pub n_embd: usize,
    pub n_embd_inp: usize,
    pub n_embd_out: usize,
    pub n_vocab: usize,
    /// Whether the residual capture callback was successfully installed.
    pub(super) capture_ok: bool,
    /// Alvo e buffer da captura deste contexto. `Box` porque o endereço é
    /// registrado no llama.cpp em `cb_eval_user_data` e não pode mudar.
    pub(super) capture: Box<Mutex<CaptureSlot>>,
    pub(super) offload_kqv: bool,
    pub(super) cache_type_k: KvCacheType,
    pub(super) cache_type_v: KvCacheType,
    pub(super) fit_memory_applied: bool,
    pub(super) effective_n_gpu_layers: i32,
    pub(super) effective_n_ubatch: u32,
    /// O que foi pedido ao `llama_new_context_with_model` e não pode ser
    /// consultado de volta. Ver `ContextParamsSnapshot`.
    pub(super) cparams_snapshot: ContextParamsSnapshot,
    /// Whether llama.cpp loaded this model in metadata-only external-MoE mode.
    /// This is part of the engine identity because a dense/native context and
    /// an external-executor context cannot share the same loaded model.
    pub(super) moe_external_executor: bool,
    /// Projector de visão carregado com o modelo, quando a implantação o pediu.
    pub(super) vision: Option<VisionContext>,
    /// A cabeça de rascunho `nextn`, quando o modelo traz uma e o operador
    /// ligou o MTP. `None` é o caso comum: modelo sem MTP, fatia não-final de
    /// um modelo com MTP, ou interruptor desligado.
    pub(super) mtp: Option<crate::mtp::MtpDrafter>,
    /// Motivo operacional pelo qual o MTP pedido não está disponível.
    pub(super) mtp_unavailable_reason: Option<String>,
    /// Falha de criação inicial do contexto MTP, distinta de modelo sem
    /// `nextn`, para acionar exatamente uma tentativa de recuo para RAM.
    pub(super) mtp_initial_context_error: Option<String>,
    /// Onde o contexto MTP pronto ficou, medido nos buffers reais.
    pub(super) mtp_allocation: Option<MtpAllocation>,
    pub(super) moe_callback: Option<Arc<MoeCallbackState>>,
}

/// Pesos mantidos entre gerações do servidor para que uma reconfiguração de
/// contexto não faça outra leitura do GGUF nem outra carga de VRAM.
#[derive(Default)]
pub struct ModelCache {
    pub(super) entry: Mutex<Option<CachedModel>>,
}

#[derive(Clone)]
pub(super) struct CachedModel {
    pub(super) model_path: String,
    pub(super) loaded: Arc<LoadedModel>,
    pub(super) fit_memory_applied: bool,
    pub(super) effective_n_gpu_layers: i32,
    /// Whether the cached target actually carries `nextn` weights. This is
    /// distinct from the operator request: a RAM fallback keeps the target
    /// model MTP-free.
    pub(super) mtp_weights_loaded: bool,
    /// A CPU companion is held by the engine, not by this cache entry. Do not
    /// reuse this target model after that fallback or the next reconstruction
    /// would need a companion that the cache does not own.
    pub(super) mtp_fallback_used: bool,
    pub(super) moe_external_executor: bool,
    pub(super) moe_external_executor_layers: Vec<u8>,
}

impl ModelCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn get(
        &self,
        model_path: &str,
        mtp_weights_loaded: bool,
        mtp_fallback_used: bool,
        moe_external_executor: bool,
        moe_external_executor_layers: &[u8],
    ) -> Option<CachedModel> {
        let entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        entry
            .as_ref()
            .filter(|cached| {
                cached.model_path == model_path
                    && cached.mtp_weights_loaded == mtp_weights_loaded
                    && cached.mtp_fallback_used == mtp_fallback_used
                    && cached.moe_external_executor == moe_external_executor
                    && cached.moe_external_executor_layers == moe_external_executor_layers
            })
            .cloned()
    }

    pub(super) fn put(
        &self,
        model_path: &str,
        loaded: Arc<LoadedModel>,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
        mtp_weights_loaded: bool,
        mtp_fallback_used: bool,
        moe_external_executor: bool,
        moe_external_executor_layers: Vec<u8>,
    ) {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        *entry = Some(CachedModel {
            model_path: model_path.to_string(),
            loaded,
            fit_memory_applied,
            effective_n_gpu_layers,
            mtp_weights_loaded,
            mtp_fallback_used,
            moe_external_executor,
            moe_external_executor_layers,
        });
    }

    pub fn clear(&self) {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        *entry = None;
    }

    /// A cached target without the requested MTP mode is not reusable on its
    /// own: the CPU-only companion belongs to the engine that just ended, and
    /// a target without nextn cannot satisfy a new MTP request. Drop that
    /// target before the next attempt, or the old weights would remain alive
    /// while the new copy is loaded.
    pub(super) fn clear_mtp_fallback(&self) -> bool {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        let should_clear = entry
            .as_ref()
            .is_some_and(|cached| !cached.mtp_weights_loaded || cached.mtp_fallback_used);
        if should_clear {
            *entry = None;
        }
        should_clear
    }
}

/// A diagnostic boundary around `llama_decode`. `backend_completion_us` is
/// measured with the public `llama_synchronize` API and therefore includes all
/// queued backend work and output transfers that must complete. It is not
/// labelled as pure kernel time.
#[derive(Debug, Clone, Default)]
pub struct DecodeTimings {
    pub enqueue_us: u64,
    pub backend_completion_us: u64,
    /// Compatibility total for the old aggregate field: ready wait plus the
    /// actual device-to-host read.
    pub residual_capture_d2h_sync_us: u64,
    pub residual_capture_ready_wait_us: u64,
    pub residual_capture_copy_us: u64,
    pub residual_capture_postprocess_us: u64,
    pub residual_capture_bytes: u64,
    pub residual_capture_copies: u64,
    pub moe_forward_us: u64,
    pub moe_executors: Vec<crate::moe::runtime::MoeExecutorKind>,
    pub moe_executor: Option<crate::moe::runtime::MoeExecutorTelemetry>,
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
pub(super) fn profile_backend_sync_value(value: Option<&str>) -> bool {
    value
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

pub fn profile_backend_sync_enabled() -> bool {
    profile_backend_sync_value(std::env::var("POM_PROFILE_SYNC").ok().as_deref())
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
        self.mtp_allocation = None;
        self.vision = None;
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_set_moe_ffn_callback(self.ctx, None, ptr::null_mut());
                sys::llama_free(self.ctx);
                self.ctx = std::ptr::null_mut();
            }
        }
        self.moe_callback = None;
        // Os pesos só saem se este era o último contexto sobre eles; com um
        // irmão vivo, `get_mut` devolve `None` e o `Drop` do último resolve.
        if let Some(loaded) = Arc::get_mut(&mut self.loaded) {
            loaded.free_now();
        }
    }

    pub fn set_moe_runtime(&mut self, runtime: Arc<crate::moe::runtime::MoeRuntime>) -> Result<()> {
        if self.moe_callback.is_some() {
            bail!("hook MoE já está instalado neste contexto");
        }
        let state = Arc::new(MoeCallbackState {
            runtime,
            telemetry: Mutex::new(MoeCallbackTelemetry::default()),
            error: Mutex::new(None),
            route_diagnostic_emitted: AtomicBool::new(false),
        });
        unsafe {
            sys::llama_set_moe_ffn_callback(
                self.ctx,
                Some(moe_ffn_callback),
                Arc::as_ptr(&state) as *mut c_void,
            );
        }
        if let Some(mtp) = self.mtp.as_ref() {
            mtp.set_moe_callback(Some(moe_ffn_callback), Arc::as_ptr(&state) as *mut c_void);
        }
        self.moe_callback = Some(state);
        Ok(())
    }

    pub fn moe_hook_enabled(&self) -> bool {
        self.moe_callback.is_some()
    }

    pub fn moe_external_executor_enabled(&self) -> bool {
        self.moe_external_executor
    }

    pub(super) fn begin_moe_callback(&self) {
        if let Some(state) = self.moe_callback.as_ref() {
            // O callback só é alcançado depois que o llama.cpp já construiu
            // e começou a avaliar o grafo. Ceder o cache nesse ponto deixava
            // o warmup competir com o prefill. O cancelamento é cooperativo,
            // preserva os handles já residentes e impede que novos candidatos
            // especulativos entrem enquanto a rota quente é montada.
            state.runtime.cancel_device_warmup_for_forward();
            state.begin();
        }
    }

    pub(super) fn ensure_moe_executor_ready(&self) -> Result<()> {
        if self.moe_external_executor && self.moe_callback.is_none() {
            bail!(
                "executor externo de MoE opt-in sem callback; recusa para evitar fallback nativo silencioso"
            );
        }
        Ok(())
    }

    pub(super) fn finish_moe_callback(&self, timings: &mut DecodeTimings) -> Result<()> {
        let Some(state) = self.moe_callback.as_ref() else {
            return Ok(());
        };
        let telemetry = state.take()?;
        if telemetry.forward_us == 0 {
            bail!("hook MoE instalado, mas nenhum bloco MoE foi executado no llama.cpp; recusa para evitar fallback nativo silencioso");
        }
        timings.moe_forward_us = telemetry.forward_us;
        timings.moe_executors = telemetry.executors;
        timings.moe_executor = Some(state.runtime.executor_telemetry());
        Ok(())
    }
}

impl Drop for LlmEngine {
    fn drop(&mut self) {
        // Destrua primeiro os objetos que podem carregar callbacks ou apontar
        // para os pesos/contexto. O caminho explícito `unload` já faz isso,
        // mas `Drop` também pode ser alcançado por um erro durante a
        // inicialização do servidor.
        self.mtp = None;
        self.vision = None;
        self.moe_callback = None;
        // Só o contexto: os pesos são de `LoadedModel`, liberados quando o
        // último contexto que os usa sai de cena. Liberar o modelo aqui, com um
        // irmão ainda vivo, seria use-after-free.
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_set_moe_ffn_callback(self.ctx, None, ptr::null_mut());
                sys::llama_free(self.ctx);
                self.ctx = ptr::null_mut();
            }
        }
    }
}

impl LoadedModel {
    /// Lê os pesos do disco. Uma vez por arquivo, não por contexto.
    pub(super) fn load(model_path: &str, plan: &FitPlan) -> Result<Arc<Self>> {
        Self::load_with_mtp(model_path, plan, plan.mtp_mode == MtpFitMode::Vram)
    }

    /// Lê os pesos com a opção MTP explícita. O recuo para RAM usa isso para
    /// retirar os pesos `nextn` que a primeira tentativa deixou na VRAM do
    /// modelo alvo, já que o rascunho passa a usar o modelo CPU-only irmão.
    pub(super) fn load_with_mtp(
        model_path: &str,
        plan: &FitPlan,
        load_mtp: bool,
    ) -> Result<Arc<Self>> {
        log_moe_route_diagnostics_configuration();
        // llama_backend_init is idempotent; call it on every load.
        unsafe { sys::llama_backend_init() };
        crate::sysmon::mark_backend_initialized();

        let path = CString::new(model_path)
            .with_context(|| format!("invalid model path: {model_path}"))?;

        let mut mparams = unsafe { sys::llama_model_default_params() };
        mparams.n_gpu_layers = plan.config.n_gpu_layers;
        if !plan.devices.is_empty() {
            mparams.devices = plan.devices.as_ptr() as *mut sys::ggml_backend_dev_t;
        }
        mparams.load_mtp = load_mtp;
        mparams.moe_external_executor = plan.config.moe_external_executor;
        if !plan.config.moe_external_executor_layers.is_empty() {
            mparams.moe_external_executor_layers =
                plan.config.moe_external_executor_layers.as_ptr();
            mparams.moe_external_executor_layer_count =
                plan.config.moe_external_executor_layers.len();
        }
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
        let mtp_weights_loaded = load_mtp
            && crate::gguf::has_nextn_tensors(std::path::Path::new(model_path)).unwrap_or(false);

        Ok(Arc::new(LoadedModel {
            model,
            vocab,
            n_embd,
            n_vocab,
            mtp_weights_loaded,
        }))
    }

    /// Carrega um segundo modelo sem pesos na GPU para o contexto MTP de
    /// recuo. Um override isolado dos tensores `nextn` não basta: o scheduler
    /// continua vendo o dispositivo da camada e pode manter o compute na GPU,
    /// além de alguns modelos usarem o output head compartilhado. O modelo
    /// CPU-only deixa todo o grafo MTP, inclusive esse head, no host sem
    /// alterar a colocação do contexto alvo.
    pub(super) fn load_mtp_cpu(model_path: &str, cfg: &EngineConfig) -> Result<Arc<Self>> {
        let mut cpu_cfg = cfg.clone();
        cpu_cfg.n_gpu_layers = 0;
        // The companion has no external executor attached to it. Loading the
        // native expert weights is necessary because MtpDrafter only runs a
        // context and does not install the target engine's MoE callback.
        cpu_cfg.moe_external_executor = false;
        cpu_cfg.moe_external_executor_layers.clear();
        let plan = empty_fit_plan(&cpu_cfg);
        Self::load_with_mtp(model_path, &plan, true)
    }

    /// Carrega um GGUF de rascunho MTP separado ("shared MTP module"), usado
    /// quando o modelo alvo não embute pesos `nextn` próprios.
    ///
    /// Ao contrário de `load_mtp_cpu` (o mesmo arquivo do alvo, só que
    /// CPU-only), este é um arquivo **diferente**: pequeno por desenho -
    /// tipicamente só o bloco `nextn` mais `token_embd`/`output` -, então
    /// carrega com offload total de GPU em vez de forçar CPU. Sem executor
    /// externo de MoE: o arquivo de rascunho não traz experts próprios.
    pub(super) fn load_mtp_draft(draft_path: &str, cfg: &EngineConfig) -> Result<Arc<Self>> {
        let mut draft_cfg = cfg.clone();
        draft_cfg.n_gpu_layers = DEFAULT_GPU_LAYERS;
        draft_cfg.moe_external_executor = false;
        draft_cfg.moe_external_executor_layers.clear();
        let plan = empty_fit_plan_with_mtp_mode(&draft_cfg, MtpFitMode::Vram);
        let loaded = Self::load_with_mtp(draft_path, &plan, true)?;
        if !loaded.has_nextn_weights() {
            bail!("o arquivo de rascunho MTP em {draft_path} não traz pesos nextn");
        }
        Ok(loaded)
    }
}
