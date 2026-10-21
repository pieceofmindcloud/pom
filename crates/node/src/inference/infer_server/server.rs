use super::*;

impl InferState {
    /// Records, when a request's prefill arrives, whether its generation
    /// starts inside a thinking block: thinking is on for it and the template
    /// opens the block in the prompt.
    pub(super) fn note_thinking(&mut self, sid_key: &str, thinking_enabled: Option<bool>) {
        let thinking = !self.no_thinking && thinking_enabled.unwrap_or(!self.no_thinking);
        // Sessions that are gone take their entries along.
        let sessions = &self.sessions;
        self.thinking_open
            .retain(|key, _| key == sid_key || sessions.contains_key(key));
        self.thinking_open
            .insert(sid_key.to_owned(), thinking && self.thinking_forced_open);
    }

    pub(super) fn thinking_open_at_start(&self, sid_key: &str) -> bool {
        self.thinking_open.get(sid_key).copied().unwrap_or(false)
    }

    pub(super) fn new(
        engine: LlmEngine,
        cfg: &NodeConfig,
        inference_path: crate::executor::InferencePath,
        moe: Option<Arc<crate::moe::runtime::MoeRuntime>>,
        moe_fallback_reason: Option<String>,
    ) -> Self {
        let max_sessions = cfg.max_sessions.max(1);
        let seq_pool = (0..max_sessions).collect();
        // The deployment's seed restarts the sampler's generator once, here,
        // when the model is loaded.
        crate::llm::set_sampler_seed(cfg.seed);
        // Whether this model's template opens the thinking block in the
        // prompt itself: then the generation starts inside it and no start
        // tag is ever generated for the reasoning budget to see.
        let thinking_start = engine.tokenize(crate::llm::THINKING_START_TAG, false, true);
        let thinking_end = engine.tokenize(crate::llm::THINKING_END_TAG, false, true);
        let thinking_forced_open = engine
            .apply_chat_template(&[("user", "x")], true, false)
            .is_ok_and(|prompt| prompt.trim_end().ends_with(crate::llm::THINKING_START_TAG));
        // llama.cpp's default DRY sequence breakers, as the tokens this
        // model has for them: a repeated run does not reach across one.
        let dry_breakers: Vec<i32> = crate::llm::DRY_SEQUENCE_BREAKERS
            .iter()
            .filter_map(
                |text| match engine.tokenize(text, false, false).as_slice() {
                    [token] => Some(*token),
                    _ => None,
                },
            )
            .collect();
        Self {
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
                presence_penalty: cfg.presence_penalty,
                frequency_penalty: cfg.frequency_penalty,
                min_p: cfg.min_p,
                dry: crate::llm::DrySampling {
                    multiplier: cfg.dry_multiplier,
                    base: cfg.dry_base,
                    allowed_length: cfg.dry_allowed_length as usize,
                    penalty_last_n: cfg.dry_penalty_last_n as usize,
                    breakers: dry_breakers,
                },
                reasoning: crate::llm::ReasoningBudget {
                    tokens: cfg.reasoning_budget,
                    start: thinking_start,
                    end: thinking_end,
                },
            },
            thinking_forced_open,
            thinking_open: HashMap::new(),
            no_thinking: cfg.no_thinking,
            // Keeps the behavior of old callers that do not send the
            // flag. The current gateway always states it explicitly per endpoint.
            chat_template: cfg.no_thinking,
            start_layer: cfg.start_layer,
            end_layer: cfg.end_layer,
            total_layers: cfg.total_layers,
            model_label: cfg.model_label.clone(),
            deploy_id: cfg.deploy_id.clone(),
            model_path: cfg.model_path.clone(),
            slot_dir: cfg
                .slot_save_path
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(std::path::PathBuf::from),
            ram_cache: {
                // Tensor parallelism keeps one session per rank in lockstep; a
                // rank parking on its own would leave the group out of step.
                let setting = if cfg.tp.as_ref().is_some_and(|tp| tp.size > 1) {
                    CacheRamSetting::Off
                } else {
                    CacheRamSetting::from_mib(cfg.cache_ram_mib)
                };
                let host = Arc::new(HostMemory::default());
                if setting != CacheRamSetting::Off {
                    host.spawn_sampler();
                }
                KvRamCache::new(setting, host)
            },
            slot_freed: Arc::new(tokio::sync::Notify::new()),
            original_block_count: None, // filled in after engine load
            inference_path,
            mtp_min_probability: cfg.mtp_min_probability,
            tp: cfg.tp.clone(),
            perf: perf::SlicePerf::default(),
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
                "external MoE executor is active, but the hook is not installed; refusing to avoid running the experts twice"
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
            .map_err(|error| anyhow::anyhow!("MoE forward failed: {error}"))?;
        timings.record_moe_telemetry(moe.executor_telemetry());
        timings.record_moe_forward(forward_started.elapsed().as_micros() as u64);
        timings.record_engine(crate::executor::ExecutionEngine::Moe);
        Ok(output)
    }

    pub(super) fn alloc_seq(&mut self) -> Option<u32> {
        if self.seq_pool.is_empty() {
            self.reap_idle();
        }
        // A retained session is not evicted here. A retained session is hot KV of
        // a live conversation - tens of thousands of tokens - and swapping it out
        // on the spot for the prefill that just arrived was what made a
        // 35-character request re-ingest 60 thousand tokens from scratch. Whoever
        // admits a new session is the scheduler, which only counts a really free
        // slot (`new_session_slots`) and only reclaims a retained session after
        // `RETAINED_RECLAIM_GRACE` of waiting (`reclaim_retained_slot`).
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

    /// The generation sequence that accompanies a prompt sequence.
    ///
    /// A fixed pair, not drawn from a pool: whoever has a prompt slot has a
    /// generation slot by construction, so opening the generation never fails for
    /// lack of a slot. See `llm::kv_seq_budget`, which reserves both halves in
    /// the context.
    pub(super) fn gen_seq_of(&self, prompt_seq: u32) -> u32 {
        prompt_seq + self.max_sessions.max(1)
    }

    /// Evicts the least recently used retained session, returning its slot.
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
        let parked = self.park_session(&sid);
        info!(
            "evicting the retained prefix of session {}… to free a slot (RAM cache: {})",
            &sid[..8.min(sid.len())],
            if parked { "stored" } else { "no" }
        );
        self.close_session(&sid);
        true
    }

    /// Copies a retained session's prompt KV to host RAM before its sequence
    /// is given away, so `unpark_session` can bring it back (`--cache-ram`).
    ///
    /// Same eligibility as `save_slot`: only a retained prompt whose cells
    /// match its logical positions. Returns whether the session was parked;
    /// the caller still closes the live session either way.
    pub(super) fn park_session(&mut self, sid: &str) -> bool {
        if !self.ram_cache.enabled() {
            return false;
        }
        let Some(session) = self.sessions.get(sid) else {
            return false;
        };
        if !session.retained
            || session.prompt_len == 0
            || !session.positions_match_cells()
            || session.prompt_cells != session.prompt_len
        {
            return false;
        }
        let seq = session.prompt_seq;
        if !self
            .ram_cache
            .accepts(self.engine.state_seq_size(seq) as u64)
        {
            return false;
        }
        let started = Instant::now();
        let data = match self.engine.state_seq_get_data(seq) {
            Ok(data) => data,
            Err(error) => {
                warn!(
                    "RAM cache: could not copy the KV of session {}…: {error:#}",
                    &sid[..8.min(sid.len())]
                );
                return false;
            }
        };
        let bytes = data.len();
        let session = self.sessions.get(sid).expect("session exists");
        let entry = ParkedSession::new(
            data,
            session.prompt_len,
            session.stable_len,
            session.prompt_tokens.clone(),
        );
        let tokens = entry.prompt_len;
        let parked = self.ram_cache.insert(sid, entry);
        if parked {
            info!(
                "RAM cache: session {}… stored ({tokens} tokens, {bytes} bytes, {} ms)",
                &sid[..8.min(sid.len())],
                started.elapsed().as_millis()
            );
        }
        parked
    }

    /// Brings a parked session back into a free sequence as a retained
    /// session, exactly as `restore_slot` does from disk. Returns `false`
    /// when the session is not parked, no sequence is free, or the engine
    /// refuses the state; in every such case the parked entry is gone and the
    /// caller proceeds as with any unknown session.
    pub(super) fn unpark_session(&mut self, sid: &str) -> bool {
        if self.sessions.contains_key(sid) {
            return false;
        }
        let Some(entry) = self.ram_cache.remove(sid) else {
            return false;
        };
        let Some(seq) = self.alloc_seq() else {
            return false;
        };
        let started = Instant::now();
        // The sequence may hold cells of whoever used it last.
        let _ = self.engine.kv_remove(seq, 0, -1);
        if let Err(error) = self.engine.state_seq_set_data(&entry.data, seq) {
            warn!(
                "RAM cache: session {}… could not be restored: {error:#}",
                &sid[..8.min(sid.len())]
            );
            let _ = self.engine.kv_remove(seq, 0, -1);
            self.free_seq(seq);
            return false;
        }
        let mut session = Session::new(seq);
        session.prompt_len = entry.prompt_len;
        session.n_past = entry.prompt_len;
        session.stable_len = entry.stable_len;
        session.prompt_cells = entry.prompt_len;
        session.prompt_tokens = entry.prompt_tokens;
        session.retained = true;
        session.last_used = Instant::now();
        self.sessions.insert(sid.to_string(), session);
        self.ram_cache.note_restored();
        info!(
            "RAM cache: session {}… restored ({} tokens, {} bytes, {} ms)",
            &sid[..8.min(sid.len())],
            entry.prompt_len,
            entry.data.len(),
            started.elapsed().as_millis()
        );
        true
    }

    /// Opens this turn's generation sequence by copying the prompt's KV.
    ///
    /// Idempotent: called before every decode of a generated token, it does work
    /// only the first time. After this nothing else writes to `prompt_seq`,
    /// which is what makes the prefix reusable without partial KV removal - see
    /// the doc of `Session`.
    pub(super) fn open_generation(&mut self, sid: &str) -> Result<()> {
        let Some(session) = self.sessions.get(sid) else {
            bail!("unknown session '{sid}' — run the prefill first");
        };
        if session.gen_seq.is_some() {
            return Ok(());
        }
        let prompt_seq = session.prompt_seq;
        let gen = self.gen_seq_of(prompt_seq);
        // The copy inherits whatever is in the destination sequence if it is not
        // clean; clearing is a total removal, always accepted.
        let _ = self.engine.kv_remove(gen, 0, -1);
        self.engine.kv_copy_seq(prompt_seq, gen);
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.gen_seq = Some(gen);
        session.retained = false;
        Ok(())
    }

    /// Decides, on chunk 0, whether the retained prefix serves this prompt - and
    /// leaves the session consistent with the decision.
    ///
    /// The rule is **append only**: the prefix serves when the retained tokens
    /// are exactly a prefix of the new ones, and not one token more. That way the
    /// prefill continues from where it stopped and **no position needs to leave
    /// the KV** - which is the difference between working on every model and
    /// depending on the engine accepting a partial `llama_memory_seq_rm`. Any
    /// other case (divergence in the middle, prompt identical to the retained
    /// one, session without a prompt) restarts the session from scratch, which
    /// is a loss of reuse on that turn, never an error.
    ///
    /// The last prompt token never enters the reused prefix: the logits of the
    /// first generated token come from it, so at least one token is always left
    /// to decode.
    pub(super) fn adopt_prompt_prefix(
        &mut self,
        sid: &str,
        tokens: Vec<i32>,
        stable_len: u32,
        requested_reuse: bool,
    ) -> Result<PrefixDecision> {
        let mut decision = PrefixDecision::default();
        if requested_reuse {
            let (retained, n_past, cells, physical_cells_match) = {
                let session = self.sessions.get(sid).ok_or_else(|| {
                    anyhow::anyhow!("unknown session '{sid}' in the prefix reuse")
                })?;
                (
                    session.prompt_tokens.clone(),
                    session.n_past,
                    session.prompt_cells,
                    session.positions_match_cells()
                        && session.prompt_tokens.len() as u32 == session.n_past,
                )
            };
            if physical_cells_match {
                decision = plan_prefix_reuse(&retained, n_past, &tokens);
            }
            if !physical_cells_match || !decision.reused {
                let common = retained
                    .iter()
                    .zip(&tokens)
                    .take_while(|(old, new)| old == new)
                    .count();
                debug!(
                    "prefix of session {}… is not usable: common={common} logical={} cells={} retained={} new={}",
                    &sid[..8.min(sid.len())],
                    n_past,
                    cells,
                    retained.len(),
                    tokens.len()
                );
                self.get_or_create(sid, true)?;
            }
        }
        let session = self
            .sessions
            .get_mut(sid)
            .ok_or_else(|| anyhow::anyhow!("session '{sid}' disappeared during the prefill"))?;
        session.prompt_tokens = tokens;
        session.stable_len = stable_len;
        session.retained = false;
        Ok(decision)
    }

    /// Closes the turn keeping the prompt: erases the whole generation sequence
    /// and returns the session to the position at the end of the prompt.
    ///
    /// It only uses **total** sequence removal, which every engine accepts -
    /// including the recurrent/hybrid ones, where `llama_memory_seq_rm` refuses
    /// any partial rollback larger than the snapshot window (`n_rs_seq`). It is
    /// the replacement for the rewind that was refused in 75 of 176 requests.
    ///
    /// Returns how many prompt tokens were left retained, or `None` when there
    /// was no prompt to retain (the session is then closed for good).
    pub(super) fn retain_prompt(&mut self, sid: &str) -> Option<u32> {
        let session = self.sessions.get(sid)?;
        if session.prompt_len == 0 {
            self.finish(sid);
            return None;
        }
        let (
            gen_seq,
            prompt_seq,
            prompt_len,
            prompt_cells,
            positions_ok,
            n_past_before,
            stable_len,
            prompt_tokens_before,
        ) = (
            session.gen_seq,
            session.prompt_seq,
            session.prompt_len,
            session.prompt_cells,
            session.positions_match_cells(),
            session.n_past,
            session.stable_len,
            session.prompt_tokens.len(),
        );
        if let Some(gen) = gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        // The prefill after the stable boundary was written to `gen_seq`; once it
        // is removed, `prompt_seq` must already contain only the retained prefix.
        // `prompt_cells` tracks only that sequence. It only tries to remove a
        // suffix when there are still extra cells in `prompt_seq`; an engine that
        // refuses a needed pruning keeps the whole prompt retained and loses the
        // reuse of this turn, without corrupting the session.
        let (trimmed_to_stable, trim_reason) = if prompt_len >= prompt_cells {
            (true, "already-at-stable-boundary")
        } else if !positions_ok {
            (false, "physical-logical-position-mismatch")
        } else if self.engine.kv_remove(prompt_seq, prompt_len as i32, -1) {
            (true, "suffix-removal-accepted")
        } else {
            (false, "engine-rejected-suffix-removal")
        };
        let session = self.sessions.get_mut(sid).expect("session exists");
        session.apply_retention(prompt_len, trimmed_to_stable);
        let reusable = session.retained && session.positions_match_cells();
        if trimmed_to_stable {
            info!(
                "prefix peer retention: session={} reusable={} reason={} retained_tokens={} stable_len={} n_past_before={} prompt_cells_before={} prompt_tokens_before={} positions_before={} positions_after={}",
                &sid[..8.min(sid.len())],
                reusable,
                trim_reason,
                prompt_len,
                stable_len,
                n_past_before,
                prompt_cells,
                prompt_tokens_before,
                positions_ok,
                session.positions_match_cells(),
            );
        } else {
            warn!(
                "prefix peer retention incomplete: session={} reusable={} reason={} retained_tokens={} stable_len={} n_past_before={} prompt_cells_before={} prompt_tokens_before={} positions_before={} positions_after={}",
                &sid[..8.min(sid.len())],
                reusable,
                trim_reason,
                prompt_len,
                stable_len,
                n_past_before,
                prompt_cells,
                prompt_tokens_before,
                positions_ok,
                session.positions_match_cells(),
            );
        }
        // A retained session is a reclaimable slot only after the grace period:
        // wake the scheduler so it can re-evaluate who is waiting.
        self.signal_slot_freed();
        Some(prompt_len)
    }

    /// Erases the KV of both sequences of a session.
    pub(super) fn wipe_kv(&mut self, session: &Session) {
        if let Some(gen) = session.gen_seq {
            let _ = self.engine.kv_remove(gen, 0, -1);
        }
        // The generation sequence may be left over from a previous turn that did
        // not go through here; clearing both is cheap and leaves no cell stuck.
        let _ = self
            .engine
            .kv_remove(self.gen_seq_of(session.prompt_seq), 0, -1);
        let _ = self.engine.kv_remove(session.prompt_seq, 0, -1);
    }

    /// Writes a session's retained prefix as a KV snapshot on disk,
    /// like llama-server's `--slot-save-path`.
    ///
    /// **It always saves `prompt_seq`, never `gen_seq`.** The volatile tail of
    /// the generation goes away at the end of the turn and is not part of a
    /// snapshot; what remains is the stable prompt that the next turn reuses.
    /// llama.cpp's `.bin` serializes the sequence's cells with their positions,
    /// and the `.json` sidecar records the identity of the model and of the slice
    /// so that the restore refuses an incompatible state.
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
            .ok_or_else(|| anyhow::anyhow!("unknown session '{sid}'"))?;
        if session.prompt_len == 0 {
            bail!("session '{sid}' has no retained prompt for a snapshot");
        }
        let seq = session.prompt_seq;
        let retained = session.prompt_len;
        if !session.positions_match_cells() || session.prompt_cells != retained {
            bail!(
                "session '{sid}' uses {} cells for the logical position {retained}; M-RoPE snapshots are not reusable",
                session.prompt_cells
            );
        }
        // The real tokens of the retained prompt, when this peer knows them (the
        // head). Empty on hops that only received hidden state.
        let real_tokens: Vec<i32> = session
            .prompt_tokens
            .get(..retained as usize)
            .map(<[i32]>::to_vec)
            .unwrap_or_default();
        // Token header of the `.bin`: the real ones when they exist, otherwise a
        // vector of the right size - llama.cpp never checks the header against the
        // cells, it is advisory.
        let header_tokens: Vec<i32> = if real_tokens.is_empty() {
            vec![0i32; retained as usize]
        } else {
            real_tokens.clone()
        };
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create the snapshots directory {}", dir.display()))?;
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
            .with_context(|| format!("write the sidecar of {name}"))?;
        Ok(sidecar)
    }

    /// Reads a KV snapshot from disk into a **new** session. Refuses a snapshot
    /// from another model, layer range or hidden state width before touching the
    /// engine; the `.bin` itself refuses an incompatible KV geometry (row
    /// type/size, layer count).
    pub(super) fn restore_slot(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        new_sid: &str,
    ) -> Result<SlotRestoreOutcome> {
        let name = sanitize_slot_name(name)?;
        if self.sessions.contains_key(new_sid) {
            bail!("session '{new_sid}' already exists; choose another id");
        }
        let raw = std::fs::read(slot_json_path(dir, name))
            .with_context(|| format!("read the sidecar of {name}"))?;
        let sidecar: SlotSidecar = serde_json::from_slice(&raw)
            .with_context(|| format!("sidecar of {name} unreadable"))?;
        self.check_slot_compatible(&sidecar)?;

        let seq = self.alloc_seq().ok_or_else(|| {
            anyhow::anyhow!("seq pool exhausted; release a session before restoring")
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
        session.prompt_cells = sidecar.retained_tokens;
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

    /// Checks that a sidecar describes the same model and the same slice that this
    /// peer serves. `cache_type_*` is not included: the `.bin` already refuses a
    /// divergent KV geometry by the row type and size per layer.
    pub(super) fn check_slot_compatible(&self, sidecar: &SlotSidecar) -> Result<()> {
        if sidecar.format_version != SLOT_SIDECAR_VERSION {
            bail!(
                "sidecar of version {} incompatible (expected {SLOT_SIDECAR_VERSION})",
                sidecar.format_version
            );
        }
        let mismatch = |field: &str, expected: String, found: String| {
            anyhow::anyhow!("incompatible KV snapshot: {field} {found} != {expected} of this peer")
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
                "layer range",
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
            // An idle conversation may still come back: keep its KV in RAM.
            self.park_session(&sid);
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
                // Total clearing of the sequences, which every model accepts.
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
            // A reset starts the session over: a parked KV of the same id is
            // a past this session no longer has.
            self.ram_cache.remove(sid);
            if let Some(mut old) = self.sessions.remove(sid) {
                let _ = old.finish_token_stream();
                self.wipe_kv(&old);
                self.free_seq(old.prompt_seq);
            }
            let seq = self.alloc_seq().ok_or_else(|| {
                anyhow::anyhow!(
                    "seq pool exhausted (max {} simultaneous sessions)",
                    self.max_sessions
                )
            })?;
            self.sessions.insert(sid.to_string(), Session::new(seq));
        }
        self.sessions
            .get_mut(sid)
            .ok_or_else(|| anyhow::anyhow!("unknown session '{}' — send reset=true first", sid))
    }

    /// Ends the session for good: both sequences are erased and the slot returns
    /// to the pool. The normal end of a turn does **not** come through here - it
    /// goes through `retain_prompt`, which keeps the prompt. What arrives here is
    /// the gateway's explicit release, cancellation and error cleanup.
    pub(super) fn finish(&mut self, sid: &str) {
        // Releasing for good also forgets the copy parked in host RAM.
        self.ram_cache.remove(sid);
        self.close_session(sid);
    }

    /// Wipes the live session and returns its sequence to the pool, leaving
    /// the host-memory cache alone - the eviction path parks first.
    pub(super) fn close_session(&mut self, sid: &str) {
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

    /// Wakes `run_scheduler` so it re-evaluates a new-session prefill that is
    /// waiting for a slot.
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
    quic: Option<crate::serve::QuicServerConfig>,
    quic_port: Option<u16>,
    auth: Option<crate::auth::AuthState>,
    model_cache: Arc<ModelCache>,
    reuse_weights: bool,
    loads: Arc<crate::pipeline::LoadTracker>,
    load_node_id: String,
    request_activity: Arc<crate::reqlog::RequestActivity>,
) -> Result<()> {
    // Derived, not copied from the request: the command-line path never fills
    // in `no_embeddings`, so without this the last slice's node installed the
    // capture callback and paid ~8 ms per token to capture a residual that
    // nobody reads. See `NodeConfig::needs_residual_export`.
    // The port first, the model afterwards. Binding the socket costs microseconds
    // and is the only thing here that fails because of the outside world; loading
    // the model costs minutes and gigabytes of VRAM. In the reverse order, each of
    // the supervisor's 10 attempts loaded the whole model only to find out at the
    // end that the port was taken - the log showed "llama context ready"
    // followed by "Address already in use", ten times, with VRAM held by
    // discarded loads.
    // The port first, the model afterwards. Binding the socket costs microseconds
    // and is the only thing here that fails because of the outside world; loading
    // the model costs minutes and gigabytes of VRAM. In the reverse order, each of
    // the supervisor's 10 attempts loaded the whole model only to find out at the
    // end that the port was taken - the log showed "llama context ready"
    // followed by "Address already in use", ten times, with VRAM held by
    // discarded loads.
    let port = cfg.inference_port;
    let listener = bind_dual_stack(port).await?;
    info!("port {port} bound; loading the model");

    let model_info = match crate::gguf::read_info(std::path::Path::new(&cfg.model_path)) {
        Ok(info) => Some(info),
        Err(error) => {
            warn!("could not read the GGUF metadata before loading: {error}");
            None
        }
    };
    let inference_mode = configured_inference_mode(cfg.inference_mode)?;
    let inference_path = crate::executor::select(inference_mode, model_info.as_ref())?;
    info!(
        "selected inference path: {} (mode {:?})",
        inference_path.as_str(),
        inference_mode
    );

    // The terminal MoE hop also needs the captured boundary residual so its
    // own executor can run before the response is sampled. Dense single-peer
    // contexts keep the old fast path without installing the callback.
    let need_embeddings = cfg.needs_residual_export() || inference_path.is_moe();
    // `rs_seq_window` (largest speculative batch width, across the two mechanisms
    // lookup and MTP) is now computed inside `engine_config_from_node` -
    // see the doc of `EngineConfig::rs_seq_window`.
    let moe_plan = if inference_path.is_moe() {
        Some(
            crate::moe::runtime::MoeRuntime::external_executor_plan(&cfg.model_path)
                .map_err(|error| anyhow::anyhow!("MoE ownership planning failed: {error}"))?,
        )
    } else {
        None
    };
    let moe_external_executor = moe_plan.as_ref().is_some_and(|plan| plan.enabled());
    if let Some(plan) = moe_plan.as_ref().filter(|plan| !plan.enabled()) {
        let reason = plan
            .reason
            .as_deref()
            .unwrap_or("no layer has an external executor available");
        return Err(anyhow::anyhow!(
            "MoE combination not supported before loading the model: GGML dtypes {:?}; {reason}; the MoE path requires an external executor and will not use native llama.cpp",
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

    // Only now do we know whether the `nextn` head exists: it depends on the
    // GGUF, not on the configuration. See `Network::mtp_ready`.
    mtp_ready.store(engine.mtp_ready(), std::sync::atomic::Ordering::Relaxed);

    let moe = if inference_path.is_moe() && moe_external_executor {
        let info = model_info.as_ref().ok_or_else(|| {
            anyhow::anyhow!("the MoE path requires GGUF metadata to prepare the router")
        })?;
        let experts = info
            .expert_count
            .ok_or_else(|| anyhow::anyhow!("the MoE path requires expert_count in the GGUF"))?;
        let top_k = info.expert_used_count.unwrap_or(1).max(1) as usize;
        let weights_bytes = std::fs::metadata(&cfg.model_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let cuda_compute_bytes = engine.cuda_compute_bytes_by_device();
        Some(Arc::new(
            crate::moe::runtime::MoeRuntime::open(
                &cfg.model_path,
                cfg.model_label.clone(),
                experts,
                top_k,
                weights_bytes,
                cuda_compute_bytes,
            )
            .map_err(|error| anyhow::anyhow!("MoE runtime unavailable: {error}"))?,
        ))
    } else {
        None
    };

    if let Some(runtime) = moe.as_ref() {
        let telemetry = runtime.executor_telemetry();
        // llama.cpp's `offloaded N/N layers to GPU` counts only the tensors it
        // allocates itself. With `moe_external_executor` the expert weights are
        // not even allocated there (see `EngineConfig::moe_external_executor`),
        // so that total is both true and misleading: on a 35 B model the model
        // buffers on GPU add up to ~1.6 GB and the operator concludes, rightly,
        // that "the model is all in RAM". Saying here whose the experts are is
        // what prevents that wrong reading.
        tracing::info!(
            model = %cfg.model_label,
            moe_executor = telemetry.executor.as_str(),
            "the expert weights are outside the llama.cpp `offloaded N/N layers` count: the POM MoE executor hosts them, not llama.cpp"
        );
        if telemetry.executor == crate::moe::runtime::MoeExecutorKind::CpuReference {
            tracing::warn!(
                model = %cfg.model_label,
                requested_n_gpu_layers = cfg.n_gpu_layers,
                moe_executor = telemetry.executor.as_str(),
                dtypes = ?telemetry.dtypes,
                reason = ?telemetry.reason,
                "MoE is using CPUReference: the expert weights stay mapped in RAM and n_gpu_layers does not take them to the GPU. A SINGLE dtype without a CUDA bridge is enough to bring down the whole route; check the `dtypes` list, not the file name"
            );
        } else {
            tracing::info!(
                model = %cfg.model_label,
                requested_n_gpu_layers = cfg.n_gpu_layers,
                moe_executor = telemetry.executor.as_str(),
                dtypes = ?telemetry.dtypes,
                devices = ?telemetry
                    .diagnostics
                    .as_ref()
                    .map(|diagnostics| &diagnostics.device_cache_devices),
                "CUDA MoE executor selected; experts will be admitted to the resident cache partitioned per GPU"
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
    state_inner.original_block_count = model_info
        .as_ref()
        .and_then(|info| info.original_block_count);

    // Outside the state mutex: the decode holds that lock and it is from inside
    // it that the reduce waits for the sum. See `handle_tp_reduce`.
    let tp_barrier = crate::collective::ReduceBarrier::new();
    let tp_size = cfg.tp.as_ref().map(|t| t.size).unwrap_or(1);
    let local_barrier_handle = crate::tp::set_local_barrier(tp_barrier.clone(), tp_size);

    let state: SharedState = Arc::new(Mutex::new(state_inner));
    // Slice speed test, once per load: it is what gives the gateway planner this
    // peer's cost per token before the first request. It runs off the startup
    // path; a request that arrives in the middle waits for the lock for less
    // than a second.
    {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let Ok(mut s) = lock_inference_state(&state) else {
                return;
            };
            match perf::run_bench(&mut s) {
                Ok(Some(bench)) => info!(
                    "speed of slice L{}-{}: prefill {:.1} tok/s, decode {:.1} tok/s (test of {} ms)",
                    s.start_layer,
                    s.end_layer,
                    1e6 / bench.prefill_us_per_token.max(1.0),
                    1e6 / bench.decode_us_per_step.max(1.0),
                    bench.duration_ms
                ),
                Ok(None) => debug!("slice speed test skipped (TP or no free slot)"),
                Err(error) => warn!("slice speed test failed: {error}"),
            }
        });
    }
    let scheduler_cfg = crate::scheduler::SchedulerConfig::from_node_config(&cfg);
    // TP participates in the same canonical batch frame. The handler opens
    // one shared decode id for the whole frame, broadcasts that frame to the
    // workers and then submits all items to this local scheduler together.
    batching_ready.store(scheduler_cfg.enabled, std::sync::atomic::Ordering::Relaxed);
    let scheduler = spawn_scheduler(state.clone(), scheduler_cfg);
    let app_state = InferenceAppState {
        state,
        scheduler,
        health_snapshot: HealthSnapshot::default(),
        tp_batch_gate: Arc::new(tokio::sync::Mutex::new(())),
        direct_forward_order: DirectForwardOrder::default(),
        loads,
        load_node_id,
        request_activity,
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
        .route("/v1/session/slot", post(handle_slot))
        .route("/v1/session/cache/clear", post(handle_cache_clear));
    let inference_routes = if let Some(auth) = auth.clone() {
        inference_routes.layer(middleware::from_fn_with_state(auth, require_inference_auth))
    } else {
        inference_routes
    };
    let inference_routes = inference_routes.with_state(app_state.clone());

    // The persistent channel has its own router because its state carries the
    // credential: the middleware checks the opening, and the handler re-checks
    // each frame. The body limit is turned off here because the body is the
    // whole generation, not a frame - each frame inside the stream remains
    // limited by the channel's framing.
    let link_state = LinkState {
        app: app_state.clone(),
        auth: auth.clone(),
    };
    let link_routes = Router::new().route(
        crate::link::LINK_PATH,
        post(handle_link).layer(DefaultBodyLimit::disable()),
    );
    let link_routes = if let Some(auth) = auth.clone() {
        link_routes.layer(middleware::from_fn_with_state(auth, require_inference_auth))
    } else {
        link_routes
    };
    let link_routes = link_routes.with_state(link_state);

    let tp_routes = Router::new().route(
        "/v1/tp/reduce",
        post(handle_tp_reduce).layer(DefaultBodyLimit::max(inference_frame_body_limit())),
    );
    let tp_routes = if let Some(auth) = auth {
        tp_routes.layer(middleware::from_fn_with_state(auth, require_inference_auth))
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
        .merge(tp_routes)
        // Every reply says which node it came from; see `peer_client::answered_by_other_node`.
        .layer(axum::middleware::map_response(
            crate::peer_client::stamp_node_id,
        ));

    info!("inference server ready on [::]:{port}");

    // See the twin note in `gateway::start_gateway_with_ready`: without
    // `tcp_nodelay`, every hop reply goes out with Nagle on. A direct-reply ack
    // is exactly the small write that it holds back. Whoever applies it now is
    // `serve`, which also decides between TLS and plaintext per connection.
    //
    // The graceful shutdown became a race between the accept loop and the
    // generation watch: when the generation changes, we stop accepting. A
    // connection in flight finishes on its own, as before.
    // The graceful shutdown is sequential on purpose: first `serve` stops
    // accepting and closes the connections that remained, only then is the VRAM
    // returned. In the reverse order an in-flight request would touch an already
    // freed context.
    let result =
        crate::serve::serve_with_quic(listener, router, tls, quic, quic_port, async move {
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

    // The warm-up is cooperative and may be reading the GGUF while the server
    // is reconfigured. Signal the stop before freeing the engine, so that no old
    // generation keeps admitting experts onto the device.
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

    // Explicit, and not by the `Arc` count: a forgotten copy anywhere - which is
    // what the stray connections did - postponed the `Drop` forever, and the
    // `undeploy` answered "done" with the model still on the GPU. After the
    // drain above nobody decodes any more, so this is safe.
    // `into_inner` on the poisoned one: a handler that panicked leaves the
    // mutex marked, and refusing the release because of that would be exactly
    // leaving the VRAM held in the case where it most needs to come back.
    run_native_cleanup({
        let state = app_state.state.clone();
        move || {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .engine
                .unload();
        }
    })
    .await?;
    info!("model memory returned on port {port}");

    result?;
    batching_ready.store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Execute native model cleanup without running it on the async executor.
async fn run_native_cleanup<F>(cleanup: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    tokio::task::spawn_blocking(cleanup)
        .await
        .map_err(|error| anyhow::anyhow!("native cleanup ended in a panic: {error}"))?;
    Ok(())
}

// ── Socket ────────────────────────────────────────────────────────────────────

/// A listener that serves IPv6 and IPv4 on the same socket, falling back to pure
/// IPv4 when the machine has no IPv6.
///
/// `IPV6_V6ONLY` only counts if it is turned off **before** the `bind`, which is
/// why the socket is assembled by hand instead of coming out of
/// `TcpListener::bind`. The old path called `setsockopt` after the bind: on Linux
/// it went unnoticed because `net.ipv6.bindv6only` is already 0 by default, but
/// on Windows the default is 1 and the inference server would simply not accept
/// any IPv4 peer - which is how the whole LAN cluster talks.
///
/// `SO_REUSEADDR` stays out on purpose: on Windows it lets another process take
/// a port that is already bound, and it is precisely the "address already in
/// use" that reveals an old server still standing.
pub(crate) fn listener_dual_stack(port: u16) -> Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    // Backlog: the same one std's `TcpListener::bind` uses.
    const BACKLOG: i32 = 128;

    let v6 = (|| -> std::io::Result<std::net::TcpListener> {
        let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        sock.set_only_v6(false)?;
        // std's `TcpListener::bind` asks for this on its own on Unix; a raw
        // socket does not. Without the flag, every connection in TIME_WAIT on this
        // port - and the gateway leaves several, probing `/health` and sending
        // inference - prevents the next server from binding for up to a minute,
        // which is more than the supervisor's ten attempts. Every new-version
        // deployment died there.
        sock.set_reuse_address(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.listen(BACKLOG)?;
        Ok(sock.into())
    })();

    match v6 {
        Ok(l) => Ok(l),
        Err(e) => {
            // Without IPv6 on the machine: pure IPv4 still serves the whole cluster.
            tracing::debug!("IPv6 bind on [::]:{port} failed ({e}); trying IPv4");
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
/// It is a `PeerClients` and not a `reqwest::Client` because the next hop is a
/// peer: with the cluster on TLS it needs that node_id's certificate pinned. The
/// global scheme (`peer_client::tls_enabled`) is what says whether there is TLS -
/// this process is the same as the node's, so the policy is already applied here.
pub(crate) fn http_client() -> &'static crate::peer_client::PeerClients {
    static CLIENT: std::sync::OnceLock<crate::peer_client::PeerClients> =
        std::sync::OnceLock::new();
    let c = CLIENT
        .get_or_init(|| crate::peer_client::PeerClients::new(std::time::Duration::from_secs(600)));
    // On every use, not only at creation: the `OnceLock` is born once and the
    // operator may flip the flag later. `set_tls` only does work when the value
    // changes, so this costs one atomic read on the common path.
    c.set_tls(crate::peer_client::tls_enabled());
    // The client is lazy and lives apart from the gateway's client. Sync the
    // QUIC preference on every use so that a change propagated by the
    // policy also reaches this node's relays and direct replies.
    c.set_quic(crate::peer_client::quic_enabled());
    c
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
        Err(error) => return err400(format!("invalid batch: {error}")),
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
                Ok(body) => handle_prefill(State(app), None, headers, Bytes::from(body)).await,
                Err(error) => err400(format!("invalid batch item: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} failed in the batch processing", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("invalid batch response: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("item response does not contain exactly one item"),
            Err(error) => return err500(format!("invalid item response: {error}")),
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
        Err(error) => return err400(format!("invalid batch: {error}")),
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
                Ok(body) => handle_step(State(app), None, headers, Bytes::from(body)).await,
                Err(error) => err400(format!("invalid batch item: {error}")),
            }
        }
    });
    let responses = futures::future::join_all(calls).await;
    let mut items = Vec::with_capacity(responses.len());
    for (original, response) in batch.items.iter().zip(responses) {
        if !response.status().is_success() {
            return (
                StatusCode::BAD_GATEWAY,
                format!("item {} failed in the batch processing", original.item_id),
            )
                .into_response();
        }
        let bytes =
            match axum::body::to_bytes(response.into_body(), INFERENCE_FRAME_BODY_LIMIT).await {
                Ok(bytes) => bytes,
                Err(error) => return err500(format!("invalid batch response: {error}")),
            };
        let decoded = match wire::decode_batch_items(&bytes) {
            Ok((_, mut decoded)) if decoded.len() == 1 => decoded.remove(0),
            Ok(_) => return err500("item response does not contain exactly one item"),
            Err(error) => return err500(format!("invalid item response: {error}")),
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

/// The tokenized prompt fits in the session window, with room for at least one
/// generated token. With the unified KV cache llama.cpp would accept a larger
/// prompt - it would take another session's cells -, so the boundary is
/// enforced here. The gateway already refuses by the estimate; this catches a
/// prompt denser than it and any caller that does not go through the gateway.
pub(super) fn check_prompt_fits_session(
    s: &super::InferState,
    prompt_tokens: usize,
) -> anyhow::Result<()> {
    let window = context_per_session(s.engine.n_ctx(), s.max_sessions);
    match prompt_window_refusal(prompt_tokens, window) {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
}

pub(super) fn prompt_window_refusal(prompt_tokens: usize, window: u32) -> Option<String> {
    (prompt_tokens >= window as usize).then(|| {
        format!(
            "the prompt has {prompt_tokens} tokens and the session window is {window}; \
             there is no room left for the answer"
        )
    })
}

/// This session's generation still fits in the window. Reaching it is a stop:
/// the gateway bounds the total (`OutputBudget::within_session_window`) and
/// ends with `length` before getting here; this refusal is the safety net for
/// the path that does not go through it and for the head that generates ahead
/// in remote-stream mode.
pub(super) fn check_decode_fits_session(s: &super::InferState, n_past: u32) -> anyhow::Result<()> {
    let window = context_per_session(s.engine.n_ctx(), s.max_sessions);
    match decode_window_refusal(n_past, window) {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
}

pub(super) fn decode_window_refusal(n_past: u32, window: u32) -> Option<String> {
    (n_past >= window).then(|| {
        format!(
            "the session window is full ({n_past} of {window} positions); \
             generation stopped at the context limit"
        )
    })
}

// ── /health ───────────────────────────────────────────────────────────────────

/// `GET /health`. With the engine busy, it answers the last snapshot marked
/// `busy: true` (with `snapshot_age_ms`); the snapshot is stored again whenever
/// the lock is free.
pub(super) async fn handle_health(State(app): State<InferenceAppState>) -> Response {
    let scheduler = app.scheduler;
    let state = app.state;
    let snapshot = app.health_snapshot;
    // `/health` is a readiness and route probe: it has to answer with the
    // engine busy. The `InferState` lock is held for a whole prefill chunk
    // (seconds), and waiting for it made the gateway's readiness probe and the
    // route plan's probe expire - the peer dropped out of the plan ("Gap at
    // layer 0") and the tunnel route was erased. When busy, return the last
    // snapshot.
    let slot_dir = match state.try_lock() {
        Ok(s) => Some(s.slot_dir.clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return err500(anyhow::anyhow!("inference state mutex poisoned"))
        }
    };
    if let Some(slot_dir) = slot_dir {
        let slot_count = slot_dir.as_deref().map(count_slot_entries).unwrap_or(0);
        if let Ok(s) = state.try_lock() {
            let body = health_body(&s, &scheduler, slot_count);
            drop(s);
            store_health_snapshot(&snapshot, &body);
            return Json(body).into_response();
        }
    }
    if let Some(body) = busy_health_body(&snapshot, &scheduler) {
        return Json(body).into_response();
    }
    // Busy and no snapshot yet (first probe after loading): wait for the
    // engine on a blocking thread, so the async runtime is not stalled.
    let built = tokio::task::spawn_blocking(move || {
        let s = lock_inference_state(&state)?;
        let slot_dir = s.slot_dir.clone();
        let slot_count = slot_dir.as_deref().map(count_slot_entries).unwrap_or(0);
        anyhow::Ok(health_body(&s, &scheduler, slot_count))
    })
    .await;
    match built {
        Ok(Ok(body)) => {
            store_health_snapshot(&snapshot, &body);
            Json(body).into_response()
        }
        Ok(Err(error)) => err500(error),
        Err(error) => err500(anyhow::anyhow!("/health did not complete: {error}")),
    }
}

/// `/health` snapshot kept for when the engine is busy.
pub(super) type HealthSnapshot = Arc<std::sync::Mutex<Option<(Value, Instant)>>>;

pub(super) fn store_health_snapshot(snapshot: &HealthSnapshot, body: &Value) {
    if let Ok(mut slot) = snapshot.lock() {
        *slot = Some((body.clone(), Instant::now()));
    }
}

/// The last snapshot, with what does not depend on the engine recomputed now:
/// the scheduler queue and the routes. `busy` says the rest comes from the
/// snapshot.
fn busy_health_body(snapshot: &HealthSnapshot, scheduler: &SchedulerHandle) -> Option<Value> {
    let mut body = snapshot_health_body(snapshot)?;
    body["scheduler"] = scheduler.stats.json(&scheduler.config);
    add_route_fields(&mut body);
    Some(body)
}

/// The stored snapshot, marked as served with the engine busy.
pub(super) fn snapshot_health_body(snapshot: &HealthSnapshot) -> Option<Value> {
    let (mut body, at) = snapshot.lock().ok()?.clone()?;
    body["busy"] = Value::Bool(true);
    body["snapshot_age_ms"] = Value::from(at.elapsed().as_millis() as u64);
    Some(body)
}

fn add_route_fields(body: &mut Value) {
    let book = crate::route_plan::book();
    body["route_rtt_ms"] = json!(book.rtt_map(crate::route_plan::Plane::Inference));
    body["gateway_rtt_ms"] = json!(book.rtt_map(crate::route_plan::Plane::Gateway));
    body["route_kinds"] = json!(book.kind_map(crate::route_plan::Plane::Inference));
    body["gateway_route_kinds"] = json!(book.kind_map(crate::route_plan::Plane::Gateway));
    body["relayed_peers"] = json!(book.relayed_nodes());
}

fn health_body(s: &InferState, scheduler: &SchedulerHandle, slot_count: usize) -> Value {
    // Keep in sync with network.rs's `self_announce`, gateway.rs's local peer
    // entry, and the capability constants in routing/pipeline.rs.
    let mut caps = vec![
        crate::pipeline::CAP_FRAME_BATCH,
        crate::pipeline::CAP_FRAME_DTYPE_F8,
        crate::pipeline::CAP_FRAME_DTYPE_I4G64,
        crate::pipeline::CAP_FRAME_DTYPE_I4G128,
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
    let ram_cache = s.ram_cache.stats();
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
    // Keep the native diagnostics as named boundaries. If a Windows native
    // access violation happens while `/health` is being assembled, the last
    // marker identifies which FFI snapshot crossed the bad pointer boundary.
    tracing::debug!("health native snapshot: load_params begin");
    let load_params = s.engine.load_params();
    tracing::debug!("health native snapshot: load_params done; placement begin");
    let placement = s.engine.placement();
    tracing::debug!("health native snapshot: placement done; context_memory begin");
    let context_memory = s.engine.context_memory();
    tracing::debug!("health native snapshot: context_memory done");
    let mut body = json!({
        "status": "ok",
        "model_loaded": true,
        "original_block_count": s.original_block_count,
        "selected_path": s.inference_path.as_str(),
        "moe_requested": moe_requested,
        "moe_executor": moe_executor,
        "moe_executor_dtypes": moe_executor_dtypes,
        "moe_executor_reason": moe_executor_reason,
        "hidden_dim": s.engine.n_embd,
        "vocab_size": s.engine.n_vocab,
        "max_sessions": s.max_sessions,
        // Two KV sequences per session: the prompt's and the generation's. See
        // `llm::kv_seq_budget`, which explains why and what it costs.
        "kv_seq_max": crate::llm::kv_seq_budget(s.max_sessions),
        "sessions_in_use": s.sessions.len(),
        "free_slots": s.seq_pool.len(),
        // Sessions parked at the end of the prompt, waiting for the next turn. It
        // is the prefix cache seen from the side that executes it.
        "sessions_retained": s.sessions.values().filter(|x| x.retained).count(),
        // Generations in progress on this peer, from any gateway: a live session
        // that is not merely retained for the prefix cache. It is the queue that
        // the planner multiplies by the slice's cost to spread the load.
        "active_generations": s.sessions.values().filter(|x| !x.retained).count(),
        // Measured speed of this slice (test on serving + real traffic).
        "perf": s.perf.json(),
        "prefix_tokens_retained": s
            .sessions
            .values()
            .filter(|x| x.retained)
            .map(|x| x.prompt_len as u64)
            .sum::<u64>(),
        // Engine diagnostic: `kv_can_shift` is llama.cpp's K-shift, and it
        // does **not** govern partial KV removal. This project's prefix reuse
        // depends on neither - see `Session`.
        "kv_can_shift": s.engine.kv_can_shift(),
        "n_ctx_total": n_ctx_total,
        // The engine reserves the public window for each session. If the fit
        // reduces the total, the per-session capacity is the effective division,
        // not the raw total that llama.cpp returns.
        "n_ctx_per_session": n_ctx_per_session,
        "offload_kqv": s.engine.offload_kqv(),
        "cache_type_k": s.engine.cache_type_k().as_str(),
        "cache_type_v": s.engine.cache_type_v().as_str(),
        "fit_memory_applied": s.engine.fit_memory_applied(),
        "n_gpu_layers_effective": s.engine.effective_n_gpu_layers(),
        "n_ubatch_effective": s.engine.effective_n_ubatch(),
        // With which llama.cpp parameters this context came up, so the
        // operator can compare the node with a `llama-cli` command line.
        // Read from the live context, not from `EngineConfig` - see
        // `LlmEngine::load_params`.
        "load_params": load_params,
        "profile_sync_enabled": crate::llm::profile_backend_sync_enabled(),
        // Where the layers and the KV cache really ended up, straight from
        // llama.cpp. `null` when the bridge did not answer - the interface says
        // it does not know, instead of deriving a map from `n_gpu_layers`, which
        // would be wrong whenever a buffer type override came into play.
        "placement": placement,
        "context_memory": context_memory,
        "inference_frame_capabilities": caps,
        "scheduler": scheduler.stats.json(&scheduler.config),
        // Pure diagnostic - `llama_memory_can_shift`, the K-shift of llama.cpp's
        // reference `--ctx-shift`. It no longer decides whether speculation runs
        // (see the doc of `should_draft`); kept here because it is real
        // engine information.
        "kv_can_shift": s.engine.kv_can_shift(),
        // Was the `nextn` draft head really attached? It depends on the loaded
        // GGUF (a non-final slice does not have the block) and on the operator's
        // switch, so neither the configuration nor the file name answers it.
        "mtp_ready": s.engine.mtp_ready(),
        "mtp_allocation": s.engine.mtp_allocation(),
        "mtp_unavailable_reason": s.engine.mtp_unavailable_reason(),
        // KV cache snapshots (`--slot-save-path`): on when a directory is
        // configured. `slot_count` counts sidecars without opening the JSON, so
        // as not to hold the engine lock during a disk read.
        "slot_persist_enabled": s.slot_dir.is_some(),
        "slot_count": slot_count,
    });
    // Host-memory KV cache (`--cache-ram`): retained sessions parked in RAM
    // after losing their sequence. See `ram_cache.rs`. A `null` limit with the
    // cache on means this host is too small (half of its RAM is under 6 GiB)
    // or was not measured yet.
    body["cache_ram"] = json!({
        "enabled": s.ram_cache.enabled(),
        "limit_bytes": s.ram_cache.limit_bytes(),
        "user_reserve_bytes": s.ram_cache.reserve_bytes(),
        "bytes": ram_cache.bytes,
        "entries": ram_cache.entries,
        "tokens": ram_cache.tokens,
        "parked": ram_cache.parked,
        "restored": ram_cache.restored,
        "evicted": ram_cache.evicted,
    });
    // Pinned routes of this node (`route_plan`): RTT and transport to the
    // inference server and to the gateway of each peer. The planning gateway
    // does not measure the edges between two other peers nor the way back to
    // itself; it reads them from here. Outside the `json!` above, which is
    // already at the macro's recursion limit.
    add_route_fields(&mut body);
    // Keep the identity fields separate from the large diagnostic object. A
    // direct addition to the json! tree would exceed the macro recursion limit
    // and would make this readiness contract depend on compiler settings.
    body["model_label"] = Value::String(s.model_label.clone());
    body["deploy_id"] = Value::String(s.deploy_id.clone());
    body["start_layer"] = Value::from(s.start_layer);
    body["end_layer"] = Value::from(s.end_layer);
    body["total_layers"] = Value::from(s.total_layers);
    body["busy"] = Value::Bool(false);
    body
}

// ── /v1/tp/reduce ────────────────────────────────────────────────────────────

/// One partial of a tensor parallelism reduce (path C). Each rank of the group
/// POSTs one of these with its own partial and the same `barrier_id`; the
/// endpoint (on the primary/rank 0) accumulates and returns the sum when all
/// arrive. It **never** touches the `InferState` mutex, and that is the fix, not
/// style.
///
/// `handle_prefill` holds that mutex during the whole decode, and it is from
/// inside that decode that the reduce callback blocks waiting for the sum. While
/// this handler took the same mutex to read the barrier, the partial that
/// arrived - from rank 0 itself or from the worker - was stuck waiting for a lock
/// that would only be released when the decode finished; and the decode would
/// only finish with the sum that was stuck at the door. The exact deadlock of
/// 18/08: both ranks logged the first reduce, 0.2 ms apart, and the pipeline
/// stopped there.
///
/// The barrier lives outside the mutex (`ReduceBarrier` is already `Clone` and
/// synchronized internally), so the collective path never contends with the
/// decode.
///
/// The partial travels as **raw little-endian f32**, not as JSON. A 4-byte float
/// becomes ~12 characters in JSON, and the partial of a 100-token prefill has
/// 410 thousand floats: ~5 MB of text per reduce, 64 times per decode. That blew
/// past axum's body limit (413) and, when it fit, spent more time formatting
/// numbers than summing them. Raw is 4x smaller and needs no parsing.
///
/// `barrier_id` and `rank` go in a header, which is where fixed-size metadata
/// belongs.
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
                "header {} is missing",
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
        None => return err400(format!("header {} is missing", crate::tp::TP_RANK_HEADER)),
    };
    if body.len() % 4 != 0 {
        return err400(format!(
            "partial of {} bytes is not a multiple of 4",
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
            Err(e) => err500(format!("barrier abandoned: {e}")),
        },
        Err(e) => err400(e),
    }
}

/// Forwards a session lifecycle call to the group's other ranks.
///
/// The body is reassembled (it is not a binary frame like the decode), and the
/// replica header avoids infinite fan-out: whoever receives it marked does not
/// forward.
pub(super) fn replicate_session_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    route: &str,
    session_id: &str,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // already the replica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("could not replicate the session: state mutex poisoned");
            return;
        };
        s.tp.clone()
    };
    let Some(tp) = tp.filter(|t| t.size > 1 && t.rank == 0) else {
        return;
    };
    let body = json!({ "session_id": session_id }).to_string().into_bytes();
    crate::tp::broadcast_to(
        &tp.members,
        route,
        "session",
        body,
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
            warn!("could not replicate the rewind: state mutex poisoned");
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
    /// `"save"`, `"restore"` or `"erase"`.
    pub(super) action: String,
    /// Live session to save (`save`). Ignored in `restore`/`erase`.
    #[serde(default)]
    pub(super) session_id: String,
    /// Name of the snapshot on disk (a safe path component).
    pub(super) slot_name: String,
    /// New session to create in `restore`. Mandatory in that action.
    #[serde(default)]
    pub(super) dest_session_id: Option<String>,
}

/// `POST /v1/session/slot` - writes, reads or erases a KV cache snapshot in the
/// local directory (`--slot-save-path`). The gateway composes the deployment
/// action by calling this route on each hop; see `gateway::snapshot_deployment`.
pub(super) async fn handle_slot(
    State(app): State<InferenceAppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SlotRequest>,
) -> Response {
    // TP: rank 0 forwards the same action to the workers, which serve the same
    // range and write their own copy to their disk. See `replicate_session_call`.
    replicate_slot_call(&app.state, &headers, &req);

    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let Some(dir) = state.slot_dir.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            "KV snapshots off on this peer (set slot_save_path)",
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
        other => err400(format!("unknown snapshot action: {other}")),
    }
}

/// `POST /v1/session/cache/clear` - drops every session parked in host RAM
/// (`--cache-ram`). Live and retained sessions keep their sequences: this only
/// gives back the memory of conversations that had already lost theirs.
pub(super) async fn handle_cache_clear(State(app): State<InferenceAppState>) -> Response {
    let mut state = match lock_inference_state(&app.state) {
        Ok(state) => state,
        Err(error) => return err500(error),
    };
    let (entries, bytes) = state.ram_cache.clear();
    drop(state);
    info!("RAM cache cleared: {entries} sessions, {bytes} bytes");
    Json(json!({ "cleared": true, "entries": entries, "bytes": bytes })).into_response()
}

pub(super) fn replicate_slot_call(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    req: &SlotRequest,
) {
    if headers.get(crate::tp::TP_DECODE_HEADER).is_some() {
        return; // already the replica
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("could not replicate the snapshot: state mutex poisoned");
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
    // The replica does not end a session on its own (see `tp::is_replica`),
    // so whoever releases it is this release - which the gateway sends **only to
    // rank 0**, because the worker is not a pipeline hop. Without forwarding, the
    // worker's session keeps holding the slot forever and the next request dies
    // with "seq pool esgotado (max 1 sessões simultâneas)" (18/08).
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

/// Confirms that the session stopped at the end of the prompt, ready for the
/// next turn.
///
/// **It rolls nothing back.** This turn's generation ran in a separate KV
/// sequence (see `Session`), so retaining the prompt is erasing that whole
/// sequence - total removal, which every engine accepts. Before this, what came
/// out here was the
/// `"este modelo não aceita recuar a sessão para o prompt"` that discarded the
/// prefix in 75 of 176 requests of a production day, and the
/// `"sessão desconhecida"` from when the EOS destroyed the session before this
/// request arrived.
///
/// Idempotent by construction: the normal end of the turn has already retained,
/// and calling again checks the same state. Only a real count divergence is a
/// refusal.
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
        return Json(json!({ "rewound": false, "reason": "unknown session" })).into_response();
    };
    if req.n_tokens != prompt_len {
        return Json(json!({
            "rewound": false,
            "reason": format!(
                "gateway prompt does not match the one of this session: {} != {prompt_len}",
                req.n_tokens
            ),
        }))
        .into_response();
    }
    let Some(retained) = state.retain_prompt(&req.session_id) else {
        return Json(json!({
            "rewound": false,
            "reason": "session without a prompt to retain",
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

/// Opens the decode in **both** roles and, on rank 0, puts the workers to run the
/// same batch.
///
/// TP only works in lockstep: every rank walks the same graph over the same
/// tokens, and the partials meet at the barrier. The gateway only talks to rank
/// 0 (the worker is deliberately excluded from the pipeline), so this is where
/// the other ranks enter the decode. Without this, rank 0 posted its partial and
/// waited forever - the 18/08 hang.
///
/// The frame is forwarded byte for byte, with the decode id in the header.
/// Whoever receives the header is a replica: it opens the decode with the
/// received id and does **not** retransmit, or the group would multiply at every
/// hop.
pub(super) fn open_tp_decode(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    route: &str,
    body: &Bytes,
) {
    if headers.get(BATCH_CHILD_HEADER).is_some() {
        return;
    }
    let tp = {
        let Ok(s) = lock_inference_state(state) else {
            warn!("could not open TP decode: state mutex poisoned");
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
        // Worker receiving a frame without the header: it came from outside the
        // group. Opening a decode here would produce reduces with an id nobody
        // else knows.
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    crate::tp::begin_decode(&id);
    crate::tp::broadcast_decode(&tp.members, route, &id, body.to_vec());
}

/// The sub-range that the sender ordered to execute, checked against the slice
/// this node loaded.
///
/// Whoever sends decides which piece runs; this node only checks that it can run
/// exactly that. What it does **not** do is adjust silently: running the whole
/// slice when a piece was asked for is exactly the failure mode that returns a
/// wrong answer with no error at all.
///
/// Returns the index of the local block where decoding must stop, or `None`
/// when the request covers the slice to the end (the common case, whole graph).
pub(super) fn resolve_exec_range(
    meta: &Value,
    slice_start: u32,
    slice_end: u32,
) -> std::result::Result<Option<u32>, String> {
    let exec_start = meta.get("exec_start").and_then(|v| v.as_u64());
    let exec_end = meta.get("exec_end").and_then(|v| v.as_u64());
    let (Some(exec_start), Some(exec_end)) = (exec_start, exec_end) else {
        // Without an instruction there is nothing to check: it runs the whole
        // slice, which is the contract of whoever only loads one range and owns
        // all of it.
        return Ok(None);
    };
    let (exec_start, exec_end) = (exec_start as u32, exec_end as u32);
    if exec_end < exec_start {
        return Err(format!("inverted sub-range: L{exec_start}-{exec_end}"));
    }
    if exec_start < slice_start || exec_end > slice_end {
        return Err(format!(
            "sub-range L{exec_start}-{exec_end} leaves the loaded slice L{slice_start}-{slice_end}"
        ));
    }
    if exec_start > slice_start {
        // The limit the captain knows: the hidden state enters through llama.cpp's
        // embedding, which always lands on block 0 of the slice. Starting above
        // the initial layer would require skipping head blocks, and the engine
        // does not know how to do that today. Refusing high is the contract;
        // silently redoing L{slice_start}-
        // L{exec_start} would be a wrong answer with no error.
        return Err(format!(
            "entering in the middle of the range is not possible yet: the request starts at L{exec_start}, but this slice starts at L{slice_start} and the engine always enters through the first block. Send the sub-range starting at L{slice_start}, or give this part to a peer that starts at L{exec_start}"
        ));
    }
    if exec_end == slice_end {
        return Ok(None);
    }
    Ok(Some(exec_end - slice_start))
}

#[cfg(test)]
mod tests {
    use super::run_native_cleanup;
    use axum::{routing::get, Router};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::oneshot;

    /// Reproduces the production failure on a single-thread executor: a
    /// synchronous native cleanup prevents an unrelated HTTP health request
    /// from being accepted until the cleanup returns.
    #[tokio::test(flavor = "current_thread")]
    async fn native_cleanup_must_not_block_health_during_reload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let (stop_tx, stop_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            crate::serve::serve_with_shutdown(listener, app, None, async move {
                let _ = stop_rx.await;
            })
            .await
        });
        tokio::task::yield_now().await;

        let cleanup_started = Arc::new(AtomicBool::new(false));
        let cleanup_started_by_task = cleanup_started.clone();
        let test_started = std::time::Instant::now();
        let cleanup = tokio::spawn(run_native_cleanup(move || {
            cleanup_started_by_task.store(true, Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(250));
        }));

        // On the buggy implementation this yield polls the cleanup task first
        // and blocks the only executor thread for the full native cleanup.
        tokio::task::yield_now().await;
        let health = tokio::spawn(async move {
            while !cleanup_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            reqwest::get(format!("http://{address}/health"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            test_started.elapsed()
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(75);
        let health_response = tokio::time::timeout_at(deadline, health).await;

        let elapsed = health_response
            .expect("a health request must be polled while native cleanup is running")
            .unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "health was not polled until native cleanup ended: {elapsed:?}"
        );
        cleanup.await.unwrap().unwrap();
        stop_tx.send(()).unwrap();
        server.await.unwrap().unwrap();
    }
}
