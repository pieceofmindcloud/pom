# MTP nextn crash prevention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Impedir que qualquer contexto MTP seja criado sobre um alvo carregado sem pesos `nextn`, mantendo o recuo para RAM funcional e seguro para uma ou duas sessões.

**Architecture:** O modelo carregado informa explicitamente se os pesos `nextn` foram carregados, e a seleção do modelo que sustenta o `MtpDrafter` valida essa capacidade antes de chamar a API de contexto MTP. O alvo principal continua responsável pelas camadas principais; no recuo para RAM, somente o companheiro CPU-only sustenta o grafo MTP. A cache nunca reaproveita um alvo sem `nextn` para uma tentativa que pede MTP.

**Tech Stack:** Rust, `cargo test`, `llama-cpp-sys-2`, GGUF de teste e testes unitários/E2E existentes em `crates/node/src/inference`.

**Spec:** `docs/subsystems/inferencia.md` e o brief de lançamento `v1 launch-brief` desta tarefa.

## Global Constraints

- Não alterar `llama.cpp`.
- Não alterar `kv_seq_budget`.
- Não desligar MTP por padrão nem desfazer a prioridade das camadas principais.
- Documentar mudanças em `crates/node/src/inference/AGENTS.md` e `docs/subsystems/inferencia.md` no mesmo commit.
- Não afirmar testes não executados; registrar limitações do ambiente.

---

### Task 1: Reproduzir e localizar o caminho inválido - concluída

**Files:**
- Inspect: `crates/node/src/inference/llm.rs`
- Inspect: `crates/node/src/inference/mtp.rs`
- Test: `crates/node/src/inference/llm.rs` e `crates/node/src/inference/mtp.rs`

**Interfaces:**
- Consumes: `MtpFitMode`, `LoadedModel`, `ModelCache`, `LlmEngine::load_with_cache` e `MtpDrafter::attach`.
- Produces: reprodução registrada para `03abd4e`, incluindo `max_sessions=1` e `max_sessions=2`, e a hipótese única que será coberta pelo teste vermelho.

- [x] **Step 1: Preparar o backend local sem alterar o repositório versionado**

  Confirmar o checkout local de `llama.cpp`, `crates/node/pom.json` e um GGUF MTP. Se o checkout estiver ausente, provisionar a cópia ignorada dentro da worktree e usar `vendor/llama-cpp-sys-2/llama.cpp` para os testes.

- [x] **Step 2: Reproduzir o caminho contra `03abd4e`**

  Rodar a prova E2E com o GGUF MTP e o teste de contexto usando `n_ctx=4096`, `mtp=true`, `mtp_cpu_fallback_enabled=true`, e repetir com `max_sessions=1` e `max_sessions=2`. Registrar se o alvo `load_mtp=false` ainda tem `llama_model_n_layer_nextn > 0` enquanto `layer.nextn.eh_proj` está ausente.

- [x] **Step 3: Escrever e executar um teste vermelho**

  Adicionar uma asserção que exija que a seleção do modelo MTP recuse um `LoadedModel` sem pesos `nextn` antes de `llama_init_from_model`, preservando o motivo operacional `o modelo carregado não traz o bloco nextn`. Rodar o teste isolado e confirmar que ele falha na base.

### Task 2: Guardar a capacidade real e corrigir a seleção no recuo - concluída

**Files:**
- Modify: `crates/node/src/inference/llm.rs`
- Modify: `crates/node/src/inference/mtp.rs`

**Interfaces:**
- Consumes: resultado da reprodução da Task 1.
- Produces: `LoadedModel` com capacidade real de MTP, seleção validada do modelo companheiro e nenhum caminho que chame `MtpDrafter::attach` sobre alvo sem `nextn`.

- [x] **Step 1: Registrar no modelo carregado se `nextn` foi carregado**

  Derivar o campo a partir da opção efetiva usada em `llama_model_params`, mantendo `load_mtp=false` explicitamente incapaz de sustentar um contexto MTP.

- [x] **Step 2: Validar a capacidade antes de construir o contexto**

  Fazer `with_model_and_mtp_offload` selecionar o companheiro quando presente e recusar com a razão de modelo sem `nextn` quando o modelo selecionado não tiver a capacidade. `MtpDrafter::attach` deve manter uma guarda defensiva própria antes de `llama_init_from_model`.

- [x] **Step 3: Corrigir todos os ramos de recuo e cache**

  Garantir que o recuo `MtpFitMode::Ram`, o recuo após falha de contexto KV e o caminho `pool` passem o companheiro ao contexto MTP. Invalidar antes da nova tentativa qualquer entrada cacheada cujo alvo não tenha `nextn`, sem tocar no orçamento de sequências.

- [x] **Step 4: Rodar o teste vermelho e confirmar o verde**

  Rodar novamente o teste isolado para `max_sessions=1` e `max_sessions=2`, confirmando que a recusa acontece em Rust e que não há abort do processo.

### Task 3: Cobrir painel, serviço e regressão E2E - concluída

**Files:**
- Modify: `crates/node/src/inference/llm.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/runtime.rs`
- Test: `crates/node/src/inference/llm.rs` e `crates/node/src/inference/infer_server.rs`

**Interfaces:**
- Consumes: razão de indisponibilidade produzida pela Task 2.
- Produces: `/health` publica a razão correta, o nó continua servindo sem MTP e a regressão contra `03abd4e` passa nos dois orçamentos de sessão.

- [x] **Step 1: Testar a mensagem de indisponibilidade**

  Exercitar o motor sem `nextn`, assertar `mtp_ready() == false`, assertar a mensagem `o modelo carregado não traz o bloco nextn` e verificar que a configuração de serviço continua disponível.

- [x] **Step 2: Testar cache e companheiro**

  Assertar que o alvo sem `nextn` não é devolvido por uma consulta MTP e que o alvo é descartado antes de uma nova carga que precisa do companheiro.

- [x] **Step 3: Executar as baterias exigidas**

  Rodar `cargo fmt --check`, `cargo check -p node --bin pom`, os filtros `mtp`, `fit_memory`, `sampler_behaviour`, `device_placement` e `cycle`, além da regressão E2E disponível no ambiente.

- [x] **Step 4: Atualizar documentação factual**

  Atualizar os dois documentos de subsistema para explicar que o `nextn_predict_layers` do metadata não prova que os pesos estão carregados, e que a guarda local ocorre antes da criação do contexto MTP.

### Task 4: Revisar e entregar

**Files:**
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/inferencia.md`
- Review: all files changed by Tasks 1-3

- [x] **Step 1: Revisar o diff e a prioridade de camadas**

  Confirmar que `choose_mtp_fit_mode` e o resultado do fit principal permanecem inalterados em comportamento.

- [x] **Step 2: Fazer a verificação final com evidência fresca**

  Ler os códigos de saída e contagens de cada comando, sem converter testes ignorados ou indisponíveis em verdes.

- [ ] **Step 3: Integrar somente em `origin/feat/implantar`**

  Executar `git fetch origin`, rebasear sobre `origin/feat/implantar`, rever o diff, criar o commit e publicar somente nessa linha.

- [ ] **Step 4: Registrar a entrega**

  Acrescentar no status o SHA e o caminho exato do defeito, da guarda e do teste que falha contra `03abd4e`.

## Test Commands

- `rtk cargo test -p node --bin pom mtp -- --nocapture`
- `rtk cargo test -p node --bin pom fit_memory -- --nocapture`
- `rtk cargo test -p node --bin pom sampler_behaviour -- --nocapture`
- `rtk cargo test -p node --bin pom device_placement -- --nocapture`
- `rtk cargo test -p node --bin pom cycle -- --nocapture`
- `rtk cargo check -p node --bin pom`
- `rtk cargo fmt --check`

Verificação executada antes da integração: o teste E2E de uma sessão passou com
`n_seq_max=2` e o de duas sessões passou com `n_seq_max=4`; ambos recusaram o
MTP antes da criação do contexto. O teste do companheiro CPU-only passou com
`mtp_allocation=ram`. As baterias focadas passaram: `mtp` (31), `fit_memory`
(16), `sampler_behaviour` (12), `device_placement` (5) e `cycle` (11), além de
`cargo check -p node --bin pom` sem avisos. `rustfmt --check` passa nos dois
arquivos Rust tocados; `cargo fmt --check` amplo ainda aponta apenas cinco
arquivos preexistentes fora do escopo.
