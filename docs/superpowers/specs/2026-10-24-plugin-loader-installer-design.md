# Plugin loader and installer - design

## Intent

Today the Hermes Agent plugin draws its own setup screen: a badge that says
`installing`, a status line, an optional model field and an Install button. It
is the plugin's own look, it shows no real progress and it makes the person
click before anything happens.

POM already owns the "a plugin is on its way" moment: the pieces animation
(`MissingPiece variant="loading"`, mounted by `PluginLoading` in
`components/PluginScreen.tsx`). This design makes that animation the **official
loader and installer of every plugin**: POM draws it, with a progress bar when
there is progress to show, and a plugin only reports what is happening - the
progress, a short message, and success or an error - through JavaScript
events. The plugin no longer draws any loading or install screen of its own.

Hermes is the first user: opening it from the menu when it is not installed
starts the installation by itself, behind POM's installer.

## What the person sees (decided with the operator)

1. **Not installed.** Opening Hermes from the menu starts the install with no
   click. POM shows the pieces with the fourth piece hovering, the title
   "Installing Hermes Agent", a progress bar with its percentage and the
   plugin's message saying what is happening (for example "Installing Python
   packages").
2. **Error.** The fourth piece turns rose, the title says the install failed,
   the plugin's error message is shown and a "Try again" button asks the
   plugin to install again.
3. **Success.** The puzzle completes, a short "installed" line, and the plugin's
   screen opens (for Hermes, the dashboard).
4. **Already installed and running.** No install screen at all: Hermes opens
   straight away. The ordinary opening animation POM already shows for every
   plugin screen stays as it is today (it lasts until the plugin's `ready()`).
5. **Installed, dashboard still starting** (on-demand, a few seconds). The
   generic pieces loader with no bar stays up for as long as the dashboard
   takes, not just the 2 s grace of today.

The mockup used in the conversation:
`.superpowers/brainstorm/*/content/install-flow.html` (not committed).

## Goals

- One loader/installer look for every plugin, drawn by POM, consistent with the
  existing pieces animation and both themes.
- A small, versioned contract a plugin uses to drive it: progress, message,
  success, error, retry, hide.
- Hermes drops its setup screen, installs automatically on first open and
  reports real progress.

## Non-goals

- Installing the plugin binary itself (the Plugins screen, license activation,
  `pom-plugin-host`). This is about the work a plugin does **inside its own
  screen** after it is loaded.
- Any change to the node (Rust) or to the IPC between POM and plugins: the
  contract lives entirely in the browser, between the shell and the plugin's
  screen module.
- Migrating plugins other than Hermes. They keep working unchanged.
- Persisting progress across reloads: the plugin's own backend is the source
  of truth and the screen reports again after a reload.

## Anti-goals

- A plugin that never reports must not get stuck behind a loader: without any
  loader request the current behavior (open on `ready()` or after the 2 s
  grace) is unchanged.
- POM must never show raw markup or an empty bar: invalid requests are refused
  (`request_invalid`), like the existing notification commands.
- Secrets and subprocess output never reach the loader: messages are short
  strings chosen by the plugin.

## Contract (host version 7)

The commands ride the existing `pom-plugin-events/v1` command channel
(`lib/pluginEvents.ts`): the same `pom:plugin-request` `CustomEvent` on
`window` (and same-origin `postMessage`), the same envelope
`{protocol, id, type, plugin_code, payload}`, the same `isPlugin` check, the
same `notification.response`-style answer. New command types:

| type | payload | effect |
|---|---|---|
| `loader.show` | `{title?, message?}` | generic pieces loader, no bar; holds the screen covered past the 2 s grace |
| `loader.progress` | `{title?, message?, progress}` with `progress` 0..100 (number) | installer: bar and percentage |
| `loader.error` | `{title?, message, retry?: boolean}` (`retry` defaults to true) | error state; "Try again" when `retry` |
| `loader.success` | `{title?, message?}` | success state, then the screen opens (about 1 s) |
| `loader.hide` | `{}` | the screen opens now (same as `ready()`) |

- `title` and `message` are plain text, trimmed, `title` <= 200 chars,
  `message` <= 2048 (the notification limits). Missing `title` falls back to
  POM's own localized text with the plugin's display name ("Installing X",
  "Opening X", "Could not install X", "X installed").
- `progress` is clamped to 0..100 and shown as an integer; the bar never moves
  backwards on screen (a lower value keeps the bar where it is but updates the
  message).
- "Try again" publishes the event `loader.retry` to that plugin only
  (`publishPluginEvent("loader.retry", {}, code)`), and the loader returns to
  the generic state until the next request.
- The loader belongs to the **mounted plugin screen** (the gate armed by
  `PluginScreen`, like `ready()`): a request from a plugin whose screen is not
  the one mounted is answered `failed` with `screen_not_mounted` and draws
  nothing. Leaving the screen discards the loader state.
- After `loader.hide`, `loader.success` or `ready()`, the screen is open; a
  later `loader.*` request covers it again (a plugin may need to reinstall).

The SDK exposes the same commands without events:
`__POM_HOST__.plugin(code).loader.{show, progress, error, success, hide}` and
`plugin(code).events.subscribe("loader.retry", ...)`. `PLUGIN_HOST_VERSION`
goes to 7; a plugin checks `version >= 7` before using them and falls back to
its own text on an older POM.

## POM side, as decided

- `PluginScreen` keeps mounting the screen under the cover, as today; the cover
  becomes a loader component that renders the current loader state for that
  plugin: generic (today's `PluginLoading`), installing (bar), error, success.
- The visuals extend `MissingPiece` (new variants or a sibling component, the
  builder's call) and use the theme tokens; the bar has `role="progressbar"`
  with `aria-valuenow`, and the message region is `aria-live="polite"`.
  Reduced motion stops the hover, as the existing animation does.
- Strings in `i18n` (en and pt-BR).

## Hermes side, as decided

- The setup panel, the badge and the optional model field go away. Hermes
  already connects to POM's models by itself and switches models in its own
  dashboard. (The `install` RPC keeps accepting `model` for compatibility of
  the call shape; the screen just stops sending it.) This is the designer's
  call, not confirmed by the operator.
- On mount the screen reads `rpc.status`:
  - `waiting` -> calls `rpc.install` at once, then reports progress.
  - `installing` -> `loader.progress` from the status.
  - `starting` -> `loader.show` (generic).
  - `ready` -> mounts the iframe and calls `ready()` when it loads (today's
    behavior, no loader request).
  - `error` -> `loader.error` with the status error; `loader.retry` calls
    `rpc.install` again.
- The supervisor's status gains `progress` (0..100) and a `step` code; the
  screen turns `step` into its own localized message. Real progress comes from
  the install steps in `installer::ensure` and `register_pom`: the downloads
  report bytes (the size is known from the pinned manifest or
  `Content-Length`); the steps without a measurable size advance by fixed
  weights. **Decided by the operator:** inside each unmeasured step the bar
  creeps forward with time (an eased approach toward the step's end that never
  reaches it), and the message always says what is happening in that step
  ("Downloading the Python runtime", "Installing Python packages", "Building
  the dashboard", "Connecting to POM's models"). The percentage is an estimate
  there, not a measurement; the bar jumps to the step's end when the step
  really finishes and never moves backwards. The weights and the creep curve
  are the builder's call; the bar must reach 100 only when the install
  finished.
- `configure` keeps launching an existing install on restart; the comment "a
  new install is always opt-in" changes to say that opening the screen is the
  opt-in.

### Settled after the builder check (designer's calls unless noted)

- **Install versus start.** The supervisor reports `installing` (bar) only when
  the `.installed` marker is missing or does not match the pinned source; the
  check `ensure` runs on every start of an existing install reports
  `starting` (generic loader). A reinstall after a plugin update that changes
  the pinned source uses the installer with the title "Updating Hermes Agent".
- **When success fires.** The bar reaches 100 when `ensure` and `register_pom`
  finish; while the dashboard then starts, the installer stays at 100 with a
  "Starting" message, and `loader.success` is sent when the iframe loads. So the
  person sees install -> success -> dashboard, never a second generic loader.
- **`ready()` versus loader states.** An explicit loader state wins: `ready()`
  only opens the screen while no `loader.*` state is active (or the state is
  the generic one the plugin did not request). Only `loader.hide` or
  `loader.success` dismiss an installer or error state. Hermes stops calling
  `ready()` for non-ready statuses.
- **No flash before the first request.** The screen module is mounted under the
  cover before the 2 s grace ends, so Hermes sends `loader.show` on its first
  render, before `rpc.status` answers; the grace never opens an empty screen.
- **Errors after a start.** If the dashboard stops later, Hermes covers it with
  `loader.error` with its own title ("Hermes Agent stopped"); "Try again"
  launches it again.
- **Older POM.** No fallback for host versions below 7: the product has not
  shipped (root `AGENTS.md`, first-version policy). Hermes requires version 7.
- **Who may install.** No new gate: any profile installs plugins on its own
  node (root `AGENTS.md`), and opening the screen is the opt-in.

## Template and docs

- The plugin skill template (`.claude/skills/creating-pom-plugins/template`)
  gets the loader helper next to `ready()` in `ui/src/host/`, and
  `reference.md`/`recipes.md` document it (the on-demand recipe points to it).
- Docs updated in the same commits: `docs/subsystems/plugins-ui.md` (the
  contract), `apps/frontend/AGENTS.md` and the `src/lib`/`src/components`
  `AGENTS.md`, the `lib/pluginHost.ts` header (version 7), and the Hermes
  repository's `AGENTS.md`.

## Left to the builder

- Component split and naming on the POM side; how the success state times out.
- Exact progress weights and step codes in Hermes.
- Tests: vitest for the command validation, the state machine and the
  rendering (jsdom, following `PluginScreen.test.tsx`); Rust tests in Hermes
  for the progress reported by the supervisor; the existing contract tests stay
  green.
