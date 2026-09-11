# Logs para admins secundários Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replicar logs de processo e auditoria de requisições para todos os admins com autoridade efetiva e permitir que admins secundários os consultem localmente.

**Architecture:** O protocolo libp2p de logs de processo será mantido e passará a fazer fan-out para todos os peers anunciados como admin. Um protocolo libp2p request/response separado carregará eventos `Begin` e `Complete` da auditoria. Cada receptor valida a própria autoridade efetiva, grava somente no banco local e nunca retransmite o evento.

**Tech Stack:** Rust, Tokio, libp2p request-response, SQLite via `PomDb`, Axum, React, Vitest.

**Spec:** `docs/superpowers/specs/2026-09-11-logs-replicacao-admin-design.md`

## Global Constraints

- A autoridade administrativa é `AdminTrust::effective_authority`, não `Config.profile` sozinho.
- O anúncio gossip é gatilho/descoberta, nunca fonte de autorização.
- A replicação é best-effort e não pode bloquear captura, inferência ou leitura local.
- O principal continua recebendo e exibindo os logs sem mudança de contrato.
- Não há retenção diferenciada nesta versão; o custo N vezes é documentado.
- Toda documentação editada permanece em pt-BR, usando hífen simples.

---

### Task 1: autoridade e seleção dos destinos

**Files:**
- Modify: `crates/node/src/admin_trust.rs`
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/network/cluster_logs_policy.rs`
- Modify: `crates/node/src/network/discovery.rs`
- Test: `crates/node/src/network/network.rs`

**Interfaces:**
- Produces `AdminTrust::is_active_admin_pubkey`, `Network::admin_peers` e a catraca que compara `authority_pubkey` com a chave primária conhecida.

- [ ] **Step 1: Write the failing tests**

  Add a test proving an announced secondary is not treated as the primary and a test proving `admin_peers` returns every live admin peer rather than one arbitrary peer.

- [ ] **Step 2: Run tests to verify they fail**

  Run `cargo test -p node --bin pom network::network::tests:: --no-default-features`.
  Expected: FAIL because the new authority predicate and multi-peer destination method do not exist.

- [ ] **Step 3: Write the minimal implementation**

  Compare the peer's `authority_pubkey` with `AdminTrust::current_primary().pubkey` for policy probing. Add a peer iterator filtered by live `profile=admin`, with all candidates returned. Keep the effective-authority check on receivers as the security boundary.

- [ ] **Step 4: Run tests to verify they pass**

  Run the same focused Cargo test and confirm all selected tests pass.

- [ ] **Step 5: Commit**

  `git add crates/node/src/admin_trust.rs crates/node/src/network && git commit -m "fix: distinguish primary from secondary admin peers"`

### Task 2: fan-out do log de processo

**Files:**
- Modify: `crates/node/src/network/cluster_logs.rs`
- Modify: `crates/node/src/network/discovery.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/network/cluster_logs.rs`

**Interfaces:**
- Consumes `Network::admin_peers` and the local effective authority computed during startup.
- Produces `ClusterLogs::set_admin_receiver`, fan-out de `LogOutbound` e ingestão aceita por qualquer admin efetivo.

- [ ] **Step 1: Write the failing tests**

  Add tests where a batch is queued for two admin destinations and where a receiver with secondary authority accepts the batch while a non-admin receiver rejects it.

- [ ] **Step 2: Run tests to verify they fail**

  Run `cargo test -p node --bin pom cluster_logs --no-default-features`.
  Expected: FAIL because flush currently chooses one `principal_peer` and remote ingest only accepts the principal flag.

- [ ] **Step 3: Write the minimal implementation**

  Replace the single-destination branch with one `LogOutbound` per live admin peer. Local admin nodes persist their own batch and still replicate it. Set receiver state from `effective_authority(...).is_some()` after the trust chain is loaded. Rename comments and discovery handling from principal-only to admin receiver while preserving the protocol and response behavior.

- [ ] **Step 4: Run tests to verify they pass**

  Run `cargo test -p node --bin pom cluster_logs --no-default-features` and inspect that the existing ordering and backpressure tests remain green.

- [ ] **Step 5: Commit**

  `git add crates/node/src/network/cluster_logs.rs crates/node/src/network/discovery.rs crates/node/src/main.rs && git commit -m "feat: replicate process logs to secondary admins"`

### Task 3: protocolo de auditoria de requisições

**Files:**
- Modify: `crates/node/src/backend/reqlog.rs`
- Modify: `crates/node/src/network/discovery.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/backend/reqlog.rs`

**Interfaces:**
- Produces `AuditEvent`, `AuditOutbound`, `AuditCodec`, `AuditLog::try_ingest_remote` e um sender de replicação best-effort.

- [ ] **Step 1: Write the failing tests**

  Add a test that sends `Begin` and `Complete` through one replication sender to a secondary audit store and lists one completed row. Add a test that delivers `Complete` before `Begin` and still produces the completed row after both arrive.

- [ ] **Step 2: Run tests to verify they fail**

  Run `cargo test -p node --bin pom reqlog --no-default-features`.
  Expected: FAIL because no audit transport or remote ingestion exists.

- [ ] **Step 3: Write the minimal implementation**

  Serialize the existing audit request/completion types, enqueue replication beside the local writer, route each event to all admin peers, and buffer out-of-order remote completions in a bounded in-memory map. Remote writes use the same `AuditDb` writer and never call the replication sender again.

- [ ] **Step 4: Run tests to verify they pass**

  Run `cargo test -p node --bin pom reqlog --no-default-features` and confirm local audit behavior remains unchanged.

- [ ] **Step 5: Commit**

  `git add crates/node/src/backend/reqlog.rs crates/node/src/network/discovery.rs crates/node/src/main.rs && git commit -m "feat: replicate request audits to admin peers"`

### Task 4: consultas, integração e documentação

**Files:**
- Modify: `crates/node/src/backend/webui/catalog.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Modify: `crates/node/src/backend/webui/tests/early.rs`
- Modify: `crates/node/src/backend/gateway/tests.rs`
- Modify: `apps/frontend/src/routes/DeploymentLogs.tsx`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/i18n/messages.csv`
- Modify: `docs/subsystems/rede-p2p.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `crates/node/src/network/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/i18n/AGENTS.md`

**Interfaces:**
- Consumes the local list/stats APIs and both replication receivers.
- Produces secondary-admin HTTP 200 reads, an integration test independent of the principal, and documentation matching the runtime.

- [ ] **Step 1: Write the failing tests**

  Add a secondary-authority route test for `/api/ui/cluster-logs`, and an integration-style test that delivers one process log and one request audit to a secondary database then lists both without constructing a primary destination. Update the frontend test to assert the normal empty-state hint does not mention the primary as a prerequisite.

- [ ] **Step 2: Run tests to verify they fail**

  Run `cargo test -p node --bin pom webui::tests::early gateway::tests:: --no-default-features` and `cd apps/frontend && npm test -- --run src/routes/DeploymentLogs.test.tsx`.
  Expected: FAIL because the cluster-log handler returns 403 for secondary and copy text still names the primary.

- [ ] **Step 3: Write the minimal implementation**

  Remove only the `local_is_primary` guard from the cluster-log read route, keep it on the policy mutation, and revise the UI descriptions to say that data is local to an admin replica and empty only means no data exists. Keep request routes under their existing effective-admin middleware.

- [ ] **Step 4: Run tests to verify they pass**

  Run `cargo test --workspace --no-default-features` and `cd apps/frontend && npm run build && npm test -- --run src/routes/DeploymentLogs.test.tsx src/routes/Logs.test.tsx`.
  Expected: exit 0 with no Rust or frontend failures.

- [ ] **Step 5: Commit**

  `git add crates/node apps/frontend/src docs/subsystems docs/superpowers && git commit -m "feat: open replicated logs to secondary admins"`
