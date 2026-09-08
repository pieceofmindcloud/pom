# Plano de trace de inferência e decomposição de outside-node

> Execução no estilo TDD. Cada etapa começa por um teste que falha, implementa
> o menor contrato necessário, roda o teste focado e só então cria um commit.

**Objetivo:** estender a telemetria atual para atribuir o tempo fora dos nós e
decompor TTFT, preservando APIs, inferência, streaming e compatibilidade.

**Arquitetura:** reutilizar o UUID da auditoria como `trace_id`, transportar
metadados de trace no JSON existente, medir durações locais com `Instant`,
acrescentar observações de gateway/peer à `RequestTrace` atual e renderizar o
snapshot final no Chat. Não haverá sistema paralelo de tracing nem alteração
do frame binário.

**Spec:** `docs/superpowers/specs/2026-09-08-inferencia-trace-outside-node-design.md`

## Tarefa 1: contrato comum e matemática de spans

**Arquivos:**

- Modificar `crates/node/src/inference/prefill/support.rs`
- Modificar `crates/node/src/backend/gateway/budget.rs`
- Testar `crates/node/src/inference/prefill/support.rs`
- Testar `crates/node/src/backend/gateway/budget.rs`

- [ ] Escrever testes para trace ID, spans fechados, grupos paralelos e
  cálculo de accounted/unaccounted sem valor negativo.
- [ ] Rodar os testes focados e confirmar a falha do contrato novo.
- [ ] Implementar tipos serializáveis de span, marcos, TTFT e streaming;
  manter campos antigos e traces legados aceitos.
- [ ] Implementar união do caminho crítico para spans locais e deixar spans de
  hosts diferentes como observações correlacionadas, nunca como subtrações.
- [ ] Rodar os testes focados e revisar nomes/serde.
- [ ] Commitar `feat: add inference trace timing model`.

## Tarefa 2: propagar o trace e medir o gateway

**Arquivos:**

- Modificar `crates/node/src/backend/gateway/generation.rs`
- Modificar `crates/node/src/backend/gateway/http.rs`
- Modificar `crates/node/src/backend/openai.rs`
- Modificar `crates/node/src/backend/messages.rs`
- Modificar `crates/node/src/backend/reqlog.rs` apenas se necessário para
  preservar o UUID existente
- Testar handlers e geração em `crates/node/src/backend/`

- [ ] Escrever testes de que uma requisição gera um único UUID reutilizado no
  audit, no `GenerationInput`, no `RequestTrace` e no `x_pom.trace`.
- [ ] Rodar testes focados e confirmar a falha.
- [ ] Iniciar spans de ingresso/validação/orquestração, seleção de pipeline,
  resolução, aquisição de conexão, dispatch, retorno do primeiro token,
  streaming e finalização nas fronteiras reais.
- [ ] Registrar cancelamento, timeout, retry, fallback e desconexão sem deixar
  spans abertos; preservar `[DONE]` e respostas atuais.
- [ ] Adicionar `server_ttft_ms` sem remover `ttft_ms`.
- [ ] Rodar testes focados e commit `feat: trace gateway inference lifecycle`.

## Tarefa 3: medir peer, transporte e contadores de decode

**Arquivos:**

- Modificar `crates/node/src/inference/infer_server/prefill.rs`
- Modificar `crates/node/src/inference/infer_server/step.rs`
- Modificar `crates/node/src/inference/infer_server/core.rs`
- Modificar `crates/node/src/inference/scheduler.rs` apenas onde a espera já
  é determinada
- Modificar `crates/node/src/inference/relay.rs`
- Modificar `crates/node/src/network/peer_client.rs`
- Modificar `crates/node/src/inference/prefill/pipeline.rs`
- Modificar `crates/node/src/inference/prefill/step.rs`
- Testar os módulos de inference e relay

- [ ] Escrever testes para `load/scheduler/lock` wait, primeiro-token local,
  pool novo/reutilizado, retry e `decode_calls`/tokens por chamada.
- [ ] Rodar testes focados e confirmar a falha.
- [ ] Propagar `trace_id`, `span_id` e parent nos metadados JSON existentes.
- [ ] Medir apenas fronteiras do Tokio, sem instrumentar cada future; separar
  processing, queue, encode, headers/body/decode e retransmissão.
- [ ] Registrar MTP/lookup como causa dos múltiplos tokens por decode usando
  os contadores já presentes, sem assumir erro nos 119/64.
- [ ] Rodar testes focados e commit `feat: trace peer transport and decode work`.

## Tarefa 4: perfis e contrato SSE

**Arquivos:**

- Modificar `crates/node/src/utils/config.rs` ou módulo de configuração
  existente, conforme o ponto de integração encontrado
- Modificar `crates/node/src/backend/gateway/budget.rs`
- Modificar `crates/node/src/backend/openai.rs`
- Modificar `crates/node/src/backend/messages.rs`
- Modificar `apps/frontend/src/lib/chatStream.ts`
- Modificar `apps/frontend/src/lib/types.ts`
- Testar Rust e frontend

- [ ] Escrever testes para `POM_PROFILE` off/basic/trace, schema aditivo e
  traces antigos.
- [ ] Rodar os testes focados e confirmar a falha.
- [ ] Integrar o nível de telemetria sem duplicar `POM_PROFILE_SYNC`; publicar
  o resumo sempre que necessário e detalhes somente no nível configurado.
- [ ] Expor `outside_node_*`, TTFT, primeiro-token, streaming, cold/warm e
  decode no `x_pom.trace` mantendo os campos atuais.
- [ ] Medir no browser `performance.now()` para primeiro chunk/token, duração
  e fim da resposta, guardando o snapshot na mensagem.
- [ ] Rodar `npm test -- --run` no conjunto focado e commit
  `feat: expose end to end inference trace`.

## Tarefa 5: timeline visual e diagnóstico honesto

**Arquivos:**

- Modificar `apps/frontend/src/lib/telemetryAnalysis.ts`
- Modificar `apps/frontend/src/routes/Chat.tsx`
- Modificar `apps/frontend/src/components/ExecutionTelemetry.tsx`
- Modificar `apps/frontend/src/i18n/locales.ts`
- Modificar testes de Chat/ExecutionTelemetry

- [ ] Escrever testes para árvore, maior componente, spans paralelos, ausência
  de dados, um peer e vários peers.
- [ ] Rodar testes focados e confirmar a falha.
- [ ] Renderizar a árvore e os resumos com unidades, labels de host e estados
  desconhecidos; não chamar heurística quando faltarem dados.
- [ ] Separar engine/server/client TTFT e distinguir RTT, rede observada,
  conexão, dispatch e entrega do primeiro token.
- [ ] Acrescentar textos em inglês e pt-BR, respeitar tokens/layout existente
  e não editar cópias geradas.
- [ ] Rodar testes focados e commit `feat: show inference trace timeline`.

## Tarefa 6: documentação, inferência real e publicação

**Arquivos:**

- Atualizar `crates/node/src/backend/AGENTS.md`
- Atualizar `crates/node/src/inference/AGENTS.md`
- Atualizar `apps/frontend/AGENTS.md` e os `AGENTS.md` das subpastas tocadas
- Atualizar `docs/subsystems/api-http.md`, `inferencia.md` e `web-ui.md`
- Atualizar `docs/AGENTS.md` com os novos ponteiros

- [ ] Documentar as fontes atuais, relógios locais, ausência de Redis e a
  diferença entre pool reqwest e socket observado.
- [ ] Rodar testes Rust focados, testes frontend e build frontend.
- [ ] Rodar `cargo test --workspace`, comparar com o baseline de 7 falhas e
  registrar somente regressões novas.
- [ ] Executar inferência real disponível e registrar os novos dados do gap;
  se não houver cluster/modelo operacional, preservar a ausência como
  bloqueio operacional sem fabricar números.
- [ ] Rodar `git diff --check`, revisar diff e confirmar que não há arquivos
  gerados ou `llama.cpp` alterados.
- [ ] Fazer `git pull --rebase origin feat/implantar3`, resolver apenas
  conflitos da própria alteração, push sem force e anexar o status `done` com
  o SHA publicado.
