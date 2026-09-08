# Chat do macOS na barra de menus - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dar ao bundle macOS do POM um item de barra de menus que abre o chat em um painel de 390 x 844 points e oferece a mesma conversa em uma janela maior, sem mudar o CLI ou outros alvos.

**Architecture:** O binário Rust continua dono do gateway e inicia uma thread de serviço Tokio. Somente no alvo macOS, uma ponte Objective-C compilada por `build.rs` mantém o `NSStatusItem`, `NSPopover`, `NSWindow` e `WKWebView` na thread principal. Os WebViews carregam `/admin-ui/chat` no mesmo origin e usam o data store padrão para reutilizar a credencial e as sessões do frontend.

**Tech Stack:** Rust, Tokio, `cc`, Objective-C/AppKit/WebKit nativos do macOS, React/TypeScript, Vite, Vitest.

**Spec:** `docs/superpowers/specs/2026-09-07-macos-menu-bar-chat-design.md`

## Global Constraints

- O código nativo será compilado somente quando `CARGO_CFG_TARGET_OS=macos`.
- Linux e Windows continuam sem dependência ou link de AppKit/WebKit.
- O processo continua aceitando os mesmos argumentos e servindo como CLI.
- A URL usa a porta de `pom.json`, com fallback ao `--gateway-port` e default 8080.
- Nenhuma credencial paralela: o frontend segue usando `GET /api/ui/node` e `pom.connectionKey`.
- Toda string visível nova passa pelos locales do frontend ou por `Localizable.strings` do bundle macOS.
- Não editar `CHANGELOG.md` nem arquivos gerados.

---

### Task 1: Rota compacta e botão do WebView no frontend

**Files:**
- Modify: `apps/frontend/src/App.tsx`
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/App.test.tsx`
- Test: `apps/frontend/src/routes/Chat.navigation.test.tsx`

**Interfaces:**
- Produces: `isEmbeddedChat` behavior for `/admin-ui/chat` and a localized button that posts `pomOpenWindow` when a native handler exists.

- [ ] **Step 1: Write failing route test**

  Extend the existing `App.test.tsx` mocked Chat assertion with a test opening
  `/admin-ui/chat`; it must find `tela-chat`. Run `cd apps/frontend && npm test -- --run src/App.test.tsx` and confirm failure because `App` does not recognize the path.

- [ ] **Step 2: Implement the route alias**

  Define `const isEmbeddedChat = location.pathname === "/admin-ui/chat"` and
  include it in the same allowed chat branch as `/chat`. Render the embedded
  chat without the administrative sidebar/topbar so the 390 point viewport is
  usable.

- [ ] **Step 3: Write failing bridge/button test**

  Add a focused test around the embedded chat control that installs
  `window.webkit.messageHandlers.pomOpenWindow.postMessage`, renders the chat,
  clicks the localized button, and asserts the handler receives `"open"`. Run
  the focused Vitest test and confirm the button is missing before production
  code changes.

- [ ] **Step 4: Implement the localized native-window control**

  Add locale keys in English and pt-BR for the button label and accessible
  description. In `Chat.tsx`, detect the optional WebKit handler, render a
  compact embedded header action, and post `"open"` on click. Keep the normal
  browser route unchanged when the handler is absent.

- [ ] **Step 5: Run the frontend tests and build**

  Run `cd apps/frontend && npm test` and `cd apps/frontend && npm run build`.
  Expected result: all tests pass and Vite emits `dist` without TypeScript or
  layout build errors.

- [ ] **Step 6: Commit the frontend behavior**

  ```bash
  git add apps/frontend/src/App.tsx apps/frontend/src/routes/Chat.tsx apps/frontend/src/i18n/locales.ts apps/frontend/src/App.test.tsx apps/frontend/src/routes/Chat.navigation.test.tsx
  git commit -m "feat(ui): add embedded menu bar chat route"
  ```

### Task 2: Cross-platform Rust entrypoint and pure URL helpers

**Files:**
- Modify: `crates/node/src/main.rs`
- Create: `crates/node/src/macos_status.rs`
- Test: `crates/node/src/main.rs` module tests

**Interfaces:**
- Produces: `async_main(args: Args) -> Result<()>` containing the existing
  startup path, `gateway_url(port: u16) -> String`, and
  `configured_gateway_port(args: &Args) -> u16`.

- [ ] **Step 1: Write failing pure helper tests**

  Add tests for `gateway_url(8080)` and a non-default port, plus a test that a
  persisted config port wins over a CLI fallback through a small pure selector
  helper. Run `cargo test -p node main_tests` and confirm the new helpers do not
  exist yet.

- [ ] **Step 2: Implement the pure helpers**

  Add a private URL formatter and a selector that receives `Option<u16>` from
  config plus the CLI port. Use `Config::load()` only at the macOS boundary to
  produce that option; invalid or absent config falls back to the parsed CLI
  value.

- [ ] **Step 3: Move argument parsing out of the async body**

  Replace `#[tokio::main] async fn main()` with a synchronous `fn main() ->
  Result<()>` that initializes logging, parses `Args` once, and calls
  `async_main(args)` on non-macOS. Preserve `respawn::remember_argv` and all
  existing startup branches byte-for-byte where possible.

- [ ] **Step 4: Add the macOS launch seam**

  Under `cfg(target_os = "macos")`, pass the resolved gateway port and a move
  closure running `async_main(args)` to `macos_status::run`. Under other targets,
  create the same Tokio runtime locally and call `async_main(args)`. The CLI
  path must not import the macOS module on other targets.

- [ ] **Step 5: Run Rust tests and non-macOS check**

  Run `cargo test -p node main_tests` and `cargo check --workspace`. Expected:
  pure tests pass, and non-macOS compilation has no AppKit/WebKit symbols.

- [ ] **Step 6: Commit the entrypoint seam**

  ```bash
  git add crates/node/src/main.rs crates/node/src/macos_status.rs
  git commit -m "feat(mac): add native status bar launch seam"
  ```

### Task 3: Native AppKit/WebKit bridge and localized unavailable state

**Files:**
- Modify: `crates/node/build.rs`
- Modify: `crates/node/Cargo.toml`
- Create: `crates/node/src/macos_status.m`
- Modify: `crates/node/src/macos_status.rs`
- Create: `assets/brand/macos/en.lproj/Localizable.strings`
- Create: `assets/brand/macos/pt-BR.lproj/Localizable.strings`

**Interfaces:**
- Consumes: `macos_status::run(port, service)` from Task 2.
- Produces: `pom_macos_status_run(url, icon_path)` C ABI, an accessory AppKit
  application, a 390 x 844 point popover, and a 1080 x 760 point window.

- [ ] **Step 1: Write the native contract test**

  Add Rust tests for the fixed dimensions and the URL passed to the bridge.
  Run the macOS-targeted test if the target is installed; otherwise run the
  host-side helper tests and record the target limitation.

- [ ] **Step 2: Implement target-gated build compilation**

  In `build.rs`, inspect `CARGO_CFG_TARGET_OS`, compile `src/macos_status.m`
  with Objective-C ARC only for `macos`, and emit AppKit and WebKit framework
  links. Emit `rerun-if-changed` for the bridge and localized resources.

- [ ] **Step 3: Implement the status item and icon**

  Create an accessory `NSApplication`, load `pom-status.png` as an `NSImage`,
  set `template = YES` and a 18 x 18 point size, attach a target/action to the
  `NSStatusItem` button, and toggle a transient `NSPopover` anchored to that
  button.

- [ ] **Step 4: Implement shared WebKit storage and the compact chat**

  Build each WebView with `WKWebsiteDataStore.defaultDataStore`, the supplied
  same-origin URL, a navigation delegate, and the `pomOpenWindow` script
  handler. Size the popover content to 390 x 844 points and use the existing
  frontend route rather than injecting credentials or request headers.

- [ ] **Step 5: Implement the larger window and failure page**

  On the script message, close the popover and show a resizable 1080 x 760
  point `NSWindow` with another WebView using the same data store. On
  provisional navigation failure, render a localized retry page with the
  gateway-unavailable message and retry action so the panel is never blank.

- [ ] **Step 6: Wire resources into the app bundle**

  Update `scripts/make-macos-app.sh` to copy `pom-mark.png` as the status icon
  and copy `assets/brand/macos/*.lproj` into `Contents/Resources`. Keep the
  existing `.icns`, `LSUIElement`, executable name, and CLI wrapper behavior.

- [ ] **Step 7: Build the macOS bundle and run focused tests**

  Run `cd apps/frontend && npm run build`, `cargo build --release` on macOS,
  `./scripts/make-macos-app.sh target/release/pom dist-release/POM.app`, and
  `plutil -lint dist-release/POM.app/Contents/Info.plist`. Expected: the
  bridge links only on macOS and the bundle contains the mark plus both locale
  directories.

- [ ] **Step 8: Commit the native bridge**

  ```bash
  git add crates/node/build.rs crates/node/Cargo.toml crates/node/src/macos_status.rs crates/node/src/macos_status.m assets/brand/macos scripts/make-macos-app.sh
  git commit -m "feat(mac): add POM menu bar chat"
  ```

### Task 4: Documentation, visual verification and delivery

**Files:**
- Modify: `AGENTS.md` only if a cross-cutting invariant is discovered
- Modify: `scripts/AGENTS.md`
- Modify: `docs/subsystems/web-ui.md` if the macOS entry point belongs there
- Create: `.lavish/` screenshots or local verification artifacts only if useful

- [ ] **Step 1: Document the current behavior**

  Update `scripts/AGENTS.md` with the native status item, resource copying and
  CLI-preserving launch behavior. Update the web UI subsystem document with
  `/admin-ui/chat`, the compact route, shared WebKit storage, and the exact
  390 x 844 point size. Do not edit generated frontend docs.

- [ ] **Step 2: Re-run the complete verification matrix**

  Run `cargo test --workspace`, `cargo check --workspace`, `cd apps/frontend && npm test`, and `cd apps/frontend && npm run build`. Run a Windows-target or Linux-target check when available and confirm no platform-specific files leak into those builds.

- [ ] **Step 3: Capture and inspect the UI**

  Start a real configured node with the gateway on its configured port, build
  and open `POM.app`, and capture the status icon, the compact 390 x 844 panel,
  and the 1080 x 760 window. Inspect light and dark themes, narrow text
  wrapping, focus/hover states, the unavailable-gateway page, and an actual
  chat request. Use `chrome-devtools-axi` for the browser-served comparison
  and macOS `screencapture` for native windows when Chrome cannot see them.

- [ ] **Step 4: Review the diff and commit documentation**

  Run `git diff --check`, inspect `git status`, and request a code review of
  the final commit range. Fix Critical and Important findings before delivery.

- [ ] **Step 5: Publish only coherent green state**

  Rebase with `git pull --rebase origin feat/implantar3` as required by the
  captain's integration rule, then publish the committed state to
  `origin/feat/implantar3` without force. Append a `done:` line naming the
  commit and branch only after the visual and automated gates pass.
