use super::*;

#[derive(Debug)]
pub struct MultimodalDecode {
    /// Number of physical token/embedding cells written to the KV cache.
    pub n_tokens: usize,
    /// Logical position immediately after the multimodal chunks.
    pub n_past: u32,
    /// One `[t, y, x, z]` tuple for every physical cell when the model uses
    /// M-RoPE. The llama.cpp embedding batch stores these as four contiguous
    /// sections, which the hidden-state path reconstructs below.
    pub mrope_positions: Option<Vec<[u32; 4]>>,
    pub timings: DecodeTimings,
}

fn write_mrope_position(
    storage: &mut [sys::llama_pos],
    total_tokens: usize,
    row: usize,
    position: [u32; 4],
) -> Result<()> {
    if storage.len() != total_tokens.saturating_mul(4) || row >= total_tokens {
        bail!("buffer de posições M-RoPE inconsistente");
    }
    let position = position
        .map(|coordinate| {
            sys::llama_pos::try_from(coordinate)
                .map_err(|_| anyhow::anyhow!("posição M-RoPE excede o limite do llama.cpp"))
        })
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    storage[row] = position[0];
    storage[total_tokens + row] = position[1];
    storage[2 * total_tokens + row] = position[2];
    storage[3 * total_tokens + row] = position[3];
    Ok(())
}

impl LlmEngine {
    /// Escolhe o modelo que pode respaldar o contexto MTP.
    ///
    /// Um companheiro presente é obrigatório no recuo para RAM: o alvo pode
    /// ter sido carregado deliberadamente sem `nextn`. A seleção devolve
    /// `None` antes de qualquer chamada que reserve o grafo MTP quando nem o
    /// alvo nem o companheiro carregam esses pesos.
    pub(super) fn mtp_model_for_attach<'a>(
        target: &'a LoadedModel,
        companion: Option<&'a LoadedModel>,
    ) -> Option<&'a LoadedModel> {
        let candidate = companion.unwrap_or(target);
        candidate.has_nextn_weights().then_some(candidate)
    }

    /// Cria um contexto e, quando permitido pelo runtime, reaproveita os
    /// pesos carregados por uma geração anterior.
    pub fn load_with_cache(
        model_path: &str,
        cfg: &EngineConfig,
        cache: &ModelCache,
        reuse_weights: bool,
    ) -> Result<Self> {
        let mtp_weights_required = cfg.mtp
            && crate::gguf::has_nextn_tensors(std::path::Path::new(model_path)).unwrap_or(true);
        if reuse_weights {
            if let Some(cached) = cache.get(
                model_path,
                mtp_weights_required,
                false,
                cfg.moe_external_executor,
                &cfg.moe_external_executor_layers,
            ) {
                tracing::info!(
                    model = %model_path,
                    "reutilizando pesos carregados; reconstruindo apenas o contexto"
                );
                let cached_engine = Self::from_loaded(
                    cached.loaded,
                    cfg,
                    cached.fit_memory_applied,
                    cached.effective_n_gpu_layers,
                )?;
                if !cfg.mtp || cached_engine.mtp_initial_context_error.is_none() {
                    return Ok(cached_engine);
                }
                warn!(
                    "pesos MTP em cache não conseguiram criar o contexto na VRAM; recarregando para tentar a RAM"
                );
                // The cache still owns an Arc to the failed target model. Drop
                // it before loading another copy or the fresh fit can observe
                // the old model's VRAM as occupied.
                cache.clear();
            }
            if cfg.mtp && cache.clear_mtp_fallback() {
                warn!(
                    "descartando pesos MTP em cache após recuo para RAM; o acompanhante CPU não é reutilizável"
                );
            }
        }

        let vram_plan = fit_plan(
            model_path,
            cfg,
            if cfg.mtp {
                MtpFitMode::Vram
            } else {
                MtpFitMode::Disabled
            },
        )?;
        let vram_loaded = LoadedModel::load(model_path, &vram_plan)?;
        let mut loaded = Some(vram_loaded.clone());
        let mut engine = Self::from_loaded(
            vram_loaded,
            &vram_plan.config,
            vram_plan.applied,
            vram_plan.config.n_gpu_layers,
        )?;
        if let Some(reason) = vram_plan.mtp_unavailable_reason.clone() {
            // The plan deliberately loaded the target without nextn after
            // rejecting its architecture. Preserve that more useful reason
            // instead of exposing the later, generic no-nextn consequence.
            engine.mtp_unavailable_reason = Some(reason);
        }
        let mut mtp_fallback_used = false;

        let mtp_vram_error = engine.mtp_initial_context_error.clone();
        let mtp_requires_ram =
            cfg.mtp && (vram_plan.mtp_mode == MtpFitMode::Ram || mtp_vram_error.is_some());
        if mtp_requires_ram {
            let vram_reason = mtp_vram_error.clone().map_or_else(
                || {
                    "MTP não foi colocado na VRAM para preservar as camadas e o contexto do modelo principal"
                        .to_string()
                },
                |error| format!("falha ao criar o contexto MTP na VRAM: {error}"),
            );
            if cfg.mtp_cpu_fallback_enabled {
                drop(engine);

                let ram_result = (|| -> Result<(Self, Arc<LoadedModel>)> {
                    let mtp_loaded = LoadedModel::load_mtp_cpu(model_path, &vram_plan.config)?;
                    let target_loaded = if vram_plan.mtp_mode == MtpFitMode::Ram {
                        // The primary-only fit already kept nextn out of the
                        // target. Reuse it so the fallback cannot reintroduce
                        // the optional weights in VRAM.
                        loaded
                            .take()
                            .expect("o plano RAM deve manter o alvo carregado")
                    } else {
                        // The target's nextn weights were loaded in VRAM for
                        // the first attempt. Release that model before loading
                        // the target again without MTP weights, otherwise the
                        // RAM fallback would still pay for the failed GPU copy.
                        loaded.take();
                        LoadedModel::load_with_mtp(model_path, &vram_plan, false)?
                    };
                    let ram_engine = Self::from_loaded_with_mtp_offload(
                        target_loaded.clone(),
                        &vram_plan.config,
                        vram_plan.applied,
                        vram_plan.config.n_gpu_layers,
                        Some(mtp_loaded),
                        false,
                    )?;
                    Ok((ram_engine, target_loaded))
                })();

                match ram_result {
                    Ok((ram_engine, target_loaded)) => {
                        engine = ram_engine;
                        loaded = Some(target_loaded);
                        mtp_fallback_used = true;
                        if !engine.mtp_ready() {
                            let ram_error =
                                engine.mtp_initial_context_error.clone().unwrap_or_else(|| {
                                    "o contexto MTP não ficou pronto na RAM".to_string()
                                });
                            engine.mtp_unavailable_reason =
                                Some(format!("{vram_reason}; RAM: {ram_error}"));
                        }
                    }
                    Err(ram_error) => {
                        warn!(
                            "recuo do MTP para RAM falhou: {ram_error:#}; restaurando o carregamento na VRAM"
                        );
                        let target_loaded = match loaded.take() {
                            Some(target_loaded) => target_loaded,
                            None => LoadedModel::load(model_path, &vram_plan)?,
                        };
                        engine = Self::from_loaded(
                            target_loaded.clone(),
                            &vram_plan.config,
                            vram_plan.applied,
                            vram_plan.config.n_gpu_layers,
                        )?;
                        loaded = Some(target_loaded);
                        engine.mtp_unavailable_reason =
                            Some(format!("{vram_reason}; RAM: {ram_error:#}"));
                    }
                }
            } else {
                engine.mtp_unavailable_reason = Some(format!(
                    "{vram_reason}; o recuo para a RAM foi recusado pelo operador"
                ));
            }
        }

        let loaded = loaded.expect("o modelo alvo deve permanecer carregado");
        let mtp_weights_loaded = loaded.has_nextn_weights();
        cache.put(
            model_path,
            loaded,
            engine.fit_memory_applied,
            engine.effective_n_gpu_layers,
            mtp_weights_loaded,
            mtp_fallback_used,
            engine.moe_external_executor,
            cfg.moe_external_executor_layers.clone(),
        );
        Ok(engine)
    }

    pub(super) fn from_loaded(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
    ) -> Result<Self> {
        Self::from_loaded_with_mtp_offload(
            loaded,
            cfg,
            fit_memory_applied,
            effective_n_gpu_layers,
            None,
            cfg.offload_kqv,
        )
    }

    pub(super) fn from_loaded_with_mtp_offload(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
        mtp_loaded: Option<Arc<LoadedModel>>,
        mtp_offload_kqv: bool,
    ) -> Result<Self> {
        match Self::with_model_and_mtp_offload(
            loaded.clone(),
            cfg,
            mtp_loaded.clone(),
            mtp_offload_kqv,
        ) {
            Ok(mut engine) => {
                engine.fit_memory_applied = fit_memory_applied;
                engine.effective_n_gpu_layers = effective_n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(cfg);
                Ok(engine)
            }
            Err(gpu_kv_error) => {
                let Some(cpu_kv_cfg) = Self::cpu_kv_fallback(cfg) else {
                    return Err(gpu_kv_error);
                };

                warn!(
                    "llama context failed with KV cache on GPU: {gpu_kv_error}; retrying with KV cache in host RAM while keeping {} GPU layers",
                    effective_n_gpu_layers
                );
                let mut engine = Self::with_model_and_mtp_offload(
                    loaded,
                    &cpu_kv_cfg,
                    mtp_loaded,
                    false,
                )
                .with_context(|| {
                    format!(
                        "llama context failed with GPU KV and host-RAM KV fallback; requested {} GPU layers",
                        effective_n_gpu_layers
                    )
                })?;
                engine.fit_memory_applied = fit_memory_applied;
                engine.effective_n_gpu_layers = effective_n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&cpu_kv_cfg);
                Ok(engine)
            }
        }
    }

    pub(super) fn cpu_kv_fallback(cfg: &EngineConfig) -> Option<EngineConfig> {
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
        let plan = fit_plan(
            model_path,
            cfg,
            if cfg.mtp {
                MtpFitMode::Vram
            } else {
                MtpFitMode::Disabled
            },
        )?;
        let loaded = LoadedModel::load(model_path, &plan)?;
        let mtp_unavailable_reason = plan.mtp_unavailable_reason.clone();
        let mtp_loaded =
            if cfg.mtp && plan.mtp_mode == MtpFitMode::Ram && cfg.mtp_cpu_fallback_enabled {
                match LoadedModel::load_mtp_cpu(model_path, &plan.config) {
                    Ok(loaded) => Some(loaded),
                    Err(error) => {
                        warn!("recuo do MTP para RAM falhou no pool: {error:#}");
                        None
                    }
                }
            } else {
                None
            };
        let por_contexto = per_context_config(&plan.config, n);
        (0..n)
            .map(|_| {
                let mut engine = if mtp_loaded.is_some() {
                    Self::with_model_and_mtp_offload(
                        loaded.clone(),
                        &por_contexto,
                        mtp_loaded.clone(),
                        false,
                    )?
                } else {
                    Self::with_model(loaded.clone(), &por_contexto)?
                };
                engine.fit_memory_applied = plan.applied;
                engine.effective_n_gpu_layers = plan.config.n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&por_contexto);
                if let Some(reason) = mtp_unavailable_reason.clone() {
                    engine.mtp_unavailable_reason = Some(reason);
                }
                Ok(engine)
            })
            .collect()
    }

    /// Um contexto novo sobre pesos já carregados.
    ///
    /// É por aqui que se monta mais de um contexto sobre o mesmo modelo, que é
    /// o que permite dois decodes ao mesmo tempo neste nó.
    pub fn with_model(loaded: Arc<LoadedModel>, cfg: &EngineConfig) -> Result<Self> {
        Self::with_model_and_mtp_offload(loaded, cfg, None, cfg.offload_kqv)
    }

    pub(super) fn with_model_and_mtp_offload(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        mtp_loaded: Option<Arc<LoadedModel>>,
        mtp_offload_kqv: bool,
    ) -> Result<Self> {
        let model = loaded.model;
        let n_embd = loaded.n_embd;
        let n_embd_inp = usize::try_from(unsafe { sys::llama_model_n_embd_inp(model) })
            .map_err(|_| anyhow::anyhow!("llama_model_n_embd_inp retornou valor inválido"))?;
        if n_embd_inp == 0 {
            bail!("llama_model_n_embd_inp retornou largura vazia");
        }
        let n_embd_out = unsafe { sys::llama_model_n_embd_out(model) } as usize;
        let n_vocab = loaded.n_vocab;

        // ── Context params ───────────────────────────────────────────────────
        let mut cparams = unsafe { sys::llama_context_default_params() };
        cparams.n_ctx = cfg.n_ctx;
        apply_context_batch_params(&mut cparams, cfg);
        cparams.n_seq_max = kv_seq_budget(cfg.max_sessions);
        cparams.kv_unified = true;
        cparams.offload_kqv = cfg.offload_kqv;
        apply_cache_types(&mut cparams, cfg.cache_type_k, cfg.cache_type_v);
        cparams.embeddings = generic_embeddings_compatible(cfg.need_embeddings, n_embd, n_embd_out);
        cparams.n_rs_seq = cfg.rs_seq_window;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        // ── Residual capture callback ────────────────────────────────────────
        let capture: Box<Mutex<CaptureSlot>> = Box::new(Mutex::new(CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 0,
            max_microbatch_tokens: effective_n_ubatch(cfg) as usize,
            n_embd,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
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
        // Tirado aqui, com os `cparams` que de fato vão para o llama.cpp: é o
        // único ponto em que o pedido e o contexto criado são a mesma coisa.
        let cparams_snapshot = ContextParamsSnapshot {
            kv_unified: cparams.kv_unified,
            embeddings: cparams.embeddings,
            n_rs_seq: cparams.n_rs_seq,
        };
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
        // Cache unificado: as sequências dividem as mesmas células, então a
        // janela por sequência é o contexto inteiro - ver `kv_seq_budget`.
        let n_ctx_seq = actual_n_ctx;
        tracing::info!(
            "llama context ready: n_seq_max={} n_ctx={} n_ctx_seq={} capture={}",
            kv_seq_budget(cfg.max_sessions),
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
        let (mtp, mtp_unavailable_reason, mtp_initial_context_error) = if cfg.mtp {
            if let Some(model_mtp) = Self::mtp_model_for_attach(&loaded, mtp_loaded.as_deref()) {
                let preflight_reason = if !model_mtp.has_nextn_weights()
                    || crate::mtp::nextn_layers(model_mtp.model_ptr()) <= 0
                {
                    Some(MTP_NO_NEXTN_REASON.to_string())
                } else {
                    crate::mtp::mtp_graph_unavailable_reason(
                        model_mtp.model_architecture().as_deref(),
                    )
                };

                if let Some(reason) = preflight_reason {
                    tracing::info!(reason, "MTP indisponível antes da criação do contexto");
                    (None, Some(reason), None)
                } else {
                    let mtp_model = mtp_loaded.clone();
                    match crate::mtp::MtpDrafter::attach(
                        model_mtp,
                        ctx,
                        cfg,
                        n_embd_out,
                        n_vocab,
                        mtp_model,
                        mtp_offload_kqv,
                        cfg.moe_external_executor && mtp_loaded.is_none(),
                    ) {
                        Ok(Some(d)) => (Some(d), None, None),
                        Ok(None) => {
                            tracing::info!(
                                "MTP pedido, mas este modelo não traz bloco nextn \
                                 - seguindo sem MTP"
                            );
                            (None, Some(MTP_NO_NEXTN_REASON.to_string()), None)
                        }
                        // Falhar aqui derrubaria um nó que serve bem sem MTP. A
                        // aceleração é opcional; a inferência não.
                        Err(e) => {
                            let reason = e.to_string();
                            warn!("MTP indisponível neste nó: {reason}");
                            (None, Some(reason.clone()), Some(reason))
                        }
                    }
                }
            } else {
                tracing::info!(
                    "MTP pedido, mas o modelo selecionado não carregou pesos nextn \
                     - seguindo sem MTP"
                );
                (None, Some(MTP_NO_NEXTN_REASON.to_string()), None)
            }
        } else {
            (None, None, None)
        };
        let mtp_allocation = mtp.as_ref().map(|drafter| drafter.allocation());
        let vision = match cfg.mmproj_path.as_deref() {
            Some(path) => match VisionContext::load(path, model, cfg) {
                Ok(vision) => Some(vision),
                Err(error) => {
                    unsafe { sys::llama_free(ctx) };
                    return Err(error);
                }
            },
            None => None,
        };

        Ok(LlmEngine {
            loaded,
            ctx,
            n_embd,
            n_embd_inp,
            n_embd_out,
            n_vocab,
            capture_ok,
            capture,
            offload_kqv: cfg.offload_kqv,
            cache_type_k: cfg.cache_type_k,
            cache_type_v: cfg.cache_type_v,
            fit_memory_applied: false,
            effective_n_gpu_layers: cfg.n_gpu_layers,
            effective_n_ubatch: effective_n_ubatch(cfg),
            cparams_snapshot,
            moe_external_executor: cfg.moe_external_executor,
            vision,
            mtp,
            mtp_unavailable_reason,
            mtp_initial_context_error,
            mtp_allocation,
            moe_callback: None,
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

    /// Explica por que o MTP pedido não está anexado, quando o motor sabe o
    /// motivo. `None` significa que o MTP não foi pedido ou está pronto.
    pub fn mtp_unavailable_reason(&self) -> Option<&str> {
        self.mtp_unavailable_reason.as_deref()
    }

    /// Onde os buffers relevantes do contexto MTP pronto ficaram de fato.
    pub fn mtp_allocation(&self) -> Option<&'static str> {
        self.mtp_allocation.map(MtpAllocation::as_str)
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

    /// `cparams.n_batch` do contexto vivo. Um `llama_decode` com mais tokens
    /// que isto dispara `GGML_ASSERT(n_tokens_all <= cparams.n_batch)`, que
    /// aborta o processo inteiro - por isso os chamadores conferem antes.
    pub fn n_batch(&self) -> u32 {
        unsafe { sys::llama_n_batch(self.ctx) }
    }

    /// O projector foi carregado neste contexto e pode receber imagens.
    pub fn vision_enabled(&self) -> bool {
        self.vision.is_some()
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

    /// Tokeniza texto e imagens pelo projector e avalia todos os chunks no
    /// mesmo cache KV. O retorno preserva a contagem de células físicas e a
    /// posição lógica efetiva, que pode divergir em modelos M-RoPE. Quando a
    /// entrada usa M-RoPE, também devolve uma posição de quatro eixos para
    /// cada célula física, para que o POM possa encaminhar o estado oculto sem
    /// reduzir as duas contagens a uma só.
    pub fn decode_multimodal(
        &mut self,
        prompt: &str,
        images: &[Vec<u8>],
        n_past: u32,
        seq_id: u32,
        logits_last: bool,
    ) -> Result<MultimodalDecode> {
        self.ensure_moe_executor_ready()?;
        let vision_ctx = self
            .vision
            .as_ref()
            .map(|vision| vision.ctx)
            .ok_or_else(|| anyhow::anyhow!("este modelo não tem visão"))?;
        let prompt = CString::new(prompt)
            .map_err(|_| anyhow::anyhow!("o prompt multimodal contém NUL"))?;
        let mut bitmaps = Vec::with_capacity(images.len());
        for image in images {
            let wrapper = unsafe {
                sys::mtmd_helper_bitmap_init_from_buf(
                    vision_ctx,
                    image.as_ptr(),
                    image.len(),
                    false,
                )
            };
            if wrapper.bitmap.is_null() {
                bail!("não foi possível decodificar uma imagem recebida");
            }
            bitmaps.push(wrapper);
        }
        struct BitmapCleanup(Vec<sys::mtmd_helper_bitmap_wrapper>);
        impl Drop for BitmapCleanup {
            fn drop(&mut self) {
                for bitmap in self.0.drain(..) {
                    unsafe {
                        if !bitmap.video_ctx.is_null() {
                            sys::mtmd_helper_video_free(bitmap.video_ctx);
                        }
                        if !bitmap.bitmap.is_null() {
                            sys::mtmd_bitmap_free(bitmap.bitmap);
                        }
                    }
                }
            }
        }
        let bitmaps = BitmapCleanup(bitmaps);
        let bitmap_ptrs = bitmaps
            .0
            .iter()
            .map(|bitmap| bitmap.bitmap as *const sys::mtmd_bitmap)
            .collect::<Vec<_>>();
        let text = sys::mtmd_input_text {
            text: prompt.as_ptr(),
            text_len: prompt.as_bytes().len(),
            add_special: n_past == 0,
            parse_special: true,
        };
        let chunks = unsafe { sys::mtmd_input_chunks_init() };
        if chunks.is_null() {
            bail!("mtmd não conseguiu criar os chunks multimodais");
        }
        struct ChunksCleanup(*mut sys::mtmd_input_chunks);
        impl Drop for ChunksCleanup {
            fn drop(&mut self) {
                unsafe { sys::mtmd_input_chunks_free(self.0) };
            }
        }
        let chunks = ChunksCleanup(chunks);
        let tokenized = unsafe {
            sys::mtmd_tokenize(
                vision_ctx,
                chunks.0,
                &text,
                bitmap_ptrs.as_ptr() as *mut *const sys::mtmd_bitmap,
                bitmap_ptrs.len(),
            )
        };
        if tokenized != 0 {
            bail!("mtmd não conseguiu tokenizar a mensagem multimodal (rc={tokenized})");
        }
        let n_tokens = unsafe { sys::mtmd_helper_get_n_tokens(chunks.0) };
        let mrope_positions = self.multimodal_mrope_positions(chunks.0, n_past)?;
        let mut new_n_past = n_past as sys::llama_pos;
        self.reset_capture_slot(n_tokens);
        let started = Instant::now();
        self.begin_moe_callback();
        let rc = unsafe {
            sys::mtmd_helper_eval_chunks(
                vision_ctx,
                self.ctx,
                chunks.0,
                n_past as sys::llama_pos,
                seq_id as sys::llama_seq_id,
                self.n_batch() as i32,
                logits_last,
                &mut new_n_past,
            )
        };
        if rc != 0 {
            bail!("mtmd falhou ao avaliar a mensagem multimodal (rc={rc})");
        }
        let mut timings = DecodeTimings {
            enqueue_us: started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let sync_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = sync_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        let n_past = u32::try_from(new_n_past)
            .map_err(|_| anyhow::anyhow!("mtmd retornou uma posição lógica inválida"))?;
        Ok(MultimodalDecode {
            n_tokens,
            n_past,
            mrope_positions,
            timings,
        })
    }

    /// Converte os chunks do mtmd para a representação que atravessa o POM.
    ///
    /// `mtmd_helper_get_n_tokens` conta células que serão escritas no KV,
    /// enquanto `mtmd_input_chunk_get_n_pos` conta posições lógicas. Em
    /// M-RoPE uma imagem tem várias células por posição temporal, por isso o
    /// POM não pode reconstruir os eixos apenas a partir de `n_tokens`.
    fn multimodal_mrope_positions(
        &self,
        chunks: *const sys::mtmd_input_chunks,
        start_pos: u32,
    ) -> Result<Option<Vec<[u32; 4]>>> {
        let rope_type = unsafe { sys::llama_model_rope_type(self.loaded.model) };
        if rope_type != sys::LLAMA_ROPE_TYPE_MROPE && rope_type != sys::LLAMA_ROPE_TYPE_IMROPE {
            return Ok(None);
        }

        let mut logical_pos = start_pos;
        let mut positions = Vec::new();
        let chunk_count = unsafe { sys::mtmd_input_chunks_size(chunks) };
        for index in 0..chunk_count {
            let chunk = unsafe { sys::mtmd_input_chunks_get(chunks, index) };
            if chunk.is_null() {
                bail!("mtmd retornou um chunk multimodal nulo");
            }
            let n_tokens = unsafe { sys::mtmd_input_chunk_get_n_tokens(chunk) };
            let n_pos = unsafe { sys::mtmd_input_chunk_get_n_pos(chunk) };
            let n_pos = u32::try_from(n_pos)
                .map_err(|_| anyhow::anyhow!("mtmd retornou uma posição de chunk inválida"))?;
            let image = unsafe { sys::mtmd_input_chunk_get_tokens_image(chunk) };
            if image.is_null() {
                for offset in 0..n_tokens {
                    let position = logical_pos
                        .checked_add(u32::try_from(offset).map_err(|_| {
                            anyhow::anyhow!("chunk multimodal excede o limite de posições")
                        })?)
                        .ok_or_else(|| anyhow::anyhow!("posição multimodal excede o limite"))?;
                    positions.push([position, position, position, 0]);
                }
            } else {
                let mut image_positions = vec![sys::mtmd_decoder_pos {
                    t: 0,
                    x: 0,
                    y: 0,
                    z: 0,
                }; n_tokens];
                unsafe {
                    sys::mtmd_helper_image_get_decoder_pos(
                        image,
                        logical_pos as sys::llama_pos,
                        image_positions.as_mut_ptr(),
                    );
                }
                positions.extend(
                    image_positions
                        .into_iter()
                        .map(|position| [position.t, position.y, position.x, position.z]),
                );
            }
            logical_pos = logical_pos
                .checked_add(n_pos)
                .ok_or_else(|| anyhow::anyhow!("posição multimodal excede o limite"))?;
        }

        let expected_tokens = unsafe { sys::mtmd_helper_get_n_tokens(chunks) };
        if positions.len() != expected_tokens {
            bail!(
                "mtmd retornou {} células, mas a tabela M-RoPE tem {} posições",
                expected_tokens,
                positions.len()
            );
        }
        Ok(Some(positions))
    }

    /// Em qual bloco desta fatia a próxima decodificação deve ser capturada.
    ///
    /// `None` volta ao normal: captura no último bloco. Com `Some(k)` a captura
    /// passa a mirar `l_out-k`. O callback copia o residual e deixa o restante
    /// do grafo continuar; isso é necessário para que cabeças obrigatórias
    /// possam ser executadas sem contaminar o residual exportado.
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

    /// O Flash Attention resolvido deste contexto, ou `None` se a ponte falhar.
    ///
    /// O par é `(ligado, decidido_pela_sonda)`. O padrão do llama.cpp é
    /// `AUTO`, então o pedido não responde a pergunta: quem responde é
    /// `get_cparams()` depois que o contexto subiu. Ver
    /// `llama_rs_context_flash_attn` no `wrapper_common.cpp`.
    pub fn flash_attn(&self) -> Option<(bool, bool)> {
        let mut enabled = false;
        let mut auto = false;
        let status = unsafe { sys::llama_rs_context_flash_attn(self.ctx, &mut enabled, &mut auto) };
        (status == sys::LLAMA_RS_STATUS_OK).then_some((enabled, auto))
    }

    /// Com que parâmetros de llama.cpp este contexto está de pé.
    ///
    /// Serve ao operador que quer comparar o nó com uma linha de comando do
    /// `llama-cli`. Os valores vêm do contexto vivo sempre que existe getter,
    /// e não do `EngineConfig` - ver `ContextParamsSnapshot` para o porquê.
    pub fn load_params(&self) -> serde_json::Value {
        let flash = self.flash_attn();
        serde_json::json!({
            "n_ctx": self.n_ctx(),
            "n_ctx_seq": unsafe { sys::llama_n_ctx_seq(self.ctx) },
            "n_batch": unsafe { sys::llama_n_batch(self.ctx) },
            "n_ubatch": unsafe { sys::llama_n_ubatch(self.ctx) },
            "n_seq_max": unsafe { sys::llama_n_seq_max(self.ctx) },
            "n_threads": unsafe { sys::llama_n_threads(self.ctx) },
            "n_threads_batch": unsafe { sys::llama_n_threads_batch(self.ctx) },
            // `null` quando a ponte de diagnóstico não respondeu: a interface
            // diz que não sabe, em vez de repetir o pedido como se fosse a
            // decisão.
            "flash_attn": flash.map(|(enabled, _)| enabled),
            "flash_attn_auto": flash.map(|(_, auto)| auto),
            "kv_unified": self.cparams_snapshot.kv_unified,
            "embeddings": self.cparams_snapshot.embeddings,
            "n_rs_seq": self.cparams_snapshot.n_rs_seq,
            "offload_kqv": self.offload_kqv,
            "cache_type_k": self.cache_type_k.as_str(),
            "cache_type_v": self.cache_type_v.as_str(),
            "n_gpu_layers": self.effective_n_gpu_layers,
            "fit_memory_applied": self.fit_memory_applied,
        })
    }

    /// A memória deste contexto por buffer: pesos, cache KV e cálculo.
    ///
    /// É o que responde onde o cache KV ficou de verdade. `offload_kqv()` é o
    /// que foi pedido; numa GPU cheia o llama.cpp recarrega com ele desligado
    /// (ver `runtime.rs`), e só a repartição por buffer mostra isso.
    pub fn context_memory(&self) -> Option<serde_json::Value> {
        unsafe { json_from_wrapper(|out| sys::llama_rs_context_memory_json(self.ctx, out)) }
    }

    /// Compute buffer CUDA medido pelo próprio llama.cpp para este contexto.
    ///
    /// O valor alimenta o orçamento do cache residente de experts. Se a ponte
    /// de memória não reportar um buffer CUDA completo, devolve `None` para que
    /// o caminho CUDA seja recusado nominalmente.
    pub fn cuda_compute_bytes(&self) -> Option<u64> {
        crate::moe::admission::cuda_compute_bytes_from_llama_memory(&self.context_memory()?)
    }

    pub fn eos_token(&self) -> i32 {
        unsafe { sys::llama_vocab_eos(self.loaded.vocab) }
    }

    /// Returns true for the model EOS or for a complete turn terminator named
    /// by the GGUF chat template. A Gemma4 turn is not necessarily the model
    /// EOS, so checking only `eos_token()` lets the next turn leak into the
    /// response.
    pub fn is_generation_stop(&self, recent: &[i32], next: i32) -> bool {
        if next == self.eos_token() {
            return true;
        }
        let Some(template) = self.chat_template() else {
            return false;
        };
        if !template.contains("<turn|>") {
            return false;
        }
        let turn_end = self.tokenize("<turn|>", false, true);
        if turn_end.is_empty() {
            return false;
        }
        let mut candidate = Vec::with_capacity(recent.len() + 1);
        candidate.extend_from_slice(recent);
        candidate.push(next);
        token_sequence_ends_with(&candidate, &turn_end)
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

    /// Devolve os bytes crus da peça do vocabulário.
    ///
    /// Uma peça pode terminar no meio de um codepoint UTF-8. A conversão para
    /// `String` fica deliberadamente fora deste limite: o caminho de streaming
    /// precisa alimentar um `Utf8Assembler` que vive na sessão da requisição.
    pub fn token_to_piece_bytes(&self, token: i32, special: bool) -> Vec<u8> {
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
            return Vec::new();
        }
        buf.truncate(n as usize);
        buf
    }

    // ── Chat template ─────────────────────────────────────────────────────────

    /// The `tokenizer.chat_template` from the model's GGUF metadata, or None.
    pub fn chat_template(&self) -> Option<String> {
        self.model_metadata_string("tokenizer.chat_template")
    }

    /// A arquitetura declarada no GGUF, sem inferência pelo nome do arquivo.
    pub fn model_architecture(&self) -> Option<String> {
        self.model_metadata_string("general.architecture")
    }

    pub(super) fn model_metadata_string(&self, key: &str) -> Option<String> {
        model_metadata_string(self.loaded.model, key)
    }

    /// Apply the model's built-in Jinja2 chat template.
    ///
    /// Gemma4 receives an explicit renderer because its `enable_thinking`
    /// variable cannot be passed through the current llama.cpp C API. Other
    /// templates must succeed in llama.cpp; a failure is returned rather than
    /// silently changing the model's control tokens.
    pub fn apply_chat_template(
        &self,
        messages: &[(&str, &str)],
        add_generation_prompt: bool,
        no_thinking: bool,
    ) -> Result<String> {
        let sys_msg = "You are a helpful assistant.";
        let full_messages: Vec<(&str, &str)> = std::iter::once(("system", sys_msg))
            .chain(messages.iter().copied())
            .collect();

        let tmpl = self.chat_template();
        let architecture = self.model_architecture();
        if architecture
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("gemma4"))
        {
            let template = tmpl.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Gemma4 exige tokenizer.chat_template no GGUF; renderer não inventado"
                )
            })?;
            let bos = unsafe { sys::llama_vocab_bos(self.loaded.vocab) };
            let bos = String::from_utf8_lossy(&self.token_to_piece_bytes(bos, true)).into_owned();
            if bos.is_empty() {
                bail!("Gemma4 exige token BOS no vocabulário GGUF");
            }
            return render_gemma4_template(
                template,
                &bos,
                messages,
                add_generation_prompt,
                no_thinking,
            );
        }

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

        // `llama_chat_apply_template` devolve **sempre** o tamanho completo do
        // texto renderizado e copia no máximo `length` bytes (`strncpy` em
        // `llama.cpp`). Um buffer fixo, portanto, não é um limite: é um corte
        // silencioso pelo fim. Com 65536 bytes fixos, dois prompts distintos de
        // ~126 KB rendiam exatamente os mesmos 12941 tokens - a última mensagem
        // do usuário simplesmente não chegava ao modelo, e o prefixo do turno
        // seguinte ficava idêntico ao do anterior. Pedir de novo com o tamanho
        // informado é o contrato: a segunda chamada cabe por construção.
        let mut out_buf = vec![0u8; CHAT_TEMPLATE_INITIAL_BUF];
        let mut n = unsafe {
            sys::llama_chat_apply_template(
                tmpl_ptr,
                raw_msgs.as_ptr(),
                raw_msgs.len(),
                add_generation_prompt,
                out_buf.as_mut_ptr() as *mut i8,
                out_buf.len() as i32,
            )
        };
        if n > 0 && (n as usize) > out_buf.len() {
            out_buf = vec![0u8; n as usize];
            n = unsafe {
                sys::llama_chat_apply_template(
                    tmpl_ptr,
                    raw_msgs.as_ptr(),
                    raw_msgs.len(),
                    add_generation_prompt,
                    out_buf.as_mut_ptr() as *mut i8,
                    out_buf.len() as i32,
                )
            };
        }

        // Reclaim tmpl_ptr if we allocated one.
        if !tmpl_ptr.is_null() {
            unsafe { drop(CString::from_raw(tmpl_ptr as *mut i8)) };
        }

        let mut text = if n > 0 {
            let escrito = (n as usize).min(out_buf.len());
            if escrito < n as usize {
                bail!(
                    "llama_chat_apply_template pediu {n} bytes e o buffer tem {}; \
                     renderizar o prompt cortado entregaria a conversa pela metade",
                    out_buf.len()
                );
            }
            out_buf.truncate(escrito);
            String::from_utf8_lossy(&out_buf).into_owned()
        } else {
            bail!(
                "llama_chat_apply_template falhou (rc={n}) para arquitetura {}; renderer explícito indisponível",
                architecture.as_deref().unwrap_or("desconhecida")
            );
        };

        if no_thinking {
            if let Some(ref tmpl_str) = tmpl {
                if tmpl_str.contains("<think>") {
                    text.push_str("<think>\n\n</think>\n");
                }
            }
        }
        Ok(text)
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
        self.ensure_moe_executor_ready()?;
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
        self.reset_capture_slot(layout.total_tokens());
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
        self.begin_moe_callback();
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
        self.ensure_moe_executor_ready()?;
        if entries.is_empty() {
            bail!("batch de hidden states vazio");
        }
        let descriptors: Vec<BatchEntry> = entries.iter().map(|e| e.descriptor.clone()).collect();
        let layout = BatchLayout::new(&descriptors)?;
        let mut feature_shape: Option<&[usize]> = None;
        let mut input_width = None;
        for entry in entries {
            let entry_width = validate_hidden_tensor(
                entry.hidden.len(),
                &entry.shape,
                entry.descriptor.tokens,
                self.n_embd,
                self.n_embd_inp,
                self.n_embd_out,
                &format!("item {}", entry.descriptor.item_id),
            )?;
            if let Some(expected_width) = input_width {
                if expected_width != entry_width {
                    bail!(
                        "item {} tem largura {}, mas o batch usa {}",
                        entry.descriptor.item_id,
                        entry_width,
                        expected_width
                    );
                }
            } else {
                input_width = Some(entry_width);
            }
            if let Some(expected_shape) = feature_shape {
                if *expected_shape != entry.shape[1..] {
                    bail!(
                        "item {} tem layout {:?}, mas o batch usa {:?}",
                        entry.descriptor.item_id,
                        &entry.shape[1..],
                        expected_shape
                    );
                }
            } else {
                    feature_shape = Some(&entry.shape[1..]);
            }
            if let Some(positions) = &entry.mrope_positions {
                if positions.len() != entry.descriptor.tokens {
                    bail!(
                        "item {} declara {} células, mas recebeu {} posições M-RoPE",
                        entry.descriptor.item_id,
                        entry.descriptor.tokens,
                        positions.len()
                    );
                }
            }
        }

        let n = i32::try_from(layout.total_tokens())
            .map_err(|_| anyhow::anyhow!("batch excede o limite de tokens do llama.cpp"))?;
        let input_width = input_width.expect("entries não vazio garante largura de entrada");
        let input_width_i32 = i32::try_from(input_width)
            .map_err(|_| anyhow::anyhow!("largura de entrada excede o limite do llama.cpp"))?;
        let enqueue_started = Instant::now();
        self.reset_capture_slot(layout.total_tokens());
        let has_mrope_positions = entries.iter().any(|entry| entry.mrope_positions.is_some());
        let mut flattened_positions = has_mrope_positions.then(|| {
            vec![0 as sys::llama_pos; layout.total_tokens().saturating_mul(4)]
        });
        if let Some(storage) = flattened_positions.as_mut() {
            let mut row = 0usize;
            for entry in entries {
                for offset in 0..entry.descriptor.tokens {
                    let scalar_position = entry.descriptor.pos + offset as u32;
                    let position = entry
                        .mrope_positions
                        .as_ref()
                        .map(|positions| positions[offset])
                        .unwrap_or([scalar_position, scalar_position, scalar_position, 0]);
                    write_mrope_position(storage, layout.total_tokens(), row, position)?;
                    row += 1;
                }
            }
        }
        let mut batch = unsafe { sys::llama_batch_init(n, input_width_i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut();
        let mut row = 0usize;
        unsafe {
            for entry in entries {
                let start = row * input_width;
                let end = start + entry.hidden.len();
                std::ptr::copy_nonoverlapping(
                    entry.hidden.as_ptr(),
                    batch.embd.add(start),
                    end - start,
                );
                for offset in 0..entry.descriptor.tokens {
                    let scalar_position = entry.descriptor.pos + offset as u32;
                    if flattened_positions.is_none() {
                        *batch.pos.add(row) = scalar_position as sys::llama_pos;
                    }
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
        let scalar_positions = batch.pos;
        if let Some(storage) = flattened_positions.as_mut() {
            batch.pos = storage.as_mut_ptr();
        }
        self.begin_moe_callback();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        batch.pos = scalar_positions;
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden batch) failed rc={rc}");
        }
        self.finish_batch(layout, need_residual, enqueue_started)
    }

    pub(super) fn reset_capture_slot(&self, n_tokens: usize) {
        let mut capture = self.capture.lock().unwrap();
        capture.buf = None;
        capture.expected_tokens = n_tokens;
        capture.expected_floats = None;
        capture.layout = None;
        capture.error = None;
        capture.last_shape = None;
        capture.last_strides = None;
        capture.last_nbytes = None;
        capture.copy_us = 0;
    }

    pub(super) fn finish_batch(
        &self,
        layout: BatchLayout,
        need_residual: bool,
        enqueue_started: Instant,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        crate::tp_probe::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        if crate::tp_probe::is_active() {
            let reduces = crate::tp_probe::reduces_in_current_decode();
            let expected = 2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64;
            if reduces != expected {
                bail!("coletiva TP reduziu {reduces} vezes neste batch, esperado {expected}");
            }
            if let Some(motivo) = crate::tp_probe::take_failure() {
                bail!("coletiva de tensor parallelism falhou: {motivo}");
            }
        }
        let residual = if need_residual {
            Some(self.take_residual(layout.total_tokens())?)
        } else {
            None
        };
        let residual_shape = residual
            .as_ref()
            .map(|captured| captured.shape.clone())
            .unwrap_or_else(|| vec![0, self.n_embd]);
        let residual = residual.map(|captured| captured.values);
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        Ok((
            BatchDecodeOutput {
                layout,
                residual,
                residual_shape,
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
        self.ensure_moe_executor_ready()?;
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot(tokens.len());
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

        self.begin_moe_callback();
        let tp_antes = crate::tp_probe::reduce_count();
        self.begin_moe_callback();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode failed rc={rc}");
        }
        crate::tp_probe::set_expected_reduces(
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
        if crate::tp_probe::is_active() {
            let reduces = crate::tp_probe::reduces_in_current_decode();
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
        if crate::tp_probe::is_active() && crate::tp_probe::reduce_count() == tp_antes {
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
        if let Some(motivo) = crate::tp_probe::take_failure() {
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
                    let reason = format!("MTP desligado nesta sessão: {e}");
                    warn!("{reason}");
                    self.mtp_unavailable_reason = Some(reason);
                    self.mtp_allocation = None;
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
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
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
        shape: &TensorShape,
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
        tokens: Option<&[i32]>,
    ) -> Result<DecodeTimings> {
        self.decode_hidden_with_positions(
            hidden,
            shape,
            n_tokens,
            pos_offset,
            seq_id,
            logits_all,
            tokens,
            None,
        )
    }

    /// Decode hidden states while preserving the physical-to-logical position
    /// map of an M-RoPE multimodal batch. `llama_batch.pos` normally has one
    /// scalar per embedding cell; for M-RoPE it instead points at four
    /// contiguous position sections, just as the mtmd helper builds its
    /// embedding batch internally.
    pub fn decode_hidden_with_positions(
        &mut self,
        hidden: &[f32],
        shape: &TensorShape,
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
        tokens: Option<&[i32]>,
        mrope_positions: Option<&[[u32; 4]]>,
    ) -> Result<DecodeTimings> {
        self.ensure_moe_executor_ready()?;
        if let Some(positions) = mrope_positions {
            if positions.len() != n_tokens {
                bail!(
                    "estado oculto declara {n_tokens} células, mas recebeu {} posições M-RoPE",
                    positions.len()
                );
            }
        }
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot(n_tokens);
        let enqueue_started = Instant::now();
        let input_width = validate_hidden_tensor(
            hidden.len(),
            shape,
            n_tokens,
            self.n_embd,
            self.n_embd_inp,
            self.n_embd_out,
            "hidden state",
        )?;
        let n = i32::try_from(n_tokens)
            .map_err(|_| anyhow::anyhow!("hidden state excede o limite de tokens do llama.cpp"))?;
        let input_width_i32 = i32::try_from(input_width)
            .map_err(|_| anyhow::anyhow!("largura de entrada excede o limite do llama.cpp"))?;
        let mut mrope_batch_positions = if let Some(positions) = mrope_positions {
            let mut storage = vec![0 as sys::llama_pos; n_tokens.saturating_mul(4)];
            for (row, &position) in positions.iter().enumerate() {
                write_mrope_position(&mut storage, n_tokens, row, position)?;
            }
            Some(storage)
        } else {
            None
        };
        let mut batch = unsafe { sys::llama_batch_init(n, input_width_i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut(); // signals embedding mode

        unsafe {
            // Copy the flat embedding data into the batch's embd buffer.
            let embd_ptr = batch.embd;
            std::ptr::copy_nonoverlapping(hidden.as_ptr(), embd_ptr, hidden.len());

            for i in 0..n_tokens {
                if mrope_batch_positions.is_none() {
                    *batch.pos.add(i) = (pos_offset as i32) + i as i32;
                }
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = if logits_all || i == n_tokens - 1 {
                    1
                } else {
                    0
                };
            }
        }

        let scalar_positions = batch.pos;
        if let Some(storage) = mrope_batch_positions.as_mut() {
            batch.pos = storage.as_mut_ptr();
        }
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        batch.pos = scalar_positions;
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden) failed rc={rc}");
        }
        crate::tp_probe::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        // Ver a nota gêmea em `decode_tokens`: a coletiva falha por callback e
        // só pode virar erro aqui fora.
        if let Some(motivo) = crate::tp_probe::take_failure() {
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
                        let reason = format!("MTP desligado nesta sessão: {e}");
                        warn!("{reason}");
                        self.mtp_unavailable_reason = Some(reason);
                        self.mtp_allocation = None;
                        drafter = None;
                    }
                }
                self.mtp = drafter;
            }
        } else if self.mtp.is_some() {
            let reason = "MTP desligado: estado oculto recebido sem ids dos tokens".to_string();
            warn!(
                "{reason} (peer velho no pipeline?), e a cabeça nextn precisa deles para acompanhar o tronco"
            );
            self.mtp_unavailable_reason = Some(reason);
            self.mtp_allocation = None;
            self.mtp = None;
        }
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        Ok(timings)
    }

    // ── Residual extraction ───────────────────────────────────────────────────

    /// Consume the residual captured during the last `decode_*` call.
    ///
    /// Must be called exactly once per decode when `need_embeddings` is true.
    pub fn take_residual(&self, n_tokens: usize) -> Result<CapturedResidual> {
        if self.capture_ok {
            let mut capture = self.capture.lock().unwrap();
            let captured = take_captured_residual(&mut capture)?;
            if captured.shape.first().copied() != Some(n_tokens) {
                bail!(
                    "residual capture: captured {} tokens, expected {} (shape {:?}, last tensor shape {:?}, strides {:?}, nbytes {:?})",
                    captured.shape.first().copied().unwrap_or(0),
                    n_tokens,
                    captured.shape,
                    capture.last_shape,
                    capture.last_strides,
                    capture.last_nbytes
                );
            }
            Ok(captured)
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
            Ok(CapturedResidual {
                values: out,
                shape: vec![n_tokens, self.n_embd],
            })
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

    /// Compartilha o KV de `src` com `dst`, sequência inteira.
    ///
    /// É o que permite **não gerar dentro da sequência do prompt**: o prompt
    /// fica numa sequência que ninguém estende, a geração corre numa cópia, e
    /// no fim do turno a cópia some inteira - remoção total, que todo motor
    /// aceita, inclusive os recorrentes que recusam remoção parcial (ver
    /// `kv_remove`). Sem isto o reuso de prefixo dependeria de o modelo aceitar
    /// recuar o cache, e essa é justamente a dependência que o desenho remove.
    ///
    /// Custo: com o cache unificado (`kv_unified`, ver `llama_context_params`)
    /// as duas sequências dividem as mesmas células e a cópia é só metadado -
    /// nenhum tensor é duplicado. O llama.cpp faz cópia-na-escrita quando a
    /// geração escreve por cima de uma célula compartilhada, inclusive no
    /// estado recorrente (`llama_memory_recurrent::find_slot`).
    pub fn kv_copy_seq(&self, src: u32, dst: u32) {
        if src == dst {
            return;
        }
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if mem.is_null() {
                return;
            }
            sys::llama_memory_seq_cp(mem, src as i32, dst as i32, -1, -1);
        }
    }

    /// `llama_memory_can_shift` - se o llama.cpp consegue reindexar (RoPE) as
    /// posições já cacheadas, o "K-shift" que o llama.cpp de referência usa
    /// para `--ctx-shift`/geração infinita quando o contexto enche. A cauda
    /// usa este valor como guarda antes de produzir o próximo `next_draft` em
    /// modelos híbridos/recorrentes; a remoção efetiva continua sendo
    /// `kv_remove` (`llama_memory_seq_rm`), uma operação separada.
    pub fn kv_can_shift(&self) -> bool {
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            !mem.is_null() && sys::llama_memory_can_shift(mem)
        }
    }

    /// Tamanho exato, em bytes, do estado de uma única sequência de KV. Só serve
    /// para dimensionar um buffer de salvamento ou orçar disco; não é usado no
    /// caminho de arquivo, que aloca sozinho.
    pub fn state_seq_size(&self, seq_id: u32) -> usize {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { sys::llama_state_seq_get_size(self.ctx, seq_id as i32) }
    }

    /// Grava o KV de uma sequência em disco com um cabeçalho de tokens, como o
    /// `--slot-save-path` do llama-server (`llama_state_seq_save_file`). Só
    /// `prompt_seq` deve ser salvo: nunca a cauda volátil da geração. `tokens`
    /// é o cabeçalho consultivo do prompt - a cabeça tem os tokens reais, os
    /// hops seguintes passam um vetor do tamanho certo. Devolve os bytes
    /// escritos; zero é falha do llama.cpp (caminho inválido, disco cheio).
    pub fn state_seq_save_file(&self, path: &Path, seq_id: u32, tokens: &[i32]) -> Result<usize> {
        if self.ctx.is_null() {
            bail!("state_seq_save_file: contexto nulo");
        }
        let cpath = path_to_cstring(path)?;
        let written = unsafe {
            sys::llama_state_seq_save_file(
                self.ctx,
                cpath.as_ptr(),
                seq_id as i32,
                tokens.as_ptr(),
                tokens.len(),
            )
        };
        if written == 0 {
            bail!("llama_state_seq_save_file devolveu 0 para {}", path.display());
        }
        Ok(written)
    }

    /// Lê um arquivo escrito por `state_seq_save_file` para dentro de
    /// `dest_seq_id` (`llama_state_seq_load_file`). Devolve `(bytes_lidos,
    /// tokens_do_cabeçalho)`. Zero bytes é falha: arquivo corrompido,
    /// incompatível com o contexto atual, ou `token_cap` pequeno demais.
    pub fn state_seq_load_file(
        &self,
        path: &Path,
        dest_seq_id: u32,
        token_cap: usize,
    ) -> Result<(usize, Vec<i32>)> {
        if self.ctx.is_null() {
            bail!("state_seq_load_file: contexto nulo");
        }
        let cpath = path_to_cstring(path)?;
        let mut tokens_out = vec![0i32; token_cap.max(1)];
        let mut n_token_count: usize = 0;
        let read = unsafe {
            sys::llama_state_seq_load_file(
                self.ctx,
                cpath.as_ptr(),
                dest_seq_id as i32,
                tokens_out.as_mut_ptr(),
                tokens_out.len(),
                &mut n_token_count,
            )
        };
        if read == 0 {
            bail!(
                "llama_state_seq_load_file devolveu 0 para {} (cap {token_cap}, contou {n_token_count})",
                path.display()
            );
        }
        tokens_out.truncate(n_token_count);
        Ok((read, tokens_out))
    }
}

/// `&Path` -> `CString` para a FFI, recusando um caminho com NUL embutido em vez
/// de truncar silenciosamente.
pub(super) fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_encoded_bytes())
        .with_context(|| format!("caminho inválido para FFI: {}", path.display()))
}
