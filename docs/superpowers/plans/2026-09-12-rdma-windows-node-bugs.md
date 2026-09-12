# Bugs de lançamento Windows - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reproduzir e corrigir, de forma independente, os três bugs de primeiro acesso Windows reportados no POM, com regressão e documentação verificáveis.

**Architecture:** O trabalho preserva as fronteiras existentes. O item 1 e o sintoma de travamento do item 3 convergem no polling de 50 ms do socket UDP do Mainline DHT em thread própria; o filtro de aplicação remove o falso alerta `WSAETIMEDOUT` sem alterar o transporte. O setup mantém a validação autoritativa no backend e torna o nome público explícito para qualquer perfil que salve uma rede pública. O item 3 ainda precisa do desenho aprovado para um diretório global separado, pois pkarr é consulta por chave e não enumeração global.

**Tech Stack:** Rust, llama.cpp via `llama-cpp-sys-2`, Tokio/Axum, TypeScript/React, Vitest, testes Rust unitários e de integração.

**Spec:** `docs/superpowers/specs/2026-09-12-rdma-windows-node-bugs-design.md`

## Global Constraints

- Não usar o workflow Windows do GitHub Actions.
- Reproduzir cada relato antes da correção e registrar a causa observada.
- Escrever e observar cada teste de regressão falhar antes do código de produção.
- Não editar `CHANGELOG.md` nem arquivos gerados, incluindo `apps/frontend/src/docs/`.
- Atualizar o `AGENTS.md` da pasta e os READMEs de subsistema afetados no mesmo commit, em pt-BR e sem travessão.
- Nunca usar o caractere de travessão longo; usar hífen simples.
- Entregar na branch `fm/rdma-windows-node-bugs`; não abrir PR.

---

### Task 1: Baseline e reprodução independente do fit CPU-only

**Files:**
- Inspect: `crates/llama-engine/src/llm/config.rs`, `crates/llama-engine/src/llm/engine.rs`, `crates/llama-engine/src/llm/model.rs`, `vendor/llama-cpp-sys-2/wrapper_common.cpp`
- Test: `crates/llama-engine/src/llm/config.rs` or the smallest existing motor test module that can exercise the no-GPU branch
- Update after confirmed cause: `crates/node/AGENTS.md`, `crates/node/src/network/AGENTS.md`, `docs/subsystems/rede-p2p.md`

**Interfaces:**
- Consumes: `EngineConfig`, `fit_plan`, `fit_device_count`, `llama_rs_get_devices` and `llama_rs_fit_params`.
- Produces: a reproducible CPU-only observation and a regression test for the confirmed Windows DHT warning source. No llama-engine change is warranted because the model reached the ready state with `n_gpu_layers=0`.

- [x] **Step 1: Reproduce the user path without GPU acceleration**

  The captain's Windows log with `n_gpu_layers=0` reaches `llama context ready`,
  the inference server, and the QUIC listener. A local CPU smoke run with
  `--memory 4096MB --n-gpu-layers=0 --single-peer --no-discovery` also reaches
  the server-ready boundary. The observed 10060 storm starts before and
  continues after model loading, so it is not a fit or llama.cpp load hang.

- [x] **Step 2: Trace the no-device data flow**

  The repeated `IO error ... os error 10060` is emitted by
  `mainline::rpc::socket::KrpcSocket::recv_from` after its intentional 50 ms
  read timeout. Mainline runs this poll in its dedicated DHT actor thread;
  it is not a relay/QUIC retry and does not hold the Tokio/UI thread.

- [x] **Step 3: Write the failing regression test**

  Added a focused node test asserting that the application filter demotes
  `mainline::rpc::socket` to `error`, preserving real errors while suppressing
  expected timeout warnings. The test was observed failing before the filter.

- [x] **Step 4: Implement the minimal warning fix**

  Added `application_log_filter` in `crates/node/src/main.rs`. It keeps genuine
  Mainline socket errors visible and removes the Windows timeout warning storm
  that made a healthy post-load node appear frozen. No vendor or fit behavior
  changed.

- [x] **Step 5: Verify the fix**

  The focused node regression, the llama-engine library suite, and the local
  CPU smoke path passed/reached readiness. Real Windows reachability and
  external IPv6 routing remain captain validation items.

### Task 2: Reproduce and fix setup profile completion

**Files:**
- Inspect: `apps/frontend/src/routes/Setup.tsx`, `apps/frontend/src/routes/setupWizardFlow.ts`, `apps/frontend/src/routes/Setup.test.tsx`, `crates/node/src/backend/webui/setup.rs`, `crates/node/src/backend/webui/routes/mod.rs`
- Test: `apps/frontend/src/routes/Setup.test.tsx`, `apps/frontend/src/routes/setupWizardFlow.test.ts`, and a Rust setup handler test module if the failure is backend-side
- Update after confirmed cause: `apps/frontend/src/components/setup-wizard/AGENTS.md`, `apps/frontend/AGENTS.md`, `crates/node/src/backend/AGENTS.md`, `docs/subsystems/web-ui.md`, `docs/subsystems/api-http.md`

**Interfaces:**
- Consumes: `setupStepsFor`, `canContinue`, `isReadyToSave`, `save_setup`, and the `/api/setup/save` JSON contract.
- Produces: exact final-step behavior and error text for `admin`, `node`, and `infer`; valid non-admin setup remains usable or its real prerequisite is explained before submit.

- [ ] **Step 1: Reproduce both non-admin flows through the real UI state machine**

  Run the setup route in the local browser/test harness on a fresh setup state.
  Select `node`, complete its final trust/health/review path, and record the
  exact button state, request payload, response status, response body, and
  displayed message. Repeat from a fresh state for `infer`. Run the same
  payloads against `save_setup` with a fresh config/database fixture to isolate
  client gating from backend validation.

- [ ] **Step 2: Compare against the working admin path**

  Diff the three payloads and validation branches. Trace every refusal to the
  exact `return` in `save_setup` or to `isReadyToSave`/`canContinue`. Decide
  whether the requirement is a legitimate prerequisite such as trust,
  connection key, bootstrap, or public network fields. Do not weaken a valid
  security or network invariant merely to make the button clickable.

- [ ] **Step 3: Write the failing regression test**

  Add one test for the confirmed behavior. If the backend rejects a valid
  non-admin request, assert the response accepts it. If a prerequisite is
  legitimate, assert the UI renders the localized prerequisite before the
  final action and that the backend error is mapped to the same explanation.
  Run the focused test and confirm the expected failure before production code.

- [ ] **Step 4: Implement the minimal backend or UI fix**

  Fix the root boundary only. Keep backend authority authoritative. Add
  localized strings for any new operator-facing explanation and ensure the
  final review shows the fields that make the selected profile valid.

- [ ] **Step 5: Verify all three profiles**

  Run the focused frontend tests, the full frontend test command, and the
  relevant Rust setup/backend tests. Exercise the browser flow again and
  capture the final request/response for `admin`, `node`, and `infer`.

### Task 3: Reproduce and design the global public-network directory

**Files:**
- Inspect: `crates/node/src/network/pkarr_node.rs`, `crates/node/src/network/discovery.rs`, `crates/node/src/network/network.rs`, `crates/node/src/backend/webui/network.rs`, `crates/node/src/backend/webui/cluster.rs`, `apps/frontend/src/routes/PublicNetworks.tsx`
- Test: the existing pkarr/network test modules and `apps/frontend/src/routes/PublicNetworks.test.tsx`; add a focused directory contract test only after the architecture is approved
- Update after confirmed cause: `crates/node/src/network/AGENTS.md`, `crates/node/src/backend/AGENTS.md`, `docs/subsystems/rede-p2p.md`, `docs/subsystems/web-ui.md`

**Interfaces:**
- Consumes: pkarr record encode/decode/query functions, public-network API response, and the Public Networks screen data contract.
- Produces: a design-approved global directory contract that does not depend on direct P2P discovery, plus a precise record of the current pkarr limitation.

- [x] **Step 1: Reproduce against a known published record**

  The pkarr record resolved and parsed correctly in the existing Mainline
  testnet integration test. The Public Networks endpoint only resolves
  candidates received through UDP/libp2p or cache; it cannot enumerate an
  arbitrary pkarr record from a fresh admin. The captain's unroutable public
  IPv6 and Tailscale-only address explain why no candidate arrived.

- [x] **Step 2: Separate timing, transport, and parsing hypotheses**

  Parser/query coverage passes with a published record. The 10060 storm is
  expected Mainline polling noise, not missing backoff in application retry.
  Mainline's DHT socket is IPv4-only, and an advertised but unroutable IPv6
  address cannot bootstrap a fresh admin. A global directory is therefore a
  product architecture change, not a parser patch.

- [x] **Step 3: Record the regression boundary**

  Existing tests cover signed-record resolution and preservation of unadmitted
  public candidates. No new directory test is written before its protocol and
  availability model are chosen.

- [ ] **Step 4: Implement the approved directory design**

  Pending architecture decision. The proposed contract is in the design spec:
  publisher-signed records are uploaded to a replicated directory, readers
  query multiple configured replicas, and clients validate signatures and
  cache records locally. Direct P2P remains a transport concern after listing.

- [ ] **Step 5: Verify listing and regression coverage**

  Run the focused Rust/frontend tests and directory contract tests after the
  architecture is approved. Windows firewall and external IPv6 validation
  remain environmental checks, not substitutes for directory tests.

### Task 4: Documentation, branch validation, and delivery

**Files:**
- Modify: every `AGENTS.md` and `docs/subsystems/*.md` named by the confirmed tasks
- Modify: `docs/superpowers/plans/2026-09-12-rdma-windows-node-bugs.md`, `docs/superpowers/specs/2026-09-12-rdma-windows-node-bugs-design.md`

**Interfaces:**
- Consumes: confirmed reproduction evidence, regression output, and the three task statuses.
- Produces: a clean committed branch with an auditable delivery status.

- [ ] **Step 1: Update documentation from verified behavior**

  Remove no existing defect note unless the corresponding bug is actually
  fixed. Add only durable project knowledge, in pt-BR, and never edit generated
  frontend docs or changelogs.

- [ ] **Step 2: Run final verification**

  Re-run the relevant Rust and frontend tests after documentation changes,
  inspect `git diff --check`, inspect `git status`, and verify each task against
  its reproduction and regression evidence. Do not claim completion from a
  partial suite.

- [ ] **Step 3: Rebase and commit**

  Run `rtk git fetch`, then `rtk git pull --rebase origin feat/implantar3` as
  required by the delivery line. Resolve only task-branch conflicts while
  preserving newer upstream commits. Commit the complete verified change on
  `fm/rdma-windows-node-bugs`.

- [ ] **Step 4: Publish only if the delivery line requires it**

  The brief contains both a local-only no-push contract and a later explicit
  `feat/implantar3` push contract. The later, specific delivery line controls:
  after the commit, use `~/.nvm/versions/node/v22.15.0/bin/gh-axi` only for
  GitHub operations if needed, push the branch result to the requested remote
  ref, confirm it with `rtk git ls-remote origin refs/heads/feat/implantar3`,
  and include the confirmed SHA and per-item status in the final status line.
