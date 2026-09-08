# Implantar Regressions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Usar a versão corrente de um `deploy_id` em todos os nós, começar o
modo modelo inteiro com o mapa vazio e evitar replanejamento de topologia para
mudanças de parâmetros de runtime, mantendo uma única revalidação por mudança
de modelo.

**Architecture:** O frontend trata a troca para `whole` como uma transição que
zera seleção e população preferida. O backend centraliza a leitura distribuída
de um registro por `deploy_id`, escolhendo a cópia com `activated_at` mais
recente e desempate por `active_version`, para que redeploy e deploy versionado
consumam a mesma corrente. O planner continua validando mudanças de topologia,
mas parâmetros de runtime ficam locais até o preflight fresco do deploy; trocar
de modelo limpa o estado pendente e dispara uma única consulta.

**Tech Stack:** React 18, TypeScript, Vitest, Rust, Tokio, Axum, SQLite, serde.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/achados-implantar.md`

## Global Constraints

- Não trocar a comparação por referência por igualdade profunda sem identificar a criação repetida dos objetos.
- Ao mudar de fatias para modelo inteiro, o mapa deve ficar vazio e o operador deve adicionar uma máquina.
- Implantação versionada deve ler a versão corrente do mesmo `deploy_id`, não uma cópia antiga local ou remota.
- Medir a latência de `/api/ui/deploy/plan` e o número de chamadas por mudança antes de corrigir a conferência.
- Não aumentar o debounce de 260 ms nem remover revalidações legítimas de modelo.
- Não tocar no `llama.cpp`, não executar `pom-ksia.sh`, não iniciar `no-mistakes` e não fazer push forçado.
- Rebasear em `origin/feat/implantar` antes de cada publicação e preservar commits de Tiago.
- Atualizar `AGENTS.md` e READMEs de subsistema afetados no mesmo commit, em pt-BR e sem travessão.
- Confirmar as 10 falhas Rust conhecidas e não atribuí-las a esta tarefa.

---

### Task 1: Reproduções dos regressions da tela e do registro

**Files:**
- Modify: `apps/frontend/src/routes/DeployModel.whole.test.tsx`
- Modify: `crates/node/src/backend/deploy.rs`

**Interfaces:**
- Consumes: `DeployModel`, `DeployPlan`, React Query test client, `find_record` e os fixtures já existentes.
- Produces: testes que falham no estado atual para a troca fatias -> inteiro esvaziar o mapa e para a leitura da cópia corrente de um `deploy_id`.

- [x] **Step 1: Add a whole-model transition test with multiple shard machines on the map and assert no machine card remains after choosing `Um modelo inteiro`.**
- [x] **Step 2: Run the transition test and confirm that the old line refuses the change instead of emptying the map.**
- [x] **Step 3: Add an async backend test with a stale local record and a newer peer copy of the same `deploy_id`.**
- [x] **Step 4: Run the targeted Rust test and confirm that the old lookup returns the stale local record.**

### Task 2: Medir o caminho de conferência e separar topologia de runtime

**Files:**
- Inspect: `apps/frontend/src/routes/DeployModel.tsx`
- Inspect: `apps/frontend/src/routes/DeployModel.whole.test.tsx`
- Inspect: `/Users/jairpereira/Sites/firstmate/state/rdma-implantar-testar.status`

**Interfaces:**
- Consumes: `params`, `settled`, `chosen`, `plan.isPlaceholderData` e as
  chamadas reais do trabalhador.
- Produces: evidência da latência do plano e da quantidade de chamadas por
  mudança de modelo e do inspetor, além da separação entre topologia e runtime.

- [x] **Step 1: Measure one inspector change and one model change in the frontend harness.**
- [x] **Step 2: Measure the real node response time through the worker's browser validation on `ksia`.**
- [x] **Step 3: Confirm by static inspection that the extra parameter request is caused by `params` entering the query key and that model changes also change the name and debounce state.**
- [x] **Step 4: Remove runtime parameters and the operational name from the planner key/body; keep the 260 ms debounce only for topology selection.**
- [x] **Step 5: Reset the settled topology synchronously on model changes and add exact-one-request and no-parameter-request regressions.**

### Task 3: Fix whole-model transition semantics

**Files:**
- Modify: `apps/frontend/src/routes/DeployModel.tsx`
- Modify: `apps/frontend/src/routes/DeployModel.whole.test.tsx`

**Interfaces:**
- Consumes: `selectKind`, map membership, `selection`, `preferredNodeIds`, and the existing whole-model add-peer action.
- Produces: an explicit fatias -> whole transition that leaves `mapNodeIds` empty until `addPeerToDeployment` is invoked.

- [x] **Step 1: Change the failing transition test to assert an empty map and no implicit whole-model target.**
- [x] **Step 2: Implement the transition state reset without changing tensor or same-mode behavior.**
- [x] **Step 3: Run the focused whole-model tests for the transition, explicit add and single-machine guard.**
- [x] **Step 4: Commit the whole-model behavior fix with its test.**

### Task 4: Reproduce and fix stale distributed deployment records

**Files:**
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/implantacao.md`

**Interfaces:**
- Consumes: `PomDb::deployment(s)`, `find_record`, `list_deployments`, `Deployment::activated_at`, `active_version`, `ensure_versions` and `publish_record`.
- Produces: one deterministic distributed record lookup that merges local and peer copies and returns the current copy for redeploy and versioned deploy decisions.

- [x] **Step 1: Add a failing async test with a stale local record and a peer copy for the same `deploy_id`, where the later activation has a newer `active_version`/`activated_at`.**
- [x] **Step 2: Run the targeted Rust test and confirm it selects the old record before the fix.**
- [x] **Step 3: Implement shared selection by newest `activated_at`, with `active_version` as the same-timestamp tie-break, and make `find_record` use it instead of returning the first copy.**
- [x] **Step 4: Run the targeted deployment test and verify it now returns the current version.**
- [x] **Step 5: Update the affected maps and subsystem documentation to describe the current lookup behavior.**
- [x] **Step 6: Commit the distributed-record fix and documentation.**

### Task 5: Validation, integration and publication

**Files:**
- Modify: none beyond the tasks above.

**Interfaces:**
- Consumes: committed fixes and tests on `fm/rdma-implantar-corrigir`.
- Produces: a clean branch rebased onto and published to `origin/feat/implantar2`.

- [x] **Step 1: Run frontend build and focused deploy tests; the focused suites pass and the build remains blocked only by the pre-existing editor-import dependencies in `RequestInspector`.**
- [x] **Step 2: Run the correct Rust test targets and the repository baseline command, confirming the known 10 failures remain known.**
- [x] **Step 3: Inspect `git diff`, status, and documentation changes for unrelated edits or generated files.**
- [x] **Step 4: Rebase onto `origin/feat/implantar`, resolve any conflict by preserving both authors, and rerun required tests.**
- [ ] **Step 5: Push the branch to `origin/feat/implantar2` without force and append the final publication status.**
