# apps/frontend

The rdma-p2p web UI ("CODAI-SERVER"), a React 18 + TypeScript SPA. This is the
product's interface: the node reads the production build from
`apps/frontend/dist` off disk at runtime (`crates/node/src/backend/webapp.rs`,
`config::ui_dist_dir()`, override `RDMA_P2P_UI_DIST`) and serves it on the
gateway port together with the API. `cargo build` never touches this directory;
editing the UI only requires `npm run build` here. The Rust-side routes and
config keep the `rdma-p2p` name; the brand is UI-only.

`README.md` in this folder is the authoritative dev walkthrough (serving model,
dev proxy setup, routing on the axum side).

## Build tooling

- Vite 6 + `@vitejs/plugin-react`; React 18, TypeScript 5.7 (project references), Tailwind 3.4 via PostCSS (`postcss.config.js`: `tailwindcss` + `autoprefixer`).
- `vite.config.ts` - dev server on port 5199, `strictPort`, and a proxy of every node-owned path (`/api`, `/v1`, `/admin`, `/peers`, `/health`, `/generate`) to `VITE_API_TARGET` (default `http://127.0.0.1:8080`; point it at a real node, e.g. `VITE_API_TARGET=http://192.168.100.138:8090`). There is no sample backend - every screen reads a live node. `cacheDir` is pinned to `node_modules/.vite` because the default fallback silently loses config (no PostCSS, no proxy) when a branch switch restarts the dev server mid-rewrite. `build.outDir = "dist"`, no sourcemaps.
- Tailwind colors are named after CSS variables declared in `src/index.css` (`tailwind.config.js` maps `ink/surface/raised/line/fg/muted/faint/iris/aqua/amber/rose/sky/lime`); fonts Space Grotesk (display), IBM Plex Sans (sans), JetBrains Mono (mono).
- Vitest 3 - `npm test` = `vitest run`. There is no vitest config file; test files that need a DOM opt in per file with the `// @vitest-environment jsdom` pragma (e.g. `routes/Chat.telemetry.test.tsx`, `i18n/provider.test.tsx`).
- tsconfig: `tsconfig.app.json` (strict, `noUnusedLocals/Parameters`, `@/* -> ./src/*` alias, excludes `*.test.*`) and `tsconfig.node.json` for the build config itself.
- `scripts/build-docs.mjs` - embeds the repository's living documentation into the bundle. Runs before dev/build/test via the npm `predev`/`prebuild`/`pretest` hooks: copies the folder `AGENTS.md` files, the `docs/subsystems/*.md` READMEs and the root README into `src/docs/` (gitignored, never committed) plus a generated `manifest.json` (`{id, path, title, section, order}`). The Documentation screen reads them through two build-time imports (`manifest.json` + `import.meta.glob`), so the docs travel inside the built JS and work on any machine that serves `dist/` - no node route, no disk reads at runtime. `src/vite-env.d.ts` references `vite/client`, which is what types `import.meta.glob` under `tsconfig.app.json`.

## package.json scripts

- `dev` - `vite --port 5199 --strictPort` (hot reload against a proxied node).
- `build` - `tsc -b && vite build` (type-check via project references, then bundle to `dist/`).
- `preview` - `vite preview --port 5199 --strictPort`.
- `test` - `vitest run`.
- `predev`/`prebuild`/`pretest` - `node scripts/build-docs.mjs` (regenerate `src/docs/` so the embedded documentation is fresh for the dev server, the build and the tests).
- Dependencies: `@tanstack/react-query`, `react-router-dom` (v6 with v7 future flags), `framer-motion`, `react-markdown` + `remark-gfm` + `rehype-highlight`, `highlight.js`, `lucide-react`, `recharts`, `clsx`, fontsource font packages.

## src root

- `index.html` - dark default (`data-theme="dark"`), two pre-paint scripts: theme from `localStorage` `codai.theme` (system preference deliberately does not decide) and `lang` from `codai.locale` / browser locales, so the first paint already matches the app state.
- `main.tsx` - provider stack: `QueryClientProvider` (defaults: `retry: 1`, `refetchOnWindowFocus: true`, `staleTime: 1000`) > `LocaleProvider` > `ThemeProvider` > `ToastProvider` > `BrowserRouter` (opts into `v7_startTransition` + `v7_relativeSplatPath`) > `App`.
- `index.css` - design tokens and global styles (see below).
- `App.tsx` - shell and route table (see below).

### App.tsx route table

`App` first decides setup vs dashboard: `useIsSetupMode()` probes `/api/ui/node` with `auth: false`; a 404 means the node runs only the `/api/setup/*` router and `Setup` renders instead (the probe never hits `/api/setup/key` because that route mints a connection key). Both Sidebar and Topbar sit in their own `ErrorBoundary` so a chrome crash cannot blank the menu.

- `/` -> Dashboard
- `/chat` -> Chat (rendered outside the `<Routes>`/transition because it owns a live stream and scroll position; unmounting to animate would drop tokens)
- `/modelos` -> WholeModels; `/modelos/fatiar` -> SliceModels
- `/fatias` -> Shards; `/fatias/andamento` -> SlicingJobs; `/fatias/implantar` -> redirects to `/implantar` (kept for old links)
- `/implantar` -> DeployModel (first-level entry for putting slices in service)
- `/transferencias` -> Transfers; `/transferencias/enviar` -> SendFile
- `/logs` -> LogsByToken; `/logs/ao-vivo` -> LogsLive
- `/documentacao` -> Documentation (the repository's living documentation, embedded in the bundle by `scripts/build-docs.mjs`; see `src/routes/AGENTS.md`)
- `/config/rede` -> NetworkSettings; `/config/keys/api` -> TokenKeys; `/config/keys/conexao` -> ConnectionKeys; `/config/pastas` -> ModelPaths
- `*` -> NotFound

Non-chat pages render inside `AnimatePresence mode="wait"` with a `Page` cross-fade wrapper; chat and dashboard get overflow-hidden main, the rest scroll.

### index.css design tokens

- One token table, two themes: light on bare `:root`, dark under both `@media (prefers-color-scheme)` coverage via `:root[data-theme="dark"]` (Tailwind's `darkMode` is `class`/`data-theme`). Tokens are space-separated RGB triplets consumed as `rgb(var(--ink) / <alpha-value>)`, so Tailwind alpha modifiers work.
- Neutrals: `--ink` (page), `--surface`, `--raised`, `--line`, `--line-strong`, `--fg`, `--muted`, `--faint`; accents: `--iris`, `--aqua`, `--amber`, `--rose`, `--sky`, `--lime`; `color-scheme` matches the theme.
- Component classes: `.panel`, `.eyebrow`, `.hairline`; `.break-path` (long ids/paths wrap or truncate, never widen); `indeterminate`/`ping` keyframes; thin scrollbars; `:focus-visible` iris outline; `prefers-reduced-motion` kills all animation; `.hljs` re-themed from the app tokens so code blocks match.

## src/ subfolders

- `src/lib/` - API client, react-query hooks, localStorage persistence, chat stream/state, formatting, toasts, all shared types. See `src/lib/AGENTS.md`.
- `src/i18n/` - en + pt-BR message table and provider. See `src/i18n/AGENTS.md`.
- `src/routes/` - one screen per route (Chat, Dashboard, DeployModel, Shards, SliceModels, WholeModels, Transfers, SendFile, SlicingJobs, LogsByToken, LogsLive, NetworkSettings, Keys, ModelPaths, Documentation, Setup, NotFound).
- `src/components/` - chrome (Sidebar, Topbar, nav, Notifications) and shared widgets (ui primitives, Markdown, Confirm, PipelineRail, ModelNetworkCanvas, PeerDetailCard, ErrorBoundary, Logo, MachineIcon, ReleaseSlice, RemoveNode, LeaveNode, UndeployPipeline, AddNodeDialog, ModelNetworkDemo).
- `src/test/` - `fixtures.ts`: fake peers/nodes/clusters with the *full* shape the node serves, for tests only. A partial `node` (e.g. one without `status`) passes one component's `?.` and crashes the next, so tests build from these fixtures and override only the field under test.

## Links

- Backend endpoints hit by the UI: `/api/ui/*` (node, cluster, models, cluster/models, paths, jobs, requests, models/delete, deploy/node, deploy/node/remove, undeploy, leave, unload, release, dev/update), `/api/transfer/*` (jobs, offers), `/admin/*` (nodes, api-keys), `/v1/*` (chat/completions, completions, models). All defined in the Rust crate under `crates/node/src/backend/` (`webui.rs`, `transfer.rs`, `admin.rs`, `openai.rs`, `devupdate.rs`).
- Auth: `api.ts` signs requests with the cluster connection key (picked up unauthenticated from `/api/ui/node`), matching `ApiKeyAuth` in the Rust `auth.rs`.
- The node serves `dist/` from disk per request; `RDMA_P2P_UI_DIST` overrides the directory (used by the Docker image).

## Defeitos anotados

- No root-level defects found in this pass. Known issues live in the subfolders: see `## Defeitos anotados` in `src/lib/AGENTS.md` (hardcoded pt-BR strings in `deleteModel.ts`/`toast.tsx`, unguarded `localStorage` writes in `networkLayout.ts`, dead `TransferState` export).
