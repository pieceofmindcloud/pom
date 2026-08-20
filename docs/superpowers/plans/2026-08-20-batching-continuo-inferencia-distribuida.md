# Batching contínuo de inferência distribuída Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** executar várias sessões independentes em lotes multi-sequência no contexto local de cada peer, transportar lotes entre peers compatíveis e manter Tensor Parallelism alinhado sem misturar KV, logits ou residuais.

**Architecture:** cada processo/peer terá um scheduler local e uma fila limitada. Réplicas da mesma faixa de camadas não compartilham scheduler nem KV: o planejador escolhe uma réplica por sessão e mantém afinidade durante a geração. Um grupo de Tensor Parallelism continua sendo uma unidade coletiva, com um scheduler primário coordenando o mesmo `batch_id` nos workers.

**Tech Stack:** Rust, Tokio mpsc/oneshot, Axum, llama.cpp via `llama-cpp-sys-2`, frames binários f16/zstd-f16, `gateway::testkit`.

**Spec:** `docs/superpowers/specs/2026-08-20-batching-continuo-inferencia-distribuida-design.md`

## Global Constraints

- Não usar `llama-server`; o POM continua usando o `llama_context` em processo.
- Não modificar `vendor/llama-cpp-sys-2/llama.cpp`.
- Nenhum `llama_decode` concorrente compartilha o mesmo contexto.
- Réplicas de uma faixa têm schedulers locais independentes e não migram KV entre si.
- Um peer sem `frame/v3-batch` recebe apenas o protocolo que anunciou.
- Falha de uma barreira TP invalida o lote inteiro.
- Nenhum peso GGUF é transferido para habilitar batching.
- Testes novos devem provar a falha antes da implementação correspondente.
- Atualizar `AGENTS.md` da pasta e os READMEs de subsistema afetados no mesmo commit.
- Nunca editar `CHANGELOG.md` nem `apps/frontend/src/docs`.

---

## Mapa de arquivos e fronteiras

| Arquivo | Responsabilidade nesta mudança |
|---|---|
| `crates/node/src/inference/scheduler.rs` | limites, admissão, fairness, seleção de microbatches e comandos locais |
| `crates/node/src/inference/llm.rs` | descritores de lote, construção de `llama_batch`, logits e residual por item |
| `crates/node/src/inference/infer_server.rs` | handlers HTTP finos, worker do scheduler, sessão e resposta por item |
| `crates/node/src/inference/wire.rs` | `frame/v3-batch`, validação de offsets e compatibilidade |
| `crates/node/src/inference/relay.rs` | envio e retorno de frames multi-item |
| `crates/node/src/inference/prefill.rs` | encaminhamento de batches por pipeline e afinidade de sessão |
| `crates/node/src/inference/tp.rs` | chaves de barreira com `batch_id` e sincronização dos ranks |
| `crates/node/src/backend/gateway.rs` | seleção de rota/replica e telemetria de fila por requisição |
| `crates/node/src/network/network.rs` | anúncio das capacidades e limites do scheduler |
| `crates/node/src/inference/runtime.rs` | configuração persistida e defaults do batching |
| `crates/node/src/backend/webui.rs` | round-trip dos parâmetros no deploy e na reconfiguração |
| `crates/node/src/main.rs` | registro do módulo e defaults para configuração por CLI |
| `crates/node/src/inference/AGENTS.md` | contrato do scheduler, wire e TP |
| `docs/subsystems/inferencia.md` | comportamento operacional, fila, batching e compatibilidade |
| `docs/subsystems/paralelismo.md` | diferença entre réplica de pipeline e grupo TP |
| `crates/node/src/backend/AGENTS.md` | afinidade de rota e backpressure no gateway |

### Task 1: limites locais e scheduler independente por peer

**Files:**
- Create: `crates/node/src/inference/scheduler.rs`
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Test: `crates/node/src/inference/scheduler.rs`

**Interfaces:**
- Produces `SchedulerConfig`, `WorkClass`, `QueueItem<T>`, `BatchSelection<T>` e `SchedulerQueue<T>`.
- `SchedulerQueue::try_push(item) -> Result<(), QueueFull>` aplica o limite antes de criar KV.
- `SchedulerQueue::take_batch(now, class) -> BatchSelection<T>` não seleciona duas vezes a mesma `session_id` no mesmo batch.
- `SchedulerConfig::from_node_config(&NodeConfig) -> Self` deriva `max_batch_sequences` de `max_sessions` e limita tokens por `n_batch`.
- Cada `run` cria um scheduler local para o próprio `InferState`; não existe singleton de fila entre peers.

- [ ] **Step 1: Escrever os testes falhos do scheduler**

  Adicionar testes unitários que criem duas filas distintas e verifiquem:

  ```rust
  #[test]
  fn replicas_da_mesma_fatia_nao_compartilham_capacidade_da_fila() {
      let mut a = SchedulerQueue::new(SchedulerConfig::test(1, 1));
      let mut b = SchedulerQueue::new(SchedulerConfig::test(1, 1));
      a.try_push(QueueItem::test("sess-a", WorkClass::Decode, 1)).unwrap();
      b.try_push(QueueItem::test("sess-b", WorkClass::Decode, 1)).unwrap();
      assert_eq!(a.len(), 1);
      assert_eq!(b.len(), 1);
  }

  #[test]
  fn um_microbatch_nao_duplica_a_mesma_sessao() {
      let mut q = SchedulerQueue::new(SchedulerConfig::test(8, 4));
      q.try_push(QueueItem::test("sess-a", WorkClass::Decode, 1)).unwrap();
      q.try_push(QueueItem::test("sess-a", WorkClass::Decode, 1)).unwrap();
      q.try_push(QueueItem::test("sess-b", WorkClass::Decode, 1)).unwrap();
      let batch = q.take_batch(Instant::now(), WorkClass::Decode);
      assert_eq!(batch.items.len(), 2);
      assert_eq!(batch.items[0].session_id, "sess-a");
      assert_eq!(batch.items[1].session_id, "sess-b");
  }

  #[test]
  fn fila_cheia_recusa_antes_de_alocar_trabalho() {
      let mut q = SchedulerQueue::new(SchedulerConfig::test(1, 1));
      q.try_push(QueueItem::test("sess-a", WorkClass::PrefillTokens, 1)).unwrap();
      assert!(matches!(
          q.try_push(QueueItem::test("sess-b", WorkClass::PrefillTokens, 1)),
          Err(QueueFull { .. })
      ));
  }
  ```

- [ ] **Step 2: Rodar os testes para confirmar a falha correta**

  Run: `cargo test -p node --bin pom scheduler::tests -- --nocapture`

  Expected: falha de compilação porque `scheduler.rs` e as interfaces ainda
  não existem. Corrigir somente erros do teste, sem adicionar implementação.

- [ ] **Step 3: Implementar a fila local e os limites**

  Criar `scheduler.rs` com `VecDeque`, `HashSet` de sessões selecionadas no
  lote, janela de espera e contagem de tokens. `take_batch` deve preservar
  ordem round robin: se a primeira sessão já foi selecionada no ciclo, a
  próxima entrada dela fica para o ciclo seguinte. `WorkClass` separa
  `PrefillTokens`, `PrefillHidden`, `DecodeTokens` e `DecodeHidden`, porque
  llama.cpp não aceita tokens e embeddings no mesmo batch.

  Adicionar em `NodeConfig` e `RuntimeParams`, com `#[serde(default)]`, os
  campos `batching_enabled`, `max_queue_requests`, `batch_window_ms`,
  `max_batch_sequences` e `max_batch_tokens`. Defaults: habilitado, 128, 4,
  `max_sessions` e `n_batch`. A validação deve rejeitar zero somente nos
  limites que precisam ser positivos e fazer clamp de `max_batch_sequences`
  ao `max_sessions` efetivo.

  Registrar `#[path = "inference/scheduler.rs"] mod scheduler;` em `main.rs` e
  passar os parâmetros por `NodeConfig::from_params`, `with_params` e o
  construtor de configuração CLI.

- [ ] **Step 4: Rodar os testes para confirmar o verde**

  Run: `cargo test -p node --bin pom scheduler::tests -- --nocapture`

  Expected: todos os testes do scheduler passam e não há sessão duplicada no
  mesmo batch.

- [ ] **Step 5: Atualizar documentação e commitar**

  Explicar em `crates/node/src/inference/AGENTS.md` que cada processo tem uma
  fila própria e que réplicas não compartilham KV. Atualizar
  `docs/subsystems/inferencia.md` com os defaults e `git diff --check`.

  ```bash
  git add crates/node/src/inference/scheduler.rs crates/node/src/inference/runtime.rs \
    crates/node/src/backend/webui.rs crates/node/src/main.rs \
    crates/node/src/inference/infer_server.rs crates/node/src/inference/AGENTS.md \
    docs/subsystems/inferencia.md docs/superpowers/plans/2026-08-20-batching-continuo-inferencia-distribuida.md
  git commit -m "feat: adicionar fila local de inferencia"
  ```

### Task 2: batch multi-sequência no `LlmEngine`

**Files:**
- Modify: `crates/node/src/inference/llm.rs`
- Test: `crates/node/src/inference/llm.rs`

**Interfaces:**
- Produces `BatchEntry`, `BatchLayout` e `BatchDecodeOutput`.
- `LlmEngine::decode_token_batch(&[BatchEntry]) -> Result<BatchDecodeOutput>`.
- `LlmEngine::decode_hidden_batch(&[BatchEntry]) -> Result<BatchDecodeOutput>`.
- `BatchDecodeOutput::logits_for(item_id)` e `residual_for(item_id)` devolvem
  somente a faixa do item solicitado.
- Os métodos antigos `decode_tokens` e `decode_hidden` passam a ser wrappers
  de uma entrada, preservando os chamadores atuais durante a migração.

- [ ] **Step 1: Escrever testes falhos para o layout multi-sequência**

  Testar que entradas com posições e tamanhos diferentes recebem ranges
  contíguos, que tokens e embeddings são classes incompatíveis e que cada
  item com `want_logits` recebe o índice global da última posição.

- [ ] **Step 2: Rodar os testes e confirmar falha**

  Run: `cargo test -p node --bin pom llm::batch_layout -- --nocapture`

- [ ] **Step 3: Implementar o layout e os decoders**

  Construir `llama_batch` com `n_tokens` total, `pos` absoluto por sessão,
  `n_seq_id=1`, `seq_id` físico e flags de logits por posição. Para hidden
  state, preencher `batch.embd` e deixar `batch.token` nulo. Proibir mistura
  entre token e embedding antes de tocar o contexto.

  Fazer a captura de residual devolver o intervalo por item, preservando a
  ordem das linhas do batch. A consulta de logits usará o índice global da
  posição marcada, não o índice do item no vetor.

- [ ] **Step 4: Rodar testes unitários e o teste real ignorado do motor**

  Run: `cargo test -p node --bin pom llm::batch_layout -- --nocapture`

  Se houver modelo disponível, executar também o teste real já existente com
  `--ignored`; sem modelo, registrar a limitação e manter o teste unitário.

- [ ] **Step 5: Atualizar docs e commitar**

  Documentar no `AGENTS.md` de inference que `decode_*_batch` é a única API
  que pode construir lote e que os wrappers de uma entrada mantêm
  compatibilidade. Commit: `feat: suportar lote multi-sequencia no motor`.

### Task 3: mover o estado para um worker scheduler e servir respostas por item

**Files:**
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/scheduler.rs`
- Test: `crates/node/src/inference/infer_server.rs`

**Interfaces:**
- `SchedulerHandle::submit(InferenceCommand) -> Result<oneshot::Receiver<InferenceResult>, SubmitError>`.
- `InferenceCommand::{Prefill, Step, Release, Rollback}` mantém os dados já
  decodificados pelo handler e o `session_id`.
- `InferenceScheduler` possui `InferState`, processa os comandos em um worker
  e publica `SchedulerSnapshot` para `/health`.

- [ ] **Step 1: Escrever teste falho de admissão e isolamento de respostas**

  Criar teste sem modelo que envie duas commands com `session_id` diferentes,
  confirme um único `BatchSelection` e verifique que cada `oneshot` recebe o
  `item_id` correspondente. Adicionar caso de fila cheia retornando
  `SubmitError::QueueFull`.

- [ ] **Step 2: Rodar o teste para confirmar falha**

  Run: `cargo test -p node --bin pom infer_server::scheduler_tests -- --nocapture`

- [ ] **Step 3: Implementar o worker e adaptar handlers**

  Os handlers `handle_prefill` e `handle_step` continuam responsáveis por
  autenticação, decode do frame e validação. Em vez de `spawn_blocking` com
  `state.lock()`, enviam command ao worker. O worker prepara as sessões,
  seleciona apenas uma entrada por sessão, chama o decoder multi-sequência e
  monta um `TimedReply` independente por item.

  `release` e `rollback` entram na mesma fila com prioridade. `/health` usa o
  snapshot atômico, sem bloquear o worker. `/v1/tp/reduce` continua fora desse
  estado, pois o callback TP espera a barreira durante o decode.

  Manter os caminhos de uma entrada para peer antigo e para especulação até a
  lane batch normal estar validada. MTP/lookup não entra no batch normal se o
  item exigir rollback incompatível.

- [ ] **Step 4: Rodar testes de rotas e scheduler**

  Run: `cargo test -p node --bin pom infer_server::tests -- --nocapture`

  Expected: limite de corpo, relay, sessão desconhecida e novos testes de
  respostas isoladas passam.

- [ ] **Step 5: Atualizar health e telemetria**

  Incluir `scheduler_wait_ms`, `scheduler_queue_depth`, `batch_size`,
  `batch_tokens` e `active_sequences` em `node_timings` e `/health`. Commit:
  `feat: executar inferencia pelo scheduler local`.

### Task 4: frame `v3-batch` e fallback por capacidade

**Files:**
- Modify: `crates/node/src/inference/wire.rs`
- Modify: `crates/node/src/inference/relay.rs`
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Test: `crates/node/src/inference/wire.rs`

**Interfaces:**
- `BatchFrame { version, kind, batch_id, items, payload }`.
- `BatchItem { item_id, session_id, shape, dtype, offset, len, meta }`.
- `wire::encode_batch(&BatchFrame) -> Result<Vec<u8>>` e
  `wire::decode_batch(&[u8]) -> Result<BatchFrame>`.
- `CAP_FRAME_BATCH: &str = "inference/frame-v3-batch"`.

- [ ] **Step 1: Escrever testes falhos de round-trip e validação**

  Cobrir round-trip com dois tensores, offset sobreposto, payload truncado,
  dtype inválido, item duplicado e frame acima de 16 MiB.

- [ ] **Step 2: Confirmar falha**

  Run: `cargo test -p node --bin pom wire::batch -- --nocapture`

- [ ] **Step 3: Implementar o frame sem alterar `encode`/`decode` antigos**

  O header mantém offsets e tamanhos em bytes. O decoder valida aritmética
  sem overflow antes de alocar e recusa shape cujo produto não cabe no payload.
  O frame batch é uma versão nova, não uma interpretação heurística de frame
  antigo.

- [ ] **Step 4: Anunciar a capacidade e selecionar fallback**

  Adicionar a capacidade no health/announce somente quando batching estiver
  habilitado. O relay escolhe `encode_batch` apenas se o peer tiver a
  capacidade; caso contrário usa um frame antigo por item.

- [ ] **Step 5: Testar wire e capacidades e commitar**

  Run: `cargo test -p node --bin pom wire::batch -- --nocapture`

  Commit: `feat: adicionar frame de inferencia em lote`.

### Task 5: batching por hop no pipeline e afinidade de réplicas

**Files:**
- Modify: `crates/node/src/inference/prefill.rs`
- Modify: `crates/node/src/inference/relay.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/routing/pipeline.rs`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/inferencia.md`
- Modify: `docs/subsystems/paralelismo.md`
- Test: `crates/node/src/inference/prefill.rs`

**Interfaces:**
- `RouteAffinity { deployment_id, session_id, hop_replicas }`.
- `BatchRoute::group(items) -> Vec<BatchRoute>` agrupa somente modelo,
  deployment, configuração, fase e próximo hop compatíveis.
- `prefill::send_batch` e `prefill::step_batch` retornam um resultado por
  `item_id` e preservam a ordem do streaming dentro de cada sessão.

- [ ] **Step 1: Escrever testes falhos de afinidade e agrupamento**

  Verificar que duas requisições podem ser distribuídas entre duas réplicas
  `[0-7]`, que cada sessão permanece na réplica escolhida e que duas sessões
  na mesma réplica formam um batch. Verificar que uma sessão nunca alterna de
  réplica entre prefill e step.

- [ ] **Step 2: Confirmar falha**

  Run: `cargo test -p node --bin pom prefill::batch_route -- --nocapture`

- [ ] **Step 3: Implementar seleção e afinidade**

  O planejador usa a réplica com menor carga/RTT entre as que cobrem a faixa.
  A escolha é salva por `session_id` e por estágio. O fato de duas réplicas
  terem a mesma faixa não gera uma rota paralela dentro da mesma requisição.
  Se o próximo estágio também tiver réplicas, ele recebe a sessão por uma
  escolha própria e mantém a mesma afinidade nas rodadas seguintes.

- [ ] **Step 4: Implementar envio multi-item por hop**

  Encaminhar frames v3 quando todos os itens do grupo suportarem a capacidade;
  no fallback, dividir em frames antigos sem alterar o resultado. Falha de um
  item deve ser identificada pelo item; falha de transporte que comprometa o
  contexto comum invalida o grupo inteiro.

- [ ] **Step 5: Testar pipeline e documentar**

  Run: `cargo test -p node --bin pom prefill::batch_route -- --nocapture`

  Commit: `feat: agrupar inferencia por hop e fixar replicas por sessao`.

### Task 6: Tensor Parallelism alinhado por batch

**Files:**
- Modify: `crates/node/src/inference/tp.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/llm.rs`
- Modify: `docs/subsystems/paralelismo.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Test: `crates/node/src/inference/tp.rs`

**Interfaces:**
- `TpBatchKey { group_id, batch_id, layer, operation, reduce_index }`.
- `TpDecodeContext { batch_id, item_ids, expected_reduces }`.
- `tp::begin_batch(context)` e `tp::end_batch()` delimitam uma execução TP.
- `ReduceBarrier::contribute` recebe a chave completa e rejeita rank, shape ou
  batch divergente.

- [ ] **Step 1: Escrever testes falhos de alinhamento**

  Criar teste com dois ranks que contribuem para o mesmo lote e teste que
  mistura `batch_id` ou shape, esperando erro e nenhuma soma parcial
  publicada. Adicionar teste de duas réplicas independentes que não usam a
  barreira uma da outra.

- [ ] **Step 2: Confirmar falha**

  Run: `cargo test -p node --bin pom tp::batch -- --nocapture`

- [ ] **Step 3: Implementar chaves e contexto de batch**

  O primário gera `batch_id` monotônico por grupo e envia a mesma lista de
  itens aos workers. O callback usa a chave completa para cada reduce. Se um
  worker falhar ou atrasar além do timeout, o contexto marca o batch inteiro
  como falho e o `decode_*_batch` propaga o erro.

- [ ] **Step 4: Replicar lifecycle por item**

  Release, rollback e cancelamento serão enviados com `session_id` e
  `batch_id` ao grupo. Nenhuma réplica TP pode encerrar a sessão apenas por
  receber EOS localmente; o primário continua sendo a autoridade.

- [ ] **Step 5: Rodar testes e commitar**

  Run: `cargo test -p node --bin pom tp::batch -- --nocapture`

  Commit: `feat: alinhar batches entre ranks de tensor parallelism`.

### Task 7: integração, documentação e validação de entrega

**Files:**
- Modify: `docs/subsystems/inferencia.md`
- Modify: `docs/subsystems/paralelismo.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/AGENTS.md` se o índice precisar listar o plano

- [ ] **Step 1: Adicionar o teste E2E do caminho HTTP**

  Usar `gateway::testkit` para duas requisições concorrentes com prompts
  distintos, verificar batch local, isolamento de streaming, afinidade de
  réplicas e resposta 429 quando a fila exceder o limite.

- [ ] **Step 2: Rodar a validação focada**

  ```bash
  cargo fmt --all
  cargo test -p node --bin pom scheduler::tests wire::batch tp::batch -- --nocapture
  git diff --check
  ```

  O build completo e o teste de todos os crates ficam para quando o usuário
  solicitar a rodada final; durante o pair programming, cada task mantém seu
  ciclo focado.

- [ ] **Step 3: Revisar o contrato de fallback**

  Confirmar que um peer antigo nunca recebe `frame/v3-batch`, que uma rota com
  peer antigo continua em item único e que a sessão não muda de réplica.

- [ ] **Step 4: Commit final da documentação e relatório**

  ```bash
  git add docs/subsystems/inferencia.md docs/subsystems/paralelismo.md \
    crates/node/src/inference/AGENTS.md crates/node/src/backend/AGENTS.md
  git commit -m "docs: documentar batching e afinidade de replicas"
  ```

  Relatar ao usuário quais tarefas estão prontas, quais comandos focados
  passaram e qualquer teste real de GPU que não pôde ser executado sem modelo.
