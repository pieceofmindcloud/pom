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

pub(crate) const MIN_CHUNK: u32 = 32;
/// The smallest chunk while the MoE expert cache is on. The cache serves
/// batches of up to 32 tokens as generation, uploading the experts they miss:
/// a 32-token prefill chunk went through it, crossed PCIe for the experts of
/// every chunk and evicted the ones generation keeps hot. Above that, prefill
/// takes the regular offload.
pub(crate) const MOE_CACHE_MIN_CHUNK: u32 = 64;
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
        min_chunk: u32,
    ) -> u32 {
        let ceiling = prompt_chunk_of(configured_max, n_batch);
        let profile = self
            .profiles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(route)
            .cloned();
        profile.map_or(ceiling, |profile| {
            choose_from_profile(ceiling, &profile, prompt_tokens, max_inflight, min_chunk)
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
    min_chunk: u32,
) -> u32 {
    if ceiling <= min_chunk || prompt_tokens == 0 {
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
    let mut candidates: Vec<u32> = (min_chunk..ceiling).step_by(CHUNK_STEP as usize).collect();
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

/// Decide whether generation may ask for MTP on the selected plan.
///
/// A gateway with no local model has no `NodeConfig` to consult. In that case,
/// the capacity announced by the plan is the evidence of remote execution.
/// `Some(false)` remains an explicit kill-switch of the node that starts the
/// generation.
pub(super) fn mtp_enabled_for_plan(
    local_mtp_enabled: Option<bool>,
    plan_supports_mtp: bool,
) -> bool {
    plan_supports_mtp && local_mtp_enabled.unwrap_or(true)
}

#[cfg(test)]
mod adaptive_chunk_tests {
    use super::*;

    /// With the MoE expert cache on, a prefill chunk never falls to the
    /// cache's generation batch (32 tokens or fewer).
    #[test]
    fn with_the_moe_cache_no_chunk_is_a_generation_batch() {
        let mut profile = AdaptivePrefillProfile::default();
        // Two hops with a tiny fixed cost and a large per-token cost: the
        // simulation alone would pick the smallest chunk.
        profile.stages = vec![StageFit::default(); 4];
        for stage in &mut profile.stages {
            stage.add(32.0, 3.2);
            stage.add(512.0, 51.2);
        }
        assert_eq!(
            choose_from_profile(512, &profile, 4000, 8, MIN_CHUNK),
            MIN_CHUNK
        );
        assert!(
            choose_from_profile(512, &profile, 4000, 8, MOE_CACHE_MIN_CHUNK) >= MOE_CACHE_MIN_CHUNK
        );
    }

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
        let chosen = choose_from_profile(512, &jev, 199, 8, MIN_CHUNK);
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
        let chosen = choose_from_profile(512, &route, 4000, 8, MIN_CHUNK);
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
        assert_eq!(
            tuner.choose("model|head|tail", 512, 256, 1000, 8, MIN_CHUNK),
            256
        );
    }
}

// The old `/v1/infer` protocol (base64 tensor, one call per peer)
// left here: today the pipeline speaks `/v1/prefill` and `/v1/step` with the
// binary framing of `inference/wire.rs`, and the chaining of the hops
// lives in `inference/prefill.rs`.

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

/// The two budgets of a generation, which are never the same number.
///
/// `leg` is what **one** backend generation can produce - the deployment's
/// `max_new_tokens`, the operator's choice. `total` is what the **request** can
/// produce. Exhausting the leg does not end the reply: the continuation loop
/// opens the next one and keeps decoding in the same session and the same SSE
/// stream. Confusing the two is the defect this budget exists to prevent - it
/// was exactly what returned `finish_reason: "length"` to the client halfway
/// through a tool call.
/// The `total` is **enforced and inviolable**: nothing crosses it, for no
/// reason, not even a tool call left open on the cut token. That is the rule of
/// both providers - `max_tokens` is a per-response ceiling, reaching it is a
/// terminal stop, and the output truncates halfway. What stands in place of
/// overshoot is the task budget (`messages::TaskBudget`), which warns the model
/// instead of stretching the ceiling. See `docs/subsystems/inference.md` (9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputBudget {
    /// Ceiling of the whole request, summing all the legs.
    pub total: u32,
    /// Ceiling of one leg.
    pub leg: u32,
}

impl OutputBudget {
    /// A budget without continuation: one leg that is already the whole request.
    /// It is what tests and benches use when the loop is not the subject.
    pub fn single(total: u32) -> Self {
        Self { total, leg: total }
    }

    /// Usable numbers: never zero, and the leg never larger than the total.
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
    /// What the whole request will be able to generate. It is the budget's total.
    #[serde(rename = "effective_max_tokens")]
    pub effective: u32,
    /// `max_new_tokens` of the deployment. It **no longer** limits the request, only
    /// one leg of the continuation loop.
    #[serde(rename = "deployment_max_tokens")]
    pub deployment: Option<u32>,
    #[serde(rename = "context_available_tokens")]
    pub context_available: Option<u32>,
    /// Tokens that fit in one leg: `min(deployment, effective)`.
    #[serde(rename = "leg_max_tokens")]
    pub leg: u32,
    /// A model with a live peer without confirmed capacity cannot safely use the
    /// local fallback. The field is internal because the request is
    /// refused before any `x_pom` response is generated.
    #[serde(skip)]
    pub(super) capacity_pending: bool,
}

impl OutputTokenLimit {
    pub fn was_clamped(self) -> bool {
        self.effective < self.requested
    }

    /// The two numbers that generation needs, without the adapter having to choose.
    pub fn budget(self) -> OutputBudget {
        OutputBudget {
            total: self.effective,
            leg: self.leg,
        }
    }

    /// This request may need more than one leg. It is not a promise
    /// that it will need to: the model may stop earlier.
    pub fn continuation_possible(self) -> bool {
        self.leg < self.effective
    }

    pub fn capacity_pending(self) -> bool {
        self.capacity_pending
    }

    pub fn rejection_reason(self) -> Option<String> {
        if self.capacity_pending {
            return Some(
                "the effective capacity of the model has not been confirmed yet by all the peers of the planned pipeline"
                    .into(),
            );
        } else if self.effective > 0 && self.leg > 0 {
            return None;
        }

        if self.context_available == Some(0) {
            Some(
                "the prompt takes the whole available context; there is no room for output tokens"
                    .into(),
            )
        } else if self.deployment == Some(0) {
            Some("the deployment max_new_tokens must be greater than zero".into())
        } else {
            Some("there are no output tokens available for this request".into())
        }
    }
}

/// Resolve one request's output budget.
///
/// Two numbers come out of here, on purpose. The request's **total** is
/// `min(requested, context left)`: the deployment's `max_new_tokens`
/// does **not** enter the total, because it was never a backend limit -
/// it was always a gateway cut, and cutting there silently is the defect. What it
/// becomes is the ceiling of **one leg**, which the continuation loop crosses as many
/// times as the total allows. No protocol adapter duplicates this calculation.
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
    // The leg never exceeds the total: asking for 10 with a deployment of 1000 is still
    // a leg of 10, and the loop does not even open.
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
        self.request_output_limit_for_head(model, None, requested, prompt)
            .await
    }

    /// [`Self::request_output_limit_for_model`] for a request that chose the
    /// machine of its first stage (`inference_head`): the capacity is that of
    /// the route generation will plan from that head, not of the route the
    /// planner would pick without it.
    pub async fn request_output_limit_for_head(
        &self,
        model: Option<&str>,
        preferred_head: Option<&str>,
        requested: u32,
        prompt: &str,
    ) -> OutputTokenLimit {
        let runtime = self.runtime.config().await;
        let (capacity, capacity_pending) =
            self.runtime_capacity_for_model(model, preferred_head).await;
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

    /// Metadata published by `/v1/models`, derived from the same capacity
    /// that the validation of a request uses.
    pub async fn advertised_output_limits(
        &self,
        model: Option<&str>,
    ) -> (Option<u32>, Option<u32>) {
        let peers = self.all_peers().await;
        self.advertised_output_limits_from_peers(&peers, model)
            .await
    }

    /// Variant that receives the same snapshot used to discover the
    /// catalog. This way a peer cannot disappear or enter a restart between
    /// the ID decision and the publication of `max_completion_tokens`.
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
            "effective capacity waiting for confirmation from the peers of the planned pipeline"
        );
        return;
    }
    if limit.was_clamped() {
        info!(
            route,
            requested_max_tokens = limit.requested,
            effective_max_tokens = limit.effective,
            context_available_tokens = ?limit.context_available,
            "output limit reduced by the context"
        );
    }
    if limit.continuation_possible() {
        info!(
            route,
            effective_max_tokens = limit.effective,
            leg_max_tokens = limit.leg,
            deployment_max_tokens = ?limit.deployment,
            "output larger than one generation of the deployment: the continuation loop stitches the legs in the same stream"
        );
    }
}

/// `/api/generate` validates only the requested budget. The runtime capacity is
/// resolved afterwards, from the effective context and the active deployment.
pub(super) fn resolve_request_max_tokens(requested: Option<u32>) -> Result<u32, String> {
    let max_tokens = requested.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens == 0 {
        return Err("max_tokens must be greater than zero".to_string());
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
    /// Terminal reason, when it is not an ordinary end.
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
    /// Only present when the `/health` of the local inference server
    /// confirmed the runtime of this generation.
    pub runtime_capacity: Option<RuntimeCapacity>,
}

// ── Streaming generation ─────────────────────────────────────────────────────

/// One event from a running generation.
#[derive(Debug, Clone)]
pub enum GenEvent {
    /// A newly decoded answer token's text.
    Token(String),
    /// A newly decoded reasoning token's text. Protocol adapters keep this
    /// separate from visible answer content (`reasoning_content`/thinking).
    Reasoning(String),
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
    /// Bytes of the images already downscaled by the client. Only the pipeline head
    /// receives this field; the other peers work with the hidden state.
    pub(crate) images: Vec<Vec<u8>>,
    pub(crate) chat_template: bool,
    /// The user's choice for this inference. The head peer still applies the
    /// deployment's global kill-switch before rendering the template.
    pub(crate) thinking_enabled: bool,
    /// The two ceilings of this request: the leg's and the total's. See
    /// [`OutputBudget`].
    pub(crate) budget: OutputBudget,
    pub(crate) stops: Vec<String>,
    /// How the prefix cache recognizes that this request continues the
    /// previous one. `None` uses the prompt itself.
    ///
    /// The rendered text does not serve as a key: `render_prompt` returns a lone user
    /// message **raw** and labels from the second on, so
    /// the prompt of turn 1 is not a prefix of the prompt of turn 2 and the
    /// 1→2 transition of any chat never hit. The key comes from the array of
    /// messages, which only grows: immune to template, to label and to the tool
    /// block. See `crate::openai::conversation_cache_key`.
    pub(crate) cache_key: Option<String>,
    /// Explicit layer-0 peer selected by the Chat UI, when any. The planner
    /// validates it against the live model heads before honoring it.
    pub(crate) preferred_head: Option<String>,
    /// The parsed HTTP request body, retained for audit without headers or
    /// credentials.
    pub(crate) request_details: serde_json::Value,
    /// Labels of a typed decision (`/v1/systemone`). The last peer returns the
    /// probability of each one in `GenSummary::trace.probe`.
    pub(crate) probe: Option<crate::prefill::DecisionProbe>,
    /// The system part (system prompt and tools) that opens this prompt, so
    /// a new conversation that shares it starts from its KV. See
    /// [`SharedPrefix`].
    pub(crate) shared_prefix: SharedPrefix,
}

/// The part of a prompt many conversations share: the system prompt and the
/// tool list, which the adapters render first. A new conversation misses the
/// prefix cache (its messages are new), but its system part is the same as
/// the last conversation's of the same client; an agent's is often tens of
/// thousands of tokens. On such a miss the gateway reads the system part
/// alone first, as a turn of its own (`Build`), pins its KV on disk, and the
/// request then continues that session. Every later conversation that opens
/// with the same system part restores it instead of reading it again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SharedPrefix {
    #[default]
    None,
    /// The system part ends `prompt_bytes` into the rendered prompt and
    /// `key_bytes` into the prefix cache key.
    Offer { prompt_bytes: usize, key_bytes: usize },
    /// This request is a system part read on its own: its KV is pinned on
    /// disk before the request it opens goes on.
    Build,
}

impl SharedPrefix {
    /// A system part shorter than this (about 2,048 tokens) is cheap to read
    /// again: pinning it would cost more than it saves.
    pub(crate) const MIN_BYTES: usize = 8 * 1024;

    /// The system part of a request whose rendered prompt starts with
    /// `root_prompt` and whose cache key starts with `root_key`, when it is
    /// long enough to be worth pinning and something follows it.
    pub(crate) fn offer(prompt: &str, key: &str, root_prompt: &str, root_key: &str) -> Self {
        let root_prompt = root_prompt.trim_end();
        if root_prompt.len() < Self::MIN_BYTES
            || root_key.is_empty()
            || prompt.len() <= root_prompt.len()
            || key.len() <= root_key.len()
            || !prompt.starts_with(root_prompt)
            || !key.starts_with(root_key)
        {
            return Self::None;
        }
        Self::Offer {
            prompt_bytes: root_prompt.len(),
            key_bytes: root_key.len(),
        }
    }

    /// `(root prompt, root key)` cut from this request's prompt and key.
    pub(crate) fn root_of<'a>(&self, prompt: &'a str, key: &'a str) -> Option<(&'a str, &'a str)> {
        match *self {
            Self::Offer {
                prompt_bytes,
                key_bytes,
            } => Some((prompt.get(..prompt_bytes)?, key.get(..key_bytes)?)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    /// Tokens generated while the model was reasoning. This is a subset of
    /// `tokens_generated`, following OpenAI completion-token semantics.
    pub reasoning_tokens: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    pub timings: GenerationTimings,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
    /// "stop" when a server-side stop sequence ended the generation.
    pub finish_reason: Option<String>,
    /// The exact stop sequence that was matched, when one ended the stream.
    pub stop_sequence: Option<String>,
    /// How many legs of the continuation loop the reply stitched. `1` is a
    /// single generation - no stitching. Greater than `1` means the output
    /// went past the deployment's `max_new_tokens` without it being changed, and
    /// it is what makes the stitching inspectable from outside.
    pub legs: u32,
    /// Detailed per-request observations. It never affects planning.
    pub trace: Option<crate::prefill::RequestTrace>,
}

/// Performance summary of a request, aggregated over the whole pipeline.
/// Each rate uses its own clock and set of tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationTimings {
    /// First observable event of the local engine. Null when the backend did not
    /// publish an explicit timestamp.
    pub local_engine_first_event_ms: Option<u64>,
    /// First token that crossed the pipeline to the gateway.
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

/// Computes rates with clocks that do not mix. `generation_ms` starts at the
/// first token and therefore does not vary with the prompt size.
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
        reasoning_tokens: 0,
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

/// Accounts for one confirmed token against the leg budget.
///
/// The leg is the ceiling of **one** backend generation (`max_new_tokens` of the
/// deployment). Closing it does not end the reply: the continuation loop opens the
/// next one and keeps decoding in the same KV session and the same SSE stream,
/// so none of this is observable on the client side. Only the total ends it, and only
/// it becomes `finish_reason: "length"`.
pub(super) fn note_leg_boundary(budget: OutputBudget, tokens_generated: u32, legs: &mut u32) {
    if tokens_generated.is_multiple_of(budget.leg) && tokens_generated < budget.total {
        *legs += 1;
        info!(
            legs = *legs,
            tokens_generated,
            leg_max_tokens = budget.leg,
            total_max_tokens = budget.total,
            "leg exhausted: continuing the same generation on the next leg"
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
