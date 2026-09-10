# Telemetria e MTP no pipeline distribuído - Plano de implementação

> **Para agentes:** use a subskill `superpowers:executing-plans` ou `superpowers:subagent-driven-development` para executar este plano tarefa por tarefa.

**Objetivo:** fazer o gateway decidir MTP pela capacidade efetiva do plano e fazer Chat/Painel exibirem o transporte e RTT observados na execução distribuída, sem confundir probes libp2p com telemetria de inferência.

**Arquitetura:** o plano selecionado é a autoridade para habilitar MTP quando o gateway é apenas roteador; uma configuração local explicitamente desabilitada continua sendo kill-switch. A API de telemetria consulta o trace persistido da requisição e deriva uma medição por aresta dos `transport_summary` do hop que realmente executou aquela aresta. Sem trace correspondente, a UI mostra ausência de medição, não um valor libp2p reaproveitado.

**Tecnologias:** Rust, Tokio, SQLite/rusqlite, serde JSON, React/TypeScript, Cargo e Vite.

**Especificação:** `/Users/jairpereira/Sites/firstmate/data/rdma-telemetria-pipeline-distribuido/spec-captain.md` e `data/rdma-telemetria-pipeline-distribuido/brief.md`.

## Restrições globais

- Não alterar o llama.cpp vendorizado nem reduzir `FIT_MAX_UBATCH` de 512.
- Não usar correção por suposição: a origem do tempo e do transporte deve estar no trace da execução.
- O executor MoE está fora de escopo.
- Detalhes por token continuam limitados ao modo profiling/debug; o caminho normal usa agregados.
- Não mascarar `unaccounted` renomeando-o.
- Não fazer push, PR ou alteração fora deste worktree.
- Documentação atualizada no mesmo commit para cada pasta/subsistema tocado.

---

### Tarefa 1: habilitação de MTP no gateway sem runtime local

**Arquivos:**
- Modificar: `crates/node/src/backend/gateway/budget.rs`
- Modificar: `crates/node/src/backend/gateway/generation.rs`
- Testar: `crates/node/src/backend/gateway/tests.rs`

**Interface:** `mtp_enabled_for_plan(Option<bool>, bool) -> bool` retorna falso sem capacidade no plano, verdadeiro quando o runtime local é desconhecido e o plano suporta MTP, e respeita `Some(false)`.

- [x] Escrever o teste vermelho para gateway sem runtime local.
- [x] Executar `cargo test -p node --bin pom gateway_sem_runtime_local_usa_capacidade_mtp_remota` e confirmar falha por função ausente.
- [x] Implementar a função e usar o resultado no gate de especulação da geração.
- [x] Registrar nos logs se a decisão veio do runtime local ou do plano.
- [x] Executar o teste focado e os testes de streaming MTP.

### Tarefa 2: fonte de transporte por execução

**Arquivos:**
- Modificar: `crates/node/src/backend/webui/catalog.rs`
- Modificar: `crates/node/src/backend/reqlog.rs`
- Testar: `crates/node/src/backend/webui/tests/core_b.rs`

**Interface:** `execution_transport_measurements(&Value, &HashSet<String>) -> Vec<ChatTelemetryMeasurement>` deriva arestas adjacentes dos `transport_summary` do hop downstream, incluindo transporte dominante, RTT mediano e chamadas.

- [x] Escrever teste com hop de controle libp2p e hop da aresta de modelo `https-link`.
- [x] Executar o teste e confirmar falha por helper ausente.
- [x] Implementar consulta do trace exato e dos traces recentes até achar as arestas pedidas.
- [x] Fazer `/api/ui/chat/telemetry` preferir `execution_id`; sem trace correspondente, retornar desconhecido em vez de `RttTable` libp2p.
- [x] Executar os testes de parsing, persistência e endpoint.

### Tarefa 3: conectar Chat e Painel à evidência

**Arquivos:**
- Modificar: `apps/frontend/src/lib/queries.ts`
- Modificar: `apps/frontend/src/lib/AGENTS.md`
- Modificar: `docs/subsystems/api-http.md` e `docs/subsystems/web-ui.md`

- [x] Enviar `execution_id` junto dos `node_ids` no hook do Chat.
- [x] Documentar que o Painel usa a última execução correspondente e deixa RTT desconhecido quando não há execução.
- [x] Rodar `npm run build`.

### Tarefa 4: relatório e validação

**Arquivos:**
- Modificar: `data/rdma-telemetria-pipeline-distribuido/report.md`
- Atualizar: `crates/node/src/backend/AGENTS.md`, `crates/node/src/backend/webui/AGENTS.md` ou documentos equivalentes tocados

- [x] Registrar causa raiz, evidência estática/dinâmica disponível, modelo antes/depois, testes, overhead e limitações.
- [x] Rodar formatação, testes focados, testes do crate e build frontend.
- [ ] Capturar E2E pós-deploy e registrar `proposed`, `accepted`, `rejected`, TTFT e tok/s.
- [ ] Verificar diff, branch e worktree limpa; criar commit local.
