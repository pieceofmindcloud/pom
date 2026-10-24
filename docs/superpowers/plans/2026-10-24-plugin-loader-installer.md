# Plugin loader and installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** POM draws the official loader and installer of every plugin screen (pieces, bar, error with retry, success), driven by `loader.*` commands a plugin sends over `pom-plugin-events/v1`; Hermes uses it to install itself on first open and drops its own setup screen.

**Architecture:** A per-plugin loader store in the frontend (`lib/pluginLoader.ts`) receives validated `loader.*` commands from the existing command channel (`runPluginCommand`) and the scoped SDK; `PluginScreen` covers the mounted screen while the ready gate is closed **or** the store holds a state, rendering the new `PluginLoader`. Hermes's supervisor reports `step`/`progress`/`mode`, and its screen maps the status to loader commands.

**Tech Stack:** React 18 + TypeScript + Vitest/jsdom (POM frontend); Rust + esbuild-bundled TSX (Hermes plugin).

**Spec:** `docs/superpowers/specs/2026-10-24-plugin-loader-installer-design.md`

## Global Constraints

- `PLUGIN_HOST_VERSION` becomes `7`; Hermes requires `hostVersion() >= 7`, no fallback screen.
- Command types exactly: `loader.show`, `loader.progress`, `loader.error`, `loader.success`, `loader.hide`; event to the plugin: `loader.retry`.
- `title` <= 200 chars, `message` <= 2048 chars, both trimmed; `progress` a finite number, clamped to 0..100, shown as an integer; bar never moves backwards.
- `loader.error.retry` defaults to `true`.
- A command for a plugin whose screen is not mounted answers `failed` with error `screen_not_mounted`; invalid payload -> `request_invalid`; unknown plugin -> `plugin_unknown` (existing).
- Success state shows for 1000 ms, then the screen opens.
- No change to the Rust node or to the plugin IPC.
- Docs in English, no absolute paths, no hostnames or personal names, never an em dash; update the folder `AGENTS.md` and subsystem docs in the same commit as the code.
- Copy (en / pt-BR), POM defaults: "Installing {name}" / "Instalando {name}"; "Updating {name}" / "Atualizando {name}"; "Could not install {name}" / "Não foi possível instalar {name}"; "{name} installed" / "{name} instalado"; "Try again" / "Tentar novamente". Generic stays `plugins.loadingNamed` ("Opening {name}").

## Review Focus

- A plugin that never sends `loader.*` (every plugin but Hermes): opening is exactly as today (open on `ready()` or the 2 s grace). Test in Task 3.
- `ready()` arriving while an installer/error state is shown: the cover stays. Test in Task 3.
- A `loader.*` command after the screen already opened (Hermes dashboard dies): the cover comes back. Test in Task 3.
- Leaving the screen mid-install and coming back: no stale state from the old mount; the plugin reports again. Test in Task 1 (unmount clears) and Task 3.
- `progress` as `NaN`, a string, negative or 250: refused or clamped, never an empty or overflowing bar. Test in Task 1.

---

## Part 1 - POM (`pom-jair`, branch `develop`)

### Task 1: Loader store and command validation

**Files:**
- Create: `apps/frontend/src/lib/pluginLoader.ts`
- Test: `apps/frontend/src/lib/pluginLoader.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export type LoaderCommandType = "loader.show" | "loader.progress" | "loader.error" | "loader.success" | "loader.hide";
  export type LoaderState =
    | { kind: "generic"; title?: string; message?: string }
    | { kind: "progress"; title?: string; message?: string; progress: number }
    | { kind: "error"; title?: string; message: string; retry: boolean }
    | { kind: "success"; title?: string; message?: string };
  export const LOADER_SUCCESS_MS = 1000;
  export function normalizeLoaderCommand(type: unknown, payload: unknown): { type: LoaderCommandType; state: LoaderState | null } | null; // null = invalid; state null for loader.hide
  export function mountPluginLoader(code: string): () => void;          // arms the store for the mounted screen; the disarm clears its state
  export function applyLoaderCommand(code: string, type: LoaderCommandType, state: LoaderState | null): "delivered" | "screen_not_mounted";
  export function usePluginLoader(code: string): LoaderState | null;   // useSyncExternalStore
  export function onLoaderOpen(code: string, listener: () => void): () => void; // fired on loader.hide and when success times out
  export function resetPluginLoaderForTest(): void;
  ```

- [ ] **Step 1: Write the failing tests** in `pluginLoader.test.ts`:
  - `normalizeLoaderCommand("loader.progress", {progress: 42.6, message: " Installing Python packages "})` -> state `{kind: "progress", progress: 43, message: "Installing Python packages"}`.
  - progress `250` -> `100`; `-5` -> `0`; `NaN`, `"40"`, missing -> `null` (invalid).
  - `loader.error` without `message` -> `null`; with `{message: "x"}` -> `retry: true`; `{message: "x", retry: false}` -> `retry: false`.
  - title of 201 chars, message of 2049 chars, unknown type `loader.spin` -> `null`.
  - `loader.hide` with `{}` -> `{type: "loader.hide", state: null}`.
  - `applyLoaderCommand("hermes", ...)` before `mountPluginLoader("hermes")` -> `"screen_not_mounted"`.
  - after mount: progress 60 then progress 40 with message "b" -> state progress stays `60`, message `"b"` (never backwards).
  - progress after an error state starts fresh (a retry may restart at 0).
  - `loader.success` -> state success; after `LOADER_SUCCESS_MS` (fake timers) state `null` and the `onLoaderOpen` listener fired once.
  - `loader.hide` -> state `null`, `onLoaderOpen` listener fired.
  - the disarm returned by `mountPluginLoader` clears the state; a new mount starts at `null`.
- [ ] **Step 2: Run** `cd apps/frontend && npx vitest run src/lib/pluginLoader.test.ts` - expected FAIL (module missing).
- [ ] **Step 3: Implement** `pluginLoader.ts`: a module-level `Map<code, {mounted: number, state}>` with subscribers, the success timer per code (cleared on any new command or disarm); reuse the 200/2048 limits as exported constants from `pluginEvents.ts` (`MAX_TITLE`, `MAX_BODY`, export them).
- [ ] **Step 4: Run** the same command - expected PASS.
- [ ] **Step 5: Commit** `git commit -m "Plugin loader store and loader.* command validation"`.

### Task 2: Command channel, SDK and host version 7

**Files:**
- Modify: `apps/frontend/src/lib/pluginEvents.ts` (`PluginEventType`, `PluginCommand["type"]`, `NotificationResponse["kind"]`, `runPluginCommand`, `listenForPluginCommands`, `scopedPluginApi`)
- Modify: `apps/frontend/src/lib/pluginHost.ts` (`PLUGIN_HOST_VERSION = 7`, header comment)
- Test: `apps/frontend/src/lib/pluginEvents.test.ts` (existing file; add cases)

**Interfaces:**
- Consumes: Task 1 `normalizeLoaderCommand`, `applyLoaderCommand`.
- Produces:
  - `PluginEventType` gains `"loader.retry"`; `PluginCommand["type"]` gains the five `LoaderCommandType`s; response `kind: "loader"`.
  - `scopedPluginApi(code).loader = { show(p?), progress(p), error(p), success(p?), hide() }`, each returning `Promise<NotificationResponse>`.
  - `publishLoaderRetry(code: string): void` = `publishPluginEvent("loader.retry", {}, code)`.

- [ ] **Step 1: Write failing tests**: a DOM `pom:plugin-request` with `{protocol, id, type: "loader.progress", plugin_code: "hermes", payload: {progress: 10}}` and a mounted loader -> `usePluginLoader`-equivalent store read shows progress 10 and a `notification.response` with `kind: "loader", action: "delivered"`; same without mount -> `action: "failed", error: "screen_not_mounted"`; invalid payload -> `request_invalid`; same-origin `postMessage` with a `loader.*` type is accepted; `scopedPluginApi("hermes").loader.hide()` clears the state; `buildPluginHost().version === 7`.
- [ ] **Step 2: Run** `npx vitest run src/lib/pluginEvents.test.ts` - expected FAIL.
- [ ] **Step 3: Implement**: in `runPluginCommand` branch on `type.startsWith("loader.")` before the notification branches (the executor is still required for `isPlugin`); widen the `postMessage` type filter.
- [ ] **Step 4: Run** the tests - expected PASS; also `npx vitest run src/lib` to keep the rest green.
- [ ] **Step 5: Commit** `git commit -m "loader.* commands on pom-plugin-events/v1; host version 7"`.

### Task 3: The loader on screen

**Files:**
- Modify: `apps/frontend/src/components/MissingPiece.tsx` (variants)
- Create: `apps/frontend/src/components/PluginLoader.tsx` (replaces the use of `PluginLoading` inside `PluginScreen`; `PluginLoading` stays as the generic view and is rendered by it)
- Modify: `apps/frontend/src/components/PluginScreen.tsx`
- Modify: `apps/frontend/src/i18n/messages.csv` (keys below, sorted)
- Modify: `apps/frontend/src/components/__fixtures__/pluginScreenModule.tsx` (new exports)
- Test: `apps/frontend/src/components/PluginScreen.test.tsx`, `apps/frontend/src/components/PluginLoader.test.tsx`
- Docs: `docs/subsystems/plugins-ui.md` ("Opening a plugin screen" and "Events and notifications": the loader contract table from the spec), `apps/frontend/src/components/AGENTS.md`, `apps/frontend/src/lib/AGENTS.md`, `apps/frontend/AGENTS.md` (one line beside `PluginBridge`).

**Interfaces:**
- Consumes: Task 1 `mountPluginLoader`, `usePluginLoader`, `onLoaderOpen`; Task 2 `publishLoaderRetry`.
- Produces:
  - `MissingPiece` `variant` adds `"failed"` (fourth piece rose, still, `data-variant="failed"`) and `"done"` (the hovering piece settles into the slot).
  - `PluginLoader({ code, state }: { code: string; state: LoaderState | null })`: `null`/`generic` -> generic view (title from `state.title` or `plugins.loadingNamed`); `progress` -> bar `role="progressbar"` `aria-valuemin=0` `aria-valuemax=100` `aria-valuenow`, the percentage text, message in `aria-live="polite"`; `error` -> `failed` variant, message, "Try again" button only when `retry` that calls `publishLoaderRetry(code)` and sets the state back to generic; `success` -> `done` variant.
  - i18n keys: `plugins.loader.installing`, `plugins.loader.updating`, `plugins.loader.failed`, `plugins.loader.installed`, `plugins.loader.retry` (copy in Global Constraints). `plugins.loader.updating` is for plugins to reuse through `title`; POM only uses it as copy.

- [ ] **Step 1: Write failing tests**:
  - `PluginLoader.test.tsx`: progress 42 renders `progressbar` with `aria-valuenow="42"` and text `42%` and the message; error with `retry: true` renders "Try again" and clicking it publishes `loader.retry` to that code only (subscribe with `subscribePluginEvents`); error with `retry: false` renders no button; default titles use the display name ("Installing harness").
  - `PluginScreen.test.tsx` with new fixture exports: `installing` (on mount sends `loader.progress` 30 then calls `ready()`) -> the cover stays after 2 s (fake timers) and shows 30%; `silent` (no loader) -> opens after the grace exactly as today; `failsLater` (sends `loader.error` after it opened) -> the cover comes back; `succeeds` (progress then `loader.success`) -> cover with `done` then the screen opens after `LOADER_SUCCESS_MS`; unmount + remount -> the second mount starts with no installer state.
- [ ] **Step 2: Run** `npx vitest run src/components/PluginScreen.test.tsx src/components/PluginLoader.test.tsx` - expected FAIL.
- [ ] **Step 3: Implement**: in `PluginScreen`, `mountPluginLoader(resolved.code)` in an effect keyed like the module load; `const loader = usePluginLoader(code)`; `onLoaderOpen` sets the ready gate open; covered = `!open || loader !== null`; render `<PluginLoader code state={loader}/>` where `PluginLoading` is rendered today. Reduced motion: no hover animation (follow the existing `useReducedMotion` use).
- [ ] **Step 4: Run** the tests - expected PASS; then `npm test` (whole suite, includes `locales.equivalence.test.ts`) and `npm run build` - both pass.
- [ ] **Step 5: Commit** code and docs together `git commit -m "POM plugin loader and installer screen"`.

## Part 2 - Hermes (`pom-plugin-hermes`, branch `main`)

### Task 4: Install progress in the supervisor

**Files:**
- Modify: `src/installer.rs` (`ensure` reports steps; new `install_mode`)
- Modify: `src/supervisor.rs` (`Status` fields, `public()`, phase choice, `configure` comment)
- Test: unit tests in both files
- Docs: `AGENTS.md` (install progress section)

**Interfaces:**
- Produces (Rust):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq)]
  pub enum Step { Source, Uv, Python, Packages, Dashboard, Connect }
  pub enum InstallMode { Fresh, Update, Installed }        // installer::install_mode(base) -> InstallMode
  pub fn ensure(base, home, run, report: impl FnMut(Step, Option<f64>)) -> Result<PathBuf, String>; // f64 = measured fraction 0..1 of the step, None = unmeasured
  pub fn progress_at(step: Step, elapsed: Duration, measured: Option<f64>) -> u8; // in supervisor.rs
  ```
  Weights (start..end): Source 0..5, Uv 5..8, Python 8..20, Packages 20..75, Dashboard 75..95, Connect 95..100. Unmeasured creep: `start + span * 0.95 * (1 - exp(-elapsed / tau))` with tau Python 30 s, Packages 180 s, Dashboard 90 s, Connect 5 s; measured steps use `start + span * fraction`. `progress_at` never returns the step's end; the end value is reported when the next step starts. 100 only after `register_pom` returns.
- `rpc.status` JSON: `{status, detail, step?, progress?, mode?}` with `step` in `source|uv|python|packages|dashboard|connect`, `mode` in `install|update`, present only while `status == "installing"`. Phase `installing` only when `install_mode` is `Fresh` or `Update`; otherwise `starting` straight away.
- `download` reports `Some(bytes / total)` using `Content-Length` (or the step's `MAX_*` when absent).

- [ ] **Step 1: Write failing tests**: `progress_at(Step::Packages, 0s, None) == 20`; `progress_at(Step::Packages, 10_000s, None) < 75`; monotone in elapsed (sample 0..600 s); `progress_at(Step::Source, _, Some(0.5)) == 2` (rounded down from 2.5); `install_mode` with no marker -> `Fresh`, marker with another sha -> `Update`, matching marker + `web_dist/index.html` -> `Installed`; `Status` public JSON for an installing status carries `step`, `progress`, `mode` and none of them when `ready`.
- [ ] **Step 2: Run** `cargo test --locked` - expected FAIL to compile.
- [ ] **Step 3: Implement**: the supervisor keeps `(Step, Instant, Option<f64>)` in the status and computes `progress` in `public()` (so the creep needs no timer thread); the `configure` comment says opening the screen is the opt-in.
- [ ] **Step 4: Run** `cargo test --locked`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` - all pass.
- [ ] **Step 5: Commit** `git commit -m "Install steps and estimated progress in rpc.status"`.

### Task 5: Hermes screen on POM's loader

**Files:**
- Modify: `ui/src/host/runtime.ts` (loader helpers)
- Modify: `ui/src/screens/Main.tsx`
- Modify: `i18n/en.json`, `i18n/pt-BR.json` (remove the `main.*` setup keys, add step and title keys)
- Docs: `AGENTS.md`, `docs/getting-started.md` if it describes the Install button

**Interfaces:**
- Consumes: Task 2 SDK (`plugin(code).loader.*`, `loader.retry` event); Task 4 status JSON.
- Produces in `runtime.ts`: `export const loader = { show, progress, error, success, hide }` (thin wrappers over `scoped()?.loader`, no-ops on a host without it) and `onLoaderRetry(handler): () => void` (via `onPomEvent`; add `"loader.retry"` to `PomEventType`).
- Mapping in `Main.tsx` (decided in the spec):
  - first render: `loader.show()` before `rpc.status` answers;
  - `waiting` -> `rpc("install", {})` once, then poll;
  - `installing` -> `loader.progress({title, progress, message})`, title "Installing Hermes Agent" or, with `mode: "update"`, "Updating Hermes Agent", message from `step`;
  - `starting` after an install in this mount -> `loader.progress({progress: 100, message: "Starting"})`; otherwise `loader.show()`;
  - `ready` -> mount the iframe; on its `load`, `loader.success()` if this mount installed, else `loader.hide()`;
  - `error` -> `loader.error({title, message: status.error})`; before ready the title is POM's default, after ready "Hermes Agent stopped"; `onLoaderRetry` calls `rpc("install", {})`.
  - Poll interval stays 2500 ms; while installing poll every 1000 ms so the bar moves.
  - Step messages (en / pt-BR): source "Downloading Hermes Agent" / "Baixando o Hermes Agent"; uv "Preparing the installer" / "Preparando o instalador"; python "Downloading the Python runtime" / "Baixando o runtime Python"; packages "Installing Python packages" / "Instalando pacotes Python"; dashboard "Building the dashboard" / "Montando o painel"; connect "Connecting to POM's models" / "Conectando aos modelos do POM"; starting "Starting Hermes Agent" / "Iniciando o Hermes Agent".
  - `ready.ts`/`useScreenReady` is no longer used by `Main.tsx` (the loader replaces it); keep the file for the template parity.

- [ ] **Step 1: Build check as the test**: there is no UI test runner in this repository; the gate is `scripts/build-ui.sh` (type-check via esbuild bundle) plus `POM_UI_REQUIRE_DIST=1 cargo test --locked` (contract tests: manifest assets, i18n keys identical across catalogs). Run them first after removing the old keys from the catalogs - expected FAIL (keys referenced in `Main.tsx`).
- [ ] **Step 2: Implement** `runtime.ts` helpers and the `Main.tsx` mapping above; delete the setup panel, the badge, the model field and `install`'s `model` param from the screen (the RPC keeps accepting it).
- [ ] **Step 3: Run** `scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked && cargo clippy --locked --all-targets -- -D warnings` - all pass.
- [ ] **Step 4: Commit** `git commit -m "Hermes screen uses POM's loader and installs on first open"`.

## Part 3 - Template and live check

### Task 6: Plugin template and skill docs

**Files:**
- Modify: `.claude/skills/creating-pom-plugins/template/ui/src/host/runtime.ts` (same `loader` and `onLoaderRetry` helpers as Task 5, `"loader.retry"` in the event type)
- Modify: `.claude/skills/creating-pom-plugins/reference.md` (section 4: `PLUGIN_HOST_VERSION = 7`, the loader commands; section 6: `loader.retry`)
- Modify: `.claude/skills/creating-pom-plugins/recipes.md` (section J on-demand: point to the loader for install/start; section C verified runtime download: report progress through it)

- [ ] **Step 1: Run** the template's own checks the skill documents: scaffold to a temp dir with `scripts/new-plugin.sh <tmp>/pom-plugin-demo --code demo --name "Demo"`, then `scripts/build-ui.sh` and `POM_UI_REQUIRE_DIST=1 cargo test --locked` in it - expected PASS after the change.
- [ ] **Step 2: Commit** in `pom-jair` `git commit -m "Plugin template: POM loader helpers"`.

### Task 7: Live verification

- [ ] **Step 1:** Build the frontend (`npm run build`) and rebuild Hermes in the dev cluster with the Plugins screen's "Rebuild and reinstall" (the dev container holds a copy of the plugin sources; copy the changed files in first, as before).
- [ ] **Step 2:** With Hermes installed: open it from the menu -> generic pieces while the dashboard starts, then the dashboard; no installer bar.
- [ ] **Step 3:** Force a fresh install in the dev container by removing the plugin directory's `.installed` marker, open Hermes: installer bar moves, messages change per step, success, dashboard. Save screenshots of the installer and the error state for the report (not committed).
- [ ] **Step 4:** Kill the dashboard process while open: `loader.error` "Hermes Agent stopped" with Try again, which brings it back.
- [ ] **Step 5:** Report results; pushes (`develop` for POM, `main` for Hermes) only after the operator says so.
