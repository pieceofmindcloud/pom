# Amostragem por núcleo do POM: plano de implementação

> **Para agentes de implementação:** use `superpowers:subagent-driven-development` (recomendado) ou `superpowers:executing-plans` para executar este plano tarefa por tarefa. Os passos usam a sintaxe de checkbox (`- [ ]`) para acompanhamento.

**Goal:** Corrigir o amostrador do POM para que `top_p` use uma distribuição normalizada, preserve a composição correta com `top_k`, temperatura e penalidade de repetição, e manter testes exatos contra regressão para a amostragem determinística acidental.

**Architecture:** Manter a amostragem no `LlmEngine` e separar a preparação dos candidatos da extração aleatória. A preparação aplica a penalidade à cópia dos logits, restringe a contagem por `top_k`, calcula o softmax com a temperatura, normaliza a massa, corta o menor prefixo ordenado que atinge `top_p` e devolve candidatos já prontos para o sorteio. O caminho de temperatura não positiva retorna o argmax após a penalidade sem passar por filtros estocásticos.

**Tech Stack:** Rust, testes unitários do binário `node`, `cargo test`, `cargo check`, documentação Markdown em pt-BR.

**Spec:** Solicitação do captain em `v1 launch-brief` nesta tarefa; não há spec dedicada separada.

## Global Constraints

- Não alterar o checkout nem os arquivos de `llama.cpp`.
- Não mexer em `kv_seq_budget` nem no dimensionamento do estado recorrente.
- Não desfazer as correções `9af8971`, `a941b7b` e `48b4a00`.
- Manter os padrões de implantação `temperature: 0.7`, `top_p: 0.9`, `top_k: 40`, `repeat_penalty: 1.15` e `repeat_last_n: 64`.
- `temperature <= 0` continua sendo argmax determinístico depois da penalidade de repetição.
- `top_k` deve limitar a quantidade de candidatos e `top_p` deve selecionar o menor núcleo de massa dentro dos sobreviventes de `top_k`.
- Testes que dependem de sorteio devem verificar o conjunto de candidatos ou usar uma fonte determinística já presente no módulo, sem expectativas probabilísticas instáveis.
- Documentar o comportamento atual em `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md`; não editar cópias geradas nem `CHANGELOG.md`.

---

### Task 1: Corrigir o núcleo do sampler, cobrir a cadeia e sincronizar a documentação

**Files:**
- Modify: `crates/node/src/inference/llm.rs`, na implementação de `sample_token_profiled` e no módulo `llm::tests::sampler_behaviour`.
- Modify: `crates/node/src/inference/AGENTS.md`, na descrição de `SamplingConfig` e das fronteiras do sampler.
- Modify: `docs/subsystems/inferencia.md`, acrescentando a semântica vigente do sampler perto da seção de completions/inferência.
- Modify: `docs/superpowers/specs/2026-08-12-inferencia-pela-internet-mudancas.md`, encerrando a decisão histórica que ainda descreve o defeito como deliberado.

**Interfaces:**
- Consome: `SamplingConfig`, `SamplingTimings`, `argmax`, os logits produzidos pelo `LlmEngine` e `recent_tokens` fornecidos por `infer_server`.
- Produz: `sample_token_profiled` com a mesma assinatura pública e telemetria existente; um helper interno compartilhado pela produção e pelos testes que exponha aos testes o conjunto de ids candidatos sem fazer sorteio.

**Decisões da implementação:**

- A ordem observável será: copiar logits, penalizar tokens distintos na janela final de `repeat_last_n`, retornar argmax para temperatura não positiva, selecionar no máximo `top_k` pelos logits, calcular pesos `exp((logit - máximo) / temperature)`, normalizar os pesos dos sobreviventes, ordenar por probabilidade e manter o prefixo mínimo cuja soma seja pelo menos `top_p`, renormalizar no sorteio e sortear multinomialmente.
- Aplicar `top_k` antes do softmax continua sendo correto para temperatura positiva porque a divisão por uma temperatura positiva e a exponenciação preservam a ordem; a implementação deve manter a seleção parcial O(V) já introduzida em `c985758`.
- O `top_k` deve devolver no máximo K ids mesmo com empate no limiar. Empates podem ser resolvidos de modo determinístico pelo índice original, sem ampliar o conjunto além de K.
- O `top_p` deve truncar o prefixo ordenado, não reter todos os ids empatados no valor de corte. Isso implementa diretamente o menor núcleo de probabilidade e deixa o conjunto testável.
- `repeat_last_n` permanece 64. A penalidade existente é unigramática, não detecta n-gramas nem blocos inteiros; ampliar a janela para 93 ou 120 mudaria a política e não garantiria a prevenção do ciclo observado. A documentação deve registrar esse raciocínio.
- O sorteio deve continuar usando o LCG protegido pelo `Mutex` já existente, sem nova dependência. O helper comum deve ser a fonte tanto do conjunto testado quanto do sorteio para impedir que o teste descreva uma lógica diferente da produção.

- [x] **Step 1: Escrever os testes falhos do comportamento correto**

Substituir o teste de caracterização do colapso guloso e remover os comentários que chamam o defeito de deliberado. Acrescentar testes no módulo `sampler_behaviour` com um helper de teste que compare ids candidatos, usando logits distintos e `repeat_penalty: 1.0`:

```rust
fn candidate_ids(logits: &[f32], c: &SamplingConfig) -> Vec<i32> {
    sampling_candidates(logits, &[], c)
        .into_iter()
        .map(|(id, _)| id as i32)
        .collect()
}

#[test]
fn top_p_keeps_the_minimum_normalised_nucleus() {
    let logits = [4.0, 3.0, 2.0, 1.0, -1.0];
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.8)), vec![0, 1]);
}

#[test]
fn top_p_near_one_keeps_the_tail_and_small_top_p_does_not() {
    let logits = [4.0, 3.0, 2.0, 1.0, -1.0];
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.999)), vec![0, 1, 2, 3, 4]);
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.2)), vec![0]);
}

#[test]
fn temperature_changes_the_nucleus_candidates() {
    let logits = [4.0, 3.0, 2.0, 1.0, 0.0];
    assert_eq!(candidate_ids(&logits, &cfg(0.5, 0, 0.9)), vec![0, 1]);
    assert_eq!(candidate_ids(&logits, &cfg(2.0, 0, 0.9)), vec![0, 1, 2, 3]);
}

#[test]
fn non_positive_temperature_remains_deterministic_argmax() {
    let logits = [0.1, 9.0, 0.3, 2.0];
    for temperature in [0.0, -1.0] {
        assert_eq!(sample_token(&logits, &[], &cfg(temperature, 1, 0.01)), 1);
    }
}

#[test]
fn top_k_then_top_p_composes_as_count_then_mass() {
    let logits = [10.0, 9.0, 8.0, 7.0, 6.0];
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 3, 0.95)), vec![0, 1, 2]);
}

#[test]
fn top_k_does_not_expand_past_k_on_a_tie() {
    let logits = [7.0, 7.0, 7.0, 1.0];
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 1, 1.0)).len(), 1);
}

#[test]
fn top_p_regression_preserves_multiple_candidates_instead_of_greedy() {
    let logits = [1.0, 8.0, 7.9, 7.8];
    assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.9)), vec![1, 2, 3]);
}
```

The existing repetition penalty regression remains and must continue to pass. Run only the sampler tests:

```bash
cargo test -p node --bin pom llm::tests::sampler_behaviour -- --nocapture
```

Expected result before production changes: the new candidate tests fail because `sampling_candidates` is not yet present and, after the helper is minimally introduced for the old behavior, the normalization and exact-count assertions fail. Confirm the failure identifies the missing normalization or candidate semantics, not a test typo.

- [x] **Step 2: Run the focused tests and record the RED result**

Run:

```bash
cargo test -p node --bin pom llm::tests::sampler_behaviour -- --nocapture
```

Record in the implementation report which assertions fail against the old sampler, including the regression case that the old implementation reduces to `[1]`.

- [x] **Step 3: Implement the shared candidate preparation minimally**

Extract the preparation path used by production and tests. It must return `Vec<(usize, f32)>` in original id order, with probabilities normalized over the final candidate set before the draw. Its essential structure is:

```rust
fn sampling_candidates(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
) -> Vec<(usize, f32)> {
    let mut adjusted = logits.to_vec();
    apply_repetition_penalty(&mut adjusted, recent, cfg);
    if cfg.temperature <= 0.0 {
        return vec![(argmax(&adjusted) as usize, 1.0)];
    }

    let ids = top_k_ids(&adjusted, cfg.top_k);
    let max_l = ids.iter().map(|&i| adjusted[i]).fold(f32::NEG_INFINITY, f32::max);
    let mut candidates = ids
        .into_iter()
        .map(|i| (i, ((adjusted[i] - max_l) / cfg.temperature).exp()))
        .collect::<Vec<_>>();
    let total = candidates.iter().map(|(_, p)| *p).sum::<f32>();
    for (_, p) in &mut candidates {
        *p /= total;
    }

    if cfg.top_p < 1.0 {
        let mut ranked = candidates.clone();
        ranked.sort_unstable_by(|a, b| {
            b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
        });
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
        let total = candidates.iter().map(|(_, p)| *p).sum::<f32>();
        for (_, p) in &mut candidates {
            *p /= total;
        }
    }
    candidates
}
```

Adapt this exact shape to preserve `SamplingTimings`: each stage must continue accumulating its existing microsecond field, and cloning/penalty work must not be performed twice. Keep the LCG draw in `sample_token_profiled`, divide by the final candidate total if the helper deliberately leaves the final renormalization to the draw, and retain the argmax fallback for an invalid zero total. The production function must call the shared helper, not a second copy of top-p logic.

- [x] **Step 4: Run the sampler tests GREEN and refactor only after green**

Run:

```bash
cargo test -p node --bin pom llm::tests::sampler_behaviour -- --nocapture
cargo fmt --all -- --check
```

If a test fails, adjust implementation rather than weakening an assertion. After all focused tests pass, remove stale characterization wording, make helper names describe normalized candidate preparation, and preserve the existing O(V) top-k path and timing fields.

- [x] **Step 5: Synchronize durable documentation**

Update `crates/node/src/inference/AGENTS.md` so its `SamplingConfig` entry states that the sampler applies repetition penalty, positive-temperature softmax, top-k count, normalized top-p nucleus and multinomial draw, while non-positive temperature is argmax. State that `repeat_last_n=64` remains the default because the penalty is token-wise and not an n-gram cycle detector.

Update `docs/subsystems/inferencia.md` with the same verified behavior and explicitly remove references ao antigo colapso guloso ou a um defeito preservado deliberadamente. Update the historical `Mudança 4` section in `docs/superpowers/specs/2026-08-12-inferencia-pela-internet-mudancas.md` to state that the normalization and candidate-chain correction was delivered, so it no longer presents the old decision as pending. Do not modify generated frontend documentation.

Run:

```bash
rg -n "de[f]ect kept deliberately" crates docs AGENTS.md
```

Expected result: no matches.

- [x] **Step 6: Validate the focused and compile paths**

Run:

```bash
cargo test -p node --bin pom llm::tests::sampler_behaviour -- --nocapture
cargo check -p node --bin pom
```

Treat warnings already emitted by unrelated modules as pre-existing, but do not accept errors or new warnings caused by the sampler change. Confirm no files under `vendor/llama-cpp-sys-2/llama.cpp` changed and no protected commits were reverted:

```bash
git status --short
git diff -- vendor/llama-cpp-sys-2/llama.cpp
git merge-base --is-ancestor 9af8971 HEAD
git merge-base --is-ancestor a941b7b HEAD
git merge-base --is-ancestor 48b4a00 HEAD
```

- [x] **Step 7: Commit the implementation and documentation together**

```bash
git add crates/node/src/inference/llm.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md docs/superpowers/specs/2026-08-12-inferencia-pela-internet-mudancas.md docs/superpowers/plans/2026-08-31-amostragem-top-p.md
git commit -m "fix(inference): corrigir amostragem top-p"
```

The commit message must not include an agent co-author.
