# Cache de Prefixo: Sincronização de Sessão e Posições Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fazer o reuso de prefixo manter a posição real de cada peer sincronizada, cobrindo cadeias de N hops e a entrega SSE de nó único sem enviar chunks impossíveis.

**Architecture:** O gateway continua sendo a autoridade do prompt e do `session_id`, mas cada frame de prefill carregará a posição absoluta que o peer deve ter antes de executar. A resposta executada de cada hop será observável no laboratório e a cabeça só avançará o índice de chunk quando o peer confirmar `next_pos`; divergência real continuará acionando o fallback frio da requisição inteira. O caminho SSE usará o mesmo `generate_stream_with_stops`, portanto a regressão passará pela rota pública em vez de testar apenas a escolha do cache.

**Tech Stack:** Rust, Tokio, Axum, reqwest, wire `frame/v1-batch`, testes de integração internos do crate `node`.

**Spec:** Brief do captain sobre `data/rdma-cache-prefixo-turnos/report.md`, decisão `data/decisions/2026-08-25-cache-prefixo.md` e ocorrência de sincronização de sessão de 2026-08-26; as duas referências de dados não estão presentes nesta cópia, então os contratos atuais em `docs/subsystems/inferencia.md` e `crates/node/src/inference/AGENTS.md` são a autoridade versionada disponível.

## Global Constraints

- Trabalhar somente na branch `fm/rdma-cache-prefixo-corrigir`, sem push, PR, merge, deploy ou reinício de produção.
- Não compilar em `/data/ksia-target`; qualquer validação CUDA usa target separado.
- O cache é funcionalidade do produto para todo modelo e não pode ser desligado por `kv_can_shift`, SWA ou recusa de remoção parcial.
- Não usar retry cego: fallback frio só pode ocorrer após divergência de contrato identificada e deve preservar a resposta correta.
- O prompt fica intacto na sequência retida; geração e cauda volátil permanecem na sequência separada.
- Atualizar `AGENTS.md`/README de subsistema afetado no mesmo commit quando o comportamento documentado mudar; não editar arquivos gerados.

---

### Task 1: Reproduzir as duas divergências no laboratório de fio

**Files:**
- Modify: `crates/node/src/inference/prefix_reuse_lab.rs`
- Test: `crates/node/src/inference/prefix_reuse_lab.rs`

**Interfaces:**
- Consumes: `prefill::run`, `infer_server::{reuse_prefix_for_forward,prompt_progress_for_forward}`, `gateway::generate_stream_with_stops` e os handlers fake já usados pela bancada.
- Produces: fixture de três hops que registra, por request, `session_id`, `chunk_index`, `start_pos`, `next_pos`, `n_past` antes/depois e `cached_tokens`; fixture de nó único que exercita `openai::chat_completions`/SSE com prompt que cruza o limite de chunk.

- [x] **Step 1: Escrever o teste de três hops antes da correção**

  Estender o fake para três `Engine`s e registrar cada ingestão em um vetor compartilhado. Executar quatro turnos no modo direto, com pelo menos dois follow-ups, e afirmar que em cada chunk `start_pos == n_past_before`, `next_pos == n_past_after`, os três peers recebem o mesmo `session_id` e os turnos 2+ têm `cached_tokens > 0`.

- [x] **Step 2: Rodar o teste e confirmar a falha correta**

  Rodar `cargo test -p node prefix_reuse_lab::three_hop -- --nocapture`. O teste deve falhar com a posição enviada positiva e o `n_past` remoto zero, ou com a identificação de sessão divergente, antes de qualquer produção ser alterada.

- [x] **Step 3: Escrever o teste de nó único via SSE antes da correção**

  Usar a rota pública de chat completions do `testkit`, `stream: true`, um prompt com mais de `prefill_chunk_tokens` tokens e uma sessão previamente retida com 42 tokens. Afirmar que o primeiro chunk avançou para a posição real do peer e que a resposta SSE contém token e evento final, sem `server_error` nem chunk 1 em posição 512 quando o peer confirmou 42.

- [x] **Step 4: Rodar a regressão SSE e confirmar a falha correta**

  Rodar `cargo test -p node prefix_reuse_lab::single_node_sse -- --nocapture` e guardar a mensagem exata, o índice do chunk e as posições observadas.

### Task 2: Corrigir a fonte do descompasso de posição e sessão

**Files:**
- Modify: `crates/node/src/inference/prefill.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Test: `crates/node/src/inference/prefix_reuse_lab.rs`

**Interfaces:**
- Consumes: os registros da Task 1 e os campos atuais `start_pos`, `next_pos`, `chunk_index`, `reuse_prefix`, `stable_len` e `session_id`.
- Produces: uma validação de posição baseada no contador remoto confirmado, fallback frio para divergência real e telemetria que diferencia escolha de cache de tokens efetivamente honrados.

- [x] **Step 1: Fixar uma hipótese única com os frames observados**

  Comparar a primeira requisição de cada hop, a resposta que volta ao gateway e o primeiro follow-up. Classificar a causa como (a) `chunk_index * chunk_size` usado como posição absoluta, (b) sessão diferente/ausente no peer, (c) chunk 0 especial ou (d) entrega fora de ordem. Não alterar código enquanto o registro não mostrar qual fronteira quebrou.

- [x] **Step 2: Escrever a asserção mínima do contrato quebrado**

  Fazer a regressão falhar especificamente quando o emissor usar `chunk_index * chunk_size` depois de uma sessão cuja posição real é 42, e quando um hop downstream aceitar `reuse_prefix=true` sem possuir uma sessão retida correspondente.

- [x] **Step 3: Implementar a correção mínima na origem**

  Fazer cada próximo chunk usar exclusivamente `next_pos` retornado pelo frame aceito, nunca o índice lógico como substituto quando a resposta traz a posição real. Fazer o peer rejeitar reuse sem sessão retida como divergência nomeada e devolver metadados de posição/session state para o gateway, que então reinicia a rodada inteira com `reuse_prefix=false` e posição zero.

- [x] **Step 4: Preservar o fallback seguro**

  Validar que a primeira tentativa não mistura hidden state novo com KV ausente no peer. O fallback deve resetar todos os hops com o mesmo `session_id`, começar no chunk 0/posição zero e só publicar o lease se todos confirmarem o rewind.

- [x] **Step 5: Rodar os testes de regressão e focados**

  Rodar `cargo test -p node prefix_reuse_lab -- --nocapture` e `cargo test -p node infer_server:: -- --nocapture`. Esperar que os testes existentes de hub/anel/direto continuem verdes e que os novos testes mostrem as posições remotas executadas.

### Task 3: Validar a rota pública SSE e a telemetria executada

**Files:**
- Modify: `crates/node/src/backend/openai.rs` se a regressão encontrar perda de erro/evento no adaptador
- Modify: `crates/node/src/backend/gateway.rs` se a telemetria não preservar `cached_tokens` honrado
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/inferencia.md`
- Modify: `docs/subsystems/api-http.md` se o contrato SSE/cache mudar
- Test: `crates/node/src/inference/prefix_reuse_lab.rs` e testes OpenAI existentes

**Interfaces:**
- Consumes: `GenSummary.trace.cached_tokens`, `usage.prompt_tokens_details.cached_tokens`, `x_pom.trace.cached_tokens` e os eventos SSE da rota `/v1/chat/completions`.
- Produces: prova repetível de quatro turnos, com TTFT, tokens honrados, chunks e estado remoto por hop.

- [x] **Step 1: Afirmar os dados executados**

  Para quatro turnos, registrar por turno `cached_tokens`, TTFT, chunks ingeridos, `n_past_before/after` em todos os hops, `session_id` e eventuais fallbacks. Exigir frio no turno 1 e `cached_tokens > 0` com TTFT menor nos turnos seguintes quando a sessão não divergir.

- [x] **Step 2: Cobrir ambos os adaptadores de entrada**

  Manter a conversa pelo caminho usado pela tela de chat e adicionar o cliente OpenAI compatível; validar que ambos recebem o mesmo `trace.cached_tokens` executado e que nenhum transforma um erro de contrato em SSE vazio seguido de `[DONE]`.

- [x] **Step 3: Medir as incertezas restantes**

  Consultar `/health` do laboratório/peer real quando disponível para `kv_can_shift`, `sessions_retained`, `prefix_tokens_retained` e `kv_seq_max`; confirmar o teto de prompt e registrar separadamente qualquer limitação por falta de GGUF/cluster real. Verificar o symlink de `vendor/llama-cpp-sys-2` e declarar qualquer mudança de campo/API do fork.

- [x] **Step 4: Atualizar documentação factual**

  Documentar a diferença entre índice de chunk e posição absoluta, a regra de sessão por pipeline e o fallback por divergência. Remover qualquer anotação de defeito corrigido e manter o texto em pt-BR com hífen simples.

### Task 4: Verificação final e commit local

**Files:**
- Modify: arquivos de implementação, testes e documentação listados acima

- [x] **Step 1: Rodar formatação e lint/teste focado**

  Rodar `cargo fmt --all -- --check`, os testes focados e a suíte `cargo test -p node`.

- [ ] **Step 2: Compilar sem tocar no target de implantação**

  A compilação release foi iniciada em `/tmp/rdma-cache-prefixo-target` e
  interrompida por ordem do captain antes de concluir; nenhuma compilação usou
  `/data/ksia-target`.

  Rodar `CARGO_TARGET_DIR=/tmp/rdma-cache-prefixo-target cargo build --release` ou target local equivalente, sem usar `/data/ksia-target`.

- [x] **Step 3: Conferir diff, branch e arquivos gerados**

  Rodar `git diff --check`, `git status --short --branch`, confirmar ausência de `CHANGELOG.md`/gerados alterados e conferir que `main` não foi modificado.

- [ ] **Step 4: Commit e status final**

  Criar commit na branch `fm/rdma-cache-prefixo-corrigir`, sem push/PR, e anexar `done: ready in branch fm/rdma-cache-prefixo-corrigir` ao status somente depois de todas as verificações.
