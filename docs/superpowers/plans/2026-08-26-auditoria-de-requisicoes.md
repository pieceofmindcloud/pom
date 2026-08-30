# Auditoria persistente de requisições Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persistir cada requisição de inferência desde o recebimento, completar o mesmo registro ao terminar e oferecer consulta autenticada por identidade da chave, prompt e período.

**Architecture:** `reqlog.rs` será o repositório SQLite e o contrato JSON da auditoria. Um escritor dedicado consome comandos de criação e conclusão por uma fila limitada sem bloquear o gateway; consultas abrem conexões de leitura em `spawn_blocking`. O gateway cria o registro antes de iniciar a geração e a UI exibe os campos completos com filtros e detalhes expansíveis.

**Tech Stack:** Rust 2021, Axum, Tokio, `rusqlite` com SQLite bundled, FTS5, React, TypeScript, TanStack Query, Tailwind e Vitest.

**Spec:** `docs/superpowers/specs/2026-08-26-auditoria-de-requisicoes-design.md`

## Global Constraints

- Nunca gravar ou retornar o segredo bruto, o hint ou qualquer trecho do segredo da API key.
- `GET /api/ui/requests` exige `ConnectionAuth`.
- Escrita de auditoria é best-effort e não pode aguardar I/O no caminho da inferência.
- O prompt persistido é o prompt final exato enviado ao pipeline.
- A criação do SQLite e sua migração são automáticas e idempotentes no arranque.
- Retenção fica fora de escopo e deve permanecer marcada como `needs-decision:`.
- Rust continua sujeito ao erro preexistente de `llama-cpp-sys-2` registrado em `target/audit-rust-baseline.log`.

---

### Task 1: Contrato e repositório SQLite

**Files:**
- Modify: `crates/node/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/node/src/backend/reqlog.rs`
- Test: `crates/node/src/backend/reqlog.rs` unit tests

**Interfaces:**
- Produces `AuditLog::new(path)`, `AuditLog::begin(request)`, `AuditLog::complete(id, completion)`, `AuditLog::list(query)` and `AuditDb` synchronous helpers for in-memory tests.
- Produces `Caller { key_id, key_label, key_node_id }` with operator and anonymous constructors.

- [x] **Step 1: Write the failing tests** for an in-memory database that creates one `in_progress` row, completes it with result and telemetry, returns the same id, distinguishes it from `ok`, and finds rows by key identity and prompt text.
- [x] **Step 2: Run the focused Rust test** with `rtk cargo test -p node reqlog --lib -q`; the existing vendor failure prevents compilation from reaching the module.
- [x] **Step 3: Add `rusqlite`** with the bundled SQLite feature and implement idempotent schema creation, indexes, FTS5 prompt table, insert/update commands, aggregate/list queries, and stale `in_progress` promotion.
- [x] **Step 4: Replace the process-global VecDeque** with the bounded writer and the SQLite-backed response model. Make queue-full and database failures log with `tracing::error!` and return without propagating to inference.
- [x] **Step 5: Run the focused tests again** and verify the existing vendor failure remains the only reported cargo failure.

### Task 2: Identity, gateway integration and protected route

**Files:**
- Modify: `crates/node/src/utils/config.rs`
- Modify: `crates/node/src/backend/admin.rs`
- Modify: `crates/node/src/backend/auth.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/backend/openai.rs`
- Modify: `crates/node/src/backend/messages.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/main.rs`

**Interfaces:**
- `ApiKey` gains a serializable opaque `id` with a default for old configs; key creation assigns a UUID and auth derives a stable fallback only for legacy rows.
- `GenerationInput` carries serialized request details. `spawn_logged_generation_with_stops` creates the initial record synchronously through `try_send`, accumulates emitted text, and completes the same record with terminal fields.
- `list_requests` receives `ConnectionAuth` and `State<GatewayState>`.

- [x] **Step 1: Add tests** for key identity not containing the secret and for a generated request being visible as `in_progress` before completion.
- [x] **Step 2: Run those tests** and observe the existing vendor failure before module compilation.
- [x] **Step 3: Add the audit handle** to every production and test `GatewayState`, initialize it from `config::project_root().join("audit.sqlite3")`, and pass request JSON details from `/api/generate`, OpenAI and Anthropic adapters.
- [x] **Step 4: Complete the same row** in the logged generation task for successful, partial, failed and abandoned outcomes, including output, source, hops, telemetry, tokens, duration, completion time and error.
- [x] **Step 5: Protect the request route** with `ConnectionAuth`, wire query parsing and return the SQLite-backed response.
- [x] **Step 6: Run the focused backend tests** and confirm the vendor failure remains unchanged.

### Task 3: Audit UI and frontend tests

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/routes/LogsByToken.tsx`
- Modify: `apps/frontend/src/routes/LogsLive.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/routes/LogsByToken.test.tsx` or the existing route test location

**Interfaces:**
- `RequestLogEntry` exposes `received_at`, `completed_at`, `api_key_id`, `prompt`, `request_details`, `result`, `telemetry`, `status` and `error` without any secret field.
- `useRequestLog` accepts `api_key`, `prompt`, `from`, `to`, `node_id`, `limit` and `offset`.

- [x] **Step 1: Write failing component tests** for the API-key/prompt/date filters, exact prompt disclosure in an expandable block, and visible incomplete state.
- [x] **Step 2: Run `rtk npm test -- --run ...`** from `apps/frontend` and fix the hoisted mock and native details assertions.
- [x] **Step 3: Implement filters and details** with the current design tokens, newest-first ordering, readable `pre` blocks with wrapping, and localized labels for each terminal state.
- [x] **Step 4: Run the focused frontend tests** and fix all failures.

### Task 4: Documentation and validation

**Files:**
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/AGENTS.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`

- [x] **Step 1: Update documentation** to describe SQLite persistence, two-stage lifecycle, FTS prompt search, key identity, auth and the retention `needs-decision:`.
- [x] **Step 2: Run `rtk cargo fmt --all -- --check`** and confirm formatting.
- [x] **Step 3: Run the normal frontend tests and `rtk npm run build`** from `apps/frontend`.
- [x] **Step 4: Run the same `rtk cargo check` baseline command** and compare the diagnostics to `target/audit-rust-baseline.log`; the same missing `vendor/llama-cpp-sys-2/llama.cpp` error remains, with only RTK's dynamic crate-count summary changing.
- [x] **Step 5: Inspect `rtk git diff --check` and the final diff**, then rebase before commit and push.
