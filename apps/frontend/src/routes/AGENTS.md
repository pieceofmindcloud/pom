# routes/

Each screen of the panel, one file per route, wired up in `../App.tsx`. Every
screen is backed by React Query hooks from `../lib/queries.ts` and speaks to the
node's HTTP gateway (`src/webui.rs`, `src/gateway.rs`); none of them keep
cluster state in the browser. All copy goes through the i18n provider
(`../i18n/provider.tsx`); several screens additionally pass server-side error
strings through `localizeServerText` (`../i18n/locales.ts`), which the backend
writes as translation keys.

Per-file list:

- `Chat.tsx` — The test chat: `POST /v1/chat/completions` with `stream: true`,
  exactly what a harness gets. Imports `../lib/chatStore` (sessions, localStorage),
  `../lib/chatStream` (SSE delta + metrics parsing), `../lib/chatScroll`
  (`isNearBottom`), `../lib/api` (`streamSse`, `ApiError`), `../lib/queries`
  (`useCluster`, `useNode`, `useServableModels`), `../components/Markdown`,
  `../components/ui`, `../lib/toast`. The `/v1` call is signed with the
  connection key by `api`.
- `Documentation.tsx` - Two audiences, one screen, switched by a tab pair at
  the top of the navigation column (`?tab=guias|referencia`).
  **Guides** (`../guides/`) are written for people: ordered pages with SVG
  diagrams, rendered by `GuideView` from `Block[]` data. **Reference** is the
  repository's living markdown, written for agents, self-contained and
  build-time embedded: `scripts/build-docs.mjs` copies the folder `AGENTS.md`
  files, the `docs/subsystems/*.md` READMEs and the root README into `../docs/`
  (gitignored) plus `manifest.json`, and this screen reads them via
  `import.meta.glob` + the manifest - no node route, so it renders on any
  machine serving `dist/`, even one that never had the repo.
  Three decisions that are not cosmetic:
  - **The screen is full-bleed** (`App.tsx` lists `/documentacao` alongside `/`
    in `isFullBleed`) and owns its scrolling: the nav column and the reader
    scroll independently, each a flex child with `min-h-0`. Without that
    `min-h-0` the child grows to its content and the overflow never engages -
    that is exactly how the search results used to run off the screen with no
    scrollbar.
  - **No file names in the navigation.** The manifest carries `label` (short
    human name) and `kind` (`overview` for a folder's `AGENTS.md`, `doc`
    otherwise). A folder's `AGENTS.md` is that section's own page, so the
    section *header* opens it - clicking "docs/" shows `docs/AGENTS.md` - and it
    is never listed as a leaf. `nameOf` never falls back to a file name;
    `withoutLeadingH1` drops the document's own `# ` title because the reader
    header already states the subject.
  - **Collapsing is honest.** The chevron toggles, the label opens the
    overview, and holding the open document no longer forces a section back
    open (`collapsed` state, explicit `true` wins).
  Search filters label, path *and* body, and replaces the tree with a flat
  result list whose hint is the section name, not the file. Test file
  `Documentation.test.tsx` covers the contract: guides first (a person must not
  land on agent markdown), the guide->reference deep link, no `.md` or
  `AGENTS.md` in the list, section header opens the overview, collapse works
  while holding the selection, the two scroll containers, `?doc=`/`?guia=` deep
  links, search + clear, empty state. Loading the embedded set requires
  `src/docs/` to exist, which `npm test`'s `pretest` hook guarantees.
- `Dashboard.tsx` — Cluster overview: coverage/machines/model/in-flight stats,
  a graph view ("minima", `ModelNetworkCanvas`) or a details grid ("estendida").
  The details grid lists the **whole network**, not just the pipeline:
  `networkPeers` (`../lib/networkPeers`) splits `c.peers` into the active
  deployment first (in layer order, `dashboard.groupDeployed*`) and the rest of
  the network after (sorted by hostname, `dashboard.groupOutside*`) - a machine
  connected without a slice is exactly the one that disappears when only the
  pipeline is looked at. The cards come from `PeerDetailCard`
  (`../components/PeerDetailCard`), the same component the Flow canvas opens on
  double-click; `GroupHeading` renders the two group labels and `otherRangesFor`
  feeds `RemoveNode` the ranges of the *other* pipeline peers. Empty state is
  `c.peers.length === 0` (was `peers.length`) with `dashboard.noMachines`
  ("Nenhuma máquina na rede"). Fetches `POST /api/ui/deploy/plan` to feed the
  "add a machine" dialog. Uses
  `../components/{LayerRibbon,PipelineRail,PeerDetailCard,UndeployPipeline,AddNodeDialog,ModelNetworkCanvas,ui}`,
  `../lib/api`, `../lib/networkPeers`, `../lib/queries`, `../lib/types`
  (`PeerView`, `DeployPlan`). The per-machine actions that used to live here
  moved to `PeerDetailCard`: update-and-restart (`UpdateNode`, dev-only when the
  peer advertises `dev_update`), `RemoveNode` (out of the model, only when
  serving a slice) and `LeaveNode` (out of the whole network, always). Next to
  "Adicionar nó" there is a `UndeployPipeline` button when a model is live -
  `undeployTargets` is the list the confirmation shows (peers on `group` with
  `total_layers > 0`, sorted by start layer); the node re-scans the cluster
  before acting, so the list is just the preview.
- `DeployModel.tsx` — "Implantar na rede". Three shapes, chosen by the
  `KindPicker` at the top and carried to the node as `kind`: **a set of shards**
  (below), **a whole model** (one model picker from `../lib/wholeModels`, one
  machine picker, no coverage ribbon/"included"/merge - a pipeline of one has a
  single answer), or **tensor parallel** (`tensor_parallel`): one model picker
  plus a `primary` select and a `workers` multi-select over `wholeModel.holders`;
  `size` is derived (`workers.length + 1`) via `tpPayload`, never a typed number,
  and the node derives the reduce URL from the primary's inference port (the
  operator no longer types `rank0_url`). `tp` is in the plan queryKey so changing
  primary/workers re-runs the node's shardability check. A plan with no `kind`
  is read as `"shards"` (`kindOf`), so an older peer mid-upgrade does not
  silence the screen. Deploy memory is namespaced `whole:<name>`, because a
  shard set and a model routinely share a name. Tests:
  `DeployModel.whole.test.tsx`, `../lib/wholeModels.test.ts`.
- `DeployModel.tsx` — the slice shape: picks a slice set (`group`), edits
  which ranges go to which machine (swaps on collision, merge toggle for
  adjacent ranges on one machine), reorders by swapping machines, tunes the
  twelve cluster-wide runtime knobs plus per-machine `n_gpu_layers`/`offload_kqv`.
  `fit_memory` is on by default for shards and whole-model deployment, and the
  screen disables it for tensor parallelism because llama.cpp cannot auto-fit
  `split-mode tensor`.
  Plan edits are debounced 260 ms and validated by `POST /api/ui/deploy/plan`;
  the deploy itself re-surveys the cluster via `POST /api/ui/deploy`, returning
  a job shown through `useJobs`. Restores the last deploy from localStorage via
  `../lib/deployMemory` (`recallDeploy`/`rememberDeploy`/`usableSelection`),
  after the first plan, pruned against what still exists.
- `Keys.tsx` — Two screens in one file. `TokenKeys`: per-node API tokens for
  harnesses — `GET/POST /admin/api-keys`, `DELETE /admin/api-keys/:key`.
  `ConnectionKeys`: the cluster operator key — `POST /admin/regenerate-key`.
- `LogsByToken.tsx` — Request log tallies and table ("who asked, which machines
  answered"), read from `GET /api/ui/requests` (`src/reqlog.rs`, in-memory,
  lost on restart — the screen says so). Exports `HopChain`, reused by
  `LogsLive`. Node-name map from `useCluster`.
- `LogsLive.tsx` — Same `GET /api/ui/requests` source (polled at 3 s), rendered
  newest-first with an animated highlight and a pause/resume that freezes a
  snapshot. Reuses `HopChain` from `LogsByToken`.
- `ModelPaths.tsx` — Edits `model_dirs` (read-only folders, auto-detect vs
  explicit list) and `shard_dir` (the one writable folder peers may pull from),
  saved via `POST /api/ui/paths`. Shows `GET /api/ui/paths` auto-detected
  folders with kind badges (lmstudio/ollama/folder). Loads server state once so
  polling does not fight the operator's typing.
- `NetworkSettings.tsx` — Admin view: this node's membership
  (`POST /api/ui/leave` / `/api/ui/rejoin`) and the known-node list from
  `GET /admin/nodes` with block/unblock (`POST /admin/nodes/:id/block|unblock`).
- `NotFound.tsx` — 404 fallback with a link back to `/`.
- `SendFile.tsx` — Offer any readable path on this machine to one peer via
  `POST /api/transfer/send` (`{to_node_id, path}`); the receiver only ever sees
  an opaque grant, so "any path out" never becomes "any path in". Pre-fills from
  `?path=` and offers a picker built from the local catalog.
- `Setup.tsx` — First run (no `pom.json`): choose Principal (mints key via
  `GET /api/setup/key`) or Nó (paste key + `ip:port` bootstrap), then
  `POST /api/setup/save`; the node writes config, respawns, and the screen
  reloads itself after 4 s. Uses `../components/Logo` and `LanguageSelect` from
  the i18n provider.
- `Shards.tsx` — "Fatias": every local slice always listed (the Nodes screen
  only shows the difference), with who is missing it. Sends via
  `POST /api/transfer/send`; deletes via `../lib/deleteModel`. `uniqueName`
  disambiguates same-hostname peers with the node_id prefix.
- `SliceModels.tsx` — "Fatiar": build a by-size slice plan (target GB per slice,
  empty = "the rest"). Every keystroke POSTs `POST /api/ui/slice/plan`
  (debounced 260 ms) and shows target vs actual range/size side by side; the
  "divide in N equal parts" shortcut also goes through plan. `POST /api/ui/slice`
  starts the job and navigates to `/fatias/andamento`. `asPlan` validates the
  POST body shape so an old build/proxy cannot take the window down. Only whole
  models (`isWholeModel`) can be sliced.
- `SlicingJobs.tsx` — Slicing jobs from `useJobs` (`GET /api/ui/jobs`), one card
  per job with progress/state/outputs. `STATE` maps job state to tone/color/icon
  written out literally (Tailwind needs literal class strings).
- `Transfers.tsx` — Three panels: pending offers (accept/reject via
  `POST /api/transfer/offers/:id/accept|reject`), active transfers (bytes +
  speed + ETA + cancel via `POST /api/transfer/jobs/:id/cancel`), and finished
  ones. State classification from `../lib/transferStatus`.
- `WholeModels.tsx` — Full-model catalog from `useLocalCatalog`, one `ModelRow`
  per model with actions: slice (links to `/modelos/fatiar?path=`), send
  (`/transferencias/enviar?path=`), delete (`../lib/deleteModel`). `ScannedRoots`
  lists the scanned folders and per-folder file counts.

Key logic carriers:

- `Chat.send` (Chat.tsx:469) — the streaming loop: snapshots turns before the
  placeholder reply, streams deltas into `consumeChatDelta` (keeps
  reasoning/content split), merges `x_pom` metrics via `finishChatStream`,
  aborts via `AbortController`, persists the session on finish. The only screen
  that talks to the OpenAI-compatible surface.
- `Chat.ExecutionTelemetry` (Chat.tsx:128) + `PhaseRow`/`LegacyHopRows` - the
  closed-by-default telemetry table: per-hop prefill/decode phases, gateway vs
  backend vs residual vs transport timing, bytes per hop; renders the legacy
  (pre-phase-split) hop shape separately, an optional TP collective table and
  a pure diagnosis card. The AI action sends both tables as private request
  content, while the initial model is shown as a non-turn mark before chat
  messages.
- `DeployModel.rowsOf`/`sameRows`/`stepOf`/`assign`/`move` (DeployModel.tsx) —
  the selection model: merged steps expand back into per-slice rows, edits are
  sent back to the plan for validation, machine collision swaps ranges, and
  pipeline reorder is a machine swap between adjacent rows.
- `DeployModel.validated`/`syncing`/`answered`/`whyNot` (DeployModel.tsx) - why
  the button is grey, and who is allowed to say so. `sameRows` compares both
  sides **sorted by layer**: the screen keeps its rows in layer order and the
  node builds its answer its own way, and comparing position by position made a
  plan that described exactly these rows look like a different plan. The
  explanation panels hang off `answered` (the node has replied about *these*
  rows), never off `validated` - gating the reason on the match is what left the
  screen silent at the exact moment it had something to say, which is the defect
  the captain reported on 2026-08-16. `whyNot` puts the one-line reason next to
  the disabled button, including the two states with no panel at all (nothing
  selected, answer still in flight). Regression test: `DeployModel.test.tsx`.
- `DeployModel.deploy` (DeployModel.tsx:602) — posts the current rows/settings
  (from `rows`/`eff`, not the nullable edit state), remembers them via
  `rememberDeploy`, and tracks the returned job id.
- `SliceModels.asPlan` (SliceModels.tsx:56) — shape guard on the `SizePlan`
  response; the one screen rendering straight off a POST body.
- `Setup.save` (Setup.tsx:56) — posts the role/key/bootstrap, then reloads; the
  node respawns itself, so there is no second response to wait for.
- `ModelPaths.save` (ModelPaths.tsx:55) — trims/filters the folder list, posts,
  and re-syncs state from the server's answer (which is authoritative).

Backend endpoints used (see `src/webui.rs` / `src/gateway.rs` / `src/openai.rs`
in the repo root): `/v1/chat/completions`, `/v1/models`, `/api/ui/cluster`,
`/api/ui/cluster/models`, `/api/ui/node`, `/api/ui/models`, `/api/ui/paths`,
`/api/ui/jobs`, `/api/ui/requests`, `/api/ui/deploy/plan`, `/api/ui/deploy`,
`/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/leave`,
`/api/ui/rejoin`, `/api/ui/release`, `/api/ui/release/candidate`,
`/api/ui/unload`, `/api/ui/undeploy`, `/api/ui/dev/update`, `/api/ui/slice/plan`, `/api/ui/slice`,
`/api/transfer/send`, `/api/transfer/jobs`, `/api/transfer/jobs/:id/cancel`,
`/api/transfer/offers`, `/api/transfer/offers/:id/accept|reject`,
`/admin/nodes`, `/admin/nodes/:id/block|unblock`, `/admin/api-keys`,
`/admin/regenerate-key`, `/api/setup/key`, `/api/setup/save`.
