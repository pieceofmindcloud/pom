# MTP Remote Transport Implementation Plan
> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Transportar `next_draft` no stream remoto existente, manter a
especulação MTP fatiada ativa na cabeça remota e degradar com razões nomeadas
quando a capacidade não existir.

**Architecture:** O produtor do `/v1/step-stream` continuará sendo o dono do
ciclo. Ele copiará `next_draft` para o próximo `Speculate` no metadado local,
enquanto o emissor anexará aos callbacks apenas o estado observável da
especulação. O gateway consumirá esse estado sem reinjetar drafts no modo
stream. O parser aceitará callbacks novos e antigos.

**Tech Stack:** Rust, Tokio, serde_json, testes unitários de `node` e testes
de integração existentes do gateway/inferência.

**Spec:** `docs/superpowers/specs/2026-09-01-mtp-transporte-remoto-design.md`

## Global constraints

- Não tocar em `vendor/llama-cpp-sys-2/llama.cpp`.
- Não adicionar esperas, temporizadores, agrupamento artificial ou travessias
  de rede por token.
- Manter `FIT_MAX_UBATCH=512` e não alterar `kv_seq_budget`.
- Preservar compatibilidade com callbacks e peers sem a nova capacidade.
- Atualizar `AGENTS.md` e READMEs de subsistema das pastas e áreas alteradas.

---

### Task 1: Tornar o contrato do callback explícito

**Files:** `crates/node/src/inference/prefill.rs`,
`crates/node/src/inference/infer_server.rs`

- Escrever testes falhando para callbacks com `speculated`,
  `speculation_active` e `speculation_disabled_reason`.
- Estender `SpeculativeStep` e `RemoteStreamToken` com o estado reportado,
  preservando os defaults para metadados antigos.
- Fazer `speculative_step_from_meta` preferir o estado explícito e manter o
  fallback histórico quando os campos não existirem.
- Fazer o lote remoto propagar os metadados da última rodada junto com os
  tokens e preservar esses campos ao emitir o frame HTTP.
- Rodar os testes focados do parser e do stream.

### Task 2: Fazer a cabeça remota encadear o rascunho sem nova rede

**Files:** `crates/node/src/inference/infer_server.rs`,
`crates/node/src/inference/prefill.rs`

- Escrever um teste de produtor que verifica que um `next_draft` da cauda é
  colocado no `speculate.draft` da chamada seguinte.
- Implementar o estado do produtor: instalar o draft na próxima rodada,
  removê-lo quando a cauda desativar a especulação e transportar a razão
  nomeada no callback.
- Manter `kv_can_shift` como diagnóstico: a cauda pronta pode gerar
  `next_draft` mesmo com esse campo falso, e o rollback efetivo via `kv_remove`
  continua falha fechada.
- Cobrir peer sem a capacidade de MTP fatiado e callbacks sem campos novos.
- Verificar por contador de chamadas que o draft não causa uma requisição
  adicional por token.

### Task 3: Integrar o gateway e a seleção de capacidade

**Files:** `crates/node/src/backend/gateway.rs`,
`crates/node/src/inference/prefill.rs`, testes correspondentes

- Escrever um teste de plano com dois ou mais hops que inicia stream remoto
  com MTP e permanece especulativo depois do primeiro callback.
- Permitir a especulação MTP inicial no stream remoto quando o plano anuncia
  a capacidade fatiada, incluindo a cabeça remota como dona do ciclo.
- No gateway, usar `speculation_active` para estatísticas e decisão de
  `especulando`, sem salvar ou reinjetar `next_draft` no stream remoto.
- Nomear as degradações como cauda sem `nextn`, modelo sem MTP, cauda híbrida
  sem rollback e peer antigo sem a capacidade de wire.
- Garantir que o caminho ronda-a-ronda antigo continue usando o draft que o
  gateway recebe diretamente.

### Task 4: Atualizar documentação e validar a entrega

**Files:** `crates/node/src/backend/AGENTS.md`,
`crates/node/src/inference/AGENTS.md`, `docs/subsystems/inferencia.md`,
`docs/subsystems/api-http.md`

- Atualizar o contrato documentado para dizer que streams remotos encadeiam o
  draft internamente na cabeça e reportam estado explícito.
- Documentar a compatibilidade com pares antigos e as razões de degradação.
- Rodar `cargo fmt --check`, testes focados, `cargo test -p node --bin pom` e
  os testes de workspace proporcionais ao risco.
- Inspecionar `git diff --check`, confirmar que somente este worktree foi
  alterado, fazer commit sem coautor automático e registrar o estado `done`.
