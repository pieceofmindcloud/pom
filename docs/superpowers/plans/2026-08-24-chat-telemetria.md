# Telemetria real do chat Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mostrar no chat a telemetria real da execução, das arestas da pipeline e de cada máquina, incluindo o estado explícito de `POM_PROFILE_SYNC`.

**Architecture:** O SSE continua carregando os tempos da execução. O gateway expõe uma leitura autenticada que agrega os RTTs direcionais já medidos pelos peers, enquanto o frontend consulta o runtime `/health` de cada hop após o fim da resposta e compõe uma seção de ambiente por máquina. O frontend mantém as medidas de execução, topologia e capacidade separadas e trata ausência como ausência.

**Tech Stack:** Rust/Axum, React 18, TypeScript, React Query, Vitest, Testing Library.

**Spec:** `docs/superpowers/specs/2026-08-24-chat-telemetria-design.md`

## Global Constraints

- Não alterar regra de negócio, decisão de rota ou frame binário entre peers.
- RTT de aresta é direcional `origem -> destino`; não reutilizar RTT do gateway.
- Camadas GPU/CPU são efetivas quando `placement` existe; intenção deve ser rotulada como intenção.
- `POM_PROFILE_SYNC` é mostrado por peer e só pelo booleano explícito do backend.
- Textos novos existem em inglês e pt-BR.
- Ausência de dado não vira zero.

---

### Task 1: Contratos de telemetria do runtime e do SSE

**Files:**
- Modify: `crates/node/src/inference/llm.rs`
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/prefill.rs`
- Modify: `apps/frontend/src/lib/chatStream.ts`
- Modify: `apps/frontend/src/lib/types.ts`
- Test: `crates/node/src/inference/infer_server.rs` and `apps/frontend/src/routes/Chat.telemetry.test.tsx`

**Interfaces:**
- Produces `PhaseTrace.round_trip_ms` as the sum of observed call RTTs per phase.
- Produces `/health.profile_sync_enabled` from the same function that gates `llama_synchronize`.
- Consumes the existing `placement`, `context_memory`, `system` and `config` shapes without changing the peer inference frame.

- [x] **Step 1: Write failing tests for the new API fields**

Add a health JSON assertion for `profile_sync_enabled` and extend the frontend phase fixture with a nonzero `round_trip_ms`; assert the telemetry summary renders the aggregate RTT.

- [x] **Step 2: Run the focused tests and verify the expected failure**

Run `rtk cargo test -p node inference::infer_server::tests` and `rtk npm test -- --run apps/frontend/src/routes/Chat.telemetry.test.tsx` from their respective roots. Expected: the new field assertions fail because the fields are not yet emitted or rendered.

- [x] **Step 3: Implement the smallest backend and frontend contract change**

Expose a public profile-sync predicate in `llm.rs`, add `profile_sync_enabled` to `/health`, accumulate `round_trip_ms` in `PhaseTrace`, and type the optional frontend fields. Do not add the profile flag to `node_timings` or any binary frame metadata.

- [x] **Step 4: Run the focused tests again**

Run the same Rust and frontend commands. Expected: PASS, with older frontend fixtures still accepted when optional runtime fields are absent.

- [x] **Step 5: Commit**

Run `rtk git add crates/node/src/inference/llm.rs crates/node/src/inference/infer_server.rs crates/node/src/inference/prefill.rs apps/frontend/src/lib/chatStream.ts apps/frontend/src/lib/types.ts apps/frontend/src/routes/Chat.telemetry.test.tsx && rtk git commit -m "feat: expose chat runtime telemetry fields"`.

### Task 2: Authenticated directional RTT snapshot for chat

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/lib/types.ts`
- Test: `crates/node/src/backend/webui.rs`

**Interfaces:**
- Consumes `POST /api/ui/network/rtt` responses from each requested peer through the existing `ConnectionAuth` credential.
- Produces `POST /api/ui/chat/telemetry` with `{ node_ids: string[] }` and `{ measurements: [{ from_node_id, to_node_id, rtt_ms, transport }] }`.

- [x] **Step 1: Write the failing backend route test**

Use `gateway::testkit` to add a route-shape test for an empty request and a helper test that accepts only reports whose `source_node_id` matches the requested peer identity. Assert that missing directional measurements are omitted rather than represented as zero.

- [x] **Step 2: Run the focused backend test and verify it fails**

Run `rtk cargo test -p node backend::webui::tests::chat_telemetry` from the repository root. Expected: FAIL because the route/helper does not exist.

- [x] **Step 3: Implement the read-only aggregation route**

Register the authenticated POST route, read the local table with the existing route fallback, query each remote peer's authenticated `/api/ui/network/rtt`, verify the report source id, filter to requested node ids, and return only real directional measurements. Keep query failures isolated per peer.

- [x] **Step 4: Add the typed frontend query**

Add `ChatTelemetryNetwork`, a query key, and `useChatTelemetryNetwork(nodeIds, enabled)` using `api.post`. The query must be disabled with an empty list, must not synthesize missing pairs, and must retain an error state separate from an empty successful response.

- [x] **Step 5: Run the focused tests**

Run `rtk cargo test -p node backend::webui::tests::chat_telemetry` and the frontend telemetry test file. Expected: PASS.

- [x] **Step 6: Commit**

Run `rtk git add crates/node/src/backend/webui.rs apps/frontend/src/lib/api.ts apps/frontend/src/lib/queries.ts apps/frontend/src/lib/types.ts && rtk git commit -m "feat: expose directional chat network telemetry"`.

### Task 3: Compose and render real chat telemetry

**Files:**
- Modify: `apps/frontend/src/lib/telemetryAnalysis.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/routes/Chat.telemetry.test.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Interfaces:**
- Consumes `RequestTrace`, the end-of-execution cluster snapshot, `ChatTelemetryNetwork` and per-hop `PeerRuntime` results.
- Produces a closed-by-default, responsive telemetry section with execution totals, directional edge rows, one-machine empty state, per-peer capacity/configuration, effective GPU/CPU/mixed placement and per-peer profile-sync status.

- [x] **Step 1: Write failing component tests for each requested item**

Add fixtures for two peers, a directional `a -> b` RTT, system/config data, a placement with GPU, CPU and mixed layers, and distinct profile-sync booleans. Assert labels distinguish edge RTT from the machine route RTT, assert the one-peer state has no fake edge, and assert missing runtime/network data is written as unavailable.

- [x] **Step 2: Run the focused frontend tests and verify failure**

Run `rtk npm test -- --run src/routes/Chat.telemetry.test.tsx` from `apps/frontend`. Expected: FAIL on the new labels and values.

- [x] **Step 3: Add pure aggregation helpers**

Implement typed helpers in `telemetryAnalysis.ts` for total call RTT, total observed network time, consecutive unique pipeline edges, and buffer-byte layer classification. Keep the helpers defensive for legacy traces and null placement.

- [x] **Step 4: Wire post-execution snapshots into Chat**

After a response with a trace finishes, capture the current `ClusterView`, query directional RTTs for its hop ids, and query `/api/ui/peer/:node_id/runtime` for each hop. Pass only the matching snapshot to that message; historical messages without a captured snapshot must say so.

- [x] **Step 5: Render the sections with explicit units and absence states**

Keep the current phase table and analysis card. Add a summary row for aggregate call RTT and observed network time, an edge table with `from -> to` labels and transport, and machine cards with RAM/CPU/GPU, runtime configuration, effective placement and profile-sync state. Use `overflow-x-auto`, `min-w-0` and responsive wrapping so one and many peers remain readable.

- [x] **Step 6: Add both locale dictionaries**

Add all new headings, labels, explanations and absence states to the English block and the pt-BR block in `locales.ts`, preserving the exact key count invariant.

- [x] **Step 7: Run the focused frontend tests and refactor while green**

Run `rtk npm test -- --run src/routes/Chat.telemetry.test.tsx`, inspect all failures, then run the file again after cleanup. Expected: PASS with no console errors.

- [x] **Step 8: Commit**

Run `rtk git add apps/frontend/src/lib/telemetryAnalysis.ts apps/frontend/src/routes/Chat.tsx apps/frontend/src/routes/Chat.telemetry.test.tsx apps/frontend/src/i18n/locales.ts && rtk git commit -m "feat: show real chat execution telemetry"`.

### Task 4: Documentation and full verification

**Files:**
- Modify: `apps/frontend/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/i18n/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/inferencia.md`
- Modify: `docs/subsystems/web-ui.md`

- [x] **Step 1: Document the current contracts**

Add concise pointers to the new chat telemetry route, the explicit health profile flag, the execution/topology distinction and the fact that placement is counted from actual buffers. Do not edit generated frontend docs or changelogs.

- [x] **Step 2: Run frontend tests and build**

Run `rtk npm test` and `rtk npm run build` from `apps/frontend`. Expected: exit 0 and no TypeScript errors.

- [x] **Step 3: Run backend tests**

Run `rtk cargo test -p node` and, if available without unrelated environment blockers, `rtk cargo test --workspace`. Record any unrelated failure instead of claiming green.

- [x] **Step 4: Inspect the changed UI in the browser**

Reload `http://localhost:5299/chat`, run a complete test execution if the local cluster permits it, open telemetry, and check both one-peer and multi-peer fixtures through the regression tests. Fix visible overflow or unlabeled units found in the changed surface.

- [x] **Step 5: Review diff and commit documentation**

Run `rtk git diff --check`, `rtk git status --short`, and `rtk git diff --stat`; then commit the documentation and any final test-only corrections with `rtk git commit -m "docs: document chat telemetry sources"`.

- [x] **Step 6: Verify branch and final state**

Run `rtk git status --short --branch`, `rtk git log -1 --oneline`, and the full relevant verification commands again after the last commit. Confirm the branch is `fm/rdma-chat-telemetria`, append `done: ready in branch fm/rdma-chat-telemetria` to the status file, and do not push or open a PR.
