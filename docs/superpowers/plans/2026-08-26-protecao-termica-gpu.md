# Proteção térmica da GPU Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ler temperatura por GPU usando o inventário existente, suspender e reimplantar o runtime com histerese segura e exibir o estado no Painel.

**Architecture:** `thermal.rs` terá a política persistida e a máquina de histerese pura. `NodeRuntime` guardará a intenção de recuperação e será dirigido por um monitor que usa `sysmon::sample`; a API expõe a política e os cards renderizam o maior valor no Fluxo e todos os valores em Detalhes.

**Tech Stack:** Rust, Tokio, serde, axum, React 18, TypeScript, Vitest, Tailwind tokens existentes.

**Spec:** `docs/superpowers/specs/2026-08-26-protecao-termica-gpu-design.md`

## Global Constraints

- Temperatura ausente é `Option::None`, nunca temperatura alta.
- Suspensão ocorre acima de 85 °C por padrão; retomada exige até 77 °C por 30 s.
- A configuração térmica é persistida em `pom.json` e editável por API e UI.
- `RuntimeStatus::Thermal` e os logs devem expor motivo, temperatura e limites.
- Não editar `CHANGELOG.md` nem artefatos gerados em `apps/frontend/src/docs`.
- Após editar a UI, executar `npm test` e `npm run build` em `apps/frontend`.
- A linha de base Rust é comparada separadamente porque as features CUDA/Vulkan podem falhar antes da compilação Rust neste ambiente.

---

### Task 1: Máquina de histerese e política térmica

**Files:**
- Create: `crates/node/src/utils/thermal.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/utils/config.rs`
- Test: `crates/node/src/utils/thermal.rs`

**Interfaces:**
- Produces `ThermalPolicy`, `ThermalMachine`, `ThermalAction` and default constants for the runtime and API.
- `ThermalMachine::observe(&mut self, readings: &[Option<f64>], now: Instant) -> ThermalAction` suspends only on known high values and resumes only when every reading is known and safe for the full cooldown.

- [x] **Step 1: Write the failing tests** for high temperature, hysteresis, cooldown and unknown readings in `thermal.rs`.
- [x] **Step 2: Run `rtk cargo test -p node thermal` and confirm the missing types or behavior fail.**
- [x] **Step 3: Implement the policy, validation and pure state machine, then add `#[serde(default)]` policy to `Config` with the documented defaults.**
- [x] **Step 4: Add `mod thermal` and update every `Config` literal with `thermal: Default::default()`.**
- [x] **Step 5: Run `rtk cargo test -p node thermal` and `rtk cargo test -p node config` and confirm they pass.**

### Task 2: Runtime protection monitor

**Files:**
- Modify: `crates/node/src/inference/runtime.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/inference/runtime.rs`

**Interfaces:**
- `NodeRuntime::start_thermal_monitor(self: &Arc<Self>, auth: AuthState)` starts one monitor after an initial sample.
- `NodeRuntime::stop` remains manual and clears thermal recovery; `apply` queues an intended configuration while blocked; thermal transitions announce no assignment and resume through the existing `apply` path.

- [x] **Step 1: Add runtime tests that seed a config and assert manual stop clears a pending thermal recovery, while unknown readings do not change normal operation.**
- [x] **Step 2: Run the focused runtime tests and confirm they fail because thermal state and monitor methods do not exist.**
- [x] **Step 3: Add the guarded runtime fields, coordinated thermal stop/resume methods and monitor loop. Use the generation watch so in-flight inference can drain or terminate through its existing clean path.**
- [x] **Step 4: Start the monitor in both normal and gateway-only startup before restoration or CLI apply.**
- [x] **Step 5: Run focused runtime tests and the default `rtk cargo test -p node`.** The focused suite passed; the full suite still has seven unrelated existing failures in deploy, GGUF fixtures, session lab and webui tests.

### Task 3: Thermal API and backend contract

**Files:**
- Modify: `crates/node/src/backend/webui.rs`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Test: `crates/node/src/backend/webui.rs`

**Interfaces:**
- Adds `ThermalPolicyView` to `NodeInfo` and `GET/POST /api/ui/thermal`.
- The POST validates finite positive values and `hysteresis_c < limit_c`, persists the config and returns the effective policy.

- [x] **Step 1: Write backend tests for legacy config defaults and rejection of unsafe policy values.**
- [x] **Step 2: Run the focused backend tests and confirm the route/validation is absent.**
- [x] **Step 3: Implement the route, `NodeInfo.thermal` and TypeScript query/type contract.**
- [x] **Step 4: Run the focused Rust tests and frontend typecheck through `rtk npm run build` from `apps/frontend`.**

### Task 4: Temperature presentation and thermal settings UI

**Files:**
- Create: `apps/frontend/src/lib/thermal.ts`
- Create: `apps/frontend/src/lib/thermal.test.ts`
- Create: `apps/frontend/src/components/GpuTemperature.tsx`
- Modify: `apps/frontend/src/components/ModelNetworkCanvas.tsx`
- Modify: `apps/frontend/src/components/PeerDetailCard.tsx`
- Modify: `apps/frontend/src/routes/NetworkSettings.tsx`
- Modify: `apps/frontend/src/test/fixtures.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Modify: `apps/frontend/src/lib/format.ts`
- Test: `apps/frontend/src/components/PeerDetailCard.test.tsx`

**Interfaces:**
- `temperatureBand` and `highestKnownTemperature` are pure helpers that return `unknown`, `normal`, `attention` or `critical` without coercing null to zero.
- `GpuTemperature` uses existing `Tone` tokens: aqua for normal, amber for attention, rose for critical, neutral for unknown.
- Fluxo renders the highest known GPU temperature; Detalhes renders one row per GPU and an explicit unknown row for no readings.

- [x] **Step 1: Write failing frontend tests for the four bands, null readings and per-GPU/detail rendering.**
- [x] **Step 2: Run the focused frontend tests and confirm the failures are feature-related.**
- [x] **Step 3: Implement pure helpers and the shared temperature component with localized copy.**
- [x] **Step 4: Add Fluxo, Detalhes and Network settings integration, including `thermal` defaults in fixtures and both locales.**
- [x] **Step 5: Run the focused tests, then full `rtk npm test` and `rtk npm run build`.**

### Task 5: Documentation, review and delivery

**Files:**
- Modify: `crates/node/src/utils/AGENTS.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `apps/frontend/AGENTS.md`
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `apps/frontend/src/lib/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `docs/subsystems/implantacao.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/AGENTS.md`

- [x] **Step 1: Document only verified current behavior in Portuguese, including thermal state and configuration endpoint.**
- [ ] **Step 2: Run `rtk cargo check --workspace`, `rtk cargo check -p node --features vulkan,cuda`, `rtk npm test` and `rtk npm run build`; compare Rust errors to the captured baseline.**
- [ ] **Step 3: Inspect `git diff --check`, `git status`, and all requirement gates.**
- [ ] **Step 4: Commit the implementation and documentation, pull with rebase from `origin/feat/implantar`, then push without force.**
