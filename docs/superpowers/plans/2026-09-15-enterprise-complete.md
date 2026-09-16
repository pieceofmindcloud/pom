# Enterprise completo Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar ativação, distribuição, host, plugin de auditoria, P2P, UI e empacotamento publicável do Enterprise.

**Architecture:** O POM usa um cliente de licença e uma bridge IPC; `pom-plugin-host` é o único processo que carrega o `cdylib`. O plugin persiste e consulta auditoria em banco próprio, enquanto o POM conserva somente eventos neutros e estado de entitlement. O wire de licença segue o servidor publicado em `pom-website` no commit `60b79a3c`.

**Tech Stack:** Rust 2021, Tokio, Axum, rusqlite, libloading, serde JSON, reqwest/rustls, Ed25519, React/TypeScript, Vitest, shell POSIX.

**Spec:** `docs/superpowers/specs/2026-09-15-enterprise-complete-design.md`

## Global Constraints

- A chave de licença nunca é persistida em texto aberto, enviada a peer ou colocada em URL.
- O `pom` nunca carrega a biblioteca Enterprise diretamente; somente `pom-plugin-host` usa `libloading`.
- Releases só entram no caminho de carregamento após compatibilidade, tamanho, SHA-256 e assinatura do manifest.
- `enterprise.active` só fica verdadeiro após grant válido, plugin pronto e lease válida.
- Logs de processo continuam Free; Logs de Requisições e Estatísticas são Enterprise.
- A UI mantém os menus Enterprise visíveis e clicáveis mesmo sem licença.
- O servidor externo não é modificado neste worktree; o cliente usa mock nos testes.

### Task 1: Congelar o wire protocol e armazenamento local

**Files:**
- Modify: `crates/enterprise-protocol/src/lib.rs`
- Modify: `crates/node/src/enterprise.rs`
- Modify: `crates/node/src/backend/db.rs`
- Test: `crates/enterprise-protocol/src/lib.rs`, `crates/node/src/enterprise.rs`

- [ ] Escrever testes para envelope compatível, manifest assinado e round-trip do estado local.
- [ ] Rodar `cargo test -p enterprise-protocol -p node enterprise` e confirmar falha para os campos incompatíveis.
- [ ] Ajustar DTOs, bearer tokens, erros e persistência versionada.
- [ ] Rodar as suítes focadas e `cargo fmt --all -- --check`.

### Task 2: Cliente de licença e rotas locais

**Files:**
- Modify: `crates/node/src/enterprise.rs`
- Modify: `crates/node/src/backend/webui/routes/common.rs`
- Modify: `crates/node/src/backend/webui/routes/admin.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Test: `crates/node/src/backend/gateway/tests.rs`

- [ ] Escrever testes para validate, activate, refresh, deactivate, retry e estados revogado/expirado.
- [ ] Rodar os testes e confirmar falha por ausência das rotas.
- [ ] Implementar cliente HTTP mockável e endpoints locais autenticados sem expor a chave.
- [ ] Rodar testes de gateway focados.

### Task 3: Host supervisionado e download atômico

**Files:**
- Modify: `crates/plugin-host/src/main.rs`
- Modify: `crates/enterprise-protocol/src/abi.rs`
- Modify: `crates/node/src/enterprise.rs`
- Test: `crates/plugin-host/src/main.rs`

- [ ] Escrever testes para query IPC, limites, timeout, hash, assinatura, restart e rollback.
- [ ] Rodar `cargo test -p pom-plugin-host` e confirmar falha dos novos comportamentos.
- [ ] Implementar supervisão, requests `query`, encerramento com drain e instalação atômica.
- [ ] Rodar testes do host e do cliente.

### Task 4: Plugin Enterprise e geração do release

**Files:**
- Create: `crates/enterprise-plugin/Cargo.toml`
- Create: `crates/enterprise-plugin/src/lib.rs`
- Create: `scripts/build-enterprise-plugin.sh`
- Modify: `Cargo.toml`
- Test: `crates/enterprise-plugin/src/lib.rs`

- [ ] Escrever testes da ABI, banco isolado, eventos begin/complete, filtros e stats.
- [ ] Rodar `cargo test -p enterprise-plugin` e confirmar falha por crate ausente.
- [ ] Implementar `cdylib`, SQLite próprio, query JSON e empacotamento por plataforma.
- [ ] Rodar testes e o script em modo de saída temporária.

### Task 5: Bridge e extração física da auditoria

**Files:**
- Create: `crates/node/src/enterprise_bridge.rs`
- Modify: `crates/node/src/backend/gateway/state.rs`
- Modify: `crates/node/src/backend/gateway/generation.rs`
- Modify: `crates/node/src/backend/webui/routes/admin.rs`
- Modify: `crates/node/src/backend/reqlog.rs`
- Test: `crates/node/src/backend/gateway/tests.rs`

- [ ] Escrever teste que Free não persiste prompt e plugin ativo recebe begin/complete/query.
- [ ] Rodar o teste e confirmar falha antes da bridge.
- [ ] Implementar fila limitada, proxy de query e migração explícita sem apagar o banco antigo.
- [ ] Rodar testes de auditoria e gateway.

### Task 6: Heartbeat e P2P híbrido

**Files:**
- Create: `crates/node/src/enterprise_p2p.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/network/*` somente nos módulos de transporte autenticado necessários
- Test: `crates/node/src/backend/gateway/tests.rs`, `crates/node/src/enterprise.rs`

- [ ] Escrever testes de fingerprint/grant por peer, pacote sem chave, peer ausente e cluster incompleto.
- [ ] Rodar os testes e confirmar falha do protocolo novo.
- [ ] Implementar mensagens limitadas no canal autenticado e reconciliação do admin.
- [ ] Rodar testes de rede e gateway.

### Task 7: UI de ativação e entitlement

**Files:**
- Create: `apps/frontend/src/routes/Enterprise.tsx`
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/lib/api.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/components/nav.ts`
- Modify: `apps/frontend/src/i18n/messages.csv`
- Test: `apps/frontend/src/routes/Enterprise.test.tsx`, `apps/frontend/src/App.test.tsx`

- [ ] Escrever testes para formulário, estados e ausência de chave no storage.
- [ ] Rodar `npm test -- src/routes/Enterprise.test.tsx` e confirmar falha.
- [ ] Implementar tela, chamadas ao nó local, progresso, retry e link de compra.
- [ ] Rodar `npm test` e `npm run build`.

### Task 8: Hardening, documentação e release

**Files:**
- Modify: `docs/subsystems/licenciamento-enterprise.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `AGENTS.md` in touched folders
- Test: workspace and frontend suites

- [ ] Escrever testes de falhas de rede, relógio, update, rollback e revogação.
- [ ] Rodar os testes novos antes dos ajustes finais.
- [ ] Atualizar documentação com URLs, comandos de geração/publicação e limites atuais.
- [ ] Rodar todos os gates: `cargo fmt --all -- --check`, `cargo test --workspace`, `npm test`, `npm run build`, `git diff --check`.
- [ ] Commitar o resultado na branch de trabalho.
