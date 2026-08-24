# Plano de implementação da interface de modelos remotos

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Objetivo:** Entregar a tela nativa do POM para buscar, inspecionar,
selecionar e baixar modelos remotos através do `ModelProvider`, com jobs
persistentes no backend, progresso, cancelamento, retry, deduplicação e
catálogo local.

**Arquitetura:** Um módulo novo de backend expõe DTOs genéricos de modelos e
downloads e recebe um `DynModelProvider` injetado no `GatewayState`;
`HuggingFaceProvider` continua sendo a implementação que conhece o CLI.
Downloads são `webui::Job` com `CancellationToken` e semáforo de concorrência,
e a UI lê o mesmo endpoint condicional e polling adaptativo já usado pelo POM.

**Stack:** Rust/Axum/Tokio/Serde/Tracing; React 18/TypeScript/React Query/
React Router/Tailwind/Vitest.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-hf-provider/spec-ui.md`; reuso: `docs/rdma-hf-provider-ui-reuso.md`.

## Restrições globais

- A UI nunca executa `hf`, baixa arquivo diretamente ou recebe credenciais do Hugging Face.
- Toda operação usa `ModelProvider`; o provider é substituível por uma implementação HTTP futura.
- Buscas, detalhes e downloads são autenticados por `ConnectionAuth`.
- `repo_id`, `revision` e `filename` passam pelas validações do contrato antes de qualquer processo.
- Downloads são somente armazenamento, verificação e registro; nenhum conteúdo remoto é executado.
- O backend é a fonte da verdade durante refresh e navegação; o browser lê `GET /api/ui/jobs` com ETag e polling adaptativo.
- Use `Panel`, `PageHeader`, `Button`, `Input`, `Select`, `Field`, `Badge`, `Progress`, `Empty`, `Skeleton`, `ErrorState`, `Modal`, `ToastProvider`, tokens existentes e `useI18n`.
- Strings novas existem em `en` e `pt-BR`; cores novas não são hardcoded.
- Testes de Rust usam provider/runner fake; testes de frontend mockam API e não acessam rede real.

### Task 1: estado do job e serviço backend

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/main.rs`
- Create: `crates/node/src/backend/model_provider.rs`
- Test: `crates/node/src/backend/model_provider.rs`

- [ ] Write failing tests for typed search/detail DTO mapping, queue limit, duplicate active download, cancellation isolation, friendly error mapping and no token in serialized job/error.
- [ ] Run `rtk cargo test -p node model_provider::tests -- --nocapture` and confirm RED.
- [ ] Add `ModelService` with injected `DynModelProvider`, two-permit semaphore and per-job cancellation tokens; add generic API request/response types and handlers for search, details/files, storage space, start, cancel and retry.
- [ ] Extend `webui::Job` with optional model-download fields and helpers that preserve monotonic progress; keep existing slice/apply JSON compatible.
- [ ] Add `Arc<ModelService>` to all `GatewayState` constructors, merge the new router and expose catalog invalidation to the service.
- [ ] Start downloads with structured args, backend-selected model directory, disk recheck, provider `DownloadProgress`, manifest result and catalog invalidation. Map `HfError` to safe UI messages without command text, paths, stdout/stderr or tokens.
- [ ] Run focused tests GREEN, then `rtk cargo fmt --all -- --check`.

### Task 2: contrato do frontend e hooks de dados

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/api.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/lib/polling.ts`
- Test: `apps/frontend/src/lib/modelProvider.test.ts`

- [ ] Write failing tests for URL query serialization, request-id/abort ordering, model-download filtering, adaptive polling, status labels and structured start/cancel/retry calls.
- [ ] Run `rtk npm test -- modelProvider.test.ts` and confirm RED.
- [ ] Add generic remote-model/download types and typed API functions; keep the HF CLI absent from all frontend names and payloads.
- [ ] Add React Query hooks for search, details, storage and jobs; debounce stays in the page while request cancellation/request id prevents stale results.
- [ ] Reuse `useJobs` ETag data and expose only `kind === "model_download"` to the screen; invalidate local catalog after completion.
- [ ] Run focused Vitest GREEN and typecheck.

### Task 3: rota nativa e componentes

**Files:**
- Create: `apps/frontend/src/routes/RemoteModels.tsx`
- Create: `apps/frontend/src/components/RemoteModelCard.tsx`
- Create: `apps/frontend/src/components/RemoteModelDetails.tsx`
- Create: `apps/frontend/src/components/DownloadModelDialog.tsx`
- Create: `apps/frontend/src/components/ModelDownloadsPanel.tsx`
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/components/nav.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: the corresponding `*.test.tsx` files

- [ ] Write failing route/component tests for opening the screen, debounce, Enter, stale-result protection, GGUF filter/quantization/size/license/sort, URL persistence, detail loading, dialog confirmation, space refusal, progress, refresh/navigation persistence, multi-download queue, cancel, retry, completion, gated/auth/install errors.
- [ ] Run focused Vitest and confirm RED.
- [ ] Compose the page from existing primitives: header/search/filter controls, compact comparable cards, details modal, confirmation modal, global-job panel and installed badges. Use a drawer-like responsive details layout only if the existing modal pattern requires it.
- [ ] Render status-specific actions and messages without exposing backend internals. Use `useToast`, `ErrorState`, `Skeleton`, `Progress`, locale strings and token tones.
- [ ] Register `/modelos/remotos` and navigation entry, preserving the current models section and profile guard behavior.
- [ ] Run focused Vitest GREEN in both locales and theme-independent token assertions.

### Task 4: documentação, validação e entrega

**Files:**
- Create: `docs/rdma-hf-provider-ui-relatorio.md`
- Modify: `apps/frontend/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/AGENTS.md`

- [ ] Add the required 13-section factual report: architecture, created files, changed files, components, endpoints/internal commands, start flow, progress, cancellation, persistence, local catalog, tests, limitations and next steps.
- [ ] Run `rtk npm run build`, focused and full `rtk npm test`, `rtk cargo fmt --all -- --check`, `rtk cargo test -p node`, `rtk cargo test --workspace`, `rtk cargo build --release` and `rtk git diff --check`; record native `llama-cpp-sys-2` failures exactly if they recur.
- [ ] Review the spec checklist against the implementation and report any uncovered item rather than claiming green.
- [ ] Commit on `fm/rdma-hf-provider`, verify clean status and append the required done status line.
