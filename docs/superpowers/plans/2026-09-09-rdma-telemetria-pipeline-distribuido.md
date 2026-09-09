# Telemetria do pipeline distribuido - plano de implementacao

> **Para agentes:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Os passos usam caixas de selecao (`- [ ]`).

**Goal:** Medir e apresentar a latencia do pipeline distribuido como execucao real, sem transformar telemetria ausente ou tempos sobrepostos em `outside-node`/`unaccounted`.
**Architecture:** Preservar o caminho de resposta no seu ritmo atual, propagar spans agregados de todos os hops pelo `remote-stream`, manter um ledger de telemetria com estado `collecting`/`complete`/`incomplete` e calcular diagnosticos somente a partir de dados completos ou de um residual explicitamente mensurado. O modo normal usa contadores e histogramas; o modo de profiling registra eventos por token.
**Tech Stack:** Rust/Tokio/Axum/serde_json no node, Vitest/React/TypeScript na UI, testes unitarios e de laboratorio existentes.
**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-telemetria-pipeline-distribuido/spec-captain.md`

## Global Constraints

- Investigar e registrar evidencias antes de alterar calculos.
- Ignorar completamente o executor MoE.
- Nunca subtrair a soma de spans acumulados do wall-clock para criar uma decomposicao exclusiva.
- Nao adicionar sincronizacao ao hot path de inferencia; detalhamento por token somente quando `POM_TELEMETRY_PROFILE=1`.
- Atualizar a documentacao da pasta e dos subsistemas afetados no mesmo commit.
- Nao alterar o llama.cpp vendorizado, nao reduzir `FIT_MAX_UBATCH`, nao fazer push ou PR.

---

## Fase 1 - evidencia e contrato de dados

- [ ] Registrar no relatorio a matriz das dez hipoteses, com referencias de codigo, testes e evidencias observadas para o caso curto/longo e para o caminho `remote-stream`.
- [ ] Adicionar testes vermelhos para: regressao linear de `unaccounted` por tokens; spans de dois e tres peers; callbacks remote-stream; transportes atrasados, duplicados e fora de ordem; classificacao com telemetria incompleta.
- [ ] Definir tipos serializaveis para identidade causal (`trace_id`, `request_id`, `pipeline_id`, `peer_id`, `span_id`, `parent_span_id`, `phase`, `token_index`, `stream_id`) e estado de coleta. Relogios entre processos serao duracoes locais ou timestamps com dominio de relogio, nunca diferencas de `Date.now()` entre peers.

## Fase 2 - agregacao no node

- [ ] Criar um ledger de telemetria distribuida que deduplica por identidade de span, aceita ordem arbitraria, conta peers/hops esperados e distingue `response_complete` de `trace_complete`.
- [ ] Propagar `hop_timings` do resultado de cada rodada pelo produtor remote-stream, agregando os hops correspondentes dentro de cada lote sem perder o peer remoto.
- [ ] Medir explicitamente abertura, lifetime, frames, bytes, primeira entrega, latencias de frame, processamento remoto e espera do remote-stream.
- [ ] Instrumentar TTFT em campos semanticamente separados: primeiro evento do motor local, primeiro token do pipeline, TTFT do servidor e TTFT observado no cliente quando disponivel.
- [ ] Permitir que recibos de transporte atrasados atualizem o estado do ledger depois da resposta, com expiracao e sem segurar a resposta do cliente.
- [ ] Expor apenas contadores/histogramas no modo normal; guardar eventos por token em ring buffer amostrado quando `POM_TELEMETRY_PROFILE=1`.

## Fase 3 - diagnostico e UI

- [ ] Remover da classificacao o calculo `timings.total_ms - processing`; retornar `remote_telemetry_incomplete` enquanto faltarem spans e `unattributed` somente apos coleta completa/timeout.
- [ ] Expor separadamente wall-clock, caminho critico medido, compute acumulado, transporte acumulado, overlap e residual verdadeiro, sem apresenta-los como partes exclusivas quando nao houver intervalos suficientes.
- [ ] Atualizar tipos, i18n e `ExecutionTelemetry` para mostrar estado de coleta, peers esperados/observados, cobertura de decode, metricas remote-stream e TTFTs distintos.
- [ ] Manter a UI honesta para snapshots parciais e tardios: resposta nao espera fechamento, e um trace pendente nao recebe uma conclusao de gargalo.

## Fase 4 - validacao e entrega

- [ ] Rodar a matriz obrigatoria A-L, incluindo RTT baixo/artificial, 2/3 peers, resposta curta/longa, remote-stream, libp2p-tunnel, spans tardios, ausentes, duplicados e fora de ordem.
- [ ] Rodar `cargo fmt --check`, testes Rust relevantes, `cd apps/frontend && npm run build`, testes Vitest de telemetria e checar ausencia de alteracoes geradas indevidas.
- [ ] Atualizar `crates/node/src/inference/AGENTS.md`, `crates/node/src/backend/AGENTS.md`, `apps/frontend/src/components/AGENTS.md`, `apps/frontend/src/lib/AGENTS.md` e os READMEs de inferencia/api/web-ui conforme os arquivos realmente tocados.
- [ ] Escrever `data/rdma-telemetria-pipeline-distribuido/report.md` com causa raiz, evidencias, arquivos, modelo antes/depois, testes, overhead e limitacoes.
- [ ] Revisar diff, garantir branch limpa apos commit e append final com o SHA da entrega.
