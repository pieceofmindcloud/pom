use super::*;

impl InferState {
    pub(super) fn new(
        engine: LlmEngine,
        cfg: &NodeConfig,
        inference_path: crate::executor::InferencePath,
        moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
        moe_fallback_reason: Option<String>,
    ) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        Self {
            tls_handshake_ms: None,
            engine,
            moe,
            moe_fallback_reason,
            sessions: HashMap::new(),
            seq_pool,
            max_sessions,
            sampling: SamplingConfig {
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                top_k: cfg.top_k as i32,
                repeat_penalty: cfg.repeat_penalty,
                repeat_last_n: cfg.repeat_last_n as usize,
            },
            no_thinking: cfg.no_thinking,
            // Mantém o comportamento dos chamadores antigos que não enviam a
            // flag. O gateway atual sempre a explicita por endpoint.
            chat_template: cfg.no_thinking,
            start_layer: cfg.start_layer,
            end_layer: cfg.end_layer,
            total_layers: cfg.total_layers,
            model_label: cfg.model_label.clone(),
            model_path: cfg.model_path.clone(),
            slot_dir: cfg
                .slot_save_path
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(std::path::PathBuf::from),
            slot_freed: Arc::new(tokio::sync::Notify::new()),
            original_block_count: None, // filled in after engine load
            inference_path,
            mtp_min_probability: cfg.mtp_min_probability,
            tp: cfg.tp.clone(),
        }
    }

    pub(super) fn apply_moe_hidden(
        &self,
        hidden: Vec<f32>,
        n_tokens: usize,
        capture_block: Option<u32>,
        timings: &mut NodeTimings,
    ) -> Result<Vec<f32>> {
        // A runtime object is not ownership. Only the explicitly selected MoE
        // path may claim the boundary block; a dense model must never execute
        // the sidecar merely because one was accidentally constructed.
        if !self.inference_path.is_moe() {
            return Ok(hidden);
        }
        if self.engine.moe_hook_enabled() {
            return Ok(hidden);
        }
        if self.engine.moe_external_executor_enabled() {
            bail!(
                "executor externo de MoE está ativo, mas o hook não está instalado; recusa para evitar executar os experts duas vezes"
            );
        }
        let Some(moe) = self.moe.as_ref() else {
            if let Some(reason) = self.moe_fallback_reason.as_deref() {
                timings.record_moe_reason(reason);
            }
            return Ok(hidden);
        };
        let layer = capture_block.unwrap_or_else(|| moe.layers().saturating_sub(1));
        let forward_started = Instant::now();
        let output = moe
            .forward_batch(layer, &hidden, n_tokens)
            .map_err(|error| anyhow::anyhow!("forward MoE falhou: {error}"))?;
        timings.record_moe_telemetry(moe.executor_telemetry());
        timings.record_moe_forward(forward_started.elapsed().as_micros() as u64);
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
        Ok(output)
    }

    pub(super) fn alloc_seq(&mut self) -> Option<u32> {
        if self.seq_pool.is_empty() {
            self.reap_idle();
        }
        if self.seq_pool.is_empty() {
            // Uma sessão retida é só cache de prompt entre turnos: devolvê-la
            // custa o reuso de um turno, e não devolvê-la custa a requisição
            // inteira ("seq pool esgotado"). O cache cede primeiro.
            self.evict_retained();
        }
        if self.seq_pool.is_empty() {
            return None;
        }
        Some(self.seq_pool.remove(0))
    }

    pub(super) fn free_seq(&mut self, seq_id: u32) {
        if !self.seq_pool.contains(&seq_id) {
            self.seq_pool.push(seq_id);
        }
    }

    /// A sequência de geração que acompanha uma sequência de prompt.
    ///
    /// Par fixo, não sorteado de um pool: quem tem slot de prompt tem slot de
    /// geração por construção, então abrir a geração nunca falha por falta de
    /// slot. Ver `llm::kv_seq_budget`, que reserva as duas metades no contexto.
    pub(super) fn gen_seq_of(&self, prompt_seq: u32) -> u32 {
        prompt_seq + self.max_sessions.max(1)
    }

    /// Despeja a sessão retida menos usada, devolvendo o slot dela.
    pub(super) fn evict_retained(&mut self) -> bool {
        let Some(sid) = self
            .sessions
            .iter()
            .filter(|(_, session)| session.retained)
            .min_by_key(|(_, session)| session.last_used)
            .map(|(sid, _)| sid.clone())
        else {
            return false;
        };
        info!(
            "despejando prefixo retido da sessão {}… para liberar slot",
            &sid[..8.min(sid.len())]
        );
        self.finish(&sid);
        true
    }

    /// Abre a sequência de geração deste turno copiando o KV do prompt.
    ///
    /// Idempotente: chamado antes de cada decodificação de token gerado, faz
    /// trabalho só na primeira. Depois disto nada mais escreve em
    /// `prompt_seq`, que é o que torna o prefixo reutilizável sem remoção
    /// parcial de KV - ver o doc de `Session`.
    pub(super) fn open_generation(&mut self, sid: &str) -> Result<()> {
        let Some(session) = self.sessions.get(sid) else {
            bail!("sessão '{sid}' desconhecida — faça o prefill antes");
        };
        if session.gen_seq.is_some() {
            return Ok(());
        }
        let prompt_seq = session.prompt_seq;
        let gen = self.gen_seq_of(prompt_seq);
        // A cópia herda o que estiver na sequência de destino se ela não
        // estiver limpa; limpar é remoção total, sempre aceita.
        let _ = self.engine.kv_remove(gen, 0, -1);
        self.engine.kv_copy_seq(prompt_seq, gen);
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.gen_seq = Some(gen);
        session.retained = false;
        Ok(())
    }

    /// Decide, no pedaço 0, se o prefixo retido serve para este prompt - e
    /// deixa a sessão coerente com a decisão.
    ///
    /// A regra é **só acrescentar**: o prefixo serve quando os tokens retidos
    /// são exatamente um prefixo dos novos, e nem um token a mais. Assim o
    /// prefill continua de onde parou e **nenhuma posição precisa sair do KV** -
    /// que é a diferença entre funcionar em todo modelo e depender de o motor
    /// aceitar `llama_memory_seq_rm` parcial. Qualquer outro caso (divergência
    /// no meio, prompt idêntico ao retido, sessão sem prompt) recomeça a sessão
    /// do zero, o que é uma perda de reuso naquele turno, nunca um erro.
    ///
    /// O último token do prompt nunca entra no prefixo aproveitado: é dele que
    /// saem os logits do primeiro token gerado, então sempre sobra pelo menos
    /// um token para decodificar.
    pub(super) fn adopt_prompt_prefix(
        &mut self,
        sid: &str,
        tokens: Vec<i32>,
        stable_len: u32,
        requested_reuse: bool,
    ) -> Result<PrefixDecision> {
        let mut decision = PrefixDecision::default();
        if requested_reuse {
            let session = self.sessions.get(sid).ok_or_else(|| {
                anyhow::anyhow!("sessão '{sid}' desconhecida no reuso de prefixo")
            })?;
            let retained = session
                .prompt_tokens
                .get(..session.n_past as usize)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "sessão '{sid}' retém {} tokens, mas só conhece {}",
                        session.n_past,
                        session.prompt_tokens.len()
                    )
                })?;
            decision = plan_prefix_reuse(retained, session.n_past, &tokens);
            if !decision.reused {
                let comum = retained
                    .iter()
                    .zip(&tokens)
                    .take_while(|(old, new)| old == new)
                    .count();
                debug!(
                    "prefixo da sessão {}… não serve: comum={comum} retido={} retidos={} novos={}",
                    &sid[..8.min(sid.len())],
                    session.n_past,
                    session.prompt_tokens.len(),
                    tokens.len()
                );
                self.get_or_create(sid, true)?;
            }
        }
        let session = self
            .sessions
            .get_mut(sid)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid}' desapareceu no prefill"))?;
        session.prompt_tokens = tokens;
        session.stable_len = stable_len;
        session.retained = false;
        Ok(decision)
    }

    /// Fecha o turno mantendo o prompt: apaga a sequência de geração inteira e
    /// devolve a sessão à posição do fim do prompt.
    ///
    /// Só usa remoção **total** de sequência, que todo motor aceita - inclusive
    /// os recorrentes/híbridos, onde `llama_memory_seq_rm` recusa qualquer
    /// recuo parcial maior que a janela de snapshots (`n_rs_seq`). É a
    /// substituição do rewind que era recusado em 75 das 176 requisições.
    ///
    /// Devolve quantos tokens de prompt ficaram retidos, ou `None` quando não
    /// havia prompt para reter (aí a sessão é encerrada de vez).
    pub(super) fn retain_prompt(&mut self, sid: &str) -> Option<u32> {
        let session = self.sessions.get(sid)?;
        if session.prompt_len == 0 {
            self.finish(sid);
            return None;
        }
        let (gen_seq, prompt_len) = (session.gen_seq, session.prompt_len);
        if let Some(gen) = gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.gen_seq = None;
        session.n_past = prompt_len;
        session.recent_tokens.clear();
        let _ = session.finish_token_stream();
        session.retained = true;
        session.last_used = Instant::now();
        // Uma sessão retida vira slot reclamável (`evict_retained`): um prefill
        // de sessão nova que esperava pode agora entrar.
        self.signal_slot_freed();
        Some(prompt_len)
    }

    /// Apaga o KV das duas sequências de uma sessão.
    pub(super) fn wipe_kv(&mut self, session: &Session) {
        if let Some(gen) = session.gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        // A sequência de geração pode ter sobrado de um turno anterior que não
        // passou por aqui; limpar as duas é barato e não deixa célula presa.
        let _ = self
            .engine
            .kv_remove(self.gen_seq_of(session.prompt_seq), 0, -1);
        let _ = self.engine.kv_remove(session.prompt_seq, 0, -1);
    }

    /// Grava o prefixo retido de uma sessão como um snapshot de KV em disco,
    /// como o `--slot-save-path` do llama-server.
    ///
    /// **Sempre salva `prompt_seq`, nunca `gen_seq`.** A cauda volátil da
    /// geração some no fim do turno e não faz parte de um snapshot; o que fica
    /// é o prompt estável que o próximo turno reaproveita. O `.bin` do
    /// llama.cpp serializa as células da sequência com suas posições, e o
    /// sidecar `.json` grava a identidade do modelo e da fatia para a
    /// restauração recusar um estado incompatível.
    pub(super) fn save_slot(
        &self,
        sid: &str,
        dir: &std::path::Path,
        name: &str,
    ) -> Result<SlotSidecar> {
        let name = sanitize_slot_name(name)?;
        let session = self
            .sessions
            .get(sid)
            .ok_or_else(|| anyhow::anyhow!("sessão '{sid}' desconhecida"))?;
        if session.prompt_len == 0 {
            bail!("sessão '{sid}' não tem prompt retido para snapshot");
        }
        let seq = session.prompt_seq;
        let retained = session.prompt_len;
        // Os tokens reais do prompt retido, quando este peer os conhece (a
        // cabeça). Vazio nos hops que só receberam hidden state.
        let real_tokens: Vec<i32> = session
            .prompt_tokens
            .get(..retained as usize)
            .map(<[i32]>::to_vec)
            .unwrap_or_default();
        // Cabeçalho de tokens do `.bin`: os reais quando existem, senão um vetor
        // do tamanho certo - o llama.cpp nunca confere o cabeçalho contra as
        // células, ele é consultivo.
        let header_tokens: Vec<i32> = if real_tokens.is_empty() {
            vec![0i32; retained as usize]
        } else {
            real_tokens.clone()
        };
        std::fs::create_dir_all(dir)
            .with_context(|| format!("criar diretório de snapshots {}", dir.display()))?;
        let bin = slot_bin_path(dir, name);
        let bin_bytes = self.engine.state_seq_save_file(&bin, seq, &header_tokens)? as u64;
        let sidecar = SlotSidecar {
            format_version: SLOT_SIDECAR_VERSION,
            model_label: self.model_label.clone(),
            model_path: self.model_path.clone(),
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            total_layers: self.total_layers,
            n_embd: self.engine.n_embd as u32,
            retained_tokens: retained,
            prompt_tokens: real_tokens,
            saved_at: chrono::Utc::now().to_rfc3339(),
            bin_bytes,
        };
        let json = serde_json::to_vec_pretty(&sidecar)?;
        std::fs::write(slot_json_path(dir, name), json)
            .with_context(|| format!("gravar sidecar de {name}"))?;
        Ok(sidecar)
    }

    /// Lê um snapshot de KV do disco para uma **nova** sessão. Recusa um
    /// snapshot de outro modelo, faixa de camadas ou largura de hidden state
    /// antes de tocar no motor; o `.bin` recusa por si só uma geometria de KV
    /// incompatível (tipo/tamanho de linha, contagem de camadas).
    pub(super) fn restore_slot(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        new_sid: &str,
    ) -> Result<SlotRestoreOutcome> {
        let name = sanitize_slot_name(name)?;
        if self.sessions.contains_key(new_sid) {
            bail!("sessão '{new_sid}' já existe; escolha outro id");
        }
        let raw = std::fs::read(slot_json_path(dir, name))
            .with_context(|| format!("ler sidecar de {name}"))?;
        let sidecar: SlotSidecar =
            serde_json::from_slice(&raw).with_context(|| format!("sidecar de {name} ilegível"))?;
        self.check_slot_compatible(&sidecar)?;

        let seq = self.alloc_seq().ok_or_else(|| {
            anyhow::anyhow!("seq pool esgotado; libere uma sessão antes de restaurar")
        })?;
        let cap = sidecar
            .retained_tokens
            .max(sidecar.prompt_tokens.len() as u32)
            .saturating_add(8) as usize;
        let bin = slot_bin_path(dir, name);
        let (_bytes, header_tokens) = match self.engine.state_seq_load_file(&bin, seq, cap) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.free_seq(seq);
                return Err(error);
            }
        };

        let mut session = Session::new(seq);
        session.prompt_len = sidecar.retained_tokens;
        session.n_past = sidecar.retained_tokens;
        session.stable_len = sidecar.retained_tokens;
        session.prompt_tokens = if sidecar.prompt_tokens.is_empty() {
            header_tokens
        } else {
            sidecar.prompt_tokens.clone()
        };
        session.retained = true;
        session.last_used = Instant::now();
        self.sessions.insert(new_sid.to_string(), session);
        Ok(SlotRestoreOutcome {
            restored_tokens: sidecar.retained_tokens,
        })
    }

    /// Confere que um sidecar descreve o mesmo modelo e a mesma fatia que este
    /// peer serve. Não entra `cache_type_*`: o `.bin` já recusa uma geometria
    /// de KV divergente com o tipo e o tamanho de linha por camada.
    pub(super) fn check_slot_compatible(&self, sidecar: &SlotSidecar) -> Result<()> {
        if sidecar.format_version != SLOT_SIDECAR_VERSION {
            bail!(
                "sidecar de versão {} incompatível (esperado {SLOT_SIDECAR_VERSION})",
                sidecar.format_version
            );
        }
        let mismatch = |campo: &str, esperado: String, achou: String| {
            anyhow::anyhow!("snapshot de KV incompatível: {campo} {achou} != {esperado} deste peer")
        };
        if sidecar.model_label != self.model_label {
            return Err(mismatch(
                "model_label",
                self.model_label.clone(),
                sidecar.model_label.clone(),
            ));
        }
        if (sidecar.start_layer, sidecar.end_layer, sidecar.total_layers)
            != (self.start_layer, self.end_layer, self.total_layers)
        {
            return Err(mismatch(
                "faixa de camadas",
                format!(
                    "{}-{}/{}",
                    self.start_layer, self.end_layer, self.total_layers
                ),
                format!(
                    "{}-{}/{}",
                    sidecar.start_layer, sidecar.end_layer, sidecar.total_layers
                ),
            ));
        }
        if sidecar.n_embd != self.engine.n_embd as u32 {
            return Err(mismatch(
                "n_embd",
                (self.engine.n_embd as u32).to_string(),
                sidecar.n_embd.to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn reap_idle(&mut self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(SESSION_IDLE_TIMEOUT_SECS))
            .unwrap_or_else(Instant::now);
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.last_used < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        let count = stale.len();
        for sid in stale {
            if let Some(mut s) = self.sessions.remove(&sid) {
                // A timeout is an aborted stream. Clear its byte tail before
                // returning the sequence slot so it cannot affect a later
                // request that reuses the slot.
                let _ = s.finish_token_stream();
                info!(
                    "reclaiming idle session {}… (seq={})",
                    &sid[..8.min(sid.len())],
                    s.prompt_seq
                );
                // Limpeza total das sequências, que todo modelo aceita.
                self.wipe_kv(&s);
                self.free_seq(s.prompt_seq);
            }
        }
        if count > 0 {
            self.signal_slot_freed();
        }
        count
    }

    pub(super) fn get_or_create(&mut self, sid: &str, reset: bool) -> Result<&mut Session> {
        if reset {
            if let Some(mut old) = self.sessions.remove(sid) {
                let _ = old.finish_token_stream();
                self.wipe_kv(&old);
                self.free_seq(old.prompt_seq);
            }
            let seq = self.alloc_seq().ok_or_else(|| {
                anyhow::anyhow!(
                    "seq pool esgotado (max {} sessões simultâneas)",
                    self.max_sessions
                )
            })?;
            self.sessions.insert(sid.to_string(), Session::new(seq));
        }
        self.sessions.get_mut(sid).ok_or_else(|| {
            anyhow::anyhow!("sessão '{}' desconhecida — envie reset=true antes", sid)
        })
    }

    /// Encerra a sessão de vez: as duas sequências são apagadas e o slot volta
    /// ao pool. O fim normal de um turno **não** passa por aqui - passa por
    /// `retain_prompt`, que guarda o prompt. Aqui chegam a liberação explícita
    /// do gateway, o cancelamento e a limpeza de erro.
    pub(super) fn finish(&mut self, sid: &str) {
        if let Some(mut s) = self.sessions.remove(sid) {
            // The output path flushes before the caller reaches this method;
            // the cleanup cases deliberately discard the result after clearing
            // it so no tail can leak into the next session.
            let _ = s.finish_token_stream();
            self.wipe_kv(&s);
            self.free_seq(s.prompt_seq);
            self.signal_slot_freed();
        }
    }

    /// Acorda o `run_scheduler` para ele reavaliar um prefill de sessão nova
    /// que ficou esperando slot.
    pub(super) fn signal_slot_freed(&self) {
        self.slot_freed.notify_one();
    }
}

// ── Start ─────────────────────────────────────────────────────────────────────

/// Load the model and serve the inference HTTP server.
///
/// Blocks until the server exits or `stop_rx` fires.
pub async fn run(
    cfg: NodeConfig,
    mut stop_rx: tokio::sync::watch::Receiver<u64>,
    generation: u64,
    mtp_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    batching_ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tls: Option<crate::serve::TlsServer>,
    auth: Option<crate::auth::AuthState>,
    model_cache: Arc<ModelCache>,
    reuse_weights: bool,
    loads: Arc<crate::pipeline::LoadTracker>,
    load_node_id: String,
) -> Result<()> {
    // Derivado, não copiado do pedido: o caminho de linha de comando nunca
    // preenche `no_embeddings`, então sem isto o nó da última fatia instalava o
    // callback de captura e pagava ~8 ms por token para capturar um residual
    // que ninguém lê. Ver `NodeConfig::needs_residual_export`.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    // A porta primeiro, o modelo depois. Ligar o socket custa microssegundos e
    // e a unica coisa aqui que falha por causa do mundo externo; carregar o
    // modelo custa minutos e gigabytes de VRAM. Na ordem inversa, cada uma das
    // 10 tentativas do supervisor carregava o modelo inteiro para descobrir no
    // fim que a porta estava tomada - o log mostrava "llama context ready"
    // seguido de "Address already in use", dez vezes, com a VRAM presa em
    // cargas descartadas.
    let port = cfg.inference_port;
    let listener = bind_dual_stack(port).await?;
    info!("porta {port} ligada; carregando o modelo");

    let model_info = match crate::gguf::read_info(std::path::Path::new(&cfg.model_path)) {
        Ok(info) => Some(info),
        Err(error) => {
            warn!("não foi possível ler metadados GGUF antes da carga: {error}");
            None
        }
    };
    let inference_mode = configured_inference_mode(cfg.inference_mode)?;
    let inference_path = crate::executor::select(inference_mode, model_info.as_ref())?;
    info!(
        "caminho de inferência selecionado: {} (modo {:?})",
        inference_path.as_str(),
        inference_mode
    );

    // The terminal MoE hop also needs the captured boundary residual so its
    // own executor can run before the response is sampled. Dense single-peer
    // contexts keep the old fast path without installing the callback.
    let need_embeddings = cfg.needs_residual_export() || inference_path.is_moe();
    // `rs_seq_window` (maior largura de lote especulativo, dos dois mecanismos
    // lookup e MTP) é computado dentro de `engine_config_from_node` agora -
    // ver o doc de `EngineConfig::rs_seq_window`.
    let moe_plan = if inference_path.is_moe() {
        Some(
            crate::moe::runtime::MoeRuntime::external_executor_plan(&cfg.model_path).map_err(
                |error| anyhow::anyhow!("planejamento de ownership MoE falhou: {error}"),
            )?,
        )
    } else {
        None
    };
    let moe_external_executor = moe_plan.as_ref().is_some_and(|plan| plan.enabled());
    if let Some(plan) = moe_plan.as_ref().filter(|plan| !plan.enabled()) {
        let reason = plan
            .reason
            .as_deref()
            .unwrap_or("nenhuma camada tem executor externo disponível");
        return Err(anyhow::anyhow!(
            "combinação MoE não suportada antes da carga do modelo: dtypes GGML {:?}; {reason}; o caminho MoE exige um executor externo e não usará o llama.cpp nativo",
            plan.dtypes
        ));
    }
    let engine_cfg = crate::runtime::engine_config_from_node(
        &cfg,
        need_embeddings,
        moe_external_executor,
        moe_plan
            .as_ref()
            .map(|plan| plan.layers.clone())
            .unwrap_or_default(),
    )?;

    let model_path = cfg.model_path.clone();
    let mut engine = tokio::task::spawn_blocking(move || {
        LlmEngine::load_with_cache(&model_path, &engine_cfg, &model_cache, reuse_weights)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    // Só agora se sabe se a cabeça `nextn` existe: depende do GGUF, não da
    // configuração. Ver `Network::mtp_ready`.
    mtp_ready.store(engine.mtp_ready(), std::sync::atomic::Ordering::Relaxed);

    let moe = if inference_path.is_moe() && moe_external_executor {
        let info = model_info.as_ref().ok_or_else(|| {
            anyhow::anyhow!("caminho MoE exige metadados GGUF para preparar o router")
        })?;
        let experts = info
            .expert_count
            .ok_or_else(|| anyhow::anyhow!("caminho MoE exige expert_count no GGUF"))?;
        let top_k = info.expert_used_count.unwrap_or(1).max(1) as usize;
        let weights_bytes = std::fs::metadata(&cfg.model_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let cuda_compute_bytes = engine.cuda_compute_bytes();
        Some(Arc::new(
            crate::moe::runtime::MoeRuntime::open(
                &cfg.model_path,
                cfg.model_label.clone(),
                experts,
                top_k,
                weights_bytes,
                cuda_compute_bytes,
            )
            .map_err(|error| anyhow::anyhow!("runtime MoE indisponível: {error}"))?,
        ))
    } else {
        None
    };

    if let Some(runtime) = moe.as_ref() {
        let telemetry = runtime.executor_telemetry();
        if telemetry.executor == crate::moe::runtime::MoeExecutorKind::CpuReference {
            tracing::warn!(
                model = %cfg.model_label,
                requested_n_gpu_layers = cfg.n_gpu_layers,
                moe_executor = telemetry.executor.as_str(),
                dtypes = ?telemetry.dtypes,
                reason = ?telemetry.reason,
                "MoE está usando CPUReference; n_gpu_layers não coloca os experts na GPU"
            );
        } else {
            tracing::info!(
                model = %cfg.model_label,
                requested_n_gpu_layers = cfg.n_gpu_layers,
                moe_executor = telemetry.executor.as_str(),
                dtypes = ?telemetry.dtypes,
                "executor MoE CUDA selecionado; experts serão admitidos no cache residente"
            );
        }
        runtime.start_device_warmup();
        engine.set_moe_runtime(Arc::clone(runtime))?;
    }
    let moe_fallback_reason = moe_plan
        .as_ref()
        .filter(|plan| !plan.enabled())
        .and_then(|plan| plan.reason.clone());
    let mut state_inner = InferState::new(engine, &cfg, inference_path, moe, moe_fallback_reason);
    state_inner.tls_handshake_ms = tls.as_ref().map(|t| t.last_handshake_ms.clone());
    state_inner.original_block_count = model_info
        .as_ref()
        .and_then(|info| info.original_block_count);

    // Fora do mutex do estado: o decode segura aquele lock e é de dentro dele
    // que o reduce espera a soma. Ver `handle_tp_reduce`.
    let tp_barrier = crate::collective::ReduceBarrier::new();
    let tp_size = cfg.tp.as_ref().map(|t| t.size).unwrap_or(1);
    let local_barrier_handle = crate::tp::set_local_barrier(tp_barrier.clone(), tp_size);

    let state: SharedState = Arc::new(Mutex::new(state_inner));
    let scheduler_cfg = crate::scheduler::SchedulerConfig::from_node_config(&cfg);
    // TP participates in the same canonical batch frame. The handler opens
    // one shared decode id for the whole frame, broadcasts that frame to the
    // workers and then submits all items to this local scheduler together.
    batching_ready.store(scheduler_cfg.enabled, std::sync::atomic::Ordering::Relaxed);
    let scheduler = spawn_scheduler(state.clone(), scheduler_cfg);
    let app_state = InferenceAppState {
        state,
        scheduler,
        tp_batch_gate: Arc::new(tokio::sync::Mutex::new(())),
        direct_forward_order: DirectForwardOrder::default(),
        loads,
        load_node_id,
    };
    crate::relay::install_local_inference(generation, Arc::new(app_state.clone())).await;

    if let Some(auth) = auth.clone() {
        http_client().set_auth_state(auth);
    }

    let inference_routes = Router::new()
        .route(
            "/v1/prefill",
            post(handle_prefill).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step",
            post(handle_step).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route(
            "/v1/step-stream",
            post(handle_step_stream).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
        )
        .route("/v1/session/release", post(handle_release))
        .route("/v1/session/rewind", post(handle_rewind))
        .route("/v1/session/rollback", post(handle_rollback))
        .route("/v1/session/slot", post(handle_slot));
    let inference_routes = if let Some(auth) = auth.clone() {
        inference_routes.layer(middleware::from_fn_with_state(
            auth,
            require_inference_auth,
        ))
    } else {
        inference_routes
    };
    let inference_routes = inference_routes.with_state(app_state.clone());

    // O canal persistente tem router próprio porque o estado dele carrega a
    // credencial: o middleware confere a abertura, e o handler reconfere cada
    // quadro. O limite de corpo é desligado aqui porque o corpo é a geração
    // inteira, não um quadro - cada quadro dentro do fluxo continua limitado
    // pelo enquadramento do canal.
    let link_state = LinkState {
        app: app_state.clone(),
        auth: auth.clone(),
    };
    let link_routes = Router::new().route(
        crate::link::LINK_PATH,
        post(handle_link).layer(DefaultBodyLimit::disable()),
    );
    let link_routes = if let Some(auth) = auth.clone() {
        link_routes.layer(middleware::from_fn_with_state(
            auth,
            require_inference_auth,
        ))
    } else {
        link_routes
    };
    let link_routes = link_routes.with_state(link_state);

    let tp_routes = Router::new().route(
        "/v1/tp/reduce",
        post(handle_tp_reduce).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
    );
    let tp_routes = if let Some(auth) = auth {
        tp_routes.layer(middleware::from_fn_with_state(
            auth,
            require_inference_auth,
        ))
    } else {
        tp_routes
    };
    let tp_routes = tp_routes.with_state((tp_barrier, tp_size));

    let router = inference_routes
        .merge(link_routes)
        .merge(
            Router::new()
                .route("/health", get(handle_health))
                .with_state(app_state.clone()),
        )
        .merge(tp_routes);

    info!("inference server pronto em [::]:{port}");

    // Ver a nota gêmea em `gateway::start_gateway`: sem `tcp_nodelay`, toda
    // resposta de hop sai com Nagle ligado, e um ack de resposta direta é
    // exatamente a escrita pequena que ele segura. Quem o aplica agora é o
    // `serve`, que também decide entre TLS e texto claro por conexão.
    //
    // O desligamento gracioso virou uma corrida entre o laço de accept e o
    // watch de geração: quando a geração muda, paramos de aceitar. Conexão em
    // voo termina sozinha, como antes.
    // O desligamento gracioso é sequencial de propósito: primeiro o `serve`
    // para de aceitar e encerra as conexões que restaram, só depois a VRAM é
    // devolvida. Na ordem inversa uma requisição em voo tocaria um contexto já
    // liberado.
    let resultado = crate::serve::serve_with_shutdown(listener, router, tls, async move {
        loop {
            if stop_rx.changed().await.is_err() {
                break;
            }
            if *stop_rx.borrow() != generation {
                break;
            }
        }
    })
    .await;
    crate::relay::clear_local_inference(generation).await;

    // O aquecimento é cooperativo e pode estar lendo o GGUF enquanto o
    // servidor é reconfigurado. Sinalize a parada antes de liberar o motor,
    // para que nenhuma geração antiga continue admitindo experts no device.
    if let Some(runtime) = app_state
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .moe
        .clone()
    {
        runtime.cancel_device_warmup();
    }

    if let Some(handle) = local_barrier_handle {
        crate::tp::clear_local_barrier(handle);
    }

    // Explícito, e não pela contagem do `Arc`: uma cópia esquecida em qualquer
    // canto — foi o que as conexões desgarradas faziam — adiava o `Drop` para
    // sempre, e o `undeploy` respondia "pronto" com o modelo ainda na GPU.
    // Depois do drenar acima ninguém mais decodifica, então isto é seguro.
    // `into_inner` no envenenado: um handler que entrou em pânico deixa o
    // mutex marcado, e recusar a liberação por causa disso seria justamente
    // deixar a VRAM presa no caso em que ela mais precisa voltar.
    app_state
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .engine
        .unload();
    info!("memória do modelo devolvida na porta {port}");

    resultado?;
    batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

// ── Socket ────────────────────────────────────────────────────────────────────

/// Um listener que atende IPv6 e IPv4 no mesmo socket, caindo para IPv4 puro
/// quando não há IPv6 na máquina.
///
/// `IPV6_V6ONLY` só vale se for desligado **antes** do `bind`, e é por isso que
/// o socket é montado à mão em vez de sair do `TcpListener::bind`. O caminho
/// antigo chamava `setsockopt` depois do bind: no Linux passava despercebido
/// porque `net.ipv6.bindv6only` já é 0 por padrão, mas no Windows o padrão é 1
/// e o servidor de inferência simplesmente não aceitaria nenhum peer IPv4 -
/// que é como todo o cluster da LAN fala.
///
/// `SO_REUSEADDR` continua fora de propósito: no Windows ele deixa outro
/// processo tomar uma porta já ligada, e é justamente o "address already in
/// use" que denuncia um servidor velho ainda de pé.
pub(crate) fn listener_dual_stack(port: u16) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    // Backlog: o mesmo que o `TcpListener::bind` da std usa.
    const BACKLOG: i32 = 128;

    let v6 = (|| -> std::io::Result<std::net::TcpListener> {
        let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        sock.set_only_v6(false)?;
        // O `TcpListener::bind` da std pede isto sozinho no Unix; um socket cru
        // nao. Sem a flag, cada conexao em TIME_WAIT nesta porta - e o gateway
        // deixa varias, sondando `/health` e mandando inferencia - impede o
        // proximo servidor de ligar por ate um minuto, que e mais que as dez
        // tentativas do supervisor. Toda implantacao de versao nova morria ai.
        sock.set_reuse_address(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.listen(BACKLOG)?;
        Ok(sock.into())
    })();

    match v6 {
        Ok(l) => Ok(l),
        Err(e) => {
            // Sem IPv6 na máquina: IPv4 puro ainda serve o cluster inteiro.
            tracing::debug!("bind IPv6 em [::]:{port} falhou ({e}); tentando IPv4");
            let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            sock.set_reuse_address(true)?;
            sock.bind(&SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port).into())?;
            sock.listen(BACKLOG)?;
            Ok(sock.into())
        }
    }
}

pub(crate) async fn bind_dual_stack(port: u16) -> Result<TcpListener> {
    let std_sock = listener_dual_stack(port)?;
    std_sock.set_nonblocking(true)?;
    Ok(TcpListener::from_std(std_sock)?)
}

// ── Error helpers ─────────────────────────────────────────────────────────────

/// One shared outbound client cache, used only when relaying to this node's own
/// next hop (`PipelineStep::supports(CAP_RELAY_CHAIN)`) — this server has no
/// other reason to make outbound HTTP calls. Same lazy-static pattern as
/// `webui::catalog_cache`.
///
/// É um `PeerClients` e não um `reqwest::Client` porque o próximo salto é um
/// peer: com o cluster em TLS ele precisa do certificado daquele node_id
/// fixado. O esquema global (`peer_client::tls_ativo`) é quem diz se há TLS -
/// este processo é o mesmo do nó, então a política já está aplicada aqui.
pub(crate) fn http_client() -> &'static crate::peer_client::PeerClients {
    static CLIENT: std::sync::OnceLock<crate::peer_client::PeerClients> =
        std::sync::OnceLock::new();
    let c = CLIENT
        .get_or_init(|| crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)));
    // A cada uso, não só na criação: o `OnceLock` nasce uma vez e o operador
    // pode virar a flag depois. `set_tls` só faz trabalho quando o valor muda,
    // então isto custa uma leitura atômica no caminho comum.
    c.set_tls(crate::peer_client::tls_ativo());
    c
}

/// Duração do último handshake TLS que este servidor aceitou, em ms.
///
/// Zero quando não há TLS, e zero também quando o salto reaproveitou uma
/// conexão do pool - que é a maioria deles. Ver o doc de
/// `SentFrame::tls_handshake_ms` sobre por que isso é a verdade e não uma
/// falha de medição.
pub(super) fn last_handshake_ms(state: &SharedState) -> u64 {
    let Ok(state) = lock_inference_state(state) else {
        warn!("não foi possível ler o último handshake: mutex do estado envenenado");
        return 0;
    };
    state
        .tls_handshake_ms
        .as_ref()
        .map(|v| v.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0)
}

pub(super) async fn handle_prefill_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/prefill", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_prefill(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

pub(super) async fn handle_step_batch(
    app: InferenceAppState,
    headers: axum::http::HeaderMap,
    batch: wire::BatchFrame,
) -> Response {
    let tp_active = app
        .state
        .lock()
        .map(|state| state.tp.as_ref().is_some_and(|tp| tp.size > 1))
        .unwrap_or(false);
    let _tp_batch_guard = if tp_active {
        Some(app.tp_batch_gate.lock().await)
    } else {
        None
    };
    let batch_body = match wire::encode_batch(&batch) {
        Ok(body) => Bytes::from(body),
        Err(error) => return err400(format!("batch inválido: {error}")),
    };
    open_tp_decode(&app.state, &headers, "/v1/step", &batch_body);
    let calls = batch.items.iter().map(|item| {
        let app = app.clone();
        let mut headers = headers.clone();
        headers.insert(
            BATCH_CHILD_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
        let body = wire::encode_batch_item(&batch, item);
        async move {
            match body {
                Ok(body) => handle_step(State(app), headers, Bytes::from(body)).await,
                Err(error) => err400(format!("item batch inválido: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} falhou no processamento do batch", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("resposta batch inválida: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("resposta de item não contém exatamente um item"),
            Err(error) => return err500(format!("resposta de item inválida: {error}")),
        };
        items.push(wire::BatchPayloadItem {
            item_id: original.item_id.clone(),
            session_id: original.session_id.clone(),
            meta: decoded.meta,
            tensor: decoded.tensor,
            dtype: decoded.dtype,
        });
    }
    match wire::encode_batch_items(batch.kind, batch.batch_id, &items) {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            body,
        )
            .into_response(),
        Err(error) => err500(error),
    }
}

pub(super) fn err400(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

pub(super) fn err500(msg: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()).into_response()
}

pub(super) fn context_per_session(n_ctx_total: u32, max_sessions: u32) -> u32 {
    n_ctx_total / max_sessions.max(1)
}

// ── /health ───────────────────────────────────────────────────────────────────

pub(super) async fn handle_health(State(app): State<InferenceAppState>) -> Response {
    let scheduler = app.scheduler;
    let state = app.state;
    let slot_dir = match lock_inference_state(&state) {
        Ok(s) => s.slot_dir.clone(),
        Err(error) => return err500(error),
    };
    let slot_count = slot_dir.as_deref().map(count_slot_entries).unwrap_or(0);
    let s = match lock_inference_state(&state) {
        Ok(s) => s,
        Err(error) => return err500(error),
    };
    // Keep in sync with network.rs's `self_announce`, gateway.rs's local peer
    // entry, and the capability constants in routing/pipeline.rs.
    let mut caps = vec![
        crate::pipeline::CAP_FRAME_BATCH,
        crate::pipeline::CAP_FRAME_DTYPE_F8,
        crate::pipeline::CAP_REMOTE_TOKEN_STREAM,
        crate::pipeline::CAP_LINK_STREAM,
        "relay/v1-chain",
        "relay/v2-direct",
        "speculate/v1-lookup",
        "speculate/v1-mtp-sliced",
    ];
    if s.engine.mtp_ready() {
        caps.push("speculate/v1-mtp");
    }
    if scheduler.config.enabled {
        caps.push(crate::pipeline::CAP_FRAME_BATCH);
    }
    if s.slot_dir.is_some() {
        caps.push(crate::pipeline::CAP_SLOT_PERSIST);
    }
    let n_ctx_total = s.engine.n_ctx();
    let n_ctx_per_session = context_per_session(n_ctx_total, s.max_sessions);
    let moe_telemetry = s.moe.as_ref().map(|runtime| runtime.executor_telemetry());
    let (moe_requested, moe_executor, moe_executor_dtypes, moe_executor_reason) =
        moe_health_summary(
            s.inference_path,
            moe_telemetry.as_ref(),
            s.moe_fallback_reason.as_deref(),
        );
    Json(json!({
        "status": "ok",
        "model_loaded": true,
        "start_layer": s.start_layer,
        "end_layer": s.end_layer,
        "original_block_count": s.original_block_count,
        "selected_path": s.inference_path.as_str(),
        "moe_requested": moe_requested,
        "moe_executor": moe_executor,
        "moe_executor_dtypes": moe_executor_dtypes,
        "moe_executor_reason": moe_executor_reason,
        "hidden_dim": s.engine.n_embd,
        "vocab_size": s.engine.n_vocab,
        "max_sessions": s.max_sessions,
        // Duas sequências de KV por sessão: a do prompt e a da geração. Ver
        // `llm::kv_seq_budget`, que explica por que e o que custa.
        "kv_seq_max": crate::llm::kv_seq_budget(s.max_sessions),
        "sessions_in_use": s.sessions.len(),
        "free_slots": s.seq_pool.len(),
        // Sessões paradas no fim do prompt, esperando o turno seguinte. É o
        // cache de prefixo visto do lado que o executa.
        "sessions_retained": s.sessions.values().filter(|x| x.retained).count(),
        "prefix_tokens_retained": s
            .sessions
            .values()
            .filter(|x| x.retained)
            .map(|x| x.prompt_len as u64)
            .sum::<u64>(),
        // Diagnóstico do motor: `kv_can_shift` é o K-shift do llama.cpp, e
        // **não** governa remoção parcial de KV. O reuso de prefixo deste
        // projeto não depende de nenhum dos dois - ver `Session`.
        "kv_can_shift": s.engine.kv_can_shift(),
        "n_ctx_total": n_ctx_total,
        // O engine reserva a janela pública para cada sessão. Se o fit reduzir
        // o total, a capacidade por sessão é a divisão efetiva, não o total
        // bruto que o llama.cpp devolve.
        "n_ctx_per_session": n_ctx_per_session,
        "offload_kqv": s.engine.offload_kqv(),
        "cache_type_k": s.engine.cache_type_k().as_str(),
        "cache_type_v": s.engine.cache_type_v().as_str(),
        "fit_memory_applied": s.engine.fit_memory_applied(),
        "n_gpu_layers_effective": s.engine.effective_n_gpu_layers(),
        "n_ubatch_effective": s.engine.effective_n_ubatch(),
        // Com que parâmetros de llama.cpp este contexto subiu, para o
        // operador comparar o nó com uma linha de comando do `llama-cli`.
        // Lido do contexto vivo, não do `EngineConfig` - ver
        // `LlmEngine::load_params`.
        "load_params": s.engine.load_params(),
        "profile_sync_enabled": crate::llm::profile_backend_sync_enabled(),
        // Onde as camadas e o cache KV ficaram de verdade, direto do
        // llama.cpp. `null` quando a ponte não respondeu - a interface diz
        // que não sabe, em vez de derivar um mapa de `n_gpu_layers`, que
        // erraria sempre que um override de buffer type entrasse.
        "placement": s.engine.placement(),
        "context_memory": s.engine.context_memory(),
        "inference_frame_capabilities": caps,
        "scheduler": scheduler.stats.json(&scheduler.config),
        // Diagnóstico puro — `llama_memory_can_shift`, o K-shift do
        // `--ctx-shift` de referência do llama.cpp. Não decide mais se a
        // especulação roda (ver o doc de `should_draft`); mantido aqui
        // porque é informação real do motor.
        "kv_can_shift": s.engine.kv_can_shift(),
        // A cabeça de rascunho `nextn` foi mesmo anexada? Depende do GGUF
        // carregado (fatia não-final não tem o bloco) e do interruptor do
        // operador, então nem a configuração nem o nome do arquivo respondem.
        "mtp_ready": s.engine.mtp_ready(),
        "mtp_allocation": s.engine.mtp_allocation(),
        "mtp_unavailable_reason": s.engine.mtp_unavailable_reason(),
        // Snapshots de KV cache (`--slot-save-path`): ligado quando há um
        // diretório configurado. `slot_count` conta sidecars sem abrir o
        // JSON, para não segurar o lock do motor em leitura de disco.
        "slot_persist_enabled": s.slot_dir.is_some(),
        "slot_count": slot_count,
    }))
    .into_response()
}

// ── /v1/tp/reduce ────────────────────────────────────────────────────────────

/// Uma parcial de um reduce de tensor parallelism (caminho C). Cada rank do
/// grupo faz um POST desta com a própria parcial e o mesmo `barrier_id`; o
/// endpoint (no primary/rank 0) acumula e devolve a soma quando todos chegam.
/// **Nunca** toca no mutex do `InferState`, e isso é a correção, não estilo.
///
/// `handle_prefill` segura esse mutex durante o decode inteiro, e é de dentro
/// desse decode que o callback do reduce bloqueia esperando a soma. Enquanto
/// este handler pegava o mesmo mutex para ler a barreira, a parcial que chegava
/// - do próprio rank 0 ou do worker - ficava presa esperando um lock que só
/// seria liberado quando o decode terminasse; e o decode só terminaria com a
/// soma que estava presa na porta. Deadlock exato de 18/08: os dois ranks
/// logavam o primeiro reduce, com 0,2 ms de diferença, e o pipeline parava ali.
///
/// A barreira vive fora do mutex (`ReduceBarrier` já é `Clone` e sincronizada
/// por dentro), então o caminho da coletiva nunca disputa com o decode.
///
/// A parcial viaja como **f32 cru em little-endian**, não como JSON. Um float
/// de 4 bytes vira ~12 caracteres em JSON, e a parcial de um prefill de 100
/// tokens tem 410 mil floats: ~5 MB de texto por reduce, 64 vezes por decode.
/// Isso estourava o limite de corpo do axum (413) e, quando cabia, gastava mais
/// tempo formatando número que somando. Cru é 4x menor e sem parse.
///
/// `barrier_id` e `rank` vão em cabeçalho, que é onde metadado de tamanho fixo
/// pertence.
pub(crate) async fn handle_tp_reduce(
    State((barrier, size)): State<(crate::collective::ReduceBarrier, u32)>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let barrier_id = match headers
        .get(crate::tp::TP_BARRIER_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) => v.to_string(),
        None => {
            return err400(format!(
                "falta o cabeçalho {}",
                crate::tp::TP_BARRIER_HEADER
            ))
        }
    };
    let rank: u32 = match headers
        .get(crate::tp::TP_RANK_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
    {
        Some(v) => v,
        None => return err400(format!("falta o cabeçalho {}", crate::tp::TP_RANK_HEADER)),
    };
    if body.len() % 4 != 0 {
        return err400(format!(
            "parcial de {} bytes não é múltiplo de 4",
            body.len()
        ));
    }
    let data: Vec<f32> = body
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    match barrier.contribute(&barrier_id, rank, size, data) {
        Ok(rx) => match rx.await {
            Ok(sum) => {
                let mut out = Vec::with_capacity(sum.len() * 4);
                for v in &sum {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                (
                    [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                    out,
                )
                    .into_response()
            }
            Err(e) => err500(format!("barreira abandonada: {e}")),
        },
        Err(e) => err400(e),
    }
}

/// Repassa uma chamada de ciclo de vida de sessão aos demais ranks do grupo.
///
/// O corpo é remontado (não é um quadro binário como o decode), e o cabeçalho
/// de réplica evita fan-out infinito: quem recebe marcado não repassa.
pub(super) fn replicate_session_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    rota: &str,
    session_id: &str,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // já é a réplica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar sessão: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let corpo = json!({ "session_id": session_id }).to_string().into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        rota,
        "session",
        corpo,
        Some("application/json"),
    );
}

pub(super) fn replicate_session_rewind(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    session_id: &str,
    n_tokens: u32,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return;
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar rewind: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let body = json!({ "session_id": session_id, "n_tokens": n_tokens })
        .to_string()
        .into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        "/v1/session/rewind",
        "session",
        body,
        Some("application/json"),
    );
}

// ── /v1/session/slot ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct SlotRequest {
    /// `"save"`, `"restore"` ou `"erase"`.
    pub(super) action: String,
    /// Sessão viva a salvar (`save`). Ignorado em `restore`/`erase`.
    #[serde(default)]
    pub(super) session_id: String,
    /// Nome do snapshot em disco (um componente de caminho seguro).
    pub(super) slot_name: String,
    /// Nova sessão a criar em `restore`. Obrigatório nessa ação.
    #[serde(default)]
    pub(super) dest_session_id: Option<String>,
}

/// `POST /v1/session/slot` - grava, lê ou apaga um snapshot de KV cache no
/// diretório local (`--slot-save-path`). O gateway compõe a ação de
/// implantação chamando esta rota em cada hop; ver `gateway::snapshot_deployment`.
pub(super) async fn handle_slot(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SlotRequest>,
) -> Response {
    // TP: rank 0 repassa a mesma ação aos workers, que servem a mesma faixa e
    // gravam a própria cópia no disco deles. Ver `replicate_session_call`.
    replicate_slot_call(&app.state, &headers, &req);

    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let Some(dir) = state.slot_dir.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            "snapshots de KV desligados neste peer (defina slot_save_path)",
        )
            .into_response();
    };

    match req.action.as_str() {
        "save" => match state.save_slot(&req.session_id, &dir, &req.slot_name) {
            Ok(sidecar) => Json(json!({
                "saved": true,
                "slot_name": req.slot_name,
                "retained_tokens": sidecar.retained_tokens,
                "bin_bytes": sidecar.bin_bytes,
            }))
            .into_response(),
            Err(error) => err400(error),
        },
        "restore" => {
            let Some(dest) = req.dest_session_id.as_deref().filter(|d| !d.is_empty()) else {
                return err400("restore exige dest_session_id");
            };
            match state.restore_slot(&dir, &req.slot_name, dest) {
                Ok(outcome) => Json(json!({
                    "restored": true,
                    "session_id": dest,
                    "restored_tokens": outcome.restored_tokens,
                }))
                .into_response(),
                Err(error) => err400(error),
            }
        }
        "erase" => {
            drop(state);
            match erase_slot(&dir, &req.slot_name) {
                Ok(removed) => Json(json!({ "erased": removed })).into_response(),
                Err(error) => err400(error),
            }
        }
        other => err400(format!("ação de snapshot desconhecida: {other}")),
    }
}

pub(super) fn replicate_slot_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    req: &SlotRequest,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // já é a réplica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível replicar snapshot: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let body = json!({
        "action": req.action,
        "session_id": req.session_id,
        "slot_name": req.slot_name,
        "dest_session_id": req.dest_session_id,
    })
    .to_string()
    .into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        "/v1/session/slot",
        "session",
        body,
        Some("application/json"),
    );
}

// ── /v1/session/release ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct ReleaseRequest {
    pub(super) session_id: String,
}

pub(super) async fn handle_release(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ReleaseRequest>,
) -> Response {
    // A réplica não encerra sessão por conta própria (ver `tp::is_replica`),
    // então quem a libera é este release - que o gateway manda **só ao rank 0**,
    // porque o worker não é hop do pipeline. Sem repassar, a sessão do worker
    // fica segurando o slot para sempre e a requisição seguinte morre com
    // "seq pool esgotado (max 1 sessões simultâneas)" (18/08).
    replicate_session_call(&app.state, &headers, "/v1/session/release", &req.session_id);
    let mut s = match lock_inference_state(&app.state) {
        Ok(s) => s,
        Err(error) => return err500(error),
    };
    let existed = s.sessions.contains_key(&req.session_id);
    s.finish(&req.session_id);
    let free = s.seq_pool.len();
    let max = s.max_sessions;
    drop(s);
    Json(json!({
        "released": existed,
        "free_slots": free,
        "max_sessions": max,
    }))
    .into_response()
}

// ── /v1/session/rewind ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct RewindRequest {
    pub(super) session_id: String,
    pub(super) n_tokens: u32,
}

/// Confirma que a sessão ficou parada no fim do prompt, pronta para o turno
/// seguinte.
///
/// **Não recua nada.** A geração deste turno correu numa sequência de KV
/// separada (ver `Session`), então reter o prompt é apagar essa sequência
/// inteira - remoção total, que todo motor aceita. Antes daqui saía o
/// `"este modelo não aceita recuar a sessão para o prompt"` que descartava o
/// prefixo em 75 das 176 requisições de um dia de produção, e o
/// `"sessão desconhecida"` de quando o EOS destruía a sessão antes deste
/// pedido chegar.
///
/// Idempotente por construção: o fim normal do turno já reteve, e chamar de
/// novo confere o mesmo estado. Só divergência real de contagem é recusa.
pub(super) async fn handle_rewind(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RewindRequest>,
) -> Response {
    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let Some(prompt_len) = state
        .sessions
        .get(&req.session_id)
        .map(|session| session.prompt_len)
    else {
        return Json(json!({ "rewound": false, "reason": "sessão desconhecida" })).into_response();
    };
    if req.n_tokens != prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!(
                "prompt do gateway não bate com o desta sessão: {} != {prompt_len}",
                req.n_tokens
            ),
        }))
        .into_response();
    }
    let Some(retained) = state.retain_prompt(&req.session_id) else {
        return Json(json!({
            "rewound": false,
            "reason": "sessão sem prompt para reter",
        }))
        .into_response();
    };
    drop(state);
    replicate_session_rewind(&app.state, &headers, &req.session_id, req.n_tokens);
    Json(json!({ "rewound": true, "n_past": retained })).into_response()
}

// ── /v1/session/rollback ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct RollbackRequest {
    pub(super) session_id: String,
    /// How many of the batch's positions are confirmed valid — see
    /// `rollback_from`'s doc comment.
    pub(super) accept_count: u32,
    /// How many positions the speculative decode actually wrote.
    pub(super) batch_width: u32,
}

/// Discard the KV cache entries for whatever part of a speculative batch
/// didn't get confirmed. Fanned out by the gateway to *every* hop in the
/// plan after each speculative round (`gateway::rollback_sessions`, mirroring
/// `release_sessions`'s existing shape) — every hop decoded the full batch
/// into its own cache, so every hop needs this, not just the tail. Same
/// best-effort ethos as `/v1/session/release`: an unknown or already-reaped
/// `session_id` is not an error, just a no-op.
pub(super) async fn handle_rollback(
    State(app): State<InferenceAppState>,
    Json(req): Json<RollbackRequest>,
) -> Response {
    match rollback_session(
        &app.state,
        &req.session_id,
        req.accept_count,
        req.batch_width,
    ) {
        Ok(true) => Json(json!({"rolled_back": true})).into_response(),
        Ok(false) => Json(json!({"rolled_back": false})).into_response(),
        Err(error) => err500(error),
    }
}

pub(super) fn rollback_session(
    state: &SharedState,
    session_id: &str,
    accept_count: u32,
    batch_width: u32,
) -> Result<bool> {
    let mut s = lock_inference_state(state)?;
    let Some((seq_id, n_past_after_batch)) = s
        .sessions
        .get(session_id)
        .map(|session| (session.seq_id(), session.n_past))
    else {
        return Ok(false);
    };
    let new_n_past = rollback_from(n_past_after_batch, batch_width, accept_count);
    if new_n_past < n_past_after_batch && !s.engine.kv_remove(seq_id, new_n_past as i32, -1) {
        return Ok(false);
    }
    if let Some(session) = s.sessions.get_mut(session_id) {
        session.n_past = new_n_past;
    }
    Ok(true)
}

// ── /v1/prefill ───────────────────────────────────────────────────────────────

/// Abre o decode nos **dois** papéis e, no rank 0, põe os workers para rodar o
/// mesmo lote.
///
/// TP só funciona em lockstep: todo rank percorre o mesmo grafo sobre os mesmos
/// tokens, e as parciais se encontram na barreira. O gateway só fala com o rank
/// 0 (o worker é excluído do pipeline de propósito), então é aqui que os outros
/// ranks entram no decode. Sem isto o rank 0 postava a parcial e esperava para
/// sempre - o travamento de 18/08.
///
/// O quadro é repassado byte a byte, com o id do decode no cabeçalho. Quem
/// recebe o cabeçalho é réplica: abre o decode com o id recebido e **não**
/// retransmite, senão o grupo se multiplicaria a cada salto.
pub(super) fn open_tp_decode(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    rota: &str,
    body: &Bytes,
) {
    if headers.get(BATCH_CHILD_HEADER).is_some() {
        return;
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("não foi possível abrir decode TP: mutex do estado envenenado");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1) else {
        return;
    };
    if let Some(id) = headers
        .get(crate::tp::TP_DECODE_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        crate::tp::begin_replica_decode(id);
        return;
    }
    if tp.rank != 0 {
        // Worker recebendo quadro sem cabeçalho: veio de fora do grupo. Abrir
        // um decode aqui produziria reduces com id que ninguém mais conhece.
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    crate::tp::begin_decode(&id);
    crate::tp::broadcast_decode(&tp.members, rota, &id, body.to_vec());
}

/// A sub-faixa que o remetente mandou executar, conferida contra a fatia que
/// este nó carregou.
///
/// Quem manda decide qual pedaço roda; este nó só confere que consegue rodar
/// exatamente aquilo. O que ele **não** faz é ajustar em silêncio: rodar a
/// fatia inteira quando pediram um pedaço é exatamente o modo de falha que
/// devolve resposta errada sem erro nenhum.
///
/// Devolve o índice do bloco local onde a decodificação deve parar, ou `None`
/// quando o pedido cobre a fatia até o fim (o caso comum, grafo inteiro).
pub(super) fn resolve_exec_range(
    meta: &Value,
    slice_start: u32,
    slice_end: u32,
) -> std::result::Result<Option<u32>, String> {
    let exec_start = meta.get("exec_start").and_then(|v| v.as_u64());
    let exec_end = meta.get("exec_end").and_then(|v| v.as_u64());
    let (Some(exec_start), Some(exec_end)) = (exec_start, exec_end) else {
        // Sem instrução não há o que conferir: roda a fatia inteira, que é o
        // contrato de quem só carrega uma faixa e é dono dela inteira.
        return Ok(None);
    };
    let (exec_start, exec_end) = (exec_start as u32, exec_end as u32);
    if exec_end < exec_start {
        return Err(format!("sub-faixa invertida: L{exec_start}-{exec_end}"));
    }
    if exec_start < slice_start || exec_end > slice_end {
        return Err(format!(
            "sub-faixa L{exec_start}-{exec_end} sai da fatia carregada L{slice_start}-{slice_end}"
        ));
    }
    if exec_start > slice_start {
        // O limite que o captain conhece: o estado oculto entra pelo embedding
        // do llama.cpp, que sempre cai no bloco 0 da fatia. Começar acima da
        // camada inicial exigiria pular blocos da cabeça, e o motor não sabe
        // fazer isso hoje. Recusar alto é o contrato; refazer L{slice_start}-
        // L{exec_start} em silêncio seria a resposta errada sem erro.
        return Err(format!(
            "entrar no meio da faixa ainda não é possível: pediram começar em L{exec_start}, \
             mas esta fatia começa em L{slice_start} e o motor sempre entra pelo primeiro bloco. \
             Mande a sub-faixa a partir de L{slice_start}, ou dê esta parte a um peer que comece em L{exec_start}"
        ));
    }
    if exec_end == slice_end {
        return Ok(None);
    }
    Ok(Some(exec_end - slice_start))
}
