# Reduzir fila fixa do scheduler de inferência Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** eliminar do caminho padrão as esperas fixas de 4 ms por chamada no scheduler local e no relay, mantendo uma janela explícita configurável para quem priorizar batching.

**Architecture:** `queue_ms` começa em `queued_at`, marcado pelo handler antes de `SchedulerHandle::submit_*`, e termina na entrada do worker, antes do mutex do motor. O worker dorme `cfg.batch_window` para coalescer chegadas quando ainda não atingiu o lote máximo. O default passa a ser zero, então o caminho normal não espera; valores não-zero continuam usando a janela existente e o batching continua selecionando lotes compatíveis. O `BatchCoordinator` do relay deixa de dormir 4 ms e agenda o flush imediatamente, mantendo agrupamento oportunista de chamadas concorrentes.

**Tech Stack:** Rust, Tokio mpsc/oneshot, Axum, serde e configuração persistida em `NodeConfig`/`RuntimeParams`.

**Spec:** `docs/superpowers/specs/2026-08-20-batching-continuo-inferencia-distribuida-design.md`, com a decisão operacional desta tarefa de usar `0 ms` como default de baixa latência e deixar a janela opt-in.

## Global Constraints

- Preservar corretude, backpressure, seleção FIFO/round robin e batching explícito.
- Não trocar a espera por espera ativa ou loop ocupado.
- Não mascarar espera do mutex: `lock_wait_ms` permanece separado de `queue_ms`.
- Atualizar `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md` no mesmo commit que o código.
- Nunca editar `CHANGELOG.md` nem `apps/frontend/src/docs`.
- A compilação Rust pode continuar falhando apenas pelos bindings ausentes já registrados na linha de base.

## Arquivos e fronteiras

| Arquivo | Responsabilidade nesta mudança |
|---|---|
| `crates/node/src/backend/webui.rs` | default público de `RuntimeParams` e teste de desserialização |
| `crates/node/src/inference/runtime.rs` | default serde de `NodeConfig` |
| `crates/node/src/main.rs` | configuração explícita do caminho CLI |
| `crates/node/src/inference/relay.rs` | flush imediato do coalescedor de frames |
| `crates/node/src/inference/AGENTS.md` | contrato do scheduler e leitura da fila |
| `docs/subsystems/inferencia.md` | comportamento operacional e interpretação da telemetria |
| `docs/superpowers/plans/2026-08-26-reduzir-fila-scheduler.md` | registro deste plano |

### Task 1: provar a nova política de default

**Files:**
- Modify: `crates/node/src/backend/webui.rs:5462`
- Verify: `rtk cargo test -p node --bin pom webui::tests::params_default_to_the_validated_cluster_settings -- --nocapture`

- [x] **Step 1: Escrever o teste regressivo**

  Alterar a expectativa existente de `p.batch_window_ms` de `4` para `0`. A mesma bateria já verifica que `batching_enabled` permanece verdadeiro, portanto o teste distingue desabilitar batching de remover a latência padrão.

- [x] **Step 2: Rodar o teste e registrar o RED**

  Executar o comando de verificação. Antes da implementação, a asserção deve reprovar com `left: 4` e `right: 0`; se a compilação parar antes por bindings, registrar essa limitação e preservar a tentativa como evidência.

- [x] **Step 3: Implementar o mínimo**

  Alterar somente `d_batch_window_ms()` e `default_batch_window_ms()` para retornar `0`, ajustar o valor inicial do caminho CLI em `main.rs` para `0`, e retirar o `sleep` fixo de 4 ms do `BatchCoordinator`, mantendo o `flush` assíncrono imediato. Não alterar `SchedulerConfig::from_values`: qualquer valor explicitamente configurado continua virando `Duration::from_millis(batch_window_ms)`.

- [x] **Step 4: Rodar o teste e registrar o GREEN ou a mesma falha de baseline**

  Executar novamente o comando. O teste deve passar quando os bindings estiverem disponíveis; nesta máquina, comparar a lista de erros com a linha de base.

### Task 2: documentar a fronteira e o diagnóstico

**Files:**
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/inferencia.md`
- Modify: `crates/node/src/inference/relay.rs`

- [x] **Step 1: Documentar a causa verificada**

  Registrar que `queue_ms` mede do `queued_at` imediatamente anterior ao submit até a entrada do worker, e que o atraso fixo vinha de `tokio::time::sleep(cfg.batch_window)` em `run_scheduler`; `lock_wait_ms` começa depois e mede o mutex separadamente. Registrar também que o relay tinha outro `sleep` fixo de 4 ms fora dessa métrica.

- [x] **Step 2: Documentar a política atual**

  Registrar default de `0 ms`, batching ainda habilitado, e janela não-zero como opt-in para coalescer chegadas. Explicar que o relay faz flush imediato e ainda pode agrupar chamadas concorrentes, e que a mudança reduz a latência padrão sem espera ativa.

- [x] **Step 3: Verificar a documentação e diff**

  Executar `rtk git diff --check` e conferir que nenhum arquivo gerado ou `CHANGELOG.md` foi alterado.

### Task 3: validação e entrega compartilhada

**Files:**
- Commitar somente os arquivos nomeados nas Tasks 1 e 2 e este plano.

- [x] **Step 1: Rodar validações frescas**

  Executar `rtk cargo fmt --all -- --check`, o teste focado, `rtk cargo test -p node --bin pom scheduler::tests -- --nocapture` e `rtk cargo build --release`. Comparar os erros Rust com a linha de base e executar `rtk git diff --check`.

- [x] **Step 2: Inspecionar o estado antes do commit**

  Executar `rtk git status --short` e `rtk git diff --stat`; rejeitar qualquer arquivo fora da lista deste plano.

- [x] **Step 3: Commitar sem staging amplo**

  Executar `rtk git add docs/superpowers/plans/2026-08-26-reduzir-fila-scheduler.md crates/node/src/backend/webui.rs crates/node/src/inference/runtime.rs crates/node/src/main.rs crates/node/src/inference/relay.rs crates/node/src/inference/AGENTS.md docs/subsystems/inferencia.md` e `rtk git commit -m "fix(inference): remove default scheduler queue delay"`.

- [x] **Step 4: Rebase e empurrar a branch compartilhada**

  Executar `rtk git pull --rebase origin feat/implantar`, resolver somente conflitos triviais nos arquivos desta tarefa, e então `rtk git push origin HEAD:feat/implantar` sem force. Registrar o SHA enviado no status do firstmate.
