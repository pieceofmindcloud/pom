# Modularização dos grandes módulos Rust Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dividir `infer_server.rs`, `gateway.rs`, `prefill.rs` e `llm.rs` em diretórios-modulo com arquivos de no máximo 2.000 linhas, preservando lógica, API e comportamento.

**Architecture:** Cada módulo mantém um `mod.rs` pequeno com os imports compartilhados, declarações dos submódulos e reexports da superfície existente. Os submódulos recebem blocos contíguos de tipos, impls, funções e testes já agrupados no arquivo original; somente imports e visibilidade interna serão ajustados para a nova fronteira.

**Tech Stack:** Rust, Cargo, rustfmt, clippy e testes direcionados dos crates `node` e `llama-engine`.

**Spec:** Solicitação `v1 launch-brief` desta tarefa.

## Global Constraints

- Não reescrever corpos de funções, assinaturas ou comportamento.
- Cada arquivo Rust resultante terá no máximo 2.000 linhas, incluindo testes.
- Não tocar no llama.cpp vendorizado.
- `FIT_MAX_UBATCH` permanece exatamente `512`, com seus comentários.
- Não apagar testes nem código privado marcado `dead_code`.
- Validar cada módulo com build, clippy e testes direcionados antes da publicação.

---

### Task 1: `infer_server`

**Files:**
- Create: `crates/node/src/inference/infer_server/mod.rs` e submódulos coerentes.
- Modify: declaração `#[path]` em `crates/node/src/main.rs` somente se necessária.
- Update: documentação de `crates/node/src/inference/AGENTS.md` para o novo layout.

- [ ] Mapear blocos contíguos e mover cada bloco sem alteração de corpo.
- [ ] Preservar reexports e visibilidade da API `infer_server::`.
- [ ] Contar linhas e validar build, clippy e testes direcionados.
- [ ] Commitar e publicar este módulo separadamente.

### Task 2: `gateway`

**Files:**
- Create: `crates/node/src/backend/gateway/mod.rs` e submódulos coerentes.
- Modify: declaração `#[path]` em `crates/node/src/main.rs` somente se necessária.
- Update: documentação de `crates/node/src/backend/AGENTS.md` para o novo layout.

- [ ] Mapear e mover blocos contíguos, incluindo testes, sem reescrita.
- [ ] Preservar a API `gateway::` por reexport.
- [ ] Contar linhas e validar build, clippy e testes direcionados.
- [ ] Commitar e publicar este módulo separadamente.

### Task 3: `prefill`

**Files:**
- Create: `crates/node/src/inference/prefill/mod.rs` e submódulos coerentes.
- Modify: declaração `#[path]` em `crates/node/src/main.rs` somente se necessária.
- Update: documentação de `crates/node/src/inference/AGENTS.md` para o novo layout.

- [ ] Mapear e mover blocos contíguos, incluindo testes, sem reescrita.
- [ ] Preservar a API `prefill::` por reexport.
- [ ] Contar linhas e validar build, clippy e testes direcionados.
- [ ] Commitar e publicar este módulo separadamente.

### Task 4: `llama-engine::llm`

**Files:**
- Create: `crates/llama-engine/src/llm/mod.rs` e submódulos coerentes.
- Modify: declaração do módulo em `crates/llama-engine/src/lib.rs` somente se necessária.
- Update: `crates/llama-engine/AGENTS.md` para apontar para o novo layout.

- [ ] Mapear e mover blocos contíguos, incluindo testes, sem reescrita.
- [ ] Confirmar textualmente `const FIT_MAX_UBATCH: u32 = 512;` e comentários associados.
- [ ] Preservar a API `llm::` por reexport.
- [ ] Contar linhas e validar build, clippy e testes direcionados.
- [ ] Commitar e publicar este módulo separadamente.

### Task 5: Relatório

**Files:**
- Create or replace: `/Users/jairpereira/Sites/firstmate/data/rdma-refactor-rust-grandes/report.md`.

- [ ] Registrar mapa, contagens, OLD, provas e saídas de validação em pt-BR.
- [ ] Registrar commits e confirmação explícita de `FIT_MAX_UBATCH=512`.
- [ ] Acrescentar estado terminal `done`.
