# Goose in POM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** POM's Chat talks to ACP agents announced by plugins, Goose is the first (served by a new on-demand `pom-plugin-goose`), Settings lists and closes background plugins, and Harness offers a Goose terminal tab only while the Goose plugin is installed.

**Architecture:** Three additive contracts in the node (plugin discovery on `host_api` plus `plugin.*` events; `host.services` kind `acp_agent` persisted for stopped plugins; `memory_bytes` on the plugin UI list), two new frontend surfaces (ACP mode in `Chat.tsx`, `/config/background-services`), one new plugin repository and a Harness change. The browser speaks ACP over the existing plugin proxy WebSocket; the plugin's bridge injects Goose's secret.

**Tech Stack:** Rust (axum, tokio, serde) in `crates/node`; React + TypeScript + vitest in `apps/frontend`; `@aaif/goose-acp-client` (npm, pinned); plugin template from `.claude/skills/creating-pom-plugins`; Node.js ESM in `pom-plugin-harness/runtime`.

**Spec:** `docs/superpowers/specs/2026-11-21-goose-acp-chat-design.md`

## Global Constraints

- Every contract change is additive (root `AGENTS.md`, compatibility policy): no field, status or route changes meaning.
- Secrets (`gateway.api_key`, `host_api.token`, upstream tokens, `GOOSE_SERVER__SECRET_KEY`) never in replies, `host.services`, logs or screens.
- User sessions (`SessionPrincipal::User`) never reach agents: the plugin proxy stays outside `user_may_call`; `/api/ui/chat/agents` returns `[]` to them.
- Plugin `query` answers within 3 s; downloads and process starts run on plugin-owned threads.
- Downloads pinned by version and SHA-256 (the digest GitHub publishes per asset); nothing vendored.
- Goose pin: the latest stable `aaif-goose/goose` release at least 14 days old on the day Task 7 starts; `@aaif/goose-acp-client` pinned to the same version.
- Route tests use `gateway::testkit`, never real UDP.
- Docs in English, no em dash, no absolute local paths, no personal data. Each task updates the folder `AGENTS.md` and the subsystem docs it touches (`docs/subsystems/plugins-ui.md`, `web-ui.md`, `http-api.md`, the skill's `reference.md`) in the same commit.
- `apps/frontend`: `npm run build` after UI edits; i18n keys identical across `en` and `pt-BR`.
- POM work on branch `develop`-based feature branch `feat/goose-acp`; never push to `main`.

## Review Focus

1. A plugin uninstalled while Chat has its agent selected: the picker must drop it and the conversation show an "agent no longer available" error, not hang. (Task 3 test `agents_are_forgotten_when_the_plugin_is_removed`, Task 5 test `drops a selected agent that disappears`.)
2. `goose serve` crashing mid-conversation: the WebSocket closes; Chat shows the error with Retry, and the plugin reports `error` then restarts. (Task 5 test `socket close mid-turn shows retry`, Task 7 test `supervisor_restarts_after_child_exit`.)
3. Install order Goose-then-Harness and Harness-then-Goose both yield the tab; removal leaves an open tab running. (Task 8 tests.)
4. The node key rotated (`host.configure` again): Goose must pick up the new key without the operator doing anything. (Task 7 test `reconfigure_rewrites_provider_and_restarts`.)
5. A malformed `acp_agent` entry (path with `..`, missing `acp`, absurd length) never reaches the Chat. (Task 2 tests.)

---

### Task 1: Plugin discovery on `host_api` and `plugin.*` events (POM)

**Files:**
- Modify: `crates/node/src/plugin_host_api.rs` (new handler `plugins`)
- Modify: `crates/node/src/backend/webui/routes/common.rs` (route `/api/plugin-host/plugins`)
- Modify: `crates/node/src/plugins.rs` (`PluginManager::installed_summaries`, event emission at install/update/deactivate)
- Docs: `docs/subsystems/plugins-ui.md` (host API table, events), `.claude/skills/creating-pom-plugins/reference.md` sections 5 and 6, `crates/node/src/AGENTS.md`

**Interfaces:**
- Produces: `GET /api/plugin-host/plugins` -> `{"plugins":[{"code":String,"version":String|null,"lifecycle":"resident"|"on_demand","running":bool}]}`, header `x-pom-plugin-host-token`.
- Produces: `PluginManager::installed_summaries(&self) -> Vec<PluginSummary>` with `#[derive(Serialize)] struct PluginSummary { code: String, version: Option<String>, lifecycle: Lifecycle, running: bool }`.
- Produces: events via `PluginManager::publish_event`: `plugin.installed` / `plugin.updated` with `{"code","version"}`, `plugin.removed` with `{"code"}`.

- [ ] **Step 1: Write the failing tests** in `plugin_host_api.rs` `mod tests`:

```rust
#[tokio::test]
async fn plugins_lists_installed_plugins_without_secrets() {
    // testkit state with two installed fake plugins "goose" (on_demand, stopped) and "harness" (resident, running)
    // GET /api/plugin-host/plugins with the host token
    // assert body == {"plugins":[{"code":"goose","version":"0.1.0","lifecycle":"on_demand","running":false},
    //                            {"code":"harness","version":"0.2.0","lifecycle":"resident","running":true}]}
    // assert the serialized body contains none of: "license", "path", "token", "preferences"
}
#[tokio::test]
async fn plugins_refuses_without_host_token() { /* 401 like `peers` */ }
```

and in `plugins.rs` tests:

```rust
#[tokio::test]
async fn install_update_and_deactivate_publish_plugin_events() {
    // install fake "goose" 0.1.0 -> last event type "plugin.installed", payload {"code":"goose","version":"0.1.0"}
    // install 0.2.0 over it      -> "plugin.updated", {"code":"goose","version":"0.2.0"}
    // deactivate                 -> "plugin.removed", {"code":"goose"}
}
```

- [ ] **Step 2: Run** `cargo test -p node plugin_host_api::tests::plugins_ plugins::tests::install_update_and_deactivate` - expect FAIL (route/function missing).
- [ ] **Step 3: Implement** `installed_summaries`, the `plugins` handler (same token check as `peers`), the route, and `publish_event` calls at the end of a successful `install_public_github_release`, `install_development_artifact` (installed vs updated decided by whether a version was recorded before) and `deactivate`.
- [ ] **Step 4: Run** the same tests, then `cargo test -p node plugin` - expect PASS.
- [ ] **Step 5: Docs, `cargo fmt --check`, `cargo clippy -p node -- -D warnings`, commit** `plugins: discovery on host_api and plugin lifecycle events`.

### Task 2: `acp_agent` service kind, persisted for stopped plugins (POM)

**Files:**
- Modify: `crates/node/src/plugin_services.rs` (`PluginService.acp: Option<AcpAgent>`, `STATUSES` unchanged, `"stopped"` only synthesised)
- Modify: `crates/node/src/plugins.rs` (persist under setting `plugin_acp_agents:<code>`, delete on deactivate)
- Docs: `plugins-ui.md` (`host.services`), skill `reference.md`

**Interfaces:**
- Produces: `#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)] struct AcpAgent { path: String, protocol_version: u32 }`.
- Produces: `PluginManager::acp_agents(&self) -> Vec<(String /*plugin_code*/, PluginService)>` - live entries of running plugins, saved entries with `status = "stopped"` for stopped on-demand plugins.

- [ ] **Step 1: Write the failing tests** in `plugin_services.rs`:

```rust
#[test]
fn acp_agent_entries_keep_a_valid_path() {
    let answer = json!({"services":[{"id":"goose","kind":"acp_agent","name":"Goose","status":"ready",
        "version":"1.60.2","acp":{"path":"/acp","protocol_version":1}}]});
    let s = parse_services("goose", &answer);
    assert_eq!(s[0].acp, Some(AcpAgent{ path: "/acp".into(), protocol_version: 1 }));
}
#[test]
fn acp_agent_entries_with_a_bad_path_are_dropped() {
    for path in ["acp", "/a/../b", &format!("/{}", "x".repeat(128)), ""] { /* parse -> empty */ }
    // also: kind acp_agent without "acp" -> dropped; protocol_version 0 -> dropped
}
#[test]
fn acp_is_ignored_on_other_kinds() { /* kind "rag" with acp -> service kept, acp None */ }
```

and in `plugins.rs`:

```rust
#[tokio::test]
async fn stopped_on_demand_plugin_keeps_its_agents_as_stopped() { /* report, close, acp_agents() -> status "stopped" */ }
#[tokio::test]
async fn agents_are_forgotten_when_the_plugin_is_removed() { /* deactivate -> acp_agents() empty, setting gone */ }
```

- [ ] **Step 2: Run** `cargo test -p node acp_` - FAIL.
- [ ] **Step 3: Implement** parsing (path starts with `/`, no `..` segment, 2-128 bytes, `protocol_version >= 1`), persistence on each `host.services` poll that changed the agent list, deletion in `deactivate` beside the saved UI manifest.
- [ ] **Step 4: Run** `cargo test -p node acp_ plugin_services` - PASS.
- [ ] **Step 5: Docs, fmt, clippy, commit** `plugins: acp_agent service kind, kept while the plugin is stopped`.

### Task 3: `GET /api/ui/chat/agents` and the operator-only guard (POM)

**Files:**
- Modify: `crates/node/src/plugin_ui.rs` (handler `chat_agents`)
- Modify: `crates/node/src/backend/webui/routes/common.rs` (route)
- Test: `crates/node/src/backend/user_session.rs` tests
- Docs: `http-api.md`, `plugins-ui.md`

**Interfaces:**
- Consumes: `PluginManager::acp_agents()` (Task 2).
- Produces: `GET /api/ui/chat/agents` -> `{"agents":[{"plugin_code","id","name","version","status","proxy_path"}]}` where `proxy_path = "/api/ui/plugins/<code>/proxy" + acp.path`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test]
async fn chat_agents_lists_agents_for_the_operator() { /* proxy_path == "/api/ui/plugins/goose/proxy/acp" */ }
#[tokio::test]
async fn chat_agents_is_empty_for_a_user_session() { /* {"agents":[]} */ }
#[test]
fn user_sessions_never_reach_a_plugin_proxy() {
    assert!(!user_may_call(&Method::GET, "/api/ui/plugins/goose/proxy/acp", false));
    assert!(!user_may_call(&Method::GET, "/api/ui/plugins/goose/proxy/acp", true));
    assert!(!user_may_call(&Method::POST, "/api/ui/plugins/goose/open", true));
}
```

- [ ] **Step 2: Run** `cargo test -p node chat_agents user_sessions_never` - FAIL on the first two.
- [ ] **Step 3: Implement** the handler (empty list when the session principal is a user).
- [ ] **Step 4: Run** - PASS.
- [ ] **Step 5: Docs, fmt, clippy, commit** `chat: list the ACP agents of local plugins for the operator`.

### Task 4: `memory_bytes` and the Background services screen (POM)

**Files:**
- Modify: `crates/node/src/plugins.rs` (resident memory of a running host's process tree), `crates/node/src/plugin_ui.rs` (`list_plugin_ui` adds `memory_bytes`)
- Create: `apps/frontend/src/routes/BackgroundServices.tsx`, `BackgroundServices.test.tsx`
- Modify: `apps/frontend/src/App.tsx` (route `/config/background-services`), `apps/frontend/src/components/nav.ts` (Settings entry), i18n catalogs `en`/`pt-BR`
- Create: `docs/guides/screens/background-services.md`, `.pt-BR.md`
- Docs: `web-ui.md`, `plugins-ui.md`

**Interfaces:**
- Produces: `GET /api/ui/plugins/ui` items gain optional `memory_bytes: u64` (only `running` plugins, omitted when unmeasurable).
- Consumes: existing `POST /api/ui/plugins/<code>/open` and `/close`.

- [ ] **Step 1: Failing tests.** Rust: `plugin_memory_is_reported_only_for_running_hosts` (Linux: sum of `VmRSS` over the host pid and its descendants from `/proc`; stopped -> field absent). Vitest in `BackgroundServices.test.tsx`: `lists running on-demand plugins with memory`, `close asks for confirmation then calls /close and moves the row to stopped`, `stopped rows offer Open`, `resident plugins are not listed`, `nav entry hidden for user sessions`.
- [ ] **Step 2: Run** `cargo test -p node plugin_memory` and `cd apps/frontend && npx vitest run src/routes/BackgroundServices.test.tsx` - FAIL.
- [ ] **Step 3: Implement.** Memory via `sysinfo` already in the workspace if present, else `/proc` on Linux, `task_info` on macOS, `K32GetProcessMemoryInfo` on Windows; omit on failure. Screen built from `components/ui.tsx`, uses the host's confirm dialog, polls `/api/ui/plugins/ui` every 5 s while visible.
- [ ] **Step 4: Run** both - PASS; `npm run build`.
- [ ] **Step 5: Guide, docs, commit** `settings: background services screen with memory and close`.

### Task 5: Chat as a generic ACP client (POM frontend)

**Files:**
- Create: `apps/frontend/src/lib/acp/connection.ts` (WebSocket + JSON-RPC transport over the proxy path), `apps/frontend/src/lib/acp/session.ts` (state reducer), `apps/frontend/src/components/chat/AgentPicker.tsx`, `AcpToolCard.tsx`, `AcpPermissionCard.tsx`
- Modify: `apps/frontend/src/routes/Chat.tsx` (agent mode), `apps/frontend/package.json` (`@aaif/goose-acp-client` exact version)
- Test: `apps/frontend/src/lib/acp/session.test.ts`, `apps/frontend/src/routes/Chat.acp.test.tsx` (fake WebSocket server)
- Docs: `web-ui.md`, `docs/guides/screens/chat*.md`

**Interfaces:**
- Consumes: `/api/ui/chat/agents` (Task 3), `/api/ui/plugins/<code>/open`.
- Produces: `openAcp(proxyPath: string): Promise<AcpConnection>`; `AcpConnection { newSession(cwd?: string, model?: string): Promise<string>; loadSession(id: string): Promise<void>; listSessions(): Promise<AcpSessionInfo[] | null>; prompt(sessionId: string, text: string): AsyncIterable<AcpUpdate>; respondPermission(requestId: string|number, optionId: string): void; close(): void }`; `type AcpUpdate = {kind:"text",delta:string} | {kind:"tool",call:AcpToolCall} | {kind:"permission",request:AcpPermissionRequest} | {kind:"plan",entries:unknown[]} | {kind:"done",stopReason:string}`. Use the official client's types and schema where it exposes them; `connection.ts` only adapts its transport to the proxy WebSocket.

- [ ] **Step 1: Failing tests** in `session.test.ts`: `accumulates agent_message_chunk deltas`, `upserts tool calls by toolCallId with status transitions`, `permission request blocks until answered`. In `Chat.acp.test.tsx`: `picker hidden without agents`, `choosing a stopped agent calls open and shows Starting Goose until ready`, `streams text and draws a tool card`, `Approve sends the chosen optionId`, `socket close mid-turn shows retry`, `drops a selected agent that disappears`, `session list comes from session/list and reopening calls session/load`, `model picker hidden when the agent advertises no model option`.
- [ ] **Step 2: Run** `npx vitest run src/lib/acp src/routes/Chat.acp.test.tsx` - FAIL.
- [ ] **Step 3: Implement.** Agent mode leaves the existing model path untouched (default "Model"). The WebSocket URL is same-origin `wss?://<host><proxy_path>`; no token in the URL.
- [ ] **Step 4: Run** those tests plus the whole frontend suite `npx vitest run` - PASS; `npm run build`.
- [ ] **Step 5: Docs, guide, commit** `chat: talk to ACP agents announced by plugins`.

### Task 6: Scaffold `pom-plugin-goose`

**Files:** new repository `../pom-plugin-goose` (remote `git@github.com:pieceofmindcloud/pom-plugin-goose.git`).

**Interfaces:**
- Produces: plugin code `goose`, entry `pom_goose_plugin_v1`, manifest `lifecycle: "on_demand"`, no menu, icon/description/docs.

- [ ] **Step 1:** `bash .claude/skills/creating-pom-plugins/scripts/new-plugin.sh ../pom-plugin-goose --code goose --name "Goose"`; `git init -b main`; set the remote.
- [ ] **Step 2:** Remove the demo screen, menu, route and `rpc.*` demo ops; set `lifecycle: "on_demand"`; keep docs, icon, assets. Adjust `tests/contract.rs` expectations (no menu).
- [ ] **Step 3: Run** `scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && tests/test_ci_plan.sh` - PASS.
- [ ] **Step 4: Commit and push** `main` of the new repository: `scaffold: goose plugin, on demand, no menu`. Add it to `pom-cluster-config/scripts/clone-repos.sh` and commit there.

### Task 7: Goose installer, supervisor, bridge and `acp_agent` (`pom-plugin-goose`)

**Files:**
- Create: `src/installer.rs` (pinned assets table `{target, asset, sha256}` for the six platform tarballs/zip of the pinned version), `src/provider.rs` (Goose custom provider file and env), `src/supervisor.rs`, `src/bridge.rs` (model: `pom-plugin-openmausbot/src/bridge.rs`)
- Modify: `src/lib.rs` (`host.services`, `ui.upstream`), `src/context.rs`
- Test: unit tests in each module, `tests/contract.rs`

**Interfaces:**
- Consumes: `host.configure` (`gateway.openai_base_url`, `gateway.api_key`, `workspace_root`).
- Produces: `host.services` -> `{"services":[{"id":"goose","kind":"acp_agent","name":"Goose","status":<installing|starting|ready|error>,"version":"<pin>","acp":{"path":"/acp","protocol_version":1}}]}`; `ui.upstream` -> bridge port and token.

- [ ] **Step 1: Failing tests:** `installer_rejects_a_digest_mismatch`, `installer_picks_the_asset_for_each_target`, `provider_file_points_at_the_gateway_and_holds_no_key` (key only in the child env `POM_API_KEY`), `bridge_refuses_without_plugin_token`, `bridge_injects_goose_secret_and_strips_pom_headers`, `bridge_proxies_websocket_upgrade`, `services_report_installing_then_ready`, `supervisor_restarts_after_child_exit`, `reconfigure_rewrites_provider_and_restarts`, `query_returns_within_3s_during_install`.
- [ ] **Step 2: Run** `cargo test --locked` - FAIL.
- [ ] **Step 3: Implement.** First read the pinned Goose source for the custom provider format (`~/.config/goose/custom_providers/*.json` or current equivalent), `GOOSE_SERVER__SECRET_KEY` header name and the ACP WebSocket path; record them in `AGENTS.md`. Child: `goose serve --host 127.0.0.1 --port <free>` with `HOME`/`XDG_CONFIG_HOME` under `<dir of POM_PLUGIN_DB>/goose/home`, cwd `workspace_root`, its own process group, stdin/stdout not inherited. Models and limits refreshed from `/v1/models` every 15 s into the provider file.
- [ ] **Step 4: Run** the suite, fmt, clippy - PASS. Manual check on a dev node (`POM_DEV_MODE=1` rebuild): Chat lists Goose, a prompt that lists the workspace triggers a tool card and a permission card.
- [ ] **Step 5: README, AGENTS.md, docs/getting-started; commit and push**; tag a prerelease `v0.1.0` through the release workflow.

### Task 8: Goose tab in Harness (`pom-plugin-harness`)

**Files:**
- Modify: `src/context.rs` (keep `host_api`), `src/lib.rs` (handle `host.event` `plugin.*`), new `src/peers.rs` (fetch `/api/plugin-host/plugins`, write `data/installed-plugins.json`)
- Modify: `runtime/agents.mjs` (agent `{ kind: "goose", ... }` gated on the file), `runtime/agent-setup.mjs` (pinned Goose download and SHA-256 check, `pom` provider in the private home), `runtime/launcher.mjs` (watch the file, open/remove tab), `scripts/fetch-runtime.sh` (`agents.json` gains the Goose pin and digests)
- Test: `tests/` (Rust) and the runtime's node tests

**Interfaces:**
- Consumes: Task 1 route and events.
- Produces: `data/installed-plugins.json` = `{"plugins":[{"code","version"}]}`, rewritten on `host.configure` and on each `plugin.*` event.

- [ ] **Step 1: Failing tests:** Rust `writes_installed_plugins_on_configure`, `rewrites_on_plugin_installed_and_removed`; runtime `goose agent enabled only when goose is installed`, `install order harness-then-goose opens the tab on the event`, `removal deletes the command and leaves the open tab`, `goose provider points at POM and the key stays in env`.
- [ ] **Step 2: Run** `cargo test --locked` and `node --test runtime/` - FAIL.
- [ ] **Step 3: Implement**, reusing the OpenCode code paths (host install reuse when `goose --version` answers, otherwise the pinned download).
- [ ] **Step 4: Run** both suites, fmt, clippy - PASS; manual check on a dev node in both install orders.
- [ ] **Step 5: README/AGENTS.md; commit and push** on Harness's working branch.
