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
    /// Kept in the telemetry shape; sparse repetition adjustments avoid this copy.
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

/// Probability of each candidate answer at one position, read from the raw
/// logits (temperature 1, no penalty, no truncation).
///
/// Used by typed decisions: instead of sampling, the last peer reports how
/// much of the model's next-token distribution falls on each candidate. A
/// candidate is a group of token ids (for example `"A"` and `" A"`); its
/// probability is the sum of theirs. `mass` is the total probability the
/// candidates cover, so a caller can tell a confident pick among them from a
/// model that wanted to say something else.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub probabilities: Vec<f32>,
    pub mass: f32,
}

pub fn probe_candidates(logits: &[f32], candidates: &[Vec<i32>]) -> ProbeResult {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if logits.is_empty() || !max.is_finite() {
        return ProbeResult { probabilities: vec![0.0; candidates.len()], mass: 0.0 };
    }
    let total: f64 = logits.iter().map(|value| f64::from((value - max).exp())).sum();
    let mut seen = std::collections::HashSet::new();
    let probabilities: Vec<f32> = candidates
        .iter()
        .map(|ids| {
            ids.iter()
                .filter(|id| **id >= 0 && (**id as usize) < logits.len() && seen.insert(**id))
                .map(|id| f64::from((logits[*id as usize] - max).exp()) / total)
                .sum::<f64>() as f32
        })
        .collect();
    let mass = probabilities.iter().sum();
    ProbeResult { probabilities, mass }
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
    let repetition_started = Instant::now();
    let adjustments = repetition_adjustments(logits, recent, cfg);
    timings.repetition_penalty_us = repetition_started.elapsed().as_micros() as u64;

    if cfg.temperature <= 0.0 {
        return vec![(argmax_adjusted(logits, &adjustments) as usize, 1.0)];
    }

    // Positive-temperature softmax preserves logit order. Repeated IDs are
    // adjusted before top-k refinement, and token ID resolves every tie.
    let selects_top_k = cfg.top_k > 0 && (cfg.top_k as usize) < logits.len();
    let mut candidates = if selects_top_k {
        let top_k_started = Instant::now();
        let ids = top_k_ids_adjusted(logits, &adjustments, cfg.top_k as usize);
        timings.top_k_us = top_k_started.elapsed().as_micros() as u64;

        let softmax_started = Instant::now();
        let max_l = ids
            .iter()
            .map(|&id| adjusted_logit_at(logits, &adjustments, id))
            .fold(f32::NEG_INFINITY, f32::max);
        let mut candidates = ids
            .into_iter()
            .map(|id| {
                (
                    id,
                    ((adjusted_logit_at(logits, &adjustments, id) - max_l) / cfg.temperature).exp(),
                )
            })
            .collect::<Vec<_>>();
        normalise_candidates_with_fallback(&mut candidates, || {
            argmax_adjusted(logits, &adjustments) as usize
        });
        timings.softmax_us = softmax_started.elapsed().as_micros() as u64;
        candidates
    } else {
        // With top-k disabled, stream token IDs directly into the probability
        // vector. Materializing a second vocabulary-sized ID list is redundant.
        let softmax_started = Instant::now();
        let max_l = if adjustments.is_empty() {
            logits.iter().copied().fold(f32::NEG_INFINITY, f32::max)
        } else {
            adjusted_logits(logits, &adjustments)
                .map(|(_, logit)| logit)
                .fold(f32::NEG_INFINITY, f32::max)
        };
        let mut candidates = if adjustments.is_empty() {
            logits
                .iter()
                .copied()
                .enumerate()
                .map(|(id, logit)| (id, ((logit - max_l) / cfg.temperature).exp()))
                .collect::<Vec<_>>()
        } else {
            adjusted_logits(logits, &adjustments)
                .map(|(id, logit)| (id, ((logit - max_l) / cfg.temperature).exp()))
                .collect::<Vec<_>>()
        };
        normalise_candidates_with_fallback(&mut candidates, || {
            argmax_adjusted(logits, &adjustments) as usize
        });
        timings.softmax_us = softmax_started.elapsed().as_micros() as u64;
        candidates
    };

    let top_p_started = Instant::now();
    if cfg.top_p < 1.0 {
        candidates.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let target = cfg.top_p.max(0.0);
        let mut mass = 0.0;
        let mut keep = 0;
        for (_, probability) in &candidates {
            mass += *probability;
            keep += 1;
            if mass >= target {
                break;
            }
        }
        candidates.truncate(keep);
        candidates.sort_unstable_by_key(|(id, _)| *id);
        normalise_candidates_with_fallback(&mut candidates, || {
            argmax_adjusted(logits, &adjustments) as usize
        });
    }
    timings.top_p_us = top_p_started.elapsed().as_micros() as u64;
    candidates
}

#[cfg(test)]
pub(super) fn sampling_candidates(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
) -> Vec<(usize, f32)> {
    sampling_candidates_profiled(logits, recent, cfg, &mut SamplingTimings::default())
}

#[cfg(test)]
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

fn repetition_adjustments(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
) -> Vec<(usize, f32)> {
    if cfg.repeat_penalty == 1.0 || recent.is_empty() {
        return Vec::new();
    }

    let window_start = recent.len().saturating_sub(cfg.repeat_last_n);
    let window: std::collections::HashSet<i32> = recent[window_start..].iter().copied().collect();
    let mut ids: Vec<usize> = window
        .into_iter()
        .filter_map(|token| usize::try_from(token).ok())
        .filter(|&token| token < logits.len())
        .collect();
    ids.sort_unstable();
    ids.into_iter()
        .map(|id| {
            let logit = logits[id];
            let adjusted = if logit > 0.0 {
                logit / cfg.repeat_penalty
            } else {
                logit * cfg.repeat_penalty
            };
            (id, adjusted)
        })
        .collect()
}

fn adjusted_logit_at(logits: &[f32], adjustments: &[(usize, f32)], id: usize) -> f32 {
    if adjustments.is_empty() {
        return logits[id];
    }
    match adjustments.binary_search_by_key(&id, |(adjusted_id, _)| *adjusted_id) {
        Ok(index) => adjustments[index].1,
        Err(_) => logits[id],
    }
}

fn adjusted_logits<'a>(
    logits: &'a [f32],
    adjustments: &'a [(usize, f32)],
) -> impl Iterator<Item = (usize, f32)> + 'a {
    let mut adjustment_index = 0;
    logits.iter().copied().enumerate().map(move |(id, logit)| {
        while adjustment_index < adjustments.len() && adjustments[adjustment_index].0 < id {
            adjustment_index += 1;
        }
        if adjustment_index < adjustments.len() && adjustments[adjustment_index].0 == id {
            let adjusted = adjustments[adjustment_index].1;
            adjustment_index += 1;
            (id, adjusted)
        } else {
            (id, logit)
        }
    })
}

fn argmax_adjusted(logits: &[f32], adjustments: &[(usize, f32)]) -> i32 {
    if adjustments.is_empty() {
        return argmax(logits);
    }
    adjusted_logits(logits, adjustments)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(id, _)| id as i32)
        .unwrap_or(0)
}

fn top_k_ids_adjusted(logits: &[f32], adjustments: &[(usize, f32)], k: usize) -> Vec<usize> {
    if adjustments.is_empty() {
        return top_k_ids_compact(logits, k);
    }

    // At most one adjusted token can displace one raw top-k token. Keeping the
    // raw top (k + adjusted_count) and every adjusted ID therefore contains the
    // exact adjusted top-k, including penalties that raise a logit.
    let raw_count = k.saturating_add(adjustments.len()).min(logits.len());
    let mut ids = top_k_ids_compact(logits, raw_count);
    ids.extend(adjustments.iter().map(|(id, _)| *id));
    ids.sort_unstable();
    ids.dedup();

    let mut ranked: Vec<(usize, f32)> = ids
        .into_iter()
        .map(|id| (id, adjusted_logit_at(logits, adjustments, id)))
        .collect();
    ranked.select_nth_unstable_by(k - 1, |a, b| {
        b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(k);
    ranked.sort_unstable_by_key(|(id, _)| *id);
    ranked.into_iter().map(|(id, _)| id).collect()
}

/// Select the highest `k` token indices and return them in ID order.
///
/// A real model vocabulary fits in a `u32`; using that compact scratch vector
/// halves the per-token allocation and memory traffic on the full-vocabulary
/// selection. Keep the wider fallback for slices too large to represent.
pub(super) fn top_k_ids_compact(logits: &[f32], k: usize) -> Vec<usize> {
    if k == 0 || k >= logits.len() {
        return (0..logits.len()).collect();
    }

    if logits.len() <= u32::MAX as usize {
        let mut ids: Vec<u32> = (0..logits.len() as u32).collect();
        ids.select_nth_unstable_by(k - 1, |&a, &b| {
            logits[b as usize]
                .total_cmp(&logits[a as usize])
                .then_with(|| a.cmp(&b))
        });
        ids.truncate(k);
        ids.sort_unstable();
        ids.into_iter().map(|id| id as usize).collect()
    } else {
        let mut ids: Vec<usize> = (0..logits.len()).collect();
        ids.select_nth_unstable_by(k - 1, |&a, &b| {
            logits[b].total_cmp(&logits[a]).then_with(|| a.cmp(&b))
        });
        ids.truncate(k);
        ids.sort_unstable();
        ids
    }
}

#[cfg(test)]
mod compact_top_k_tests {
    use super::*;

    fn full_sort_reference(logits: &[f32], k: usize) -> Vec<usize> {
        let mut ids: Vec<usize> = (0..logits.len()).collect();
        ids.sort_unstable_by(|&a, &b| logits[b].total_cmp(&logits[a]).then_with(|| a.cmp(&b)));
        ids.truncate(k);
        ids.sort_unstable();
        ids
    }

    #[test]
    fn compact_indices_match_full_sort_with_ties_and_total_float_order() {
        let cases = [
            (vec![7.0, 7.0, 7.0, 1.0], 1),
            (vec![3.0, 3.0, 2.0, 2.0, 1.0], 3),
            (
                vec![f32::NAN, 0.0, -0.0, f32::INFINITY, f32::NEG_INFINITY],
                3,
            ),
            ((0..257).map(|i| ((i % 11) as f32) - 5.0).collect(), 40),
        ];

        for (logits, k) in cases {
            assert_eq!(
                top_k_ids_compact(&logits, k),
                full_sort_reference(&logits, k)
            );
        }
    }

    #[test]
    fn compact_indices_match_full_sort_on_a_large_tied_vocabulary() {
        const VOCAB: usize = 151_936;
        let logits: Vec<f32> = (0..VOCAB)
            .map(|i| ((i.wrapping_mul(48_271) % 32_768) as i32 - 16_384) as f32 * 0.125)
            .collect();

        assert_eq!(
            top_k_ids_compact(&logits, 40),
            full_sort_reference(&logits, 40)
        );
    }
}

#[cfg(test)]
mod sampler_profile_regressions {
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

    fn candidate_ids(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> Vec<usize> {
        sampling_candidates(logits, recent, cfg)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    fn legacy_top_k_ids(logits: &[f32], top_k: i32) -> Vec<usize> {
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

    fn legacy_sampling_candidates_profiled(
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

        let top_k_started = Instant::now();
        let ids = legacy_top_k_ids(&adjusted, cfg.top_k);
        timings.top_k_us = top_k_started.elapsed().as_micros() as u64;
        let softmax_started = Instant::now();
        let max_logit = ids
            .iter()
            .map(|&id| adjusted[id])
            .fold(f32::NEG_INFINITY, f32::max);
        let mut candidates: Vec<(usize, f32)> = ids
            .into_iter()
            .map(|id| (id, ((adjusted[id] - max_logit) / cfg.temperature).exp()))
            .collect();
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

    fn measure_candidates(
        sampler: fn(&[f32], &[i32], &SamplingConfig, &mut SamplingTimings) -> Vec<(usize, f32)>,
        logits: &[f32],
        recent: &[i32],
        cfg: &SamplingConfig,
        rounds: usize,
    ) -> (f64, SamplingTimings) {
        let mut totals = SamplingTimings::default();
        let started = Instant::now();
        for _ in 0..rounds {
            let mut timings = SamplingTimings::default();
            std::hint::black_box(sampler(logits, recent, cfg, &mut timings));
            totals.logits_clone_us += timings.logits_clone_us;
            totals.repetition_penalty_us += timings.repetition_penalty_us;
            totals.softmax_us += timings.softmax_us;
            totals.top_k_us += timings.top_k_us;
            totals.top_p_us += timings.top_p_us;
        }
        (
            started.elapsed().as_secs_f64() * 1_000.0 / rounds as f64,
            totals,
        )
    }

    #[test]
    #[ignore = "benchmark manual em release para comparar o caminho anterior e o atual"]
    fn sampler_cost_profiles_on_a_qwen_vocabulary() {
        const VOCAB: usize = 151_936;
        const ROUNDS: usize = 100;
        let logits: Vec<f32> = (0..VOCAB).map(|i| ((i % 977) as f32) * 0.01).collect();
        let recent: Vec<i32> = (0..64).map(|i| (i * 1979 % VOCAB) as i32).collect();
        let actual_defaults = SamplingConfig {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
        };
        let cases = [
            ("top_k_enabled", cfg(0.7, 40, 1.0), &[][..]),
            ("deployed_defaults", actual_defaults, recent.as_slice()),
            ("top_k_disabled", cfg(0.7, 0, 0.9), &[][..]),
        ];

        for (name, config, recent) in cases {
            assert_eq!(
                legacy_sampling_candidates_profiled(
                    &logits,
                    recent,
                    &config,
                    &mut SamplingTimings::default()
                ),
                sampling_candidates_profiled(
                    &logits,
                    recent,
                    &config,
                    &mut SamplingTimings::default()
                ),
                "candidate distribution changed for {name}"
            );
            let legacy = measure_candidates(
                legacy_sampling_candidates_profiled,
                &logits,
                recent,
                &config,
                ROUNDS,
            );
            let current = measure_candidates(
                sampling_candidates_profiled,
                &logits,
                recent,
                &config,
                ROUNDS,
            );
            let avg = |total: u64| total as f64 / ROUNDS as f64;
            println!(
                "{name}: legacy wall={:.1}us clone={:.1}us repetition={:.1}us softmax={:.1}us top_k={:.1}us top_p={:.1}us; current wall={:.1}us clone={:.1}us repetition={:.1}us softmax={:.1}us top_k={:.1}us top_p={:.1}us",
                legacy.0 * 1_000.0,
                avg(legacy.1.logits_clone_us),
                avg(legacy.1.repetition_penalty_us),
                avg(legacy.1.softmax_us),
                avg(legacy.1.top_k_us),
                avg(legacy.1.top_p_us),
                current.0 * 1_000.0,
                avg(current.1.logits_clone_us),
                avg(current.1.repetition_penalty_us),
                avg(current.1.softmax_us),
                avg(current.1.top_k_us),
                avg(current.1.top_p_us),
            );
        }
    }

    #[test]
    fn sparse_adjustments_preserve_candidates_across_sampling_options() {
        let logits = [4.0, 4.0, 1.0, -2.0, -2.0, 0.5, 3.0];
        let recent = [0, 0, 3, 5, -1, 99];
        let configs = [
            SamplingConfig {
                repeat_penalty: 1.15,
                repeat_last_n: 5,
                ..cfg(0.7, 3, 0.9)
            },
            SamplingConfig {
                repeat_penalty: 0.5,
                repeat_last_n: 6,
                ..cfg(1.2, 0, 0.5)
            },
            SamplingConfig {
                repeat_penalty: 2.0,
                repeat_last_n: 4,
                ..cfg(0.0, 2, 0.1)
            },
            cfg(1.0, 40, 1.0),
        ];

        for config in configs {
            let expected = legacy_sampling_candidates_profiled(
                &logits,
                &recent,
                &config,
                &mut SamplingTimings::default(),
            );
            assert_eq!(
                sampling_candidates(&logits, &recent, &config),
                expected,
                "config={config:?}"
            );
        }
    }

    #[test]
    fn active_repetition_penalty_does_not_clone_the_full_vocabulary() {
        const VOCAB: usize = 151_936;
        let logits: Vec<f32> = (0..VOCAB).map(|i| ((i % 977) as f32) * 0.01).collect();
        let recent: Vec<i32> = (0..64).map(|i| (i * 1979 % VOCAB) as i32).collect();
        let config = SamplingConfig {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
        };

        let expected = legacy_sampling_candidates_profiled(
            &logits,
            &recent,
            &config,
            &mut SamplingTimings::default(),
        );
        assert_eq!(sampling_candidates(&logits, &recent, &config), expected);
        let (_, timings) = sample_token_profiled(&logits, &recent, &config);
        assert_eq!(timings.logits_clone_us, 0);
    }

    #[test]
    fn disabled_top_k_does_not_materialize_a_vocabulary_sized_id_list() {
        const VOCAB: usize = 151_936;
        let logits = vec![0.25; VOCAB];
        let config = cfg(0.7, 0, 1.0);

        let (_, timings) = sample_token_profiled(&logits, &[], &config);
        assert_eq!(timings.top_k_us, 0);
        assert!(timings.softmax_us > 0);
    }

    #[test]
    fn repetition_penalty_is_applied_before_top_k() {
        let cfg = SamplingConfig {
            repeat_penalty: 2.0,
            repeat_last_n: 4,
            ..cfg(1.0, 2, 1.0)
        };

        assert_eq!(candidate_ids(&[10.0, 9.0, 8.0, 7.0], &[0, 0], &cfg), [1, 2]);
    }

    #[test]
    fn repetition_penalty_below_one_can_raise_a_token_into_top_k() {
        let cfg = SamplingConfig {
            repeat_penalty: 0.5,
            repeat_last_n: 4,
            ..cfg(1.0, 2, 1.0)
        };

        assert_eq!(
            candidate_ids(&[9.0, 8.0, 7.0, 6.0, 5.0], &[4], &cfg),
            [0, 4]
        );
    }

    #[test]
    fn disabled_top_k_keeps_the_full_distribution_for_top_p() {
        assert_eq!(
            candidate_ids(&[3.0, 2.0, 1.0], &[], &cfg(1.0, 0, 0.8)),
            [0, 1]
        );
    }

    #[test]
    fn top_k_and_top_p_ties_keep_the_lowest_token_ids() {
        assert_eq!(
            candidate_ids(&[7.0, 7.0, 7.0, 1.0], &[], &cfg(1.0, 1, 1.0)),
            [0]
        );
        assert_eq!(candidate_ids(&[1.0; 4], &[], &cfg(1.0, 0, 0.5)), [0, 1]);
    }
}

#[cfg(test)]
pub(super) fn normalise_candidates(candidates: &mut Vec<(usize, f32)>, adjusted: &[f32]) {
    normalise_candidates_with_fallback(candidates, || argmax(adjusted) as usize);
}

fn normalise_candidates_with_fallback(
    candidates: &mut Vec<(usize, f32)>,
    fallback: impl FnOnce() -> usize,
) {
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
        candidates.push((fallback(), 1.0));
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

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn candidates_get_their_share_of_the_full_distribution() {
        // ln(4) vs ln(1): ids 0 and 1 hold 4/7 and 1/7 of the mass.
        let logits = [4f32.ln(), 1f32.ln(), 2f32.ln()];
        let result = probe_candidates(&logits, &[vec![0], vec![1]]);
        assert!((result.probabilities[0] - 4.0 / 7.0).abs() < 1e-5);
        assert!((result.probabilities[1] - 1.0 / 7.0).abs() < 1e-5);
        assert!((result.mass - 5.0 / 7.0).abs() < 1e-5);
    }

    #[test]
    fn spelling_variants_add_up_and_an_id_counts_once() {
        let logits = [0.0f32, 0.0, 0.0, 0.0];
        let result = probe_candidates(&logits, &[vec![0, 1], vec![1, 2], vec![99, -1]]);
        assert_eq!(result.probabilities, vec![0.5, 0.25, 0.0]);
        assert!(probe_candidates(&[], &[vec![0]]).probabilities == vec![0.0]);
    }
}
