# Seletor de motor de inferência - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expor uma escolha persistida de `auto`, llama.cpp ou MoE para implantações MoE, com contrato honesto para whole, shards e Tensor Parallelism.

**Architecture:** O backend deriva `is_moe` do cabeçalho GGUF e devolve capacidades explícitas no plano. A UI só renderiza o controle quando o plano confirma MoE, envia `inference_mode` nos três pedidos e mostra uma recusa de TP MoE em vez de esconder a escolha. O campo é transportado por `RuntimeParams`/`NodeConfig`, compatível com o `InferenceMode` de `feat/ftk`, sem tocar no diagnóstico de divergência.

**Tech Stack:** Rust/Axum/serde, React 18/TypeScript, React Query, Vitest, Cargo.

**Spec:** `docs/superpowers/specs/2026-08-24-inference-engine-selector-design.md`

## Global Constraints

- Local-only: não fazer push, PR, deploy real, SSH ou reinício da ksia.
- Modificar backend somente para servir o frontend: contratos, metadata e propagação da configuração.
- Não alterar `executor.rs`, `InferencePath`, `executed_engine` ou `engine_mismatch` de `feat/ftk@55f8e89`.
- Nunca classificar MoE pelo nome do arquivo; usar metadata GGUF real com `expert_count > 1`.
- O controle fica oculto em modelo denso e visível com limitação explícita em TP MoE.

---

### Task 1: Metadata real e contrato do plano

**Files:**
- Modify: `crates/node/src/models/gguf.rs`
- Modify: `crates/node/src/models/catalog.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `apps/frontend/src/lib/types.ts`
- Test: testes unitários existentes de GGUF, catálogo e deploy

**Interfaces:**
- Produces `DiscoveredModel.is_moe: boolean` e `DeployPlan.is_moe: boolean`.
- Produces `DeployPlan.inference_mode_supported: boolean` e `inference_mode_reason: string | null`.

- [ ] **Step 1: Write the failing tests** para `expert_count > 1`, dense sem experts e os três tipos de plano.
- [ ] **Step 2: Run the focused Rust tests** e confirmar falha por ausência de metadata/campos.
- [ ] **Step 3: Add GGUF expert metadata** e propagar `is_moe` no catálogo e no survey do planejador.
- [ ] **Step 4: Add plan capability fields**; para `tensor_parallel` MoE retornar suporte falso com razão explícita.
- [ ] **Step 5: Run focused tests** e confirmar que whole, shards e TP carregam a mesma informação.

### Task 2: Propagação do modo de inferência

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/deployMemory.ts`
- Test: testes de serde, `NodeConfig::from_params`/`with_params` e deploy payload

**Interfaces:**
- `RuntimeParams.inference_mode: "auto" | "dense" | "moe"`, default `auto`.
- `NodeConfig.inference_mode` guarda o mesmo valor para a configuração retomada;
  `infer_server::run` dá precedência a valores explícitos sobre o ambiente.

- [ ] **Step 1: Write failing round-trip tests** para default, whole, shards e TP.
- [ ] **Step 2: Run the tests** e confirmar que o campo é rejeitado/descartado hoje.
- [ ] **Step 3: Add the serde field and copy it** through plan, launch, apply and persisted deployment records.
- [ ] **Step 4: Validate mode values** at the existing request boundary without changing planner decisions unrelated to the engine.
- [ ] **Step 5: Run focused backend tests** and inspect serialized JSON for all three modes.

### Task 3: Seletor condicional no Deploy Studio

**Files:**
- Modify: `apps/frontend/src/components/RuntimeParamsForm.tsx`
- Modify: `apps/frontend/src/routes/DeployModel.tsx`
- Modify: `apps/frontend/src/lib/queries.ts` only if a plan key needs the mode
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/routes/DeployModel.test.tsx` and `DeployModel.whole.test.tsx`

**Interfaces:**
- Consumes `DeployPlan.is_moe`, `inference_mode_supported` and
  `inference_mode_reason`.
- Produces `RuntimeParams.inference_mode` in `POST /api/ui/deploy/plan` and
  `POST /api/ui/deploy` for shards, whole and tensor parallelism.

- [ ] **Step 1: Write failing UI tests** for dense hidden, MoE visible, mode persistence and TP warning.
- [ ] **Step 2: Run the focused Vitest tests** and confirm the selector is absent or the fields are missing.
- [ ] **Step 3: Render a radio/select control** only when the plan says MoE, keeping `auto` as the initial value.
- [ ] **Step 4: Include the mode in plan invalidation and deploy memory**, and show the backend reason when unsupported.
- [ ] **Step 5: Run focused frontend tests** and inspect all three mode payloads.

### Task 4: Divergence visibility and documentation

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/telemetryAnalysis.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `crates/node/src/models/AGENTS.md`, `crates/node/src/backend/AGENTS.md`, `crates/node/src/inference/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`, `apps/frontend/src/components/AGENTS.md`, `apps/frontend/src/routes/AGENTS.md`, `apps/frontend/src/i18n/AGENTS.md`
- Test: telemetry analysis and chat telemetry tests

**Interfaces:**
- Consumes `selected_path`, `executed_engine`, `engine_mismatch` and
  `mixed_execution` from the existing ftk telemetry contract.
- Produces a visible warning that is independent of the selector's checked state.

- [ ] **Step 1: Write the failing mismatch test** with selected MoE and executed llama.cpp.
- [ ] **Step 2: Run it** and confirm the current renderer does not expose the mismatch.
- [ ] **Step 3: Add the warning** without changing telemetry derivation or the other worker's inference code.
- [ ] **Step 4: Run frontend tests and build**, then update docs with the three-mode contract and TP limitation.
- [ ] **Step 5: Run full Rust/TS suites, format, browser E2E, inspect the diff, and commit locally.**
