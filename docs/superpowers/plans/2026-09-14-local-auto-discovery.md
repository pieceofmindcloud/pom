# Local Auto-Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a secure LAN auto-discovery handshake to first access, expose a disabled-by-default admin toggle, and integrate automatic trust/key delivery into the setup wizard.

**Architecture:** Reuse the existing UDP `Network` socket and `P2PMessage` envelope. A shared protocol state machine sends a public trust offer only after an admin-enabled probe, then encrypts the connection key with an ephemeral X25519 session after an explicit proof. Setup mode owns the same state machine on its configured P2P port; runtime mode delegates messages through `Network` and persists only the disabled/enabled preference.

**Tech Stack:** Rust, Tokio UDP, serde JSON, X25519, ChaCha20-Poly1305, Axum, React, TypeScript, Vitest.

**Spec:** `docs/superpowers/specs/2026-09-14-local-auto-discovery-design.md`

## Global Constraints

- Never put `connection_key` in Probe, Offer, Proof, logs, or status responses.
- The runtime toggle persists as `Config.local_auto_discovery` and defaults to `false`.
- The wizard listener exists only for the setup process and is not enabled by the saved runtime configuration.
- Preserve manual trust import as a fallback when local discovery does not find an administrator.
- Reuse `network::DISCOVERY_PORT` and the configured P2P socket rather than creating a second discovery protocol or daemon.
- Update folder `AGENTS.md` files and `docs/subsystems/rede-p2p.md`, `docs/subsystems/api-http.md`, and `docs/subsystems/web-ui.md` in the same worktree pass.
- Run Rust tests for the new module, frontend tests, `npm run build`, and a release Rust build before reporting validation.

---

### Task 1: Protocol state machine

**Files:**
- Create: `crates/node/src/network/local_discovery.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/Cargo.toml`
- Test: `crates/node/src/network/local_discovery.rs` unit tests

**Interfaces:**
- Produces `LocalAutoDiscoveryMessage`, `LocalDiscoveryRuntime`, `LocalDiscoveryStatus`, and the handshake methods consumed by Network and setup routes.
- `LocalDiscoveryRuntime::set_admin_material(enabled, trust_document, connection_key)` stores only in memory.
- `LocalDiscoveryRuntime::begin_search()` returns a request id and broadcasts a probe.
- `LocalDiscoveryRuntime::handle(message, source)` returns outbound messages and updates the result state.

- [ ] **Step 1: Write failing protocol tests** for a complete handshake, absence of the connection key in probe/offer/proof serialization, rejection without proof, and rejection of replayed/expired sessions.
- [ ] **Step 2: Run `cargo test -p node local_discovery` and confirm the new tests fail because the protocol module does not exist.**
- [ ] **Step 3: Implement wire types, bounded session storage, transcript construction, X25519 shared-secret derivation, ChaCha20-Poly1305 encryption/decryption, and TTL/replay checks.
- [ ] **Step 4: Add the module declaration and direct crypto dependencies, then run `cargo test -p node local_discovery` until all protocol tests pass.**

### Task 2: Runtime Network integration and configuration

**Files:**
- Modify: `crates/node/src/utils/config.rs`
- Modify: `crates/node/src/network/network.rs`
- Modify: `crates/node/src/main.rs`
- Test: `crates/node/src/network/network.rs` existing tests plus new auto-discovery integration tests

**Interfaces:**
- `Network` owns a `LocalDiscoveryRuntime` attached to its existing UDP socket.
- `Network::set_local_auto_discovery(enabled)` updates the runtime and clears secrets when disabled.
- `Network::local_auto_discovery_status()` returns a status without secrets.

- [ ] **Step 1: Add failing tests for `Config.local_auto_discovery` defaulting false and for Network forwarding an admin probe only when enabled.**
- [ ] **Step 2: Run the focused Rust tests and confirm failure.**
- [ ] **Step 3: Add the serde-default config field, initialize the runtime in `Network::from_bound_socket`, route `LocalAutoDiscovery` through `handle_message`, and seed admin material from the loaded trust/config without changing GatewayState fixtures.**
- [ ] **Step 4: Wire startup to the persisted toggle and run focused network/config tests.**

### Task 3: Setup endpoints and setup-mode listener

**Files:**
- Modify: `crates/node/src/backend/webui/setup.rs`
- Modify: `crates/node/src/main.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Test: `crates/node/src/backend/webui/setup.rs`

**Interfaces:**
- Setup status: `GET /api/setup/auto-discovery/status`.
- Admin start: `POST /api/setup/auto-discovery/start` with `{ trust_document, connection_key }`.
- Node search: `POST /api/setup/auto-discovery/search`.
- Setup state owns an `Arc<LocalDiscoveryRuntime>` and starts its receive loop on the configured P2P port.

- [ ] **Step 1: Add failing endpoint tests for status redaction, admin start, and node search result shape.**
- [ ] **Step 2: Run focused setup tests and confirm failure.**
- [ ] **Step 3: Add the shared setup runtime, routes, redacted status/result DTOs, and validation that only an admin can start the waiting mode.**
- [ ] **Step 4: Run focused setup tests and a local UDP handshake test with two runtimes.**

### Task 4: Admin preference API

**Files:**
- Create: `crates/node/src/backend/webui/auto_discovery.rs`
- Modify: `crates/node/src/backend/webui/routes/admin.rs`
- Modify: `crates/node/src/backend/webui/routes/mod.rs`
- Test: `crates/node/src/backend/webui/auto_discovery.rs`

**Interfaces:**
- `GET /api/ui/network/auto-discovery` returns `{ enabled, active, port }`.
- `POST /api/ui/network/auto-discovery` accepts `{ enabled: bool }`, persists the config, and calls Network in memory.

- [ ] **Step 1: Add failing handler tests for disabled default, enabling, disabling, and redacted response.**
- [ ] **Step 2: Run focused handler tests and confirm failure.**
- [ ] **Step 3: Implement handlers using `GatewayState.auth` and `GatewayState.network`, with admin route scope and clearing material when disabled.**
- [ ] **Step 4: Run the handler tests and route-scope assertions.**

### Task 5: Wizard stage and public-key removal

**Files:**
- Modify: `apps/frontend/src/routes/setupWizardTypes.ts`
- Modify: `apps/frontend/src/routes/setupWizardFlow.ts`
- Modify: `apps/frontend/src/routes/Setup.tsx`
- Modify: `apps/frontend/src/routes/Setup.test.tsx`
- Modify: `apps/frontend/src/i18n/messages.csv`
- Test: `apps/frontend/src/routes/setupWizardFlow.test.ts`

**Interfaces:**
- Add `setup-discovery` to `WizardStage` and role-specific flow.
- `AutoDiscoveryScreen` calls setup status/start/search endpoints and receives `trust_document`, `trust_fingerprint`, and connection key only after a completed handshake.
- `AnchorPreview` no longer exposes or renders `public_key`; the trust JSON remains downloadable and collapsed.

- [ ] **Step 1: Add failing flow, screen, and InviteScreen tests for the new stage and no separate public-key card.**
- [ ] **Step 2: Run focused Vitest tests and confirm failure.**
- [ ] **Step 3: Implement stage transitions, polling/action state, automatic state hydration, admin waiting copy, and node search copy while preserving manual trust fallback.**
- [ ] **Step 4: Add translations and run focused frontend tests.**

### Task 6: Preferences toggle UI

**Files:**
- Modify: `apps/frontend/src/routes/Preferences.tsx`
- Modify: `apps/frontend/src/routes/Preferences.test.tsx`
- Modify: `apps/frontend/src/i18n/messages.csv`
- Modify: `apps/frontend/src/routes/AGENTS.md`

**Interfaces:**
- `AutoDiscoveryPanel` reads the status endpoint and posts `{ enabled }` to the admin endpoint.
- The panel is rendered only in the administrative preferences page and starts visually off when the backend is off.

- [ ] **Step 1: Add a failing panel/toggle test.**
- [ ] **Step 2: Run the focused Preferences test and confirm failure.**
- [ ] **Step 3: Implement the panel with the existing switch and toast patterns.**
- [ ] **Step 4: Run `npm exec -- vitest run src/routes/Preferences.test.tsx src/routes/Setup.test.tsx src/routes/setupWizardFlow.test.ts`.**

### Task 7: Documentation and full validation

**Files:**
- Modify: `crates/node/src/network/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `docs/subsystems/rede-p2p.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`

- [ ] **Step 1: Document the shared UDP handshake, disabled-by-default runtime policy, endpoints, and secret boundary using pointers to the implementation.**
- [ ] **Step 2: Run `cargo fmt --check`, focused Rust tests, `cargo test -p node`, frontend tests, and `cd apps/frontend && npm run build`.**
- [ ] **Step 3: Run the release Rust build with the project’s local feature path and inspect the diff for generated files or secret material.**
- [ ] **Step 4: Remove `pom.json` and `.pom-admin-trust.json`, restart the isolated local node through `POM_REPO_MAC="$PWD" ~/scripts/pom-mac.sh run`, verify the setup URL, and append the required `working:` and `paused:` status lines.**
