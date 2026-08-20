# Deploy Studio Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar uma tela de implantação em tela inteira que planeja fatias,
modelo inteiro e Tensor Parallelism por recursos, latência, cobertura e carga,
com nome de implantação separado do nome técnico do modelo.

**Architecture:** O backend continua calculando e validando o plano. O planner
ganha estratégia, peers preferidos, capacidade detalhada, cobertura, links e
rotas alternativas; o frontend apenas mantém a intenção do operador e renderiza
um mapa denso com drag and drop. O runtime carrega `model_label` para roteamento
e `deployment_name` para identidade operacional, sem alterar o protocolo do
fork nativo `llama.cpp`.

**Tech Stack:** Rust, Axum, Tokio, serde, tabela de rotas existente, React,
TypeScript, React Query, Vitest, Tailwind tokenizado e documentação Markdown.

**Spec:** `docs/superpowers/specs/2026-08-19-deploy-studio-design.md`

## Global Constraints

- A tela ocupa 100% da largura e altura disponíveis, como Documentação.
- O nome técnico do modelo e o nome operacional da implantação são campos separados.
- `simple` escolhe o menor conjunto de recursos suficiente; `complete` usa os peers selecionados para ocupar mais GPU.
- Um peer sem arquivo continua elegível e recebe transferência durante o deploy.
- O planner nunca cria novos limites GGUF silenciosamente; cortes novos continuam na tela Fatiar.
- Em Fatias, cobertura completa, lacunas e sobreposições ficam visíveis antes do deploy.
- Rotas alternativas são sugestões; não criam execução paralela sobreposta sem seleção explícita.
- Contexto, sampling, batch e concorrência global são propriedades do plano; GPU, KV cache, fit e rank são propriedades do peer.
- Tensor usa o modelo inteiro em todos os ranks e não mostra seleção de fatias.
- Não modificar nenhum arquivo do fork `llama.cpp`.
- Testes novos devem seguir RED, GREEN e verificação completa.
- Atualizar `AGENTS.md` e subsistemas afetados no mesmo commit da implementação.

---

### Task 1: Modelar estratégia, nome de implantação e capacidade no contrato Rust

**Files:**
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/network/network.rs` only if the status payload needs the new operational name
- Test: `crates/node/src/backend/deploy.rs` tests
- Test: `crates/node/src/inference/runtime.rs` tests

**Interfaces:**
- Add `DeployStrategy::{Simple, Complete}` with serde `simple` and `complete`.
- Extend `DeployRequest` with `deployment_name: Option<String>`, `strategy: DeployStrategy` and `preferred_node_ids: Vec<String>`, all serde-defaulted for old clients.
- Extend the matching plan request path so `/api/ui/deploy/plan` and `/api/ui/deploy` use the same normalized values.
- Extend `LaunchRequest`, `ApplyRequest` and `NodeConfig` with `deployment_name: String` while preserving `model_label` as the routing identity.
- Expose the operational name in node status and job payloads without changing `PeerInfo.model`.
- Normalize blank deployment names to the selected model group and reject names longer than the existing UI/API limit with a localized error.

- [ ] **Step 1: Write failing backend tests**

Add tests named `deployment_name_defaults_to_group`,
`deployment_name_is_carried_separately_from_model_label`,
`old_deploy_payload_uses_simple_strategy`, and
`blank_deployment_name_is_rejected_or_normalized`. Assert that `model_label`
stays equal to the model group while `deployment_name` is the custom value in
the launch/apply request and runtime config.

- [ ] **Step 2: Run RED**

Run: `rtk cargo test -p node deploy::tests::deployment_name`

Expected: compile or assertion failures because the new enum and fields do not exist.

- [ ] **Step 3: Implement the compatibility contract**

Add the enum and fields with serde defaults. Normalize the name once at the
deploy boundary, pass it through `deploy::run`, `webui::launch` and `webui::apply`,
and persist it in `NodeConfig` and runtime snapshots. Keep all routing, gossip,
`/v1/models` and request `model` values based on `model_label`.

- [ ] **Step 4: Run GREEN**

Run: `rtk cargo test -p node deploy::tests::deployment_name runtime::tests`

Expected: PASS.

- [ ] **Step 5: Add the frontend contract types**

Add `DeployStrategy`, `deployment_name`, `preferred_node_ids`, capacity fields,
coverage segments, route options and load distribution types to
`apps/frontend/src/lib/types.ts`. Keep all newly added response fields
optional so a rolling cluster with an older peer still renders.

### Task 2: Extend the deploy planner with simple/complete scoring

**Files:**
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `crates/node/src/network/route.rs` if a read-only RTT accessor is needed
- Test: `crates/node/src/backend/deploy.rs` tests
- Test: `crates/node/src/network/route.rs` tests when the accessor changes

**Interfaces:**
- `plan(...)` receives `DeployStrategy` and `preferred_node_ids`.
- `Machine`/`MachineView` expose GPU kind, VRAM total/free, RAM available,
  disk free, load, runtime state and coordinator/adjacent RTT values.
- The serialized plan adds `coverage`, `route_options` and `load_distribution`.
- A route option contains ordered node ids, layer ranges, cumulative RTT,
  transfer bytes and a per-node session budget.

- [ ] **Step 1: Write planner RED tests**

Create deterministic machine fixtures with different GPU/RAM capacities,
latencies and held slices. Add tests named
`simple_uses_the_smallest_complete_machine_set`,
`complete_prefers_gpu_capacity_over_ram`,
`preferred_machine_without_a_slice_remains_eligible`,
`coverage_reports_missing_and_overlapping_layers`,
`route_options_are_sorted_by_score`, and
`load_distribution_does_not_concentrate_sessions_on_one_peer`.

- [ ] **Step 2: Run RED**

Run: `rtk cargo test -p node deploy::tests::simple_uses_the_smallest_complete_machine_set deploy::tests::complete_prefers_gpu_capacity_over_ram`

Expected: FAIL because the planner has no strategy, capacity score or route option output.

- [ ] **Step 3: Add capacity and RTT survey fields**

Extend the existing `survey` parsing of `/api/ui/node` to retain GPU kind,
VRAM total/free, RAM available, utilization, runtime state and current load.
Use measured RTT when available, preserve `None` as "não medida", and never
convert an unavailable value to zero. Keep the existing memory fallback rules.

- [ ] **Step 4: Implement deterministic strategy scoring**

Keep the existing longest-chain behavior as the simple baseline, then filter
and sort candidates by complete coverage, safe weights plus KV estimate, GPU
capacity, RTT, transfer cost and available session capacity. Complete mode can
use every preferred peer; simple mode stops after the smallest sufficient set.
Keep selection requests authoritative and apply the new score only when the
operator did not send explicit rows.

- [ ] **Step 5: Generate coverage, route alternatives and load budgets**

Build coverage segments from the chosen rows, including `covered`, `missing`
and `overlap` states. Enumerate compatible alternative chains from the same
candidate graph, cap the result to a small deterministic number, and calculate
per-peer session budgets from `max_sessions`, free memory and the selected
strategy. Do not launch alternatives automatically.

- [ ] **Step 6: Run GREEN and backend suite**

Run: `rtk cargo test -p node deploy::tests`

Expected: all deploy planner tests pass, including old behavior tests.

### Task 3: Expose route and load suggestions without changing inference semantics

**Files:**
- Modify: `crates/node/src/routing/pipeline.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/backend/deploy.rs`
- Modify: `apps/frontend/src/lib/types.ts`
- Test: `crates/node/src/routing/pipeline.rs` tests
- Test: `crates/node/src/backend/gateway.rs` tests

**Interfaces:**
- Add a read-only route scoring helper that reuses `LoadTracker` data and
  never changes the selected request path by itself.
- Keep the current `PipelinePlan` and `LoadTracker` behavior compatible.
- Expose `route_options` in the deploy preview as explanation data. The live
  request router may choose an equivalent route when a peer is saturated, but
  must not select an overlapping layer path.

- [ ] **Step 1: Write failing route tests**

Add tests named `route_score_prefers_lower_rtt_when_coverage_matches`,
`load_tracker_penalizes_a_busy_equivalent_peer`, and
`route_options_never_overlap_layers`.

- [ ] **Step 2: Run RED**

Run: `rtk cargo test -p node routing::pipeline::tests::route_score`

Expected: FAIL because the route score and alternative enumeration do not exist.

- [ ] **Step 3: Implement the smallest reusable route helpers**

Factor scoring into pure functions receiving peer capacity, edge RTT and load.
Use them from deploy preview and the existing request planner only where the
route is equivalent and complete. Keep the current direct, relay and TP modes
unchanged.

- [ ] **Step 4: Run GREEN**

Run: `rtk cargo test -p node routing::pipeline::tests gateway::tests`

Expected: PASS.

### Task 4: Build reusable Deploy Studio visual components

**Files:**
- Create: `apps/frontend/src/components/deploy/DeployNodeCard.tsx`
- Create: `apps/frontend/src/components/deploy/DeployPeerStrip.tsx`
- Create: `apps/frontend/src/components/deploy/DeployCoverage.tsx`
- Create: `apps/frontend/src/components/deploy/DeployInspector.tsx`
- Create: `apps/frontend/src/components/deploy/DeployNetworkCanvas.tsx`
- Create: `apps/frontend/src/components/deploy/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Test: `apps/frontend/src/components/deploy/*.test.tsx`

**Interfaces:**
- Components receive typed `DeployPlan`, rows, selected node id and callbacks;
  they do not call the API directly.
- `DeployNetworkCanvas` emits `onDropPeer(nodeId)` and `onSelectPeer(nodeId)`.
- `DeployPeerStrip` renders every peer, including peers outside the plan, and
  emits the same drop callback.
- `DeployCoverage` renders one segment per layer and exposes accessible labels
  for covered, missing and overlapped layers.
- `DeployInspector` accepts global params and one optional machine override;
  it emits global and local edits separately.

- [ ] **Step 1: Write frontend RED tests**

Add tests for the coverage states, peer strip rendering all peers, keyboard
selection of a peer, inspector global versus local controls, and a drop event
from the peer strip to the canvas.

- [ ] **Step 2: Run RED**

Run: `cd apps/frontend && rtk npm test -- --run src/components/deploy`

Expected: FAIL because the new components do not exist.

- [ ] **Step 3: Implement the compact visual system**

Use existing `Panel`, `Badge`, `Meter`, `Input`, `Select`, `Progress`,
`MachineIcon` and `trackOf` primitives. Keep the canvas dense and responsive:
node cards show hostname, GPU/RAM, layer range, rank and RTT; links show the
primary path and secondary route options; the bottom strip remains scrollable.
Use native drag events plus keyboard fallback so adding a peer is accessible.

- [ ] **Step 4: Implement the inspector semantics**

Render global fields for context, batch, sampling, max sessions, prefill chunk,
fit policy and speculative switches. When a peer is selected, render GPU layers,
KV placement, local fit, local session cap, layer range and TP rank. Display
`global`, `suggested` and `local override` source badges.

- [ ] **Step 5: Run GREEN**

Run: `cd apps/frontend && rtk npm test -- --run src/components/deploy`

Expected: all new component tests pass.

### Task 5: Replace DeployModel with the full-screen Deploy Studio shell

**Files:**
- Modify: `apps/frontend/src/routes/DeployModel.tsx`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/App.tsx` only if the route shell needs full-bleed handling
- Test: `apps/frontend/src/routes/DeployModel.test.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`

**Interfaces:**
- Keep existing deploy endpoints and `rememberDeploy` behavior.
- Add `strategy`, `deployment_name`, `preferred_node_ids` and explicit selected
  peer state to the plan/deploy payloads.
- Drag/drop updates preferred peers and invalidates the debounced plan query.
- The top button uses the existing deploy confirmation for partial pipelines.

- [ ] **Step 1: Write route RED tests**

Extend or create `DeployModel.test.tsx` with tests for full-screen regions,
separate model and deployment name fields, Simple/Complete controls, coverage
bar, all-peer bottom strip, drag/drop re-plan payload, and Tensor mode hiding
the slice table while showing rank members.

- [ ] **Step 2: Run RED**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/DeployModel.test.tsx`

Expected: FAIL because the current screen still renders stacked panels, lacks
the strategy field, and does not send deployment name or preferred peers.

- [ ] **Step 3: Move route state to the studio model**

Keep catalog selection and existing row editing, add deployment name defaulted
from the chosen model, strategy default `simple`, preferred peer ids, selected
peer id and route option selection. Debounce plan requests as today and preserve
the last valid plan while a new drop is being surveyed.

- [ ] **Step 4: Render the full-screen layout**

Use a full-bleed wrapper like Documentation. Place the mode tabs, model/name
fields, strategy, and deploy action in the top bar. Put coverage above the
canvas for Fatias, the network canvas in the center, the dense inspector on the
right and the complete peer strip at the bottom. Keep error, loading, job and
partial-deploy states visible without collapsing the map.

- [ ] **Step 5: Implement drag/drop and suggestion explanation**

On drop, add the peer to `preferred_node_ids`, select it, and let the backend
return the best placement. Render the reason, copy status, GPU/RAM choice,
RTT and alternative routes in the card and inspector. A peer without a file is
shown as a transfer task, not removed from the plan.

- [ ] **Step 6: Implement Tensor mode in the same shell**

Replace row/coverage content with TP members, primary/rank badges, full-model
presence, rank ordering and primary RTT. Send all selected members through the
existing `TpLaunch` payload. Do not render the old slice-selection card.

- [ ] **Step 7: Run GREEN and UI build**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/DeployModel.test.tsx`

Run: `cd apps/frontend && rtk npm run build`

Expected: route tests and production build pass.

### Task 6: Update documentation and project maps

**Files:**
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Create: `apps/frontend/src/components/deploy/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `AGENTS.md` only if the new component folder changes the repository map

- [ ] **Step 1: Document the current delivered behavior**

Document the strategy contract, deployment name separation, coverage bar,
preferred peers, route alternatives, load distribution, drag/drop behavior and
Tensor full-model semantics. Record that no native `llama.cpp` file is changed.

- [ ] **Step 2: Verify docs embedding**

Run: `cd apps/frontend && rtk npm test -- --run src/routes/Documentation.test.tsx`

Expected: PASS and the new subsystem wording is copied by the existing docs build.

### Task 7: Integrated verification and handoff

**Files:**
- Modify: `docs/superpowers/plans/2026-08-19-deploy-studio.md`

- [ ] **Step 1: Run formatting and focused tests**

Run: `rtk cargo fmt --all -- --check`

Run: `rtk cargo test -p node deploy::tests routing::pipeline::tests`

Run: `cd apps/frontend && rtk npm test -- --run src/components/deploy src/routes/DeployModel.test.tsx`

- [ ] **Step 2: Run full suites**

Run: `rtk cargo test -p node`

Run: `cd apps/frontend && rtk npm test -- --run`

Run: `cd apps/frontend && rtk npm run build`

- [ ] **Step 3: Verify repository constraints**

Run: `rtk git diff --check`

Run: `rtk git -C llama.cpp status --short`

Expected: no whitespace errors and no native fork changes.

- [ ] **Step 4: Review the final diff and commit**

Run: `rtk git status --short --branch && rtk git diff --stat`

Commit the implementation without generated docs and without modifying the
forked `llama.cpp` directory.
