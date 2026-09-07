use super::*;

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

pub(super) static SAMPLER_RNG: Mutex<u64> = Mutex::new(0x853c49e6748fea9b);

#[cfg(test)]
pub(super) static SAMPLER_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    let kept = sampling_candidates_profiled(logits, recent, cfg, &mut timings);
    if cfg.temperature <= 0.0 {
        let selection_started = Instant::now();
        let token = kept[0].0 as i32;
        timings.selection_us = selection_started.elapsed().as_micros() as u64;
        return (token, timings);
    }
    // ── Multinomial draw ─────────────────────────────────────────────────────
    //
    // The candidates are normalized and in ascending id order, so this is the
    // only place that draws from the distribution.
    let selection_started = Instant::now();
    let rng_val: f32 = {
        // A simple LCG, to avoid pulling in `rand`.
        let mut r = SAMPLER_RNG.lock().unwrap();
        next_sampler_rng_value(&mut r)
    };

    let mut cumsum = 0.0f32;
    let mut best = (0usize, f32::NEG_INFINITY);
    for (i, p) in &kept {
        cumsum += *p;
        if rng_val <= cumsum {
            timings.selection_us = selection_started.elapsed().as_micros() as u64;
            return (*i as i32, timings);
        }
        // Rounding can leave `rng_val` just past the final cumulative sum; the
        // most likely token is the answer then, matching the old fallback.
        if *p >= best.1 {
            best = (*i, *p);
        }
    }
    timings.selection_us = selection_started.elapsed().as_micros() as u64;
    (best.0 as i32, timings)
}

pub(super) fn next_sampler_rng_value(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mantissa = ((*state >> 33) as u32) & 0x007f_ffff;
    let bits = mantissa | 0x3f800000;
    f32::from_bits(bits) - 1.0
}

pub(super) fn sampling_candidates_profiled(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
    timings: &mut SamplingTimings,
) -> Vec<(usize, f32)> {
    let clone_started = Instant::now();
    let mut adjusted = logits.to_vec();
    timings.logits_clone_us = clone_started.elapsed().as_micros() as u64;

    let repetition_started = Instant::now();
    apply_repetition_penalty(&mut adjusted, recent, cfg);
    timings.repetition_penalty_us = repetition_started.elapsed().as_micros() as u64;

    if cfg.temperature <= 0.0 {
        return vec![(argmax(&adjusted) as usize, 1.0)];
    }

    // Selecting the k largest raw logits is O(V): positive-temperature softmax
    // preserves their order, and index breaks a tie without widening the set.
    let top_k_started = Instant::now();
    let ids = top_k_ids(&adjusted, cfg.top_k);
    timings.top_k_us = top_k_started.elapsed().as_micros() as u64;

    let softmax_started = Instant::now();
    let max_l = ids
        .iter()
        .map(|&i| adjusted[i])
        .fold(f32::NEG_INFINITY, f32::max);
    let mut candidates = ids
        .into_iter()
        .map(|i| (i, ((adjusted[i] - max_l) / cfg.temperature).exp()))
        .collect::<Vec<_>>();
    normalise_candidates(&mut candidates, &adjusted);
    timings.softmax_us = softmax_started.elapsed().as_micros() as u64;

    let top_p_started = Instant::now();
    if cfg.top_p < 1.0 {
        let mut ranked = candidates.clone();
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let target = cfg.top_p.max(0.0);
        let mut mass = 0.0;
        let mut keep = 0;
        for (_, probability) in &ranked {
            mass += *probability;
            keep += 1;
            if mass >= target {
                break;
            }
        }
        ranked.truncate(keep);
        ranked.sort_unstable_by_key(|(id, _)| *id);
        candidates = ranked;
        normalise_candidates(&mut candidates, &adjusted);
    }
    timings.top_p_us = top_p_started.elapsed().as_micros() as u64;
    candidates
}

#[cfg(test)]
pub(super) fn sampling_candidates(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> Vec<(usize, f32)> {
    sampling_candidates_profiled(logits, recent, cfg, &mut SamplingTimings::default())
}

pub(super) fn apply_repetition_penalty(logits: &mut [f32], recent: &[i32], cfg: &SamplingConfig) {
    if cfg.repeat_penalty == 1.0 || recent.is_empty() {
        return;
    }
    let window_start = recent.len().saturating_sub(cfg.repeat_last_n);
    let window: std::collections::HashSet<i32> = recent[window_start..].iter().copied().collect();
    for tok in window {
        let token = tok as usize;
        if token < logits.len() {
            logits[token] = if logits[token] > 0.0 {
                logits[token] / cfg.repeat_penalty
            } else {
                logits[token] * cfg.repeat_penalty
            };
        }
    }
}

pub(super) fn top_k_ids(logits: &[f32], top_k: i32) -> Vec<usize> {
    if top_k <= 0 || (top_k as usize) >= logits.len() {
        return (0..logits.len()).collect();
    }

    let k = top_k as usize;
    let mut ids: Vec<usize> = (0..logits.len()).collect();
    ids.select_nth_unstable_by(k - 1, |&a, &b| {
        logits[b].total_cmp(&logits[a]).then_with(|| a.cmp(&b))
    });
    ids.truncate(k);
    ids.sort_unstable();
    ids
}

pub(super) fn normalise_candidates(candidates: &mut Vec<(usize, f32)>, adjusted: &[f32]) {
    let total = candidates
        .iter()
        .map(|(_, probability)| *probability)
        .sum::<f32>();
    if total > 0.0 {
        for (_, probability) in candidates {
            *probability /= total;
        }
    } else {
        candidates.clear();
        candidates.push((argmax(adjusted) as usize, 1.0));
    }
}

/// Index of the largest value, matching `max_by`'s "last one wins" on ties.
pub(super) fn argmax(values: &[f32]) -> i32 {
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

pub(super) fn template_marker<'a>(template: &str, marker: &'a str) -> Result<&'a str> {
    if template.contains(marker) {
        Ok(marker)
    } else {
        bail!("GGUF Gemma4 chat_template não contém o marcador {marker:?}")
    }
}

/// Render the Gemma4 template shape stored in the GGUF.
///
/// llama.cpp's public C helper accepts the Jinja source but has no argument
/// for `enable_thinking`. This renderer only supports the exact control-flow
/// markers it finds in the model's own template and refuses a drifted
/// template instead of substituting a Qwen-like prompt.
pub(super) fn render_gemma4_template(
    template: &str,
    bos: &str,
    messages: &[(&str, &str)],
    add_generation_prompt: bool,
    no_thinking: bool,
) -> Result<String> {
    let turn = template_marker(template, "<|turn>")?;
    let turn_end = template_marker(template, "<turn|>")?;
    let think = template_marker(template, "<|think|>")?;
    let channel = template_marker(template, "<|channel>")?;
    let channel_end = template_marker(template, "<channel|>")?;
    let enable_thinking = !no_thinking;

    let mut out = String::from(bos);
    let mut loop_messages = messages;
    let has_system_block = enable_thinking
        || messages
            .first()
            .is_some_and(|(role, _)| *role == "system" || *role == "developer");
    if has_system_block {
        out.push_str(turn);
        out.push_str("system\n");
        if enable_thinking {
            out.push_str(think);
            out.push('\n');
        }
        if let Some((role, content)) = messages.first() {
            if *role == "system" || *role == "developer" {
                out.push_str(content.trim());
                loop_messages = &messages[1..];
            }
        }
        out.push_str(turn_end);
        out.push('\n');
    }

    for (role, content) in loop_messages {
        let role = if *role == "assistant" { "model" } else { role };
        out.push_str(turn);
        out.push_str(role);
        out.push('\n');
        out.push_str(content.trim());
        out.push_str(turn_end);
        out.push('\n');
    }

    if add_generation_prompt {
        out.push_str(turn);
        out.push_str("model\n");
        if !enable_thinking {
            out.push_str(channel);
            out.push_str("thought\n");
            out.push_str(channel_end);
        }
    }
    Ok(out)
}

pub(super) fn token_sequence_ends_with(tokens: &[i32], suffix: &[i32]) -> bool {
    !suffix.is_empty() && tokens.len() >= suffix.len() && tokens.ends_with(suffix)
}
