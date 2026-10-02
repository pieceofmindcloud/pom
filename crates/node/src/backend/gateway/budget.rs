use super::*;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

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

/// Time of one call of a pipeline stage as `fixed + per_token * tokens`,
/// fitted by weighted least squares over the points seen so far. Older points
/// decay, so the fit follows a route that changes.
#[derive(Debug, Clone, Copy, Default)]
struct StageFit {
    weight: f64,
    sum_x: f64,
    sum_y: f64,
    sum_xx: f64,
    sum_xy: f64,
}

impl StageFit {
    fn decay(&mut self, factor: f64) {
        self.weight *= factor;
        self.sum_x *= factor;
        self.sum_y *= factor;
        self.sum_xx *= factor;
        self.sum_xy *= factor;
    }

    fn add(&mut self, tokens: f64, ms: f64) {
        if !(tokens > 0.0 && ms.is_finite() && ms >= 0.0) {
            return;
        }
        self.weight += 1.0;
        self.sum_x += tokens;
        self.sum_y += ms;
        self.sum_xx += tokens * tokens;
        self.sum_xy += tokens * ms;
    }

    /// `(fixed_ms, ms_per_token)`. With a single chunk size seen, the slope
    /// is unknown and the whole cost is treated as per token (no fixed part),
    /// which never makes chunks smaller than the data supports.
    fn line(&self) -> Option<(f64, f64)> {
        if self.weight <= 0.0 || self.sum_x <= 0.0 {
            return None;
        }
        let mean_x = self.sum_x / self.weight;
        let mean_y = self.sum_y / self.weight;
        let var_x = self.sum_xx / self.weight - mean_x * mean_x;
        let per_token = if var_x > 1e-6 {
            (self.sum_xy / self.weight - mean_x * mean_y) / var_x
        } else {
            mean_y / mean_x
        };
        let per_token = per_token.max(1e-4);
        let fixed = (mean_y - per_token * mean_x).max(0.0);
        Some((fixed, per_token))
    }
}

/// What the tuner knows about one route: for every hop, the link into it and
/// its compute, each as a fixed-plus-per-token stage.
#[derive(Debug, Clone, Default)]
struct AdaptivePrefillProfile {
    /// Pipeline order: link into hop 0, hop 0, link into hop 1, hop 1, ...
    stages: Vec<StageFit>,
}

/// Learns from completed prefill traces and chooses a chunk without waiting on
/// the request path. The configured chunk and `n_batch` remain hard ceilings.
///
/// The choice simulates the chunked pipeline: every hop and every link is a
/// stage whose call costs `fixed + per_token * chunk`, chunks flow through
/// the stages in order with at most `max_inflight` of them in flight, and the
/// chunk size with the earliest finish of the last chunk wins. The fixed part
/// is what the previous heuristic missed: on a fast LAN it read a near-zero
/// RTT and cut a 199-token prompt into five 48-token chunks, paying the
/// per-call overhead (GPU sync, frame encode) five times on every hop.
#[derive(Debug, Default)]
pub(crate) struct AdaptiveChunkTuner {
    profiles: StdMutex<HashMap<String, AdaptivePrefillProfile>>,
}

const MIN_CHUNK: u32 = 32;
const CHUNK_STEP: u32 = 16;
/// Weight kept by older observations each time a trace is added.
const FIT_DECAY: f64 = 0.8;

impl AdaptiveChunkTuner {
    pub(crate) fn default_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Chunk size for a prompt of about `prompt_tokens` new tokens.
    pub(crate) fn choose(
        &self,
        route: &str,
        configured_max: u32,
        n_batch: u32,
        prompt_tokens: u32,
        max_inflight: usize,
    ) -> u32 {
        let ceiling = prompt_chunk_of(configured_max, n_batch);
        let profile = self
            .profiles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(route)
            .cloned();
        profile.map_or(ceiling, |profile| {
            choose_from_profile(ceiling, &profile, prompt_tokens, max_inflight)
        })
    }

    pub(crate) fn observe(
        &self,
        route: &str,
        trace: &crate::prefill::RequestTrace,
        prompt_tokens: u32,
        cached_tokens: u32,
    ) {
        let processed = f64::from(prompt_tokens.saturating_sub(cached_tokens));
        if processed <= 0.0 || trace.hops.is_empty() {
            return;
        }
        let chunk = f64::from(trace.prefill_chunk_tokens);
        let mut points: Vec<Vec<(f64, f64)>> = Vec::with_capacity(trace.hops.len() * 2);
        for hop in &trace.hops {
            let phase = &hop.prefill;
            if phase.calls == 0 {
                return;
            }
            let calls = f64::from(phase.calls);
            let mean_tokens = processed / calls;
            // Link into the hop: the bytes scale with the chunk, the round
            // trip does not.
            let rtt = phase
                .transport_summary
                .transports
                .iter()
                .filter(|transport| transport.calls > 0)
                .map(|transport| transport.net_rtt_median_ms as f64)
                .fold(0.0, f64::max);
            let wire = (phase.network_ms as f64 / calls - rtt).max(0.0);
            points.push(vec![
                (mean_tokens, rtt + wire),
                (mean_tokens * 2.0, rtt + wire * 2.0),
            ]);
            // Compute: the mean call (a partial last chunk pulls it below a
            // full one) and the median call, which is a full chunk.
            let mut compute = vec![(mean_tokens, phase.processing_ms as f64 / calls)];
            if chunk > 0.0 && phase.node_p50_ms > 0.0 && (chunk - mean_tokens).abs() > 0.5 {
                compute.push((chunk.min(processed), phase.node_p50_ms));
            }
            points.push(compute);
        }

        let mut profiles = self
            .profiles
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let profile = profiles.entry(route.to_string()).or_default();
        if profile.stages.len() != points.len() {
            profile.stages = vec![StageFit::default(); points.len()];
        }
        for (stage, samples) in profile.stages.iter_mut().zip(points) {
            stage.decay(FIT_DECAY);
            for (tokens, ms) in samples {
                stage.add(tokens, ms);
            }
        }
    }
}

/// Finish time of the last chunk when `tokens` go through `stages` in chunks
/// of `chunk`, at most `max_inflight` in flight.
fn simulate_pipeline(stages: &[(f64, f64)], tokens: u32, chunk: u32, max_inflight: usize) -> f64 {
    let chunk = chunk.max(1);
    let count = tokens.div_ceil(chunk).max(1) as usize;
    let window = max_inflight.max(1);
    // done[s] for the previous chunk; finished[j] = last-stage finish of chunk j.
    let mut done = vec![0.0_f64; stages.len()];
    let mut finished: Vec<f64> = Vec::with_capacity(count);
    for index in 0..count {
        let size = if index + 1 == count {
            tokens - chunk * (count as u32 - 1)
        } else {
            chunk
        } as f64;
        let mut ready = if index >= window {
            finished[index - window]
        } else {
            0.0
        };
        for (stage, (fixed, per_token)) in stages.iter().enumerate() {
            let start = ready.max(done[stage]);
            done[stage] = start + fixed + per_token * size;
            ready = done[stage];
        }
        finished.push(ready);
    }
    finished.last().copied().unwrap_or(0.0)
}

fn choose_from_profile(
    ceiling: u32,
    profile: &AdaptivePrefillProfile,
    prompt_tokens: u32,
    max_inflight: usize,
) -> u32 {
    if ceiling <= MIN_CHUNK || prompt_tokens == 0 {
        return ceiling;
    }
    let Some(stages) = profile
        .stages
        .iter()
        .map(StageFit::line)
        .collect::<Option<Vec<_>>>()
    else {
        return ceiling;
    };
    let mut candidates: Vec<u32> = (MIN_CHUNK..ceiling).step_by(CHUNK_STEP as usize).collect();
    candidates.push(ceiling);
    if prompt_tokens < ceiling {
        // One chunk holding the whole prompt.
        candidates.retain(|chunk| *chunk < prompt_tokens);
        candidates.push(prompt_tokens);
    }
    // Ties go to the larger chunk: fewer calls, fewer chances to fail.
    candidates
        .into_iter()
        .map(|chunk| {
            (
                chunk,
                simulate_pipeline(&stages, prompt_tokens, chunk, max_inflight),
            )
        })
        .fold(None, |best: Option<(u32, f64)>, (chunk, time)| match best {
            Some((_, best_time)) if time > best_time + 1e-9 => best,
            Some((best_chunk, best_time))
                if (time - best_time).abs() <= 1e-9 && best_chunk > chunk =>
            {
                best
            }
            _ => Some((chunk, time)),
        })
        // One chunk holding the whole prompt wins: send the ceiling, since the
        // gateway's token count is an estimate and a slightly longer prompt
        // must not spill into a tiny second chunk.
        .map_or(ceiling, |(chunk, _)| {
            if chunk >= prompt_tokens {
                ceiling
            } else {
                chunk.max(1)
            }
        })
}

/// Same clamp as `prompt_chunk_of`, for the same reason: a speculative batch
/// (`1 + drafted tokens`) is decoded as one batch on the head too, so it
/// can't exceed `n_batch` either. Trivially satisfied at the defaults (4 «
/// 512), but an operator's `speculative_k` shouldn't be trusted alone.
pub fn speculative_k_of(speculative_k: u32, n_batch: u32) -> u32 {
    speculative_k.max(1).min(n_batch.max(1))
}

/// Decide se a geração pode pedir MTP para o plano selecionado.
///
/// Um gateway sem modelo local não tem `NodeConfig` para consultar. Nesse
/// caso, a capacidade anunciada pelo plano é a evidência da execução remota.
/// `Some(false)` continua sendo um kill-switch explícito do nó que inicia a
/// geração.
pub(super) fn mtp_enabled_for_plan(
    local_mtp_enabled: Option<bool>,
    plan_supports_mtp: bool,
) -> bool {
    plan_supports_mtp && local_mtp_enabled.unwrap_or(true)
}

#[cfg(test)]
mod adaptive_chunk_tests {
    use super::*;

    fn fitted(points: &[(f64, f64)]) -> StageFit {
        let mut fit = StageFit::default();
        for (tokens, ms) in points {
            fit.add(*tokens, *ms);
        }
        fit
    }

    fn profile(stages: &[(f64, f64)]) -> AdaptivePrefillProfile {
        // Two exact points per stage reproduce `fixed + per_token * x`.
        AdaptivePrefillProfile {
            stages: stages
                .iter()
                .map(|(fixed, per_token)| {
                    fitted(&[
                        (10.0, fixed + per_token * 10.0),
                        (100.0, fixed + per_token * 100.0),
                    ])
                })
                .collect(),
        }
    }

    #[test]
    fn the_fit_separates_fixed_and_per_token_cost() {
        // The JEV trace: hop 2 had a 48-token median call of 79.8 ms and a
        // 39.8-token mean call of 71.2 ms.
        let (fixed, per_token) = fitted(&[(39.8, 71.2), (48.0, 79.8)]).line().unwrap();
        assert!((per_token - 1.049).abs() < 0.01, "{per_token}");
        assert!((fixed - 29.5).abs() < 0.5, "{fixed}");
        // A single chunk size: no slope to fit, all cost is per token.
        assert_eq!(fitted(&[(50.0, 100.0)]).line().unwrap(), (0.0, 2.0));
    }

    #[test]
    fn a_short_prompt_with_per_call_overhead_goes_in_one_chunk() {
        // Fixed/per-token costs fitted from the JEV trace (two hops on a LAN).
        let jev = profile(&[(1.0, 0.0), (20.0, 0.1), (0.0, 0.1), (29.5, 1.05)]);
        let forty_eight = simulate_pipeline(
            &jev.stages
                .iter()
                .map(|s| s.line().unwrap())
                .collect::<Vec<_>>(),
            199,
            48,
            8,
        );
        let chosen = choose_from_profile(512, &jev, 199, 8);
        let best = simulate_pipeline(
            &jev.stages
                .iter()
                .map(|s| s.line().unwrap())
                .collect::<Vec<_>>(),
            199,
            chosen,
            8,
        );
        assert_eq!(chosen, 512);
        assert!(best < forty_eight * 0.8, "{best} vs {forty_eight}");
    }

    #[test]
    fn a_long_prompt_still_overlaps_when_calls_are_cheap() {
        // Small fixed cost, real per-token cost on two balanced hops and a
        // slow link: several chunks in flight beat one big call.
        let route = profile(&[(1.0, 0.0), (2.0, 1.0), (2.0, 0.5), (2.0, 1.0)]);
        let chosen = choose_from_profile(512, &route, 4000, 8);
        assert!(chosen < 512, "{chosen}");
        assert!(chosen >= MIN_CHUNK);
    }

    #[test]
    fn the_inflight_window_limits_overlap() {
        let stages = [(0.0, 1.0), (0.0, 1.0)];
        // Unlimited window: the second stage trails by one chunk.
        assert_eq!(simulate_pipeline(&stages, 100, 10, 100), 110.0);
        // One chunk in flight: no overlap at all.
        assert_eq!(simulate_pipeline(&stages, 100, 10, 1), 200.0);
    }

    #[test]
    fn unmeasured_route_uses_configured_limit_without_waiting() {
        let tuner = AdaptiveChunkTuner::default_arc();
        assert_eq!(tuner.choose("model|head|tail", 512, 256, 1000, 8), 256);
    }
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
/// teto. Ver `docs/subsystems/inference.md` (9.1).
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

    /// The total fits in one session's window: real prompt plus output never
    /// exceeds `window` (the `n_ctx_per_session` announced by `/health`).
    ///
    /// Admission only had the prompt estimate (`estimate_tokens`); after
    /// prefill the real size is known, and a prompt denser than the estimate
    /// let the generation cross the window - with the unified KV cache,
    /// taking another session's cells and, at the limit, positions beyond the
    /// training context. Reaching the window is a `length` stop, like reaching
    /// the client's `max_tokens`. The first token already came with the
    /// prefill, so the total never drops below one.
    pub(super) fn within_session_window(self, window: Option<u32>, prompt_tokens: u32) -> Self {
        let Some(window) = window.filter(|window| *window > 0) else {
            return self;
        };
        let total = self.total.min(window.saturating_sub(prompt_tokens)).max(1);
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
    // A typed decision (`/v1/systemone`) asks for exactly one token, so it
    // always ends by `length`: that is its normal, complete outcome.
    let decision = summary
        .trace
        .as_ref()
        .is_some_and(|trace| trace.jev.is_some() || trace.probe.is_some());
    if decision {
        if summary.partial {
            "partial"
        } else {
            "ok"
        }
    } else if summary.finish_reason.as_deref() == Some("length") {
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
    /// Generation failed after collecting partial prefill telemetry.
    FailedWithTelemetry {
        error: String,
        trace: Option<crate::prefill::RequestTrace>,
    },
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
    /// Escolha do usuário para esta inferência. O peer head ainda aplica o
    /// kill-switch global da implantação antes de renderizar o template.
    pub(crate) thinking_enabled: bool,
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
    /// Explicit layer-0 peer selected by the Chat UI, when any. The planner
    /// validates it against the live model heads before honoring it.
    pub(crate) preferred_head: Option<String>,
    /// The parsed HTTP request body, retained for audit without headers or
    /// credentials.
    pub(crate) request_details: serde_json::Value,
    /// Rótulos de uma decisão tipada (`/v1/systemone`). O último peer devolve a
    /// probabilidade de cada um em `GenSummary::trace.probe`.
    pub(crate) probe: Option<crate::prefill::DecisionProbe>,
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
    /// Primeiro evento observável do motor local. Nulo quando o backend não
    /// publicou uma marca temporal explícita.
    pub local_engine_first_event_ms: Option<u64>,
    /// Primeiro token que atravessou o pipeline até o gateway.
    pub pipeline_first_token_ms: u64,
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
        local_engine_first_event_ms: None,
        pipeline_first_token_ms: ttft_ms,
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
    trace.reconcile_direct_receipts();
    // Do not manufacture `outside_node` by subtracting accumulated peer
    // processing from E2E. Those clocks are wall-clock and accumulated work,
    // respectively, and the distributed pipeline overlaps them. The fields
    // remain available only when an explicit, causally measured residual is
    // supplied by a future synchronized trace.
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
    trace.refresh_telemetry(total_ms);
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
