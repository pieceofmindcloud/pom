# Saída de erro de configuração no macOS - Plano de implementação

> **Para trabalhadores agentic:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o binário `pom` reportar uma configuração inválida com mensagem e código de saída diferente de zero no macOS.

**Architecture:** O processo Rust continua dono do resultado de `async_main`. A ponte Objective-C do item de barra deve apenas acordar e parar o loop do AppKit quando o serviço termina; ela não pode chamar uma operação que encerre o processo antes de `macos_status::run` fazer `join()` e devolver o erro ao `main`.

**Tech Stack:** Rust, Tokio, Cargo integration test, Objective-C/AppKit, WebKit.

**Spec:** Brief `v1 launch-brief` da tarefa `rdma-mac-nao-sobe-saida-zero`.

## Restrições globais

- Não editar o `pom.json` do captain nem qualquer configuração runtime existente.
- Não alterar o `llama.cpp` vendorizado.
- Manter o gateway e a UI funcionando para configurações válidas.
- Não introduzir compatibilidade com esquemas antigos.
- Não publicar via force push nem descartar commits concorrentes.
- Toda saída que impeça o nó de servir deve nomear a razão e retornar código diferente de zero.

---

### Tarefa 1: Teste de regressão do processo

**Arquivos:**
- Criar: `crates/node/tests/startup_config.rs`

**Interfaces:**
- Consome o binário Cargo `pom` via `CARGO_BIN_EXE_pom`.
- Produz uma prova E2E de que `role: infer` é rejeitado com mensagem e status não zero.

- [x] **Passo 1: Escrever o teste que falha**

  Criar uma pasta runtime ignorada dentro do worktree, escrever um `pom.json` mínimo com `role: infer` e `profile: infer`, executar `pom --list-models` com `POM_HOME` apontando para ela e afirmar `!status.success()` e `stderr` contendo `Failed to parse config`.

- [x] **Passo 2: Rodar o teste para confirmar a falha**

  Rodar `cargo test -p node --test startup_config invalid_role_exits_with_config_error -- --exact --nocapture`.

  Resultado esperado antes da correção: falha porque o processo termina com status 0 e não imprime a mensagem de parse.

### Tarefa 2: Preservar o erro através do loop nativo do macOS

**Arquivos:**
- Modificar: `crates/node/src/macos_status.m`

**Interfaces:**
- Mantém `pom_macos_status_stop()` como sinal de parada chamado pela thread de serviço.
- Faz `pom_macos_status_run()` retornar ao chamador Rust sem pedir que AppKit finalize o processo.

- [x] **Passo 1: Fazer a menor mudança no loop**

  Trocar a chamada de encerramento do aplicativo por parada do loop de eventos mais um evento de aplicação que o acorde. O `join()` existente em `macos_status.rs` então recebe o `Result` original do serviço.

- [x] **Passo 2: Rodar o teste de regressão**

  Repetir o comando da Tarefa 1 e confirmar status não zero com `Failed to parse config`.

- [x] **Passo 3: Verificar o caminho válido**

  Executar `pom --list-models` com a configuração válida existente e confirmar que a listagem continua saindo e que o comando termina normalmente.

### Tarefa 3: Documentação e validação

**Arquivos:**
- Modificar: `crates/node/AGENTS.md` se necessário para registrar o contrato de encerramento do serviço macOS.
- Modificar: README de subsistema afetado se a documentação atual não descrever a propagação de erros de inicialização.

- [x] **Passo 1: Atualizar apenas a documentação que ficou incompleta**

  Registrar que a ponte nativa não pode encerrar o processo, porque o retorno do serviço Rust contém o código de saída e a mensagem de configuração inválida.

- [x] **Passo 2: Rodar as verificações**

  `cargo check -p node` e o teste de regressão passaram. `cargo clippy -p node --all-targets --no-deps` terminou com status 0, com avisos preexistentes no crate; o clippy completo falha nos cinco erros preexistentes de `llama-engine`. A suíte completa do node observou sete falhas preexistentes; elas ficam nomeadas no handoff e não são tocadas por esta correção.

### Tarefa 4: Publicação coerente

- [ ] **Passo 1: Conferir diff, testes e branch**
- [ ] **Passo 2: `git pull --rebase` de `origin/feat/implantar3`**
- [ ] **Passo 3: Commitar somente o estado validado**
- [ ] **Passo 4: Fazer push sem force para `origin/feat/implantar3`**
