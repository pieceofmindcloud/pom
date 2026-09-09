# Chat da rede Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar um canal de conversa global autenticado e retransmitido pelo principal, nativo do enxame libp2p, com texto, arquivos, typing, histórico persistido, política de cluster e UI bilingue.

**Architecture:** Um codec request/response próprio carrega ações até o principal efetivo. O principal valida API key, `node_id`, PeerId, bloqueio, limite e taxa, persiste e retransmite somente ações aceitas; peers aceitam apenas frames retransmitidos pelo principal. API/UI e SQLite ficam separados do túnel de inferência, com filas `try_send` e worker de persistência limitados.

**Tech Stack:** Rust, Tokio, libp2p request/response, Axum, SQLite/Rusqlite, React, TypeScript, React Query, Tailwind e i18n CSV.

**Spec:** `docs/superpowers/specs/2026-09-09-chat-da-rede-design.md`

## Global Constraints

- O principal efetivo é a única autoridade de validação e retransmissão.
- Toda ação originada na UI contém a API key POM; ela nunca é persistida ou retransmitida.
- Identidade de remetente, destinatário e sala usa `node_id`, nunca hostname ou endereço.
- Chat usa `/pom/network-chat/1.0.0`, separado de `infer_tunnel` e `cluster_logs`.
- Fila limitada usa `try_send`, descarta sob pressão e contabiliza perdas.
- Inferência tem prioridade no loop do swarm e o teste de carga precisa demonstrar isso.
- Documentação pt-BR, i18n en/pt-BR, sem editar arquivos gerados.

---

### Task 1: Contratos puros do protocolo e política

**Files:**
- Create: `crates/node/src/network/network_chat.rs`
- Create: `crates/node/src/network/network_chat_policy.rs`
- Modify: `crates/node/src/main.rs`, `crates/node/src/network/network.rs`
- Test: módulos `#[cfg(test)]` nos dois arquivos novos e em `network.rs`

**Interfaces:**
- `ChatAction`, `ChatRequest`, `ChatResponse`, `ChatEvent` serializáveis.
- `NetworkChat::validate_action`, `NetworkChat::rate_allows` e fila limitada.
- `network_chat_policy::{on_announce, adopt_or_keep}`.
- `Network` expõe `network_chat_enabled`, setter, probe e anuncia o gatilho.

- [ ] Escrever testes vermelhos para limites, fila cheia, token/node, bloqueio, taxa e catraca fail-closed.
- [ ] Rodar `rtk cargo test -p node network_chat` e confirmar falhas por símbolos ausentes.
- [ ] Implementar os contratos e testes mínimos até ficarem verdes.
- [ ] Adicionar `network_chat_enabled` a `Config` e aos fixtures necessários.
- [ ] Rodar os testes focados novamente e revisar o diff.
- [ ] Commitar `feat: define network chat protocol and policy`.

### Task 2: SQLite, serviço e integração libp2p

**Files:**
- Modify: `crates/node/src/backend/db.rs`
- Modify: `crates/node/src/network/discovery.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/backend/gateway/state.rs`
- Test: testes do banco, serviço e discovery

**Interfaces:**
- `PomDb::append_network_chat`, `network_chat_messages` e `network_chat_file`.
- `NetworkChat` com `submit`, `accept_from_peer`, `accept_relay` e `subscribe`.
- `Discovery` recebe `ChatOutbound`, mantém respostas pendentes e envia pelo
  behaviour próprio.

- [ ] Escrever testes vermelhos de persistência, histórico sem typing e leitura do arquivo.
- [ ] Rodar `rtk cargo test -p node network_chat db` e confirmar a falha esperada.
- [ ] Criar schema/indexes e worker de persistência com `try_send`.
- [ ] Adicionar o behaviour `/pom/network-chat/1.0.0`, fila da UI, resposta pendente e prioridade explícita para eventos do swarm.
- [ ] Implementar validação no principal e aceitação de relay somente do principal.
- [ ] Rodar `rtk cargo test -p node` focado em discovery, gateway e banco.
- [ ] Commitar `feat: route network chat through libp2p primary`.

### Task 3: API HTTP, política do admin e teste de prioridade

**Files:**
- Modify: `crates/node/src/backend/gateway/http.rs`
- Modify: `crates/node/src/backend/webui/catalog.rs`
- Modify: `crates/node/src/backend/webui/routes/common.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Modify: `crates/node/src/backend/gateway/state.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/backend/webui/tests/core_b.rs`, gateway/discovery tests

- [ ] Escrever testes vermelhos para GET/POST, chave inválida, bloqueio, disabled e resposta de política.
- [ ] Rodar os testes focados e confirmar a falha correta.
- [ ] Implementar endpoints de estado, ação, arquivo e política, todos com limites e respostas estáveis.
- [ ] Conectar o interruptor somente ao principal e ao ratchet autenticado.
- [ ] Adicionar teste de carga que enche filas de chat e mede RTT de inferência sem alterar o caminho de frames.
- [ ] Rodar `rtk cargo test -p node` nos módulos relevantes.
- [ ] Commitar `feat: expose authenticated network chat API`.

### Task 4: Frontend, Preferências e i18n

**Files:**
- Create: `apps/frontend/src/components/NetworkChat.tsx`
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/routes/Preferences.tsx`
- Modify: `apps/frontend/src/lib/api.ts`, `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/messages.csv`
- Test: testes Vitest de Chat/NetworkChat/Preferences

- [ ] Escrever testes vermelhos para abas, envio com API key, histórico, typing, arquivo e estado disabled.
- [ ] Rodar `rtk npm test -- --run ...` e confirmar falha por componente/contrato novo.
- [ ] Implementar API client com `ensureChatApiKey` em toda ação e polling cancelável.
- [ ] Implementar a aba Rede com participantes por `node_id`, anexos e cópia de mensagens.
- [ ] Implementar o painel de Preferências e exibir estado remoto não editável.
- [ ] Acrescentar todas as chaves em inglês e pt-BR.
- [ ] Rodar `rtk npm test` e `rtk npm run build`.
- [ ] Commitar `feat: add network chat interface`.

### Task 5: Documentação, revisão e entrega

**Files:**
- Modify: `crates/node/src/network/AGENTS.md`
- Modify: `docs/subsystems/rede-p2p.md`
- Modify: `apps/frontend/src/routes/AGENTS.md` e `apps/frontend/src/lib/AGENTS.md` quando os contratos mudarem

- [ ] Atualizar documentação pt-BR com protocolo, política, limites, estrela e prioridade.
- [ ] Rodar `rtk cargo test -p node`, `rtk npm test` e `rtk npm run build` em árvores limpas de saída.
- [ ] Rodar o cenário E2E visual com dois nós de testkit ou harness e revisar a tela no Chrome.
- [ ] Inspecionar `rtk git diff --check`, status e histórico, sem tocar `llama.cpp`.
- [ ] Rebasear sobre `origin/feat/implantar3`, resolver sem descartar commits e repetir validações.
- [ ] Commitar documentação/revisão final, fazer push sem force para `origin/feat/implantar3` e registrar SHA no status.
