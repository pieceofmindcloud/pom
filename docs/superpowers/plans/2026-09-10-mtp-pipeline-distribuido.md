# MTP no pipeline distribuído Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Permitir que o pipeline fatiado do Ornith use a cabeça MTP da cauda e envie vários tokens candidatos por rodada, sem confundir K-shift com rollback de KV.

**Architecture:** O planejador continua negociando `CAP_SPECULATE_MTP` na cauda e `CAP_SPECULATE_MTP_SLICED` em todos os hops. O gate de MTP da cauda será baseado em `mtp_ready` e no rollback efetivo feito por `kv_remove`; `kv_can_shift` continuará exposto como diagnóstico, mas não bloqueará a produção de `next_draft`. O stream remoto já transporta `next_draft`, `tokens` e `mtp_k`, então a mudança não cria uma nova travessia.

**Tech Stack:** Rust, Tokio, serde/JSON, `llama-engine` e o wire binário `inference/frame-v1-batch`.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-telemetria-pipeline-distribuido/brief.md`, seção “MTP NO PIPELINE”.

## Global Constraints

- Não alterar o fork vendorizado do llama.cpp.
- Não reduzir `FIT_MAX_UBATCH` nem `n_batch`; o limite público continua 512 ou maior.
- Não introduzir uma segunda chave de perfil; a telemetria usa `POM_PROFILE` e `POM_PROFILE_SYNC`.
- Ignorar o executor MoE, que permanece fora deste caminho.
- Preservar a falha fechada se `kv_remove` real recusar o rollback, evitando avançar `n_past` incorretamente.
- Manter MTP e prompt-lookup como fontes mutuamente exclusivas de especulação.

### Task 1: Provar o gate funcional incorreto

**Files:**
- Modify: `crates/node/src/inference/infer_server/direct.rs:190-210`
- Test: `crates/node/src/inference/infer_server/direct.rs:414-427`

**Interfaces:**
- Consumes: `mtp_ready`, `mtp_unavailable_reason` e o diagnóstico atualmente chamado `kv_can_shift`.
- Produces: `mtp_tail_speculation_reason(mtp_ready, mtp_unavailable_reason)` deve retornar `None` quando a cabeça MTP está pronta, mesmo que K-shift seja falso.

- [x] **Step 1: Alterar primeiro a expectativa da regressão**

  No teste `razoes_da_cauda_separam_nextn_modelo_e_prontidao`, trocar somente a expectativa do caso pronto com K-shift falso para `None`, mantendo as expectativas de ausência de `nextn` e erro de alocação.

- [x] **Step 2: Rodar a regressão e confirmar RED**

  Run: `cargo test -p node --bin pom infer_server::direct::speculative_tests::razoes_da_cauda_separam_nextn_modelo_e_prontidao -- --exact --nocapture`

  Expected: FAIL porque a implementação atual devolve `Some("hybrid_tail_no_rollback")` quando `kv_can_shift=false`.

- [x] **Step 3: Documentar a prova operacional no relatório**

  Registrar que o health do tail Salem mostrou `mtp_ready=true`, `mtp_allocation="vram"`, `kv_can_shift=false`, enquanto o trace anterior mostrou 89 chamadas de decode e zero tokens MTP propostos/aceitos/rejeitados. Registrar também que `llama_memory_can_shift` é diferente de `llama_memory_seq_rm`, usado por `LlmEngine::kv_remove`.

### Task 2: Liberar MTP quando houver cabeça e preservar rollback real

**Files:**
- Modify: `crates/node/src/inference/infer_server/direct.rs:180-210`
- Modify: `crates/node/src/inference/infer_server/step_blocking.rs:350-405,505-555`
- Test: `crates/node/src/inference/infer_server/direct.rs:414-429`

**Interfaces:**
- Consumes: `LlmEngine::mtp_ready`, `LlmEngine::mtp_draft`, `LlmEngine::mtp_accept` e `LlmEngine::kv_remove`.
- Produces: cauda MTP retorna `next_draft` com K-shift falso; rollback remoto continua exigindo `kv_remove=true` e aborta a rodada se a remoção efetiva falhar.

- [x] **Step 1: Remover somente o ramo de bloqueio por K-shift**

  Em `mtp_tail_speculation_reason`, manter a recusa por `!mtp_ready` e remover a condição `if !kv_can_shift`. O parâmetro pode ser removido da assinatura se todos os chamadores e testes forem atualizados; `kv_can_shift` continua em `/health` para diagnóstico.

- [x] **Step 2: Atualizar os chamadores sem alterar o rollback**

  Em `step_blocking`, calcular a razão da cauda apenas com prontidão MTP e motivo de indisponibilidade. Manter intactos os blocos que chamam `s.engine.kv_remove(...)` e retornam erro quando o rollback remoto não é aceito.

- [x] **Step 3: Rodar a regressão e confirmar GREEN**

  Run: `cargo test -p node --bin pom infer_server::direct::speculative_tests::razoes_da_cauda_separam_nextn_modelo_e_prontidao -- --exact --nocapture`

  Expected: PASS, incluindo `mtp_ready=true` com K-shift falso e as recusas de motor sem `nextn`.

- [x] **Step 4: Rodar testes de contrato do stream e wire**

  Run: `cargo test -p node --bin pom infer_server::step::tests -- --nocapture`

  Expected: PASS, incluindo propagação de `next_draft`, `speculation_active`, `speculation_disabled_reason`, `tokens` e `mtp_k`.

### Task 3: Atualizar documentação e telemetria do achado

**Files:**
- Modify: `crates/node/src/inference/AGENTS.md:819,982-983,1258-1261`
- Modify: `docs/subsystems/inferencia.md:1600-1610`
- Modify: `data/rdma-telemetria-pipeline-distribuido/report.md` na próxima seção de validação MTP

**Interfaces:**
- Consumes: comportamento validado de `mtp_tail_speculation_reason` e o contrato de rollback de `kv_remove`.
- Produces: documentação que não chama K-shift de requisito de rollback e relatório com causa, trace pré-correção e métricas pós-deploy.

- [x] **Step 1: Corrigir a descrição da guarda**

  Explicar que o gate exige `mtp_ready`; K-shift é somente diagnóstico. A segurança do rollback é provada pela chamada real a `kv_remove`, que continua falha fechada.

- [x] **Step 2: Registrar o estado pré-correção**

  Incluir os campos observados no cluster, o trace `chatcmpl-4a70496f44c24917aa68e55fddaa495f`, zero drafts e a razão causal `hybrid_tail_no_rollback`.

### Task 4: Verificar localmente e publicar para redeploy

**Files:**
- Test: suítes existentes de `node`, `llama-engine` e `prefill` conforme os arquivos alterados.

- [x] **Step 1: Formatar e verificar a árvore**

  Run: `cargo fmt --all -- --check` e `git diff --check`. O diff de formato
  global contém alterações preexistentes fora do escopo; os arquivos tocados
  nesta correção foram mantidos no formato local e `git diff --check` passou.

- [x] **Step 2: Rodar a suíte direcionada**

  Run: `cargo test -p node --bin pom -- --test-threads=1`

  Expected: todos os testes aprovados, sem editar o fork do llama.cpp.

- [ ] **Step 3: Commitar em partes coerentes**

  Run: `git add crates/node/src/inference/infer_server/direct.rs crates/node/src/inference/infer_server/step_blocking.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md data/rdma-telemetria-pipeline-distribuido/report.md docs/superpowers/plans/2026-09-10-mtp-pipeline-distribuido.md && git commit -m "fix: liberar MTP para cauda recorrente"`

- [ ] **Step 4: Rebasear e publicar sem force**

  Run: `git fetch origin && git pull --rebase origin feat/implantar3 && git push origin HEAD:feat/implantar3`

- [ ] **Step 5: Registrar o SHA e aguardar redeploy**

  Confirmar `git rev-parse HEAD` e `git rev-parse origin/feat/implantar3`. O captain fará o redeploy nas duas máquinas com `POM_PROFILE=trace` e `POM_PROFILE_SYNC=1`; a rodada E2E deve medir `mtp_draft_tokens_proposed`, `mtp_draft_tokens_accepted`, `mtp_draft_tokens_rejected`, batch width, generation tok/s, wall tok/s e TTFT.

## Self-review

- O plano cobre a causa demonstrada no tail, a regressão RED/GREEN, o rollback efetivo, a documentação e a publicação.
- Não há mudança no protocolo de wire: ele já carrega os campos necessários.
- Não há fallback silencioso para lookup: se MTP falhar após a correção, a resposta nomeia a razão real ou falha fechada no rollback.
