# Logs da implantação - Implementation Plan

> **For agentic workers:** execute este plano task-by-task, mantendo o ciclo
> RED/GREEN e verificando cada etapa antes de seguir.

**Goal:** Entregar a consola persistente e em tempo quase real dos logs de
processo do cluster, recebidos somente pelo nó principal.

**Architecture:** Uma camada de tracing captura sem bloqueio em fila limitada.
Uma task em lotes envia por protocolo libp2p próprio apenas ao principal, que
persiste em SQLite pela sequência de chegada. A UI consulta uma janela ordenada
e o interruptor adota política autenticada pela catraca inspirada no TLS.

**Tech Stack:** Rust, Tokio, tracing-subscriber, libp2p request/response,
SQLite via rusqlite, Axum, React, React Query, Tailwind e Vitest.

**Spec:** `docs/superpowers/specs/2026-09-04-logs-implantacao-design.md`

## Global Constraints

- Captura, envio e persistência usam filas limitadas e nunca bloqueiam a
  inferência.
- O protocolo de logs é separado de `infer_tunnel.rs`.
- A ordem é a sequência de chegada ao principal; timestamp de origem não
  reordena eventos.
- Só o principal recebe lotes e só ele pode alterar a política.
- `FIT_MAX_UBATCH` permanece em 512 e o llama.cpp vendorizado não é alterado.
- Documentação nova e atualizações de `AGENTS.md` ficam em pt-BR e usam hífen.
- O benchmark de tok/s ligado/desligado deve ser evidência real no commit.

---

### Task 1: contratos puros e banco

**Files:**
- Create: `crates/node/src/network/cluster_logs.rs`
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/backend/db.rs`

**Steps:**

- [ ] Escrever testes RED para sequência de chegada, descarte limitado,
  associação ao `deploy_id` e política `adopt_or_keep`.
- [ ] Implementar os tipos de linha, fila, protocolo e operações SQLite
  mínimas para deixar os testes GREEN.
- [ ] Manter o schema novo explícito e adicionar os testes de schema/reabertura.

### Task 2: swarm dedicado e política autenticada

**Files:**
- Modify: `crates/node/src/network/discovery.rs`
- Create or modify: `crates/node/src/network/cluster_logs_policy.rs`
- Modify: `crates/node/src/main.rs`

**Steps:**

- [ ] Escrever testes RED para gatilho somente do principal e falha que preserva
  o valor local.
- [ ] Adicionar behaviour próprio, filas best-effort e ingestão apenas no
  principal.
- [ ] Integrar a catraca e a inicialização tanto no caminho CLI quanto no
  gateway-only.

### Task 3: rotas administrativas

**Files:**
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/utils/config.rs`
- Modify: `crates/node/src/backend/gateway.rs` tests

**Steps:**

- [ ] Escrever testes RED para autoridade, toggle ao vivo e listagem filtrada.
- [ ] Implementar `/api/ui/cluster-logs`, `/api/ui/network/logs` e
  `/api/cluster/logs` autenticadas conforme o boundary atual.
- [ ] Confirmar que nenhum nó não-principal persiste lote remoto.

### Task 4: tela e navegação

**Files:**
- Create: `apps/frontend/src/routes/DeploymentLogs.tsx`
- Create: `apps/frontend/src/routes/DeploymentLogs.test.tsx`
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/components/nav.ts`
- Modify: `apps/frontend/src/routes/Preferences.tsx`
- Modify: `apps/frontend/src/routes/Deployments.tsx`
- Modify: `apps/frontend/src/lib/api.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Steps:**

- [ ] Escrever teste RED da rota, rótulo distinto, linha com nó, ordem e filtro.
- [ ] Implementar a tela terminal responsiva, com foco visível e estados vazio,
  erro e perdas.
- [ ] Colocar o item logo abaixo de Histórico e o link mínimo no Histórico.
- [ ] Adicionar o interruptor junto do TLS, desabilitado fora do principal.
- [ ] Rodar `npm run build` e a suíte da UI.

### Task 5: validação e publicação

**Files:**
- Modify: `crates/node/src/network/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `docs/subsystems/rede-p2p.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`

**Steps:**

- [ ] Rodar testes Rust focados e `cargo test`.
- [ ] Rodar build/test da UI.
- [ ] Medir tok/s desligado e ligado nas mesmas condições e registrar os
  números no commit.
- [ ] Rebasear sobre `origin/feat/implantar3`, resolver apenas conflitos da
  própria parte e publicar sem force.
