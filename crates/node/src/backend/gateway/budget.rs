use super::*;

/// Prompt tokens a peer running `cfg` can accept.
///
/// `n_ctx` is the context window for one request. `max_sessions` only controls
/// how many such windows the inference context must reserve at once.
///
/// `n_batch` deliberately does **not** appear here. It did while the prompt
/// crossed as a single `llama_batch`; since [`crate::prefill`] advances the
/// prompt in chunks, `n_batch` bounds one *chunk* — see
/// [`prompt_chunk_tokens`]. Leaving it in this formula capped every prompt at
/// 512 tokens no matter how large `--n-ctx` was, which is what a harness sending
/// a real conversation hits immediately.
pub fn prompt_budget(cfg: &crate::runtime::NodeConfig) -> u32 {
    prompt_budget_of(cfg.n_ctx, cfg.max_sessions, cfg.max_new_tokens)
}

/// The same figure from loose numbers, for a configuration that is still being
/// chosen and has no running node behind it yet — the deploy screen quotes it
/// while the operator is editing the knobs, and quoting it from anywhere but
/// here is how the two go out of step.
pub fn prompt_budget_of(n_ctx: u32, _max_sessions: u32, max_new_tokens: u32) -> u32 {
    // Room has to be left for the tokens we are about to generate.
    n_ctx.saturating_sub(max_new_tokens)
}

/// Tokens per prefill chunk — this is what `n_batch` actually constrains.
///
/// A chunk is decoded as one batch on the peer, so a configured chunk larger
/// than `n_batch` would abort llama.cpp exactly the way whole prompts used to.
pub fn prompt_chunk_tokens(cfg: &crate::runtime::NodeConfig) -> u32 {
    prompt_chunk_of(cfg.prefill_chunk_tokens, cfg.n_batch)
}

pub fn prompt_chunk_of(prefill_chunk_tokens: u32, n_batch: u32) -> u32 {
    prefill_chunk_tokens.max(1).min(n_batch.max(1))
}

/// Same clamp as `prompt_chunk_of`, for the same reason: a speculative batch
/// (`1 + drafted tokens`) is decoded as one batch on the head too, so it
/// can't exceed `n_batch` either. Trivially satisfied at the defaults (4 «
/// 512), but an operator's `speculative_k` shouldn't be trusted alone.
pub fn speculative_k_of(speculative_k: u32, n_batch: u32) -> u32 {
    speculative_k.max(1).min(n_batch.max(1))
}

// O protocolo antigo `/v1/infer` (tensor em base64, uma chamada por peer)
// saiu daqui: hoje o pipeline fala `/v1/prefill` e `/v1/step` com o
// enquadramento binario de `inference/wire.rs`, e o encadeamento dos hops
// mora em `inference/prefill.rs`.

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub model: Option<String>,
    pub prompt: String,
    /// Total completion-token budget. The first token is emitted by prefill
    /// but counts here, just as it does in the other HTTP APIs.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Option<StopInput>,
}

/// Os dois orçamentos de uma geração, que nunca são o mesmo número.
///
/// `leg` é o que **uma** geração de backend pode produzir - o `max_new_tokens`
/// da implantação, escolha do operador. `total` é o que a **requisição** pode
/// produzir. Esgotar a perna não encerra a resposta: o laço de continuação
/// abre a próxima e segue decodificando na mesma sessão e no mesmo fluxo SSE.
/// Confundir os dois é o defeito que este orçamento existe para impedir - era
/// exatamente ele que devolvia `finish_reason: "length"` ao cliente no meio de
/// uma chamada de ferramenta.
/// O `total` é **imposto e inviolável**: nada o ultrapassa, por motivo nenhum,
/// nem uma chamada de ferramenta aberta no token do corte. É a regra dos dois
/// provedores - `max_tokens` é teto por resposta, atingi-lo é parada terminal,
/// e a saída trunca no meio. O que existe no lugar do transbordo é o orçamento
/// de tarefa (`messages::TaskBudget`), que avisa o modelo em vez de esticar o
/// teto. Ver `docs/subsystems/inferencia.md` (9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    /// Teto da requisição inteira, somando todas as pernas.
    pub total: u32,
    /// Teto de uma perna.
    pub leg: u32,
}

impl OutputBudget {
    /// Um orçamento sem continuação: uma perna que já é a requisição inteira.
    /// É o que os testes e as bancadas usam quando o laço não é o assunto.
    pub fn single(total: u32) -> Self {
        Self { total, leg: total }
    }

    /// Números utilizáveis: nunca zero, e a perna nunca maior que o total.
    pub(super) fn normalized(self) -> Self {
        let total = self.total.max(1);
        Self {
            total,
            leg: self.leg.clamp(1, total),
        }
    }
}

/// The output budget visible to a protocol adapter after applying the selected
/// deployment and the remaining context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OutputTokenLimit {
    #[serde(rename = "requested_max_tokens")]
    pub requested: u32,
    /// O que a requisição inteira vai poder gerar. É o total do orçamento.
    #[serde(rename = "effective_max_tokens")]
    pub effective: u32,
    /// `max_new_tokens` da implantação. **Não** limita mais a requisição, só
    /// uma perna do laço de continuação.
    #[serde(rename = "deployment_max_tokens")]
    pub deployment: Option<u32>,
    #[serde(rename = "context_available_tokens")]
    pub context_available: Option<u32>,
    /// Tokens que cabem numa perna: `min(deployment, effective)`.
    #[serde(rename = "leg_max_tokens")]
    pub leg: u32,
    /// Um modelo com peer vivo sem capacidade confirmada não pode usar o
    /// fallback local com segurança. O campo é interno porque a requisição é
    /// recusada antes de qualquer resposta `x_pom` ser gerada.
    #[serde(skip)]
    pub(super) capacity_pending: bool,
}

impl OutputTokenLimit {
    pub fn was_clamped(self) -> bool {
        self.effective < self.requested
    }

    /// Os dois números que a geração precisa, sem o adaptador ter de escolher.
    pub fn budget(self) -> OutputBudget {
        OutputBudget {
            total: self.effective,
            leg: self.leg,
        }
    }

    /// Esta requisição pode precisar de mais de uma perna. Não é promessa de
    /// que vai precisar: o modelo pode parar antes.
    pub fn continuation_possible(self) -> bool {
        self.leg < self.effective
    }

    pub fn capacity_pending(self) -> bool {
        self.capacity_pending
    }

    pub fn rejection_reason(self) -> Option<String> {
        if self.capacity_pending {
            return Some(
                "a capacidade efetiva do modelo ainda não foi confirmada por todos os peers da pipeline planejada"
                    .into(),
            );
        } else if self.effective > 0 && self.leg > 0 {
            return None;
        }

        if self.context_available == Some(0) {
            Some(
                "o prompt ocupa todo o contexto disponível; não há espaço para tokens de saída"
                    .into(),
            )
        } else if self.deployment == Some(0) {
            Some("max_new_tokens da implantação precisa ser maior que zero".into())
        } else {
            Some("não há tokens de saída disponíveis para esta requisição".into())
        }
    }
}

/// Resolve one request's output budget.
///
/// Dois números saem daqui, e é de propósito. O **total** da requisição é
/// `min(pedido, contexto que sobrou)`: o `max_new_tokens` da implantação
/// **não** entra no total, porque ele nunca foi limite do backend -
/// sempre foi um corte do gateway, e cortar aí calado é o defeito. O que ele
/// vira é o teto de **uma perna**, que o laço de continuação atravessa quantas
/// vezes o total permitir. Nenhum adaptador de protocolo duplica esta conta.
pub fn output_token_limit(
    requested: u32,
    deployment: Option<u32>,
    context_length: Option<u32>,
    prompt_tokens: u32,
) -> OutputTokenLimit {
    let requested = requested.max(1);
    let context_available = context_length.map(|context| context.saturating_sub(prompt_tokens));
    let effective = [Some(requested), context_available]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0);
    // A perna nunca passa do total: pedir 10 com implantação de 1000 continua
    // sendo uma perna de 10, e o laço nem abre.
    let leg = deployment.unwrap_or(effective).min(effective);

    OutputTokenLimit {
        requested,
        effective,
        deployment,
        context_available,
        leg,
        capacity_pending: false,
    }
}

impl GatewayState {
    /// Resolve an adapter request against the live deployment configuration
    /// and the remaining context reported by the active runtime.
    pub async fn request_output_limit(&self, requested: u32, prompt: &str) -> OutputTokenLimit {
        self.request_output_limit_for_model(None, requested, prompt)
            .await
    }

    /// Resolve the request against the capacity of the peers selected by the
    /// current pipeline plan. The deployment leg remains separate from the
    /// request's context-derived total; idle planned peers are reloaded by
    /// generation before the first inference hop.
    pub async fn request_output_limit_for_model(
        &self,
        model: Option<&str>,
        requested: u32,
        prompt: &str,
    ) -> OutputTokenLimit {
        let runtime = self.runtime.config().await;
        let (capacity, capacity_pending) = self.runtime_capacity_for_model(model).await;
        if capacity_pending {
            return OutputTokenLimit {
                requested: requested.max(1),
                effective: 0,
                deployment: None,
                context_available: None,
                leg: 0,
                capacity_pending: true,
            };
        }
        output_token_limit(
            requested,
            capacity
                .map(|current| current.max_new_tokens)
                .or_else(|| runtime.as_ref().map(|cfg| cfg.max_new_tokens)),
            capacity
                .map(|current| current.n_ctx)
                .or_else(|| runtime.as_ref().map(|cfg| cfg.n_ctx)),
            estimate_tokens(prompt),
        )
    }

    /// Metadados publicados por `/v1/models`, derivados da mesma capacidade
    /// que a validação de uma requisição usa.
    pub async fn advertised_output_limits(
        &self,
        model: Option<&str>,
    ) -> (Option<u32>, Option<u32>) {
        let peers = self.all_peers().await;
        self.advertised_output_limits_from_peers(&peers, model)
            .await
    }

    /// Variante que recebe a mesma fotografia usada para descobrir o
    /// catálogo. Assim um peer não pode desaparecer ou entrar em restart entre
    /// a decisão do ID e a publicação de `max_completion_tokens`.
    pub async fn advertised_output_limits_from_peers(
        &self,
        peers: &[PeerInfo],
        model: Option<&str>,
    ) -> (Option<u32>, Option<u32>) {
        let runtime = self.runtime.config().await;
        let (capacity, capacity_pending) = match model {
            Some(model) => runtime_capacity_from_peers(peers, model),
            None => (self.local_runtime_capacity().await, false),
        };
        if capacity_pending {
            return (None, None);
        }
        let context = capacity
            .map(|current| current.n_ctx)
            .or_else(|| runtime.as_ref().map(|cfg| cfg.n_ctx));
        (context, context)
    }
}

pub fn log_output_limit(route: &str, limit: OutputTokenLimit) {
    if limit.capacity_pending() {
        info!(
            route,
            "capacidade efetiva aguardando confirmação dos peers da pipeline planejada"
        );
        return;
    }
    if limit.was_clamped() {
        info!(
            route,
            requested_max_tokens = limit.requested,
            effective_max_tokens = limit.effective,
            context_available_tokens = ?limit.context_available,
            "limite de saída reduzido pelo contexto"
        );
    }
    if limit.continuation_possible() {
        info!(
            route,
            effective_max_tokens = limit.effective,
            leg_max_tokens = limit.leg,
            deployment_max_tokens = ?limit.deployment,
            "saída maior que uma geração da implantação: o laço de continuação \
             costura as pernas no mesmo fluxo"
        );
    }
}

/// `/api/generate` valida apenas o orçamento pedido. A capacidade do runtime é
/// resolvida depois, a partir do contexto efetivo e da implantação ativa.
pub(super) fn resolve_request_max_tokens(requested: Option<u32>) -> Result<u32, String> {
    let max_tokens = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens == 0 {
        return Err("max_tokens deve ser maior que zero".to_string());
    }
    Ok(max_tokens)
}

pub(super) fn remaining_generation_steps(max_tokens: u32, tokens_generated: u32) -> u32 {
    max_tokens.saturating_sub(tokens_generated)
}

/// Decide whether a generation step has a terminal reason. The budget is
/// checked first because the backend may report EOS on the same step that
/// consumed the last permitted token; the public API must call that result
/// `length`, not a normal stop.
pub(super) fn generation_finish_reason(
    max_tokens: u32,
    tokens_generated: u32,
    model_done: bool,
) -> Option<&'static str> {
    if tokens_generated >= max_tokens {
        Some("length")
    } else if model_done {
        Some("stop")
    } else {
        None
    }
}

/// Audit has a distinct state for a response cut by its effective output
/// budget. Transport failures remain `partial`, even when they happened to
/// produce the same amount of text.
pub(super) fn audit_status_for_summary(summary: &GenSummary) -> &'static str {
    if summary.finish_reason.as_deref() == Some("length") {
        "incomplete"
    } else if summary.partial {
        "partial"
    } else {
        "ok"
    }
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "pipeline:<hop-chain>" | "local" | "peer:<node_id>" | "error"
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub timings: Option<GenerationTimings>,
    pub stop_sequence: Option<String>,
    /// Motivo terminal. `cycle` é uma interrupção explícita do detector de
    /// degenerescência, distinta de `stop` e do limite `length`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub partial: bool,
    pub trace: Option<crate::prefill::RequestTrace>,
}

pub(super) fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Serialize)]
pub struct PeersResponse {
    pub peers: Vec<PeerSummary>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct PeerSummary {
    pub node_id: String,
    pub addr: String,
    pub model: String,
    pub layers: String,
    pub ollama_url: Option<String>,
    pub inference_url: Option<String>,
    pub load: u32,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub ollama_available: bool,
    pub peer_count: usize,
    pub pipeline_peers: usize,
    pub default_model: String,
    /// Só fica presente quando o `/health` do servidor de inferência local
    /// confirmou o runtime desta geração.
    pub runtime_capacity: Option<RuntimeCapacity>,
}

// ── Streaming generation ─────────────────────────────────────────────────────

/// One event from a running generation.
#[derive(Debug, Clone)]
pub enum GenEvent {
    /// A newly decoded token's text.
    Token(String),
    /// Generation finished (possibly early — see `partial`).
    Done(GenSummary),
    /// Generation produced nothing usable.
    Failed(String),
}

/// Inputs shared by the blocking, streaming and logged generation adapters.
/// Keeping them together prevents each adapter from growing a subtly
/// different list of generation parameters.
pub(crate) struct GenerationInput {
    /// Correlation ID chosen at the HTTP boundary. When absent (for an
    /// internal/test caller), the audit adapter creates one before spawning.
    pub(crate) trace_id: Option<String>,
    pub(crate) state: GatewayState,
    /// API key validated at the client gateway. It is retained only as input
    /// to the generation adapter and is never sent to an inference peer.
    pub(crate) inference_token: String,
    pub(crate) model: String,
    pub(crate) prompt: String,
    /// Bytes das imagens já reduzidas pelo cliente. Só a cabeça do pipeline
    /// recebe este campo; os outros peers trabalham com o estado oculto.
    pub(crate) images: Vec<Vec<u8>>,
    pub(crate) chat_template: bool,
    /// Os dois tetos desta requisição: o da perna e o do total. Ver
    /// [`OutputBudget`].
    pub(crate) budget: OutputBudget,
    pub(crate) stops: Vec<String>,
    /// Como o cache de prefixo reconhece que esta requisição continua a
    /// anterior. `None` usa o próprio prompt.
    ///
    /// O texto renderizado não serve como chave: `render_prompt` devolve uma
    /// mensagem de usuário solitária **crua** e rotula a partir da segunda, de
    /// modo que o prompt do turno 1 não é prefixo do prompt do turno 2 e a
    /// transição 1→2 de qualquer chat nunca acertava. A chave vem do array de
    /// mensagens, que só cresce: imune a template, a rótulo e a bloco de
    /// ferramentas. Ver `crate::openai::conversation_cache_key`.
    pub(crate) cache_key: Option<String>,
    /// The parsed HTTP request body, retained for audit without headers or
    /// credentials.
    pub(crate) request_details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub timings: GenerationTimings,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
    /// "stop" when a server-side stop sequence ended the generation.
    pub finish_reason: Option<String>,
    /// The exact stop sequence that was matched, when one ended the stream.
    pub stop_sequence: Option<String>,
    /// Quantas pernas do laço de continuação a resposta costurou. `1` é uma
    /// geração só - nenhuma costura. Maior que `1` quer dizer que a saída
    /// passou do `max_new_tokens` da implantação sem que ele fosse alterado, e
    /// é o que torna a costura inspecionável de fora.
    pub legs: u32,
    /// Detailed per-request observations. It never affects planning.
    pub trace: Option<crate::prefill::RequestTrace>,
}

/// Resumo de desempenho de uma requisição, agregado sobre o pipeline inteiro.
/// Cada taxa usa seu próprio relógio e conjunto de tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationTimings {
    pub ttft_ms: u64,
    /// TTFT measured by the gateway, from generation ingress to first token
    /// emitted into the protocol adapter.
    pub server_ttft_ms: Option<u64>,
    /// TTFT measured locally by the inference peer. It is absent when the
    /// peer did not return a first-token marker.
    pub engine_ttft_ms: Option<u64>,
    /// TTFT measured by the browser with `performance.now()`.
    pub client_ttft_ms: Option<u64>,
    pub prefill_ms: u64,
    pub prefill_tokens: u32,
    pub prefill_tokens_per_sec: Option<f64>,
    pub generation_ms: u64,
    pub generation_tokens: u32,
    pub generation_tokens_per_sec: Option<f64>,
    pub total_ms: u64,
    pub total_tokens_per_sec: f64,
}

/// OpenAI's `stop` accepts either one string or a list. Keeping this parser in
/// the gateway lets all HTTP adapters share the same matcher and keeps the
/// internal inference protocol unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopInput {
    One(String),
    Many(Vec<String>),
}

impl StopInput {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(stop) => vec![stop],
            Self::Many(stops) => stops,
        }
    }
}

/// Normalize request-provided stop sequences once at the gateway boundary.
/// Empty strings cannot delimit meaningful generated text, and duplicates do
/// not change matching semantics.
pub fn normalize_stop_sequences(stops: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for stop in stops {
        if stop.is_empty() || normalized.iter().any(|known| known == &stop) {
            continue;
        }
        normalized.push(stop);
    }
    normalized
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopEmission {
    pub text: String,
    pub matched: Option<String>,
}

/// Incremental stop matcher for already-valid UTF-8 output.
///
/// A suffix that could still become a stop sequence is retained between
/// token events. This is deliberately byte-indexed only after `char_indices`
/// has found a UTF-8 boundary, so a split multibyte character is never cut.
#[derive(Debug, Clone)]
pub struct StopMatcher {
    pub(super) stops: Vec<String>,
    pub(super) pending: String,
    pub(super) matched: Option<String>,
}

impl StopMatcher {
    pub fn new(stops: impl IntoIterator<Item = String>) -> Self {
        Self {
            stops: normalize_stop_sequences(stops),
            pending: String::new(),
            matched: None,
        }
    }

    pub fn feed(&mut self, text: &str) -> StopEmission {
        if self.matched.is_some() {
            return StopEmission {
                text: String::new(),
                matched: self.matched.clone(),
            };
        }

        self.pending.push_str(text);
        if self.stops.is_empty() {
            return StopEmission {
                text: std::mem::take(&mut self.pending),
                matched: None,
            };
        }

        let mut match_at: Option<(usize, usize)> = None;
        let mut matched_stop = None;
        for (index, stop) in self.stops.iter().enumerate() {
            if let Some(found) = self.pending.find(stop) {
                if match_at.is_none_or(|current| (found, index) < current) {
                    match_at = Some((found, index));
                    matched_stop = Some(stop.clone());
                }
            }
        }

        if let Some((index, _)) = match_at {
            let emitted = self.pending[..index].to_string();
            self.pending.clear();
            self.matched = matched_stop.clone();
            return StopEmission {
                text: emitted,
                matched: matched_stop,
            };
        }

        let hold = self
            .pending
            .char_indices()
            .filter_map(|(index, _)| {
                let suffix = &self.pending[index..];
                self.stops
                    .iter()
                    .any(|stop| stop.starts_with(suffix))
                    .then_some(suffix.len())
            })
            .max()
            .unwrap_or(0);
        let emit_len = self.pending.len().saturating_sub(hold);
        let emitted = self.pending[..emit_len].to_string();
        self.pending.drain(..emit_len);
        StopEmission {
            text: emitted,
            matched: None,
        }
    }

    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StopSend {
    Continue,
    Stopped(String),
    Disconnected,
}

pub(super) async fn send_text_with_stop(
    tx: &mpsc::Sender<GenEvent>,
    matcher: &mut StopMatcher,
    text: &str,
) -> StopSend {
    let emission = matcher.feed(text);
    if !emission.text.is_empty() && tx.send(GenEvent::Token(emission.text)).await.is_err() {
        return StopSend::Disconnected;
    }
    match emission.matched {
        Some(stop) => StopSend::Stopped(stop),
        None => StopSend::Continue,
    }
}

pub(super) async fn flush_stop_matcher(
    tx: &mpsc::Sender<GenEvent>,
    matcher: &mut StopMatcher,
) -> bool {
    let tail = matcher.finish();
    tail.is_empty() || tx.send(GenEvent::Token(tail)).await.is_ok()
}

pub(super) fn rate_if_measurable(tokens: u32, milliseconds: u64) -> Option<f64> {
    (tokens > 0 && milliseconds > 0).then(|| tokens as f64 / (milliseconds as f64 / 1000.0))
}

/// Calcula taxas com relógios que não se misturam. `generation_ms` começa no
/// primeiro token e, portanto, não varia com o tamanho do prompt.
pub(crate) fn timing_metrics(
    total_ms: u64,
    ttft_ms: u64,
    prefill_ms: u64,
    prefill_tokens: u32,
    generation_tokens: u32,
) -> GenerationTimings {
    let generation_ms = total_ms.saturating_sub(ttft_ms);
    GenerationTimings {
        ttft_ms,
        server_ttft_ms: None,
        engine_ttft_ms: None,
        client_ttft_ms: None,
        prefill_ms,
        prefill_tokens,
        prefill_tokens_per_sec: rate_if_measurable(prefill_tokens, prefill_ms),
        generation_ms,
        generation_tokens,
        generation_tokens_per_sec: rate_if_measurable(generation_tokens, generation_ms),
        total_ms,
        total_tokens_per_sec: rate(generation_tokens, total_ms as f64 / 1000.0),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn summary_for(
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    mut trace: crate::prefill::RequestTrace,
    partial: bool,
    finish_reason: Option<String>,
    stop_sequence: Option<String>,
) -> GenSummary {
    let total_ms = request_started.elapsed().as_millis() as u64;
    let ttft_ms = first_token_at
        .saturating_duration_since(request_started)
        .as_millis() as u64;
    trace.output_tokens = tokens_generated;
    trace.set_generated_tokens(tokens_generated);
    trace.complete = !partial;
    trace.server_ttft_ms = Some(ttft_ms);
    trace.set_outside_node_total_ms(
        total_ms.saturating_sub(trace.node_processing_ms()),
    );
    let mut timings = timing_metrics(
        total_ms,
        ttft_ms,
        trace.prefill_ms,
        trace.input_tokens.saturating_sub(trace.cached_tokens),
        tokens_generated,
    );
    timings.server_ttft_ms = Some(ttft_ms);
    timings.engine_ttft_ms = trace.engine_ttft_ms;
    timings.client_ttft_ms = trace.client_ttft_ms;
    GenSummary {
        source: format!(
            "pipeline:{}{}",
            plan.hop_chain(),
            if partial { " (partial)" } else { "" }
        ),
        tokens_generated,
        tokens_per_sec: timings.total_tokens_per_sec,
        duration_ms: timings.total_ms,
        timings,
        partial,
        finish_reason,
        stop_sequence,
        legs,
        trace: Some(trace),
    }
}

/// Contabiliza um token confirmado contra o orçamento de perna.
///
/// A perna é o teto de **uma** geração de backend (`max_new_tokens` da
/// implantação). Fechá-la não encerra a resposta: o laço de continuação abre a
/// seguinte e segue decodificando na mesma sessão de KV e no mesmo fluxo SSE,
/// então nada disso é observável do lado do cliente. Só o total encerra, e só
/// ele vira `finish_reason: "length"`.
pub(super) fn note_leg_boundary(budget: OutputBudget, tokens_generated: u32, legs: &mut u32) {
    if tokens_generated.is_multiple_of(budget.leg) && tokens_generated < budget.total {
        *legs += 1;
        info!(
            legs = *legs,
            tokens_generated,
            leg_max_tokens = budget.leg,
            total_max_tokens = budget.total,
            "perna esgotada: continuando a mesma geração na perna seguinte"
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn send_stopped_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
    stop_sequence: String,
) {
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        false,
        Some("stop".to_string()),
        Some(stop_sequence),
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

pub(super) async fn send_length_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
) {
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        false,
        Some("length".to_string()),
        None,
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

pub(super) async fn send_cycle_summary(
    tx: &mpsc::Sender<GenEvent>,
    plan: &PipelinePlan,
    request_started: Instant,
    first_token_at: Instant,
    tokens_generated: u32,
    legs: u32,
    trace: crate::prefill::RequestTrace,
    cycle: crate::cycle::CycleMatch,
) {
    info!(
        tokens_generated,
        period_tokens = cycle.period,
        repetitions = cycle.repetitions,
        "ciclo de repetição detectado; encerrando a geração"
    );
    let summary = summary_for(
        plan,
        request_started,
        first_token_at,
        tokens_generated,
        legs,
        trace,
        true,
        Some("cycle".to_string()),
        None,
    );
    let _ = tx.send(GenEvent::Done(summary)).await;
}

/// Run one generation, emitting each token as it is decoded.
///
/// This is the single generation path in the gateway: `/api/generate` collects
/// the events into one response, the OpenAI endpoints forward them as SSE.
/// The established path keeps one round trip through every peer per token;
/// `remote-stream` moves the decode loop to the remote head and reports
/// opportunistic batches through the callback.
pub async fn generate_stream_with_stops(input: GenerationInput, tx: mpsc::Sender<GenEvent>) {
    let cancellation = crate::prefill::Cancellation::new();
    let monitor_cancel = cancellation.clone();
    let monitor_tx = tx.clone();
    let monitor = tokio::spawn(async move {
        monitor_tx.closed().await;
        monitor_cancel.cancel();
    });

    generate_stream_inner(input, tx, cancellation).await;
    monitor.abort();
}

pub(super) fn remote_stream_capable(
    plan: &PipelinePlan,
    local_node_id: &str,
    direct_reply_available: bool,
) -> bool {
    !plan.steps.is_empty()
        && plan.steps[0].node_id != local_node_id
        && plan.steps[0].supports(crate::pipeline::CAP_REMOTE_TOKEN_STREAM)
        && direct_reply_available
}

pub(super) fn remote_stream_speculation(
    plan: &PipelinePlan,
    speculative_enabled: bool,
    mtp_enabled: bool,
    speculative_k: u32,
    speculative_m: u32,
    mtp_k: u32,
    draft_source: crate::prefill::DraftSource,
) -> Option<crate::infer_server::Speculate> {
    let sliced_mtp = mtp_enabled && plan.supports_mtp() && plan.steps.len() >= 2;
    if sliced_mtp {
        return Some(crate::infer_server::Speculate {
            k: mtp_k,
            m: speculative_m,
            source: crate::infer_server::DraftSource::Mtp,
            draft: None,
        });
    }
    (plan.steps.len() == 1 && plan.supports_speculation() && speculative_enabled).then(|| {
        crate::infer_server::Speculate {
            k: speculative_k,
            m: speculative_m,
            source: match draft_source {
                crate::prefill::DraftSource::Lookup => crate::infer_server::DraftSource::Lookup,
                crate::prefill::DraftSource::Mtp => crate::infer_server::DraftSource::Mtp,
            },
            draft: None,
        }
    })
}

pub(super) fn sliced_mtp_disable_reason(plan: &PipelinePlan) -> &'static str {
    if !plan
        .steps
        .iter()
        .all(|step| step.supports(crate::pipeline::CAP_SPECULATE_MTP_SLICED))
        || !plan
            .steps
            .iter()
            .all(|step| step.supports(crate::pipeline::CAP_SPECULATE_LOOKUP))
    {
        crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
    } else if !plan
        .steps
        .last()
        .is_some_and(|step| step.supports(crate::pipeline::CAP_SPECULATE_MTP))
    {
        crate::infer_server::SPECULATION_REASON_MODEL_WITHOUT_MTP
    } else {
        crate::infer_server::SPECULATION_REASON_PEER_WITHOUT_SLICED_MTP_WIRE
    }
}

pub(super) fn sliced_mtp_round_continues(
    remote_stream_mode: bool,
    draft_sent: bool,
    step: &crate::prefill::SpeculativeStep,
) -> bool {
    if remote_stream_mode {
        step.speculation_active
    } else {
        !((draft_sent && !step.speculated) || (!draft_sent && step.next_draft.is_none()))
    }
}

pub(super) fn gateway_rollback_needed(
    remote_stream_mode: bool,
    accept_count: u32,
    batch_width: u32,
) -> bool {
    !remote_stream_mode && accept_count < batch_width
}
