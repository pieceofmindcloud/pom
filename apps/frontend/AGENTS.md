# apps/frontend

The POM web UI, a React 18 + TypeScript SPA. This is the
product's interface: the node reads the production build from
`apps/frontend/dist` off disk at runtime (`crates/node/src/backend/webapp.rs`,
`config::ui_dist_dir()`, override `POM_UI_DIST`) and serves it on the
gateway port together with the API. `cargo build` never touches this directory;
editing the UI only requires `npm run build` here. The Rust-side routes and
config keep the `pom` name; the brand is UI-only.

`README.md` in this folder is the authoritative dev walkthrough (serving model,
dev proxy setup, routing on the axum side). Its deployment section mirrors the
current planner contract: one machine may own several valid ranges, while a
global gap or overlap blocks the plan. The whole-model mode starts empty when
chosen from slices, so the operator explicitly adds its machines.
Topology edits query the planner after the 260 ms debounce; runtime settings
remain local until the deploy preflight.

## Build tooling

- Vite 6 + `@vitejs/plugin-react`; React 18, TypeScript 5.7 (project references), Tailwind 3.4 via PostCSS (`postcss.config.js`: `tailwindcss` + `autoprefixer`).
- `vite.config.ts` - dev server on port 5199, `strictPort`, base `/admin-ui/`, and a proxy of every node-owned path (`/api`, `/v1`, `^/admin(?!-ui)`, `/peers`, `/health`, `/generate`) to `VITE_API_TARGET` (default `http://127.0.0.1:8080`; point it at a real node, e.g. `VITE_API_TARGET=http://<node-ip>:8090`). **`/admin-ui` is not proxied**, even though the node also serves it: it is this application's `base`, and proxying it made `npm run dev` return the node's already built bundle instead of the local code - the screen never changed no matter how much you edited it. And `/admin` must be a regular expression, because the Vite proxy matches its text key by string prefix and not by segment: `/admin-ui/src/App.tsx` starts with `/admin`. There is no sample backend - every screen reads a live node. `cacheDir` is pinned to `node_modules/.vite` because the default fallback silently loses config (no PostCSS, no proxy) when a branch switch restarts the dev server mid-rewrite. `build.outDir = "dist"`, no sourcemaps. The `noRegexLookbehind` plugin fails the build when a non-docs chunk carries a regex lookbehind: the macOS app's WebKit on macOS 11/12 cannot parse one, and the screen importing it dies. `package.json` `overrides` pins `mdast-util-gfm-autolink-literal` to 2.0.0 for that reason (2.0.1 added a lookbehind through `remark-gfm`). Production chunks split route screens, common vendors, the shared Markdown renderer, ICU messages and each generated reference document; `npm run build` verifies the resulting sizes.
- Tailwind colors are named after CSS variables declared in `src/index.css` (`tailwind.config.js` maps `ink/surface/raised/line/fg/muted/faint/iris/aqua/amber/rose/sky/lime`); plugin purchase colors are theme-specific custom tokens; fonts Space Grotesk (display), IBM Plex Sans (sans), JetBrains Mono (mono).
- Vitest 3 - `npm test` = `vitest run`. There is no vitest config file; test files that need a DOM opt in per file with the `// @vitest-environment jsdom` pragma (e.g. `routes/Chat.telemetry.test.tsx`, `i18n/provider.test.tsx`).
- tsconfig: `tsconfig.app.json` (strict, `noUnusedLocals/Parameters`, `@/* -> ./src/*` alias, excludes `*.test.*`) and `tsconfig.node.json` for the build config itself.
- `scripts/build-brand.mjs` - brings the product identity into the bundle. Runs before dev/build/test via the same npm pre-hooks: reads `brand.json` at the repository root (the file the Rust binary embeds with `include_str!`), copies a URL-rewritten manifest to `src/brand/brand.json` and the images it points at to `public/brand/`. Both are gitignored - build output, and a committed copy is a copy that drifts. Vite cannot import from outside its root, which is the whole reason this step exists. **Published names carry a content hash** (`pom-icon-512.f95e6ea3.png`): the node serves everything except `index.html` as `immutable, max-age=1y` on the premise that asset names are hashed, and files under `public/` are copied verbatim by Vite - a fixed name pinned the old logo in the browser for a year, which is exactly what happened the first time this ran. `vite.config.ts`'s `brandHtml` plugin reads the **generated** manifest for the same reason, and the folder is recreated each run so old hashes do not pile up in `dist/`.
- `scripts/build-docs.mjs` - embeds the repository's living documentation into the bundle. Runs before dev/build/test via the npm `predev`/`prebuild`/`pretest` hooks: copies the folder `AGENTS.md` files, the `docs/subsystems/*.md` READMEs and the root README into `src/docs/` (gitignored, never committed) plus a generated `manifest.json` (`{id, path, title, label, kind, scope, section, order}`). `label` is the short human name (title minus "Subsystem: "/"AGENTS.md - " and minus everything after the first ":"/" - "/"("), `kind` is `overview` for a folder's `AGENTS.md` and `doc` otherwise, `scope` the folder an overview covers - the screen must never parse paths or show a file name. The Documentation screen reads them through two build-time imports (`manifest.json` + `import.meta.glob`), so the docs travel inside the built JS and work on any machine that serves `dist/` - no node route, no disk reads at runtime. `src/vite-env.d.ts` references `vite/client`, which is what types `import.meta.glob` under `tsconfig.app.json`. The same script reads the guides written for people (`docs/guides/**` and `docs/human/**/HUMAN.md`, parsed by `scripts/guides-md.mjs`) into `src/docs/guides.json` and publishes their screenshots to `public/guide-images/` (gitignored) with a content hash in each name; see `src/guides/AGENTS.md`.

`scripts/build-document-reader.mjs` - publishes what the Chat loads at run time to read a PDF (the legacy pdf.js worker and its data, the OCR worker, the LSTM engines and the `por` and `eng` languages) to `public/document-reader/<versions>/` and the folder's name to `src/brand/document-reader.json`, both gitignored. They come from this node and not from a CDN, the versions in the folder keep the year-long `immutable` from pinning an old copy, and they add about 34 MB to `dist/` and to an `embed-ui` binary. Runs in the same npm pre-hooks, after `build-brand.mjs`.

`scripts/build-licenses.mjs` - generates the versioned catalog `src/lib/licenses.generated.ts` of everything a build ships, because the permissive licenses ask for their notice wherever the code is distributed: every `package-lock.json` entry that is not development-only, every crate a workspace member reaches through normal or build dependencies (any target, transitive ones included; POM's own unlicensed crates from `pieceofmindcloud` are POM itself), and the native components in `NATIVE` (llama.cpp/ggml, the NVIDIA CUDA runtime and container image). The same run writes `THIRD-PARTY-NOTICES.txt` at the repository root: that catalog with the full text of every license file the packages carry (identical texts once), POM's `LICENSE` and NVIDIA's terms; every release package and both Docker images ship it, and `src/lib/licenses.test.ts` fails when it and the catalog were not regenerated together. The texts come from `node_modules` and the Cargo registry, so run it after `npm ci` and a Cargo build (or `cargo fetch`). Run `npm run licenses` after changing `package.json`, `package-lock.json`, `Cargo.toml` or `Cargo.lock`, and version both generated files.

## package.json scripts

- `dev` - `vite --port 5199 --strictPort` (hot reload against a proxied node).
- `build` - `tsc -b && vite build` (type-check via project references, then bundle to `dist/`).
- `preview` - `vite preview --port 5199 --strictPort`.
- `test` - `vitest run`.
- `licenses` - `node scripts/build-licenses.mjs` (updates the dependency and license catalog used by About).
- `predev`/`prebuild`/`pretest` - `build-brand.mjs`, `build-docs.mjs` and `build-document-reader.mjs` (regenerate what the dev server, the build and the tests read but Git does not keep).
- Dependencies: `leaflet` (the map of the network location dialog, lazy chunk), `@tanstack/react-query`, `react-router-dom` (v6 with v7 future flags), `framer-motion`, `react-markdown` + `remark-gfm` + `rehype-highlight`, `@uiw/react-md-editor` + `@uiw/react-markdown-preview` + `rehype-sanitize` for trusted read-only Markdown surfaces, `highlight.js`, `lucide-react`, `recharts`, `clsx`, fontsource font packages.

## src root

- `index.html` - dark default (`data-theme="dark"`), two pre-paint scripts: theme from `localStorage` `codai.theme` (system preference deliberately does not decide) and `lang` from `codai.locale` / browser locales, so the first paint already matches the app state.
- `main.tsx` - provider stack: `QueryClientProvider` (defaults: `retry: 1`, `refetchOnWindowFocus: true`, `staleTime: 1000`) > `LocaleProvider` > `ProfileProvider` > `ThemeProvider` > `ToastProvider` > `BrowserRouter` (basename `/admin-ui`, opts into `v7_startTransition` + `v7_relativeSplatPath`) > `App`. Before rendering, an address outside the basename (the node answers any unknown GET with this page, e.g. `/setup`) is replaced by the same path under `/admin-ui`; without that the router renders nothing and the screen is blank.
- `index.css` - design tokens and global styles (see below).
- `App.tsx` - shell and route table (see below); it probes
  the public administrative exposure and gates the whole shell with the human
  access password only when needed. The real login is a browser-recognized form
  that asks only for the password, drawn like the pieceofmind.cloud home (dark
  floating panel, `src/assets/pom-mark-dark.png`, `.pom-login*` in `index.css`,
  entrance/glow/pulse animations off under reduced motion, a shake on a refused
  password, and the site's PT/EN flag pill switching the locale in place);
  the password stays in module memory and the server-side `HttpOnly` session is
  what survives a reload. This gate is only a UI flow: the server remains the
  authorization boundary.

  A machine on its first run gets the first run wizard
  (`components/FirstRunWizard.tsx`) in place of the shell, past the gate:
  one name for the machine and its network, a welcome animation, then the hints. The older first access wizard
  is a different thing, deprecated and unreachable: `routes/Setup.tsx`,
  `routes/SetupWizard.tsx` and `components/setup-wizard/*` are kept in the
  source but `App.tsx` neither imports nor routes them (see
  `docs/subsystems/web-ui.md`, "First access wizard"). On a new node the
  panel password is `admin`: while `/api/access` reports `default_password`,
  `useAdminSurface` enters with it instead of showing the gate, and the bell
  shows a notice asking to change it, once per start of the node
  (`lib/defaultPasswordNotice.ts`, keyed by the `boot_id` of `/api/access`).

### App.tsx route table

`App` first asks which shell is safe: `useAdminSurface()` probes `/api/ui/node` with `auth: false`; there is no setup answer, and a local-only refusal renders the server-equivalent bilingual no-navigation state (the probe never hits `/api/setup/key` because that route mints a connection key). Both Sidebar and Topbar sit in their own `ErrorBoundary` so a chrome crash cannot blank the menu.

After that comes the **profile guard**: `canAccessRoute(pathname, profile)` (`lib/access.ts`). A closed route renders `Forbidden` in place of the screen, and the guard sits in `<main>`, before `isChat`, because the chat renders outside `<Routes>` - a per-route guard would leave exactly it out. `isFullBleed` also depends on the guard: when blocked, the screen falls back to the center column. The menu reads the same table through `navForProfile`. Tests: `src/App.test.tsx`.

Screens, menu, routes and plugin texts come from the active plugin's
`pom-plugin-ui/v1` manifest (`lib/pluginUi.ts`, `components/PluginScreen.tsx`,
`lib/pluginHost.ts`, `lib/pluginI18n.ts`; see `docs/subsystems/plugins-ui.md`).
`pluginUi.ts` normalizes menu and route paths to `/<plugin_code>/...`;
`canAccessRoute`, `navForProfile` and `App` read that same registry. The basename
`/admin-ui` stays in the `BrowserRouter`, so a route `/example` of plugin `base`
opens at `/admin-ui/base/example`. Plugins may replace the native `/logs` and
`/statistics` slots by declaring those destinations; without a single
declaration, the `EnterpriseFeatureShowcase` (`src/components/`) remains as the
fallback. New menus enter at the root, in manifest order, right after the entries
the same plugin already holds (Quota after Statistics), else before
Documentation; an item with `children` becomes an expandable section with
submenus. `components/PluginBridge.tsx` connects the shell to the plugins through
the `pom-plugin-events/v1` protocol (`lib/pluginEvents.ts`); over the same
protocol a plugin drives POM's loader and installer over its own screen
(`lib/pluginLoader.ts`, `components/PluginLoader.tsx`, host version 7). POM shows and opens
the interfaces of all plugins in the `admin`, `worker` and `visitor` profiles; the
manifest's `roles` and `profiles` are validated but do not restrict this UI. The
native restrictions of the `visitor` profile still apply to the POM routes. The
Logs/Statistics code does not live here: it lives in `pom-plugin-logs`. The API
(`/api/ui/requests`, `/api/ui/requests/stats`) remains the authority and refuses
the data without an entitlement.

The `visitor` profile is a reduced view of the interface: conversations,
Network Chat only when the network opens it (`NetworkSettings.chat_visitors`,
read through `useNetworkChatOpen`; otherwise the entry is hidden and the Sidebar
and `PluginBridge` do not poll the snapshot), documentation, API key, Plugins (catalog, installation and local
development builds on this machine) and the local network screen (read-only: the
role is decided at admission, and the interface does not offer becoming `worker`
in its place). Plugins and the menus of installed plugins sit below the
conversations list.
It never sees deployment logs (denied in `lib/access.ts` even where a plugin
claims the route); request logs and statistics open only once an active plugin
serves them, with its own node's requests, and never as the advertising
fallback. Its preferences hide
the GPU thermal protection and the Hugging Face token. The cluster's connection
key shows only to `admin` (`/config/keys/connection` and the Sharing screen). The value
comes from `/api/ui/node`, it is a persisted property of the node, and it appears
in the toolbar. This does not replace backend authorization: the server also
limits the network answer and the promotions.
In that profile's Sidebar, the history and `New chat` are the main menu;
the sessions share state with the Chat, the list scrolls within the available
space, and the network always stays below it. Documentation and Settings sit in
the footer next to the flags, and "Leave network" (`LeaveNetworkButton`) sits
beside the theme switch. Administrator and node keep the screen navigation
menu.

On the Dashboard, the add-machine and undeploy controls appear only for
`profile: "admin"`; the API repeats the gate and localizes the refusal in
English. The button to remove one's own machine remains a separate action
available to the other profiles. The Details view - the per-machine cards, and
the same card a double click on the Flow used to open - is also the
administrator's (`canSeePeerDetails`, in `src/lib/access.ts`): outside it only
the Flow remains, and **the view selector is not drawn**, because one view is not
a choice.

- `/` -> Dashboard
- `/chat` -> Chat (rendered outside the `<Routes>`/transition because it owns a live stream and scroll position; unmounting to animate would drop tokens); `/network-chat` -> NetworkChat, its own page with the same visual pattern as the AI chat, with a full-bleed header aligned to the other screens and automatic following of the end of the history
- The Sidebar queries the Network Chat snapshot in the background to show
  unread messages; opening `/network-chat` marks the current snapshot as read.
  For an administrator, with the lobby on, a second, amber badge counts new
  lobby messages; opening the lobby tab marks them read (separate marker).
  On mobile, the list of online users stays closed until `See online` is
  triggered.
- `/admin-ui/chat` -> the compact Chat for the native macOS status bar panel; it keeps only the chat screen, with an `h-full min-h-0 overflow-hidden` layout, and the larger-window button appears only when the native WebKit provides the `pomOpenWindow` bridge.
- `/models` -> WholeModels; `/models/remote` -> RemoteModels (search,
  details and downloads persisted on the node); `/models/slice` -> SliceModels, tensor parallel
  shards only (development mode, off the menu, opened from a model's Slice
  button), with the slicing jobs and their progress bars
- `/shards` -> redirects to `/models`, `/shards/deploy` to `/deploy` (old
  links; the Shards screen was removed with pipeline slicing)
- `/models`, `/models/remote`, `/models/slice`,
  and `/transfers` are full-bleed, in the same
  pattern as `/deployments`/`/deployments/logs`: fixed header
  (`FullBleedHeader`, `components/ui.tsx`) and a table or content scrolling
  inside. Whole models show the `mmproj` file name when the
  model/round has vision (`DiscoveredModel.mmproj_path`/
  `ShardSet.mmproj_path`, already coming from the catalog - the screen only
  renders it). See `src/routes/AGENTS.md`.
- `/deploy` -> DeployModel (first-level entry for putting models in service,
  full-bleed Deploy Studio with central cluster map, property inspector and
  draggable peer strip; whole-model mode takes more machines while their
  ranges cover every layer, one machine only under an inference service)
- `/deployments` -> Deployments, full-bleed like Logs: the same
  `DeploymentInspector` from the deploy screen opens on the right, in read-only
  mode, with the parameters of the chosen version
- `/deployments/logs` -> DeploymentLogs, the terminal console of the cluster's
  process logs, in arrival order on the principal and filterable by `deploy_id`
- `/setup` is not a route: the deprecated wizard has no path.
- `/transfers` -> Transfers, with the send panel first (one menu entry);
  `/transfers/send` redirects there keeping `?filename=`
- Plugin routes -> `PluginScreen` under `/<plugin_code>/<manifest route>`,
  outside `<Routes>` and before the native table. The basename turns, for
  example, `base` + `/plugin-de-base` into `/admin-ui/base/plugin-de-base`.
  The component's identity includes the normalized route, so navigating between
  two screens of the same plugin remounts the correct screen. `/logs` and
  `/statistics` remain native fallback routes when no plugin claims them; the
  alias `/logs/statistics` still redirects to `/statistics`.
- `/documentation` -> Documentation (full-bleed, two tabs, each with its search: the guides for people (markdown under `docs/guides/` and `docs/human/`, rendered by `src/guides/`) and the repository's living markdown embedded by `scripts/build-docs.mjs`; see `src/routes/AGENTS.md` and `src/guides/AGENTS.md`). On mobile the sidebar takes the whole screen and the reader disappears until an explicit selection or a deep link; selecting opens the reader with a Back button at the top, which only closes the mobile view (the selection in `?doc=`/`?guide=` remains).
- `/config/network` -> NetworkSettings; the main My Network section also gathers
  `/config/network/participants`, `/config/administration` (Administrators),
  `/config/sharing` (TrustAnchor with machine, root and connection key)
  and `/config/keys/api`. The old route `/config/trust` remains as a compatible
  alias of the trust root screen.
- `/plugins` -> Plugins, lists every plugin in the catalog, active or not, with
  license-key activation that identifies the plugins granted by that key,
  per-plugin version updates and deactivation. With
  `pom_dev_mode=true`, on any profile, it also builds and installs plugins directly from mounted
  sources without requiring an active license; local builds grant no licensed
  features and do not change existing entitlement. Updating the `pom` binary
  is handled separately in `/config/about`. This replaced the former
  single-product `/enterprise` screen (`Enterprise.tsx`); see
  `src/routes/AGENTS.md` and `crates/node/src/plugins.rs`.
- `/config/preferences` -> Preferences with exposure, Hugging Face and thermal
  protection; `/config/storage` -> ModelPaths, which also configures the shared
  persistent root for projects and dynamic plugin data;
  `/config/preferences/admin`
  -> administrative preferences for TLS, QUIC, logs and network chat; and
  `/config/about` -> About, with the POM's explicit provisional license, the
  catalog grouped by license of the frontend's and the Rust's direct
  dependencies, and the check/apply of the POM update
  (`/api/ui/pom-update/*`, GitHub releases then the site, no license; the
  Plugins screen's "Check for updates" dialog uses the same routes), see
  `crates/node/src/pom_core_update.rs`. About is the last item of the Settings
  section.
- `*` -> NotFound

Non-chat pages render inside `AnimatePresence mode="wait"` with a `Page` cross-fade wrapper; chat and dashboard get overflow-hidden main, the rest scroll inside the app shell. The document itself is not scrollable, and nested panes use `overscroll-y-contain` so a wheel event at a pane's boundary cannot move an outer page.

### index.css design tokens

- One token table, two themes: light on bare `:root`, dark under both `@media (prefers-color-scheme)` coverage via `:root[data-theme="dark"]` (Tailwind's `darkMode` is `class`/`data-theme`). Tokens are space-separated RGB triplets consumed as `rgb(var(--ink) / <alpha-value>)`, so Tailwind alpha modifiers work.
- Neutrals: `--ink` (page), `--surface`, `--raised`, `--line`, `--line-strong`, `--fg`, `--muted`, `--faint`; accents: `--iris`, `--aqua`, `--amber`, `--rose`, `--sky`, `--lime`; `color-scheme` matches the theme.
- Component classes: `.panel`, `.eyebrow`, `.hairline`, and `.plugin-buy-button` (theme-specific high-contrast purchase action); `.break-path` (long ids/paths wrap or truncate, never widen); `indeterminate`/`ping` keyframes; thin scrollbars; `:focus-visible` iris outline; `prefers-reduced-motion` kills all animation; `.hljs` re-themed from the app tokens so code blocks match.

## src/ subfolders

- `src/lib/` - API client, react-query hooks, localStorage persistence, chat stream/state, shared types, the generated license catalog, image resizing by context budget, and the contract of TTFT, prefill, generation and total metrics. See `src/lib/AGENTS.md`.
- `src/i18n/` - en + pt-BR message table and provider, including telemetry and initial-model copy. See `src/i18n/AGENTS.md`.
- `src/routes/` - one screen per route, including the chat telemetry tables, the post-execution directional RTT snapshot, per-machine capacity/configuration cards, diagnosis card, AI analysis action, initial-model mark and `RemoteModels`. See `src/routes/AGENTS.md`.
- `src/components/` - chrome (Sidebar, Topbar, nav, Notifications, TlsIndicator, ChatSessionList) and shared widgets (ui primitives, Markdown, Confirm, PipelineRail, ModelNetworkCanvas, PeerDetailCard, ErrorBoundary, Logo, MachineIcon, ReleaseSlice, RemoveNode, LeaveNode, UndeployPipeline, AddNodeDialog, ApiKeysTable, DeploymentLogDock, OpenNativeChatWindowButton). `nav.ts` carries both `nav.chat` (`/chat`) and `nav.networkChat` (`/network-chat`), and that is how the two Chat surfaces are reached; `NetworkChat.tsx` uses `Tabs` (`components/ui.tsx`) to switch between the network room and the lobby room. `DeploymentInspector` is the single editor of the deployment's settings, in the Deploy Studio and in the History's redeploy panel, including the f32/f16/f8/i4 hidden-state formats (f16 default).
- `src/brand/` + `public/brand/` - **generated** by `scripts/build-brand.mjs` from the root `brand.json`; read through `src/lib/brand.ts` (`brand`, `storageKey`). Never edit or commit.
- `src/guides/` - the human half of the documentation: the loader of the markdown guides, their renderer and the SVG diagrams. See `src/guides/AGENTS.md`.
- `src/test/` - `fixtures.ts`: fake peers/nodes/clusters with the *full* shape the node serves, for tests only. A partial `node` (e.g. one without `status`) passes one component's `?.` and crashes the next, so tests build from these fixtures and override only the field under test.

`Chat.tsx` keeps only the AI conversation; the network Chat is reached from the
Sidebar's nav entry, not from inside Chat. `NetworkChat.tsx` is the
`/network-chat` page: it queries the authenticated snapshot, sends text, files
and typing actions with the node's API key, resolves each `node_id` through the
name map the panel uses, and keeps the history at the end while the person has
not scrolled up. `Preferences.tsx` exposes the network chat policy only to the
administrator. `PublicNetworks.tsx` derives the country and city combos from the
received public metadata, keeping the city limited to the selected country.
Every published address also gets a browser-side `/health` check, with a short
timeout, a 30 second cache, and a new attempt only on an explicit refresh; the
green/red result represents observed reachability, not a change to the address
pkarr published.

The public catalog now comes from federated tickets configured on first access.
The wizard saves comma-separated HTTP(S) URLs in `publicDirectory`; they are
metadata output only and do not open the private network to public peers. A
public join replaces the current network, because the runtime keeps a single P2P
session per process.

## Links

- Backend endpoints hit by the UI: `/api/ui/*` (node, cluster, models, cluster/models, deployments, paths, thermal, jobs, requests, requests/stats, chat/telemetry, cluster-logs, plugins list/activate/refresh/deactivate/update, pom-update status/check/apply, network/info, network/geo/*, network/logs, models/delete, deploy/node, deploy/node/remove, undeploy, leave, unload, release, dev/update), `/api/transfer/*` (jobs, offers), `/admin/*` (nodes, api-keys), `/v1/*` (chat/completions, completions, models). All defined in the Rust crate under `crates/node/src/backend/` (`webui/`, `transfer.rs`, `admin.rs`, `reqlog.rs`, `openai.rs`, `devupdate.rs`). A request audit response contains prompt content and is protected by the connection key. The process-log console is distinct from `/logs` and uses the principal's persisted arrival order.
- Auth: `api.ts` sends same-origin credentials so the gateway can use its
  `HttpOnly` admin session. Administrative requests use the cluster connection
  key, while `/v1/*` chat requests use the dedicated `pom-[node_id]` API key
  created by `/api/ui/chat-key`; a rejected key is replaced once before
  retrying. The human access password is never persisted by the browser, and
  the node stores only its Argon2id hash.
- The node serves `dist/` from disk per request; `POM_UI_DIST` overrides the directory (used by the Docker image).

## Open items

Test fixtures: a hand-written partial peer can still crash a component (see
`src/components/AGENTS.md`).

## Defects noted

- A non-text, non-PDF attachment (a `.docx`, for example) is still read with
  `file.text()` in `routes/Chat.tsx`, so the model receives binary garbage instead
  of a refusal.
- Two hard-coded Portuguese words show in the English interface: the
  Simple/Complete strategy switch of the Deploy screen (`routes/DeployModel.tsx`,
  `"Simples"`/`"Completa"`, development mode only) and the `resposta` step of the
  deploy map's route legend (`components/DeployNetworkCanvas.tsx`). Both belong
  in `i18n/messages.csv`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.