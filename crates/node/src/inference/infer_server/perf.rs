//! Velocidade medida da fatia deste peer: quanto ela custa por token de
//! prefill e por passo de decode.
//!
//! É o dado que o planejador do gateway precisa para escolher a pipeline que
//! entrega mais cedo, e não só a de menor latência de rede (ver
//! `routing::pipeline::plan_pipeline_fastest`). Duas fontes:
//!
//! - **Teste ao servir** (`run_bench`): um prefill curto e alguns passos de
//!   decode na própria fatia, sem rede, logo depois que o modelo carrega. A
//!   entrada é sintética (tokens fixos na cabeça, estado oculto zerado nos
//!   demais hops): o custo de um passo não depende do conteúdo.
//! - **Tráfego real** (`observe_*`): cada prefill e cada decode atualizam uma
//!   média móvel, para o número acompanhar carga, temperatura e contexto.
//!
//! Publicado no `/health` (`perf`).

use super::*;

/// Peso de uma amostra nova na média móvel.
const EWMA_ALPHA: f64 = 0.2;

/// Tokens do prefill do teste. Do tamanho de um pedaço de prefill real.
const BENCH_PREFILL_TOKENS: usize = 64;

/// Passos de decode do teste; o primeiro é aquecimento e fica fora da conta.
const BENCH_DECODE_STEPS: usize = 9;

/// Um prefill com menos tokens que isto mede mais o custo fixo da chamada que
/// o custo por token, e não entra na média de prefill.
const PREFILL_MIN_TOKENS: usize = 8;

/// Um decode com mais tokens que isto é uma verificação larga de rascunho, não
/// um passo típico, e não entra na média de decode.
const DECODE_MAX_TOKENS: usize = 8;

#[derive(Debug, Clone, Default)]
pub(crate) struct SlicePerf {
    prefill_us_per_token: Option<f64>,
    decode_us_per_step: Option<f64>,
    prefill_samples: u64,
    decode_samples: u64,
    bench: Option<BenchResult>,
}

#[derive(Debug, Clone)]
pub(crate) struct BenchResult {
    pub(crate) prefill_us_per_token: f64,
    pub(crate) decode_us_per_step: f64,
    pub(crate) duration_ms: u64,
}

fn ewma(current: Option<f64>, sample: f64) -> f64 {
    match current {
        Some(value) => value + EWMA_ALPHA * (sample - value),
        None => sample,
    }
}

impl SlicePerf {
    pub(crate) fn observe_prefill(&mut self, elapsed: std::time::Duration, n_tokens: usize) {
        if n_tokens < PREFILL_MIN_TOKENS {
            return;
        }
        let per_token = elapsed.as_secs_f64() * 1e6 / n_tokens as f64;
        self.prefill_us_per_token = Some(ewma(self.prefill_us_per_token, per_token));
        self.prefill_samples += 1;
    }

    pub(crate) fn observe_decode(&mut self, elapsed: std::time::Duration, n_tokens: usize) {
        if n_tokens == 0 || n_tokens > DECODE_MAX_TOKENS {
            return;
        }
        let step = elapsed.as_secs_f64() * 1e6;
        self.decode_us_per_step = Some(ewma(self.decode_us_per_step, step));
        self.decode_samples += 1;
    }

    /// O teste semeia a média; o tráfego real a corrige depois.
    fn seed(&mut self, bench: BenchResult) {
        if self.prefill_us_per_token.is_none() {
            self.prefill_us_per_token = Some(bench.prefill_us_per_token);
        }
        if self.decode_us_per_step.is_none() {
            self.decode_us_per_step = Some(bench.decode_us_per_step);
        }
        self.bench = Some(bench);
    }

    pub(crate) fn json(&self) -> Value {
        let ms = |us: Option<f64>| us.map(|us| (us / 1000.0 * 1000.0).round() / 1000.0);
        let tps = |us: Option<f64>| {
            us.filter(|us| *us > 0.0)
                .map(|us| (1e6 / us * 10.0).round() / 10.0)
        };
        json!({
            "prefill_ms_per_token": ms(self.prefill_us_per_token),
            "decode_ms_per_step": ms(self.decode_us_per_step),
            "prefill_tokens_per_sec": tps(self.prefill_us_per_token),
            "decode_tokens_per_sec": tps(self.decode_us_per_step),
            "prefill_samples": self.prefill_samples,
            "decode_samples": self.decode_samples,
            "bench": self.bench.as_ref().map(|bench| json!({
                "prefill_ms_per_token": ms(Some(bench.prefill_us_per_token)),
                "decode_ms_per_step": ms(Some(bench.decode_us_per_step)),
                "duration_ms": bench.duration_ms,
            })),
        })
    }
}

/// Largura por token e forma (sem o eixo de tokens) do estado oculto que uma
/// fatia do meio recebe. Com vários fluxos de residual, `n_embd_out` é um
/// múltiplo de `n_embd` e o estado viaja em três eixos.
pub(crate) fn hidden_input_layout(n_embd: usize, n_embd_inp: usize, n_embd_out: usize) -> (usize, Vec<usize>) {
    if n_embd > 0 && n_embd_out > n_embd && n_embd_out % n_embd == 0 {
        (n_embd_out, vec![n_embd, n_embd_out / n_embd])
    } else {
        (n_embd_inp.max(1), vec![n_embd_inp.max(1)])
    }
}

/// Se o teste de velocidade da fatia roda ao carregar o modelo. `POM_SLICE_BENCH=0`
/// desliga: um defeito aqui não pode derrubar um nó em produção sem saída.
pub(crate) fn bench_enabled() -> bool {
    !matches!(
        crate::brand::env("SLICE_BENCH").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Mede a fatia com uma sequência descartável. Segura o estado inteiro
/// enquanto roda (menos de um segundo numa GPU), então é chamado uma vez, logo
/// depois de o modelo carregar. `None` quando não dá para medir com segurança:
/// grupo TP (o decode esperaria os workers) ou nenhum slot livre.
pub(super) fn run_bench(s: &mut InferState) -> Result<Option<BenchResult>> {
    if s.tp.is_some() || !bench_enabled() {
        return Ok(None);
    }
    let Some(seq_id) = s.seq_pool.pop() else {
        return Ok(None);
    };
    let result = bench_on_sequence(s, seq_id);
    let _ = s.engine.discard_sequence(seq_id);
    s.seq_pool.push(seq_id);
    let bench = result?;
    s.perf.seed(bench.clone());
    Ok(Some(bench))
}

fn bench_on_sequence(s: &mut InferState, seq_id: u32) -> Result<BenchResult> {
    let started = Instant::now();
    let is_head = s.start_layer == 0;
    let is_last = s.end_layer + 1 >= s.total_layers;
    // A forma que a fatia anterior manda de verdade, a mesma da captura do
    // residual (`capture.rs`): `[tokens, n_embd]` num modelo comum e
    // `[tokens, n_embd, fluxos]` num com vários fluxos de residual
    // (hyper-connections, Qwen3.8 "qwen4exp"), com `n_embd_out` floats por
    // token. Um estado oculto na forma errada aborta o processo dentro do
    // llama.cpp (`GGML_ASSERT` no reshape), sem erro que dê para tratar.
    let hidden_layout = hidden_input_layout(s.engine.n_embd, s.engine.n_embd_inp, s.engine.n_embd_out);
    let n_vocab = s.engine.n_vocab.max(2) as i32;
    let _ = s.engine.set_capture_layer(None);

    // Uma chamada do motor com o mesmo desfecho que o caminho real: a cauda
    // lê os logits, os demais leem o estado oculto capturado. As duas leituras
    // sincronizam a GPU, então o tempo medido é o do trabalho, não o do envio.
    let mut run = |s: &mut InferState, n: usize, pos: u32| -> Result<std::time::Duration> {
        let call_started = Instant::now();
        if is_head {
            let tokens: Vec<i32> = (0..n)
                .map(|i| 100 + ((pos as usize + i) * 7 % 4000) as i32)
                .map(|token| token % n_vocab)
                .collect();
            s.engine.decode_tokens(&tokens, pos, seq_id, !is_last)?;
        } else {
            let (width, feature_shape) = &hidden_layout;
            let hidden = vec![0.0f32; n * width];
            let mut shape: TensorShape = vec![n];
            shape.extend_from_slice(feature_shape);
            s.engine
                .decode_hidden_without_mtp(&hidden, &shape, n, pos, seq_id, !is_last)?;
        }
        if is_last {
            let _ = s.engine.logits_ith(n as i32 - 1);
        } else {
            s.engine.take_residual(n)?;
        }
        Ok(call_started.elapsed())
    };

    let prefill = run(s, BENCH_PREFILL_TOKENS, 0)?;
    let mut steps = Vec::with_capacity(BENCH_DECODE_STEPS);
    for step in 0..BENCH_DECODE_STEPS {
        steps.push(run(s, 1, (BENCH_PREFILL_TOKENS + step) as u32)?);
    }
    let mut measured: Vec<f64> = steps
        .into_iter()
        .skip(1)
        .map(|elapsed| elapsed.as_secs_f64() * 1e6)
        .collect();
    measured.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let decode_us_per_step = measured[measured.len() / 2];
    Ok(BenchResult {
        prefill_us_per_token: prefill.as_secs_f64() * 1e6 / BENCH_PREFILL_TOKENS as f64,
        decode_us_per_step,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn estado_oculto_com_varios_fluxos_viaja_em_tres_eixos() {
        // Qwen3.8 "qwen4exp": 4 fluxos de 2560.
        assert_eq!(hidden_input_layout(2560, 2560, 10240), (10240, vec![2560, 4]));
        // Modelo comum.
        assert_eq!(hidden_input_layout(4096, 4096, 4096), (4096, vec![4096]));
    }

    #[test]
    fn media_movel_segue_o_trafego_e_ignora_amostras_atipicas() {
        let mut perf = SlicePerf::default();
        perf.seed(BenchResult {
            prefill_us_per_token: 1000.0,
            decode_us_per_step: 20_000.0,
            duration_ms: 1,
        });
        // Decode típico puxa a média; verificação larga não conta.
        perf.observe_decode(Duration::from_millis(30), 1);
        perf.observe_decode(Duration::from_millis(500), 64);
        assert_eq!(perf.decode_samples, 1);
        assert!((perf.decode_us_per_step.unwrap() - 22_000.0).abs() < 1.0);
        // Pedaço de prefill pequeno demais não conta.
        perf.observe_prefill(Duration::from_millis(10), 2);
        assert_eq!(perf.prefill_samples, 0);
        perf.observe_prefill(Duration::from_millis(64), 64);
        assert!((perf.prefill_us_per_token.unwrap() - 1000.0).abs() < 1.0);

        let body = perf.json();
        assert_eq!(body["decode_tokens_per_sec"], 45.5);
        assert_eq!(body["prefill_tokens_per_sec"], 1000.0);
    }
}
