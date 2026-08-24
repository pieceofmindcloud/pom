# RDMA Hugging Face Compile Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o crate `node` da linha `feat/implantar` compilar e testar com o provider Hugging Face, corrigindo os erros de tipos, ownership e lifetime sem enfraquecer a redação de dados sensíveis.

**Architecture:** Preservar a fronteira atual entre `ModelProvider`, o runner `hf` e o backend de jobs. Tornar a função de redação aceita tanto texto emprestado quanto possuído, mover apenas dados próprios para tasks `'static`, e documentar o preparo local do fork nativo sem alterar o checkout primário.

**Tech Stack:** Rust 2021, Tokio, Axum, Cargo, `llama-cpp-sys-2`, Hugging Face CLI, npm/Vite.

**Spec:** Requisitos da tarefa `FIRSTMATE_OP: v1 launch-brief` para corrigir a compilação de `feat/implantar`.

## Global Constraints

- Trabalhar somente na branch local `fm/rdma-hf-compile-fix`, baseada em `origin/feat/implantar`.
- Não tocar no fork nativo do `llama.cpp` nem no checkout primário.
- Não modificar `CHANGELOG.md` nem arquivos gerados.
- Manter a redação de tokens, headers de autorização e caminhos sensíveis antes de expor mensagens.
- Verificar com `cargo build -p node`, `cargo test -p node`, `cargo fmt --check`, `cargo clippy` e a suíte/build da frontend antes do commit.

---

### Task 1: Redação segura e métricas do download

**Files:**
- Modify: `crates/node/src/models/provider.rs:122-123,319-323,tests`
- Test: `crates/node/src/models/provider.rs` unit tests

**Interfaces:**
- Consumes: `HfError` e `LocalModelStorage` existentes.
- Produces: `redact_sensitive<T: AsRef<str>>(text: T) -> String`, mantendo os mesmos marcadores e saída `[REDACTED]`; `total_bytes` explicitamente `u64`.

- [x] **Step 1: Write the failing test**

  Acrescentar um teste unitário que passe `String` possuída a `redact_sensitive` e verifique que o token e o caminho sensível não aparecem no retorno, mantendo o teste existente para `&str`.

- [x] **Step 2: Run test to verify it fails**

  Run: `cargo test -p node model_provider::tests::redact_sensitive_accepts_owned_text -- --exact`

  Expected: falha de compilação porque a assinatura atual aceita apenas `&str`.

- [x] **Step 3: Write minimal implementation**

  Trocar o parâmetro por `T: AsRef<str>` e iniciar a cópia com `text.as_ref().to_string()`. Anotar `let total_bytes: u64 = ...` para resolver a inferência usada na aritmética de velocidade.

- [x] **Step 4: Run test to verify it passes**

  Run: `cargo test -p node model_provider::tests::redact_sensitive_accepts_owned_text_without_leaking_secrets -- --exact`

  Expected: PASS, sem o token nem o caminho sensível na saída.

### Task 2: Ownership e lifetime do backend de downloads

**Files:**
- Modify: `crates/node/src/backend/model_provider.rs:9-13,384-456,500-521,939-960`
- Test: o build/teste de `node`, usando a compilação existente como regressão dos diagnósticos E0597, E0382 e E0507.

**Interfaces:**
- Consumes: `ModelFiles`, `Job`, `DownloadRequest`, `run_download` e `is_installed_file` existentes.
- Produces: tasks assíncronas que capturam somente valores possuídos e `model_provider` sem import não usado.

- [x] **Step 1: Run the failing compile regression**

  Run: `cargo build -p node`

  Expected: falha nos erros de empréstimo de `files.files`, movimento parcial de `job` e movimento de `filename` dentro do closure `FnMut`, além do import `ModelSummary` não usado.

- [x] **Step 2: Write minimal implementation**

  Clonar o `ModelFile` encontrado antes de criar a task e usar o valor possuído dentro de `tokio::spawn`. Em `retry_download`, consumir os campos de `job` sem chamar `drop(job)` depois do movimento parcial. Em `is_installed_file`, emprestar `&filename` em cada `directory.join`. Remover `ModelSummary` do import.

- [x] **Step 3: Run the compile regression**

  Run: `cargo build -p node`

  Resultado: o crate compila sem os 16 erros reportados e sem o aviso de
  `ModelSummary`; a suíte também revelou e corrigiu a divisão por zero do
  cálculo de velocidade quando o download termina em menos de 1 ms.

### Task 3: Preparação reproduzível do fork nativo

**Files:**
- Modify: `docs/subsystems/build-execucao.md` seção do fork `llama.cpp`
- Modify: `crates/node/AGENTS.md` seção de build
- Modify: `crates/node/src/models/AGENTS.md` nota do provider
- Modify: `crates/node/src/backend/AGENTS.md` nota do job de download
- Modify: `vendor/llama-cpp-sys-2/AGENTS.md` preparo de worktree

**Interfaces:**
- Consumes: `vendor/llama-cpp-sys-2/build.rs`, que lê `<CARGO_MANIFEST_DIR>/llama.cpp`.
- Produces: instrução explícita para um worktree criar `llama.cpp` local ou clonar o fork diretamente sobre o symlink, sem alterar o checkout primário.

- [x] **Step 1: Document the verified setup**

  Registrar o comando de verificação e a preparação local usando um checkout fora do worktree, além de indicar que o alvo precisa conter `ggml` antes de `cargo build -p node`.

- [x] **Step 2: Document the code invariants**

  Atualizar os mapas das pastas para registrar que mensagens do provider passam por `redact_sensitive` e que tasks de download não podem capturar referências de respostas locais.

### Task 4: Verificação completa e entrega local

**Files:**
- Modify: `docs/superpowers/plans/2026-08-24-rdma-hf-compile-fix.md` checkboxes

- [x] **Step 1: Run Rust formatting and lint**

  Run: `cargo fmt --check` e `cargo clippy -p node`.

  Resultado: ambos terminaram com código `0`. O clippy ainda reporta avisos
  preexistentes fora do diff, mas nenhum aviso novo foi introduzido por esta
  correção.

- [x] **Step 2: Run Rust build and tests**

  Run: `cargo build -p node` e `cargo test -p node`.

  Resultado: build com código `0`; 752 testes executados, 747 passaram, 0
  falharam e 5 foram ignorados.

- [x] **Step 3: Run frontend checks**

  Run: `npm run build` e a suíte `npm test` em `apps/frontend`, conforme os scripts reais do `package.json`.

  Resultado: build com código `0`; 48 arquivos e 287 testes passaram.

- [x] **Step 4: Inspect the diff and commit**

  Run: `git diff --check` e `git status --short`; confirmar que o fork nativo ignorado não entrou no diff, então fazer `git add` somente dos arquivos versionados e `git commit -m "fix: compile hugging face provider"`.
