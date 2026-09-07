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
chosen from slices, so the operator explicitly adds its single machine.
Topology edits query the planner after the 260 ms debounce; runtime settings
remain local until the deploy preflight.

## Build tooling

- Vite 6 + `@vitejs/plugin-react`; React 18, TypeScript 5.7 (project references), Tailwind 3.4 via PostCSS (`postcss.config.js`: `tailwindcss` + `autoprefixer`).
- `vite.config.ts` - dev server on port 5199, `strictPort`, base `/admin-ui/`, and a proxy of every node-owned path (`/api`, `/v1`, `^/admin(?!-ui)`, `/peers`, `/health`, `/generate`) to `VITE_API_TARGET` (default `http://127.0.0.1:8080`; point it at a real node, e.g. `VITE_API_TARGET=http://192.168.100.138:8090`). **`/admin-ui` não é encaminhado**, ainda que o nó também o sirva: é o `base` desta aplicação, e encaminhá-lo fazia o `npm run dev` devolver o bundle já construído do nó em vez do código local - a tela não mudava por mais que se editasse. E `/admin` precisa ser expressão regular, porque a chave de texto do proxy do Vite casa por prefixo de string e não por segmento: `/admin-ui/src/App.tsx` começa com `/admin`. There is no sample backend - every screen reads a live node. `cacheDir` is pinned to `node_modules/.vite` because the default fallback silently loses config (no PostCSS, no proxy) when a branch switch restarts the dev server mid-rewrite. `build.outDir = "dist"`, no sourcemaps. Production chunks split the route screens, common vendors and each generated reference document; `npm run build` verifies the resulting sizes.
- Tailwind colors are named after CSS variables declared in `src/index.css` (`tailwind.config.js` maps `ink/surface/raised/line/fg/muted/faint/iris/aqua/amber/rose/sky/lime`); fonts Space Grotesk (display), IBM Plex Sans (sans), JetBrains Mono (mono).
- Vitest 3 - `npm test` = `vitest run`. There is no vitest config file; test files that need a DOM opt in per file with the `// @vitest-environment jsdom` pragma (e.g. `routes/Chat.telemetry.test.tsx`, `i18n/provider.test.tsx`).
- tsconfig: `tsconfig.app.json` (strict, `noUnusedLocals/Parameters`, `@/* -> ./src/*` alias, excludes `*.test.*`) and `tsconfig.node.json` for the build config itself.
- `scripts/build-brand.mjs` - brings the product identity into the bundle. Runs before dev/build/test via the same npm pre-hooks: reads `brand.json` at the repository root (the file the Rust binary embeds with `include_str!`), copies a URL-rewritten manifest to `src/brand/brand.json` and the images it points at to `public/brand/`. Both are gitignored - build output, and a committed copy is a copy that drifts. Vite cannot import from outside its root, which is the whole reason this step exists. **Published names carry a content hash** (`pom-icon-512.f95e6ea3.png`): the node serves everything except `index.html` as `immutable, max-age=1y` on the premise that asset names are hashed, and files under `public/` are copied verbatim by Vite - a fixed name pinned the old logo in the browser for a year, which is exactly what happened the first time this ran. `vite.config.ts`'s `brandHtml` plugin reads the **generated** manifest for the same reason, and the folder is recreated each run so old hashes do not pile up in `dist/`.
- `scripts/build-docs.mjs` - embeds the repository's living documentation into the bundle. Runs before dev/build/test via the npm `predev`/`prebuild`/`pretest` hooks: copies the folder `AGENTS.md` files, the `docs/subsystems/*.md` READMEs and the root README into `src/docs/` (gitignored, never committed) plus a generated `manifest.json` (`{id, path, title, label, kind, scope, section, order}`). `label` is the short human name (title minus "Subsistema: "/"AGENTS.md - " and minus everything after the first ":"/" - "/"("), `kind` is `overview` for a folder's `AGENTS.md` and `doc` otherwise, `scope` the folder an overview covers - the screen must never parse paths or show a file name. The Documentation screen reads them through two build-time imports (`manifest.json` + `import.meta.glob`), so the docs travel inside the built JS and work on any machine that serves `dist/` - no node route, no disk reads at runtime. `src/vite-env.d.ts` references `vite/client`, which is what types `import.meta.glob` under `tsconfig.app.json`.

## package.json scripts

- `dev` - `vite --port 5199 --strictPort` (hot reload against a proxied node).
- `build` - `tsc -b && vite build` (type-check via project references, then bundle to `dist/`).
- `preview` - `vite preview --port 5199 --strictPort`.
- `test` - `vitest run`.
- `predev`/`prebuild`/`pretest` - `node scripts/build-docs.mjs` (regenerate `src/docs/` so the embedded documentation is fresh for the dev server, the build and the tests).
- Dependencies: `@tanstack/react-query`, `react-router-dom` (v6 with v7 future flags), `framer-motion`, `react-markdown` + `remark-gfm` + `rehype-highlight`, `@uiw/react-md-editor` + `@uiw/react-markdown-preview` + `rehype-sanitize` for trusted read-only Markdown surfaces, `highlight.js`, `lucide-react`, `recharts`, `clsx`, fontsource font packages.

## src root

- `index.html` - dark default (`data-theme="dark"`), two pre-paint scripts: theme from `localStorage` `codai.theme` (system preference deliberately does not decide) and `lang` from `codai.locale` / browser locales, so the first paint already matches the app state.
- `main.tsx` - provider stack: `QueryClientProvider` (defaults: `retry: 1`, `refetchOnWindowFocus: true`, `staleTime: 1000`) > `LocaleProvider` > `ProfileProvider` > `ThemeProvider` > `ToastProvider` > `BrowserRouter` (basename `/admin-ui`, opts into `v7_startTransition` + `v7_relativeSplatPath`) > `App`.
- `index.css` - design tokens and global styles (see below).
- `App.tsx` - shell and route table (see below); after setup detection it probes
  the public administrative exposure and gates the whole shell with the human
  access token only when needed. The real login is a browser-recognized form;
  the token stays in module memory and the server-side `HttpOnly` session is
  what survives a reload. This gate is only a UI flow: the server remains the
  authorization boundary.

### App.tsx route table

`App` first decides setup vs dashboard: `useAdminSurface()` probes `/api/ui/node` with `auth: false`; a 404 means the node runs only the `/api/setup/*` router and `Setup` renders instead, while a local-only refusal renders the server-equivalent bilingual no-navigation state (the probe never hits `/api/setup/key` because that route mints a connection key). Both Sidebar and Topbar sit in their own `ErrorBoundary` so a chrome crash cannot blank the menu.

Depois disso vem o **guarda de perfil**: `canAccessRoute(pathname, profile)` (`lib/access.ts`). Rota fechada renderiza `Forbidden` no lugar da tela, e o guarda fica no `<main>`, antes do `isChat`, porque o chat renderiza fora do `<Routes>` - um guarda por rota deixaria justamente ele de fora. `isFullBleed` também depende do guarda: barrada, a tela volta para a coluna central. O menu lê a mesma tabela via `navForProfile`. Testes: `src/App.test.tsx`.

O terceiro perfil, `inference`, é uma vista reduzida da interface: só conversa,
documentação e API keys do próprio nó. Isso não é autorização e não altera a chave de conexão
aceita pelo backend; a fronteira real ficou para `rdma-perfis-escopo-backend`.

- `/` -> Dashboard
- `/chat` -> Chat (rendered outside the `<Routes>`/transition because it owns a live stream and scroll position; unmounting to animate would drop tokens)
- `/admin-ui/chat` -> Chat compacto para o painel nativo da barra de status do macOS; mantém somente a tela de chat, com layout `h-full min-h-0 overflow-hidden`, e o botão de janela maior só aparece quando o WebKit nativo fornece a ponte `pomOpenWindow`.
- `/modelos` -> WholeModels; `/modelos/remotos` -> RemoteModels (busca,
  detalhes e downloads persistidos no nó); `/modelos/fatiar` -> SliceModels, que também
  mostra os jobs de fatiamento e suas barras de progresso
- `/fatias` -> Shards; `/fatias/implantar` -> redirects to `/implantar` (kept for old links)
- `/implantar` -> DeployModel (first-level entry for putting models in service,
  full-bleed Deploy Studio with central cluster map, property inspector and
  draggable peer strip; whole-model mode accepts exactly one map machine)
- `/implantacoes` -> Deployments, full-bleed como a de Logs: o mesmo
  `DeploymentInspector` da tela de implantar abre à direita, em somente
  leitura, com os parâmetros da versão escolhida
- `/implantacoes/logs` -> DeploymentLogs, consola terminal dos logs de processo
  do cluster, em ordem de chegada no principal e filtrável por `deploy_id`
- `/setup` -> SetupWizard, **fora da moldura**: sem Sidebar nem Topbar, como o
  `Setup` do primeiro arranque (`isWizard` no `App.tsx` devolve antes do shell)
- `/transferencias` -> Transfers; `/transferencias/enviar` -> SendFile
- `/logs` -> Logs, tela full-bleed da auditoria SQLite com barra compacta de
  filtros, tabela densa, scroll infinito e inspetor com Markdown renderizado,
  expansão para leitura e redação de credenciais; `/estatisticas` -> Statistics,
  tela separada dos agregados da auditoria. `/logs/estatisticas` redireciona
  para a rota nova.
- `/documentacao` -> Documentation (full-bleed, two tabs: human guides from `src/guides/` and the repository's living markdown embedded by `scripts/build-docs.mjs`; see `src/routes/AGENTS.md` and `src/guides/AGENTS.md`)
- `/config/rede` -> NetworkSettings; `/config/preferencias` -> Preferences,
  including administrative exposure and gateway port plus the moved TLS choice;
  `/config/keys/api` -> TokenKeys; `/config/keys/conexao` -> ConnectionKeys;
  `/config/pastas` -> ModelPaths
- `*` -> NotFound

Non-chat pages render inside `AnimatePresence mode="wait"` with a `Page` cross-fade wrapper; chat and dashboard get overflow-hidden main, the rest scroll.

### index.css design tokens

- One token table, two themes: light on bare `:root`, dark under both `@media (prefers-color-scheme)` coverage via `:root[data-theme="dark"]` (Tailwind's `darkMode` is `class`/`data-theme`). Tokens are space-separated RGB triplets consumed as `rgb(var(--ink) / <alpha-value>)`, so Tailwind alpha modifiers work.
- Neutrals: `--ink` (page), `--surface`, `--raised`, `--line`, `--line-strong`, `--fg`, `--muted`, `--faint`; accents: `--iris`, `--aqua`, `--amber`, `--rose`, `--sky`, `--lime`; `color-scheme` matches the theme.
- Component classes: `.panel`, `.eyebrow`, `.hairline`; `.break-path` (long ids/paths wrap or truncate, never widen); `indeterminate`/`ping` keyframes; thin scrollbars; `:focus-visible` iris outline; `prefers-reduced-motion` kills all animation; `.hljs` re-themed from the app tokens so code blocks match.

## src/ subfolders

- `src/lib/` - API client, react-query hooks, localStorage persistence, chat stream/state, tipos compartilhados e o contrato de métricas de TTFT, prefill, geração e total. See `src/lib/AGENTS.md`.
- `src/i18n/` - en + pt-BR message table and provider, including telemetry and initial-model copy. See `src/i18n/AGENTS.md`.
- `src/routes/` - one screen per route, including the chat telemetry tables, the post-execution directional RTT snapshot, per-machine capacity/configuration cards, diagnosis card, AI analysis action, initial-model mark and `RemoteModels`. See `src/routes/AGENTS.md`.
- `src/components/` - chrome (Sidebar, Topbar, nav, Notifications, TlsIndicator) and shared widgets (ui primitives, Markdown, Confirm, PipelineRail, ModelNetworkCanvas, PeerDetailCard, ErrorBoundary, Logo, MachineIcon, ReleaseSlice, RemoveNode, LeaveNode, UndeployPipeline, AddNodeDialog). `RuntimeParamsForm` é o editor compartilhado dos ajustes da implantação, incluindo a reserva de segurança de VRAM por dispositivo e o detector configurável de ciclos de geração.
- `src/brand/` + `public/brand/` - **generated** by `scripts/build-brand.mjs` from the root `brand.json`; read through `src/lib/brand.ts` (`brand`, `storageKey`). Never edit or commit.
- `src/guides/` - the human half of the documentation: guide content per locale, its renderer and the SVG diagrams. See `src/guides/AGENTS.md`.
- `src/test/` - `fixtures.ts`: fake peers/nodes/clusters with the *full* shape the node serves, for tests only. A partial `node` (e.g. one without `status`) passes one component's `?.` and crashes the next, so tests build from these fixtures and override only the field under test.

## Links

- Backend endpoints hit by the UI: `/api/ui/*` (node, cluster, models, cluster/models, paths, thermal, jobs, requests, requests/stats, chat/telemetry, cluster-logs, network/logs, models/delete, deploy/node, deploy/node/remove, undeploy, leave, unload, release, dev/update), `/api/transfer/*` (jobs, offers), `/admin/*` (nodes, api-keys), `/v1/*` (chat/completions, completions, models). All defined in the Rust crate under `crates/node/src/backend/` (`webui.rs`, `transfer.rs`, `admin.rs`, `reqlog.rs`, `openai.rs`, `devupdate.rs`). A request audit response contains prompt content and is protected by the connection key. The process-log console is distinct from `/logs` and uses the principal's persisted arrival order.
- Auth: `api.ts` sends same-origin credentials so the gateway can use its
  `HttpOnly` admin session. Administrative requests use the cluster connection
  key, while `/v1/*` chat requests use the dedicated `pom-[node_id]` API key
  created by `/api/ui/chat-key`; a rejected key is replaced once before
  retrying. The human access token is never persisted by the browser.
- The node serves `dist/` from disk per request; `POM_UI_DIST` overrides the directory (used by the Docker image).

## Pendências

As pendências de produto e de fixtures desta árvore estão consolidadas em
`TODO.md`, itens 22 e 23.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
