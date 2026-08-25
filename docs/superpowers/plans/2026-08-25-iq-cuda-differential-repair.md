# IQ CUDA Differential Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** corrigir a primeira divergência numérica comprovada das pontes CUDA IQ sem reativar IQ2_S ou IQ4_XS antes da prova contra o decoder GGML.

**Architecture:** o decoder CPU do projeto e as funções `dequantize_row_*` do GGML continuam sendo os oráculos. A prova CUDA compara gate/up, ativação e down com o mesmo payload, hidden e rota; somente o dtype cujo erro passar a tolerância é reabilitado em `cuda_supported`. A salvaguarda de `e8e6eb8` permanece para IQ2_S e IQ4_XS até que cada um tenha prova independente.

**Tech Stack:** Rust, `llama-cpp-sys-2`, CUDA C++, testes unitários do binário `pom`, GGUF sintético e GGUF real quando disponível.

**Spec:** `docs/subsystems/freetoken-moe.md`, seções `F5-E5`, `Estado da cobertura IQ heterogênea` e `Invariantes de segurança`.

## Global Constraints

- O caminho denso continua em `llama.cpp` e não recebe callback MoE.
- Nenhum peso atravessa a rede por causa do fatiamento.
- IQ2_S e IQ4_XS continuam em `CPUReference` até uma prova numérica própria passar.
- A prova aceita somente erro finito e `abs(a-b) <= 1e-4` para a fixture atual; tolerância maior exige evidência do oráculo e decisão explícita.
- Toda fase roda `cargo test -p node --bin pom`; falhas fora da mudança são registradas pelos nomes e mensagens exatas.
- A build CUDA obrigatória usa `CARGO_TARGET_DIR=/data/ftk-cuda-target`, `CUDACXX=/usr/local/cuda-12/bin/nvcc`, e não é substituída por uma build sem CUDA.
- Nenhum deploy, restart, SSH ou push force é permitido.

### Task 1: tornar a reprodução por dtype e estágio inequívoca

**Files:**
- Modify: `crates/node/src/inference/moe/format_iq_quants.rs:1086-1104`
- Modify: `crates/node/src/inference/moe/cuda.rs:1560-1704`
- Test: os testes existentes `cada_iq_priorizado_tem_bloco_e_dequantizacao_determinados`, `executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu` e `diagnostico_cuda_iq_divergencia_por_estagio`

**Interfaces:**
- `dequantize_row` continua devolvendo `Result<Vec<f32>, IqQuantError>`.
- O trace CUDA continua usando `pom_moe_iquant_trace_single_resident` e passa a identificar dtype, estágio, índice, valor atual, esperado, erro absoluto e erro relativo na primeira divergência.

- [ ] **Step 1: Write the failing test**

  Alterar a asserção de finitude da fixture CPU para informar o dtype e o índice do primeiro valor não finito, e alterar a comparação CUDA para falhar no primeiro estágio que exceder a tolerância. Não relaxar a tolerância nem reativar dtype.

- [ ] **Step 2: Run test to verify it fails**

  Run: `CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom cada_iq_priorizado_tem_bloco_e_dequantizacao_determinados -- --nocapture`

  Expected: o teste reproduz o primeiro dtype com valor não finito ou passa no host sem CUDA; se o teste CUDA for compilável, `executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu` deve reportar o primeiro dtype/estágio divergente.

- [ ] **Step 3: Write minimal implementation**

  Adicionar uma função de diagnóstico de erro finito/relativo compartilhada pelos testes e preservar os dados de trace existentes. O diagnóstico não pode alterar o resultado de produção.

- [ ] **Step 4: Run test to verify it passes**

  Run: `CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom cada_iq_priorizado_tem_bloco_e_dequantizacao_determinados -- --nocapture`

  Expected: o teste CPU passa ou informa exatamente a fixture problemática; a prova CUDA permanece separada e não é declarada executada sem a feature e o toolkit reais.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/node/src/inference/moe/format_iq_quants.rs crates/node/src/inference/moe/cuda.rs docs/superpowers/plans/2026-08-25-iq-cuda-differential-repair.md
  git commit -m "test: tornar divergencia IQ diagnosticavel por estagio"
  ```

### Task 2: corrigir somente o primeiro dtype com evidência

**Files:**
- Modify: `crates/node/src/inference/moe/cuda_kernels.cu`
- Modify: `crates/node/src/inference/moe/cuda.rs` or `crates/node/src/inference/moe/format_iq_quants.rs` only when the diagnostic proves a Rust boundary error
- Test: `executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu` and `diagnostico_cuda_iq_divergencia_por_estagio`

**Interfaces:**
- Os kernels continuam recebendo dtype, payload residente, hidden e rota como dados.
- `IqQuantFormat::cuda_supported` só muda depois da comparação CUDA do dtype corrigido passar.

- [ ] **Step 1: Write the failing test**

  Executar a prova diferencial no worktree CUDA com a mesma fixture e guardar a primeira divergência por estágio. Se a primeira divergência for IQ2_S ou IQ4_XS, manter `cuda_supported` falso enquanto o teste falha.

- [ ] **Step 2: Run test to verify it fails**

  Run: `CARGO_TARGET_DIR=/data/ftk-cuda-target CUDACXX=/usr/local/cuda-12/bin/nvcc cargo test -p node --features vulkan,cuda --bin pom executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu -- --ignored --nocapture`

  Expected: saída CUDA literal com dtype e estágio divergente, ou erro literal do ambiente se o comando não puder executar.

- [ ] **Step 3: Write minimal implementation**

  Corrigir apenas a indexação, escala, codebook, geometria ou cópia que a primeira divergência demonstrar. Adicionar um teste unitário que fixa o caso corrigido. Não copiar código do FreeToken e não misturar a correção com cache ou co-execução.

- [ ] **Step 4: Run test to verify it passes**

  Repetir o teste diferencial do dtype corrigido e o trace por estágio. O dtype só pode ser reabilitado quando gate/up, ativação e down forem finitos e passarem a tolerância.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/node/src/inference/moe/cuda_kernels.cu crates/node/src/inference/moe/cuda.rs crates/node/src/inference/moe/format_iq_quants.rs
  git commit -m "fix: alinhar ponte CUDA IQ ao decoder GGML"
  ```

### Task 3: regressão densa, documentação e publicação

**Files:**
- Modify: `docs/subsystems/freetoken-moe.md`
- Modify: `crates/node/src/inference/AGENTS.md` when the runtime contract changes

**Interfaces:**
- A telemetria mantém `CPUReference` e o motivo para IQ ainda não provado.
- O caminho denso continua sem importação do runtime MoE.

- [ ] **Step 1: Run the dense suite**

  Run: `CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom`

  Record total, passed, failed and ignored, including the exact names/messages of failures.

- [ ] **Step 2: Run formatting and focused tests**

  Run: `cargo fmt --all -- --check` and the focused IQ tests. A build CUDA sem ambiente válido deve ser reportada pela saída exata, sem afirmar que foi executada.

- [ ] **Step 3: Update documentation**

  Atualizar a tabela de cobertura IQ, o dtype reabilitado, o primeiro estágio provado e as lacunas restantes. Se nenhum dtype passar por falta de CUDA, registrar o comando e a saída exata e manter a salvaguarda.

- [ ] **Step 4: Commit**

  ```bash
  git add docs/subsystems/freetoken-moe.md crates/node/src/inference/AGENTS.md
  git commit -m "docs: registrar prova diferencial IQ CUDA"
  ```

- [ ] **Step 5: Publish without force**

  Conferir `git status --short --branch`, `git log --oneline origin/fm/ftk-cuda-check..HEAD` e publicar somente se o push for fast-forward:

  ```bash
  git push origin HEAD:refs/heads/fm/ftk-cuda-check
  ```

  Se o remoto divergir novamente, não usar merge nem force; registrar a saída literal e preservar os commits locais.
