# Enterprise Client Plugin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Congelar e implementar o primeiro contrato cliente do Enterprise, com licenciamento mockável, estado de entitlement, host de plugin em processo separado e integração segura da UI.

**Architecture:** O `pom` mantém somente o cliente de licença e o estado; o host auxiliar carrega a biblioteca Enterprise por uma ABI C mínima e oferece JSON enquadrado para a integração futura do cliente IPC. O domínio de auditoria permanece fora desta rodada; as telas ficam navegáveis para explicar o produto, mas as rotas recusam dados quando o plugin ainda não está ativo.

**Tech Stack:** Rust, Tokio, Axum, Reqwest, Serde JSON, `libloading`, React, TanStack Query, Vitest.

**Spec:** `/Users/jairpereira/Sites/firstmate/data/rdma-enterprise-licenciamento-design/report.md`

## Global Constraints

- O plugin nunca é carregado diretamente pelo processo `pom`.
- A licença e a chave de licença nunca são propagadas entre peers nem persistidas em texto aberto.
- O endpoint de licenciamento real permanece mockável e não é requisito para testar o cliente.
- `enterprise.active` só é verdadeiro quando grant, manifest, lease e host estiverem prontos.
- Logs de Requisições e Estatísticas permanecem visíveis e clicáveis no menu de todos os perfis, mesmo sem licença.
- Logs de Processo em `/api/ui/cluster-logs` continuam Free; somente Logs de Requisições e Estatísticas são Enterprise.
- A Fase 3 não remove `reqlog.rs` nesta rodada; o gap e a não persistência futura serão documentados para a próxima etapa.

---

### Task 1: Contrato neutro Enterprise v1

**Files:**
- Create: `crates/enterprise-protocol/Cargo.toml`
- Create: `crates/enterprise-protocol/src/lib.rs`
- Modify: `Cargo.toml`
- Test: `crates/enterprise-protocol/src/lib.rs`

**Interfaces:**
- Produces `EnterpriseStatus`, `EnterpriseFeatures`, `LicenseState`, `Fingerprint`, `PluginManifest`, `Grant`, `RequestEnvelope<T>` and `ResponseEnvelope<T>` for the client and host.

- [x] **Step 1: Write failing serialization and fingerprint tests**
- [x] **Step 2: Run `cargo test -p enterprise-protocol` and verify the missing crate/types failure**
- [x] **Step 3: Implement versioned serde DTOs and SHA-256 fingerprint helper**
- [x] **Step 4: Run `cargo test -p enterprise-protocol` and verify all tests pass**
- [x] **Step 5: Commit `feat: add enterprise client protocol contract`**

### Task 2: Cliente mockável de licença e estado local

**Files:**
- Create: `crates/node/src/enterprise.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/Cargo.toml`
- Test: `crates/node/src/enterprise.rs`

**Interfaces:**
- Produces `LicenseService`, `HttpLicenseService`, `MockLicenseService`, `EnterpriseState`, `EnterpriseState::status()` and `EnterpriseState::activate()`.
- Consumes protocol DTOs from `enterprise-protocol`.

- [x] **Step 1: Write failing tests for activation, invalid key, revoked lease, grace and idempotency**
- [x] **Step 2: Run the focused node tests and verify the expected missing implementation failure**
- [x] **Step 3: Implement fingerprint collection with no raw component export, mock service and state transitions**
- [x] **Step 4: Implement HTTPS envelope requests and an in-memory activation snapshot without the license key**
- [x] **Step 5: Run focused tests and then `cargo test -p node enterprise`**
- [x] **Step 6: Commit `feat: add mockable enterprise license client`**

### Task 3: Host auxiliar, ABI C e IPC

**Files:**
- Create: `crates/plugin-host/Cargo.toml`
- Create: `crates/plugin-host/src/main.rs`
- Modify: `Cargo.toml`
- Test: `crates/plugin-host/src/main.rs`

**Interfaces:**
- Produces the `pom-plugin-host` binary, `PluginApiV1`, length-delimited JSON IPC, `ping`, `ready` and `shutdown` handling, manifest checks and a circuit breaker.
- The POM invokes the host through stdin/stdout and never uses `libloading` itself.

- [x] **Step 1: Write failing tests for frame limits, ABI rejection and shutdown**
- [x] **Step 2: Run `cargo test -p pom-plugin-host` and verify failure**
- [x] **Step 3: Implement `repr(C)` ABI types and no-unwind adapter**
- [x] **Step 4: Implement host framing, absolute plugin path validation, dynamic symbol loading and bounded response handling**
- [x] **Step 5: Run host tests and verify an invalid plugin is rejected before loading**
- [x] **Step 6: Commit `feat: isolate enterprise plugin in helper host`**

### Task 4: Backend and frontend entitlement boundary

**Files:**
- Modify: `crates/node/src/backend/gateway/state.rs`
- Modify: `crates/node/src/backend/webui/routes/common.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/lib/access.ts`
- Modify: `apps/frontend/src/components/nav.ts`
- Modify: `apps/frontend/src/App.tsx`
- Test: `crates/node/src/backend/webui/tests/early.rs`
- Test: `apps/frontend/src/App.test.tsx`
- Test: `apps/frontend/src/lib/access.test.ts`
- Test: `apps/frontend/src/components/nav.test.ts`

**Interfaces:**
- Produces `GET /api/ui/enterprise/status`, stable `403 enterprise_required` responses for request logs/statistics, and `useEnterpriseStatus()`.
- Consumes `EnterpriseState` and preserves `/api/ui/cluster-logs` as an operational Free route.

- [x] **Step 1: Write failing backend and UI tests for inactive status, bookmark handling and visible menu items**
- [x] **Step 2: Run focused Rust and Vitest tests and verify failure**
- [x] **Step 3: Add the backend status endpoint and route entitlement guard**
- [x] **Step 4: Add the frontend entitlement query, menu filtering, route callout and purchase link**
- [x] **Step 5: Run `cd apps/frontend && npm run build` and focused Vitest tests**
- [x] **Step 6: Commit `feat: gate enterprise screens by entitlement`**

### Task 5: Documentation, verification and delivery

**Files:**
- Modify: `AGENTS.md`
- Modify: `crates/AGENTS.md`
- Modify: `crates/node/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Create: `docs/subsystems/licenciamento-enterprise.md`

- [x] **Step 1: Document current delivered phases and exact Fase 3/Fase 4 gaps in pt-BR**
- [x] **Step 2: Run `cargo fmt --check`, focused Rust tests, `cargo test --workspace`, frontend build and frontend tests**
- [x] **Step 3: Inspect `git diff --check`, status, and the final commit tree**
- [x] **Step 4: Commit documentation and validation fixes**
- [ ] **Step 5: Fetch/rebase `origin/feat/enterprise` if it exists and push `HEAD:feat/enterprise`**
- [ ] **Step 6: Confirm `git ls-remote origin refs/heads/feat/enterprise` equals the pushed SHA**
