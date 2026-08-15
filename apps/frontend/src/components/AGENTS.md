# components/

Reusable UI building blocks, split into two kinds. The small primitives live in
`ui.tsx` (the design system: panels, buttons, inputs, badges, meters, states,
formatters) and everything is composed from them. The bigger pieces are the
app chrome (`Sidebar`/`Topbar`/`Notifications`/`nav.ts`), the cluster
visualizations (`PipelineRail`, `ModelNetworkCanvas`), and the dialogs/guards
for destructive or remote actions (`Confirm`/`Modal`, `ErrorBoundary`,
`AddNodeDialog`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
`UndeployPipeline`). Test files present: `AddNodeDialog.test.tsx`,
`Confirm.test.tsx`, `LeaveNode.test.tsx`, `UndeployPipeline.test.tsx`,
`ModelNetworkCanvas.test.tsx` (ignored for content here).

Per-file list:

- `ui.tsx` — The design system in one file on purpose. `Panel`/`PanelHeader`/
  `PageHeader` (surfaces), `Button` (4 variants, sizes, loading spinner),
  `Input`/`Select`/`Field`, `Badge`/`Dot`/`Progress`/`Meter` (the shared `Tone`
  palette: neutral/iris/aqua/amber/rose/sky/lime), `Empty`/`Skeleton`/
  `ErrorState` (renders `localizeServerText` on the error message),
  `Mono`/`ScrollX`/`Footnote`/`KeyValue`. Everything is token-based class names
  (e.g. `bg-iris`, `border-line`) with `clsx`, no runtime styling.
- `Markdown.tsx` — memoized assistant-answer renderer: `ReactMarkdown` with
  `remark-gfm` + `rehype-highlight` (auto-detect language), inline vs fenced
  code split, every fenced block gets a copy button (answers here are mostly
  commands). Used by `Chat`.
- `nav.ts` — `NAV`: the menu as data — an ordered array of `NavSection`
  (icon + `to` or `children: NavLeaf[]`), each leaf with `labelKey`, route and
  `end` (whether the sidebar should match child routes too). The sidebar and the
  App router both consume it; the routes themselves are declared in `../App.tsx`.
- `Sidebar.tsx` — `Sidebar` (desktop aside) and `SidebarContent` (reused by the
  mobile drawer in `Topbar`). `Section`/`Leaf` render `NAV` with an animated
  active pill; a section holding the current path stays open. Bottom controls:
  EN/PT-BR switch, dark/light theme toggle (`../lib/theme`), and the node's
  `build_commit` from `useNode`.
- `Topbar.tsx` — Sticky header: `NodeChip` (hostname, short_id, peer count,
  coverage), the bell (`Notifications`), and `NodeActions` — a menu to unload
  this node's slice (`POST /api/ui/unload`) or leave the network
  (`POST /api/ui/leave`), each behind a `Confirm`. Mobile drawer renders
  `SidebarContent` inside an animated panel.
- `Notifications.tsx` — The activity bell: counts running slicing jobs
  (`useJobs`), active transfers (`useTransfers`) and pending offers
  (`useOffers`), and shows progress bars inside the popover. Offers can be
  accepted/rejected right there (`POST /api/transfer/offers/:id/accept|reject`).
  All state is read back from the node, so a reload in the middle of a transfer
  shows the same bar.
- `PipelineRail.tsx` — The cluster as the path a request takes: `PipelineRail`
  draws peer cards in execution order connected by `Conduit` wires labelled
  with the layer boundary the hidden state crosses; `LayerRibbon` is one tick
  per layer colored by owning peer, exposing coverage holes. Exports
  `pipelinePeers` (peers on the active model sorted by start layer) and
  `trackOf` (the repeating color palette, `TRACKS`) — both reused by
  `Dashboard` and `DeployModel`. Currently rendered hidden on Dashboard.
- `ModelNetworkCanvas.tsx` — The primary operational view: a pan/zoom SVG graph
  of nodes (range cards with VRAM/RAM/load meters) and curved edges, driven by
  `graphFromCluster` (`../lib/networkGraph`). Edges have three states via
  `edgeStyle` (principal / paralela / inativa) from `cluster.routing_edges` and
  `cluster.candidate_edges` (replicas = ready alternate routes). Positions are
  persisted per model-deployment key through `../lib/networkLayout`
  (`readPositions`/`writePositions`/`clearPositions`/`fallbackPosition`/
  `deploymentLayoutKey`), drag-restrained to the canvas.
- `ModelNetworkDemo.tsx` — A static "SIMULAÇÃO" mock of the canvas with
  hardcoded nodes/edges. Not imported anywhere in the app — a leftover from
  building the canvas.
- `AddNodeDialog.tsx` — "Add a machine to a live model" dialog:
  `POST /api/ui/deploy/node` applies to the target only (no redeploy of the
  cluster). Overlap is the point — it creates a parallel route — so it is
  flagged, not blocked. Exports `rangeOverlap` (shared layers between a chosen
  range and existing ones). Slices come from `Dashboard`'s `deploy/plan`.
- `Confirm.tsx` - Two exports. `Modal` is the shell (veil + panel + header +
  footer) rendered via `createPortal` straight into `document.body`. The portal
  is not preference: `position: fixed` stops being relative to the viewport when
  any ancestor carries `transform`, `filter` **or** `backdrop-filter` - that
  ancestor becomes the containing block. The top bar and the Dashboard header
  have `backdrop-blur`, so dialogs opened from there were centered inside a
  ~76 px strip, title off-screen (measured at `y = -66` on a 2029 px viewport).
  `Confirm` is the question wrapper on top of `Modal` (Escape cancels, `busy`
  disables both buttons, `tone` selects primary/danger); the split exists
  because not everything in a modal is a question - the undeploy report window
  reuses the shell and has no "cancelar". Body area scrolls when long
  (`max-h-[70vh] overflow-y-auto`).
- `LeaveNode.tsx` - Take **one** machine off the network, from its card on the
  Dashboard. Same act as the top bar's "Este nó › Sair da rede", but that one
  only ever detached the machine the panel is open on; reaching each machine's
  own interface to switch it off was a detour, so the request goes to the local
  node with the target's `node_id` and it forwards (`POST /api/ui/leave`,
  `webui::forward_leave`). `isSelf` sends `{}` (local, no relay). Explicit
  confirmation because re-entry happens from that machine's own Rede tab, not
  from here.
- `UndeployPipeline.tsx` - The inverse of "Implantar na rede": take the whole
  pipeline out of service at once via `POST /api/ui/undeploy` (`{group}`), the
  counterpoint to the per-machine `ReleaseSlice`. "Descarregar modelo" only ever
  freed one box, and undoing a four-machine deploy was four visits to four
  panels; a machine left behind keeps RAM/VRAM busy and serves nothing, since
  the pipeline only plans with closed coverage. Two promises to the operator:
  ask before acting (naming each machine and its layer range) and report
  machine by machine - a peer that did not confirm still holds its memory, and
  the report shows `released: false` with the reason instead of a rounded-up
  "pronto!". Nothing is deleted from disk (`undeploy_endpoint` in `deploy.rs`).
  Renders both a `Confirm` and a result `Modal` (no cancel).
- `ErrorBoundary.tsx` — Class boundary placed around the routed page in
  `App.tsx` (the chrome survives a broken screen). Clears on `resetKey` change
  (the route path); `fallback` lets the chrome collapse to a line; retry button
  plus the error message in a `<pre>`.
- `Logo.tsx` — `LogoMark` (placeholder CODAI mark: three layer bars converging
  on an output) + `Logo` with wordmark. Callers size it through `size` only;
  swapping the real brand means replacing the SVG inside `LogoMark`.
- `MachineIcon.tsx` — `machineKind(system)` classifies a machine from its
  `SystemLoad` (nvidia GPU box, Apple Silicon Mac, Windows, Linux) into a
  label/icon/accent color; `MachineIcon` renders it. Reused by Dashboard,
  NetworkSettings and PipelineRail to make a heterogeneous cluster readable.
- `ReleaseSlice.tsx` — "Give this machine's memory back": probes
  `GET /api/ui/release/candidate` to say truthfully whether another machine
  picks the slice up (or the layers go down), then `POST /api/ui/release`. Only
  rendered on the machine's own card — the node owns its memory.
- `RemoveNode.tsx` — Take a machine out of a live model without touching the
  others: `removeNode` (`../lib/api`, `POST /api/ui/deploy/node/remove`), with a
  confirmation that distinguishes "loses a parallel route" from "these layers
  go uncovered". Exports `layersLeftUncovered` and `rangeText` (compact range
  rendering), computed the same way the node does.

Key logic carriers:

- `ui.tsx:Button` (ui.tsx:100) — the single button primitive: variant/size/
  loading/icon, `active:scale` press feedback, disabled while loading. Every
  other component here builds on it.
- `ui.tsx:ErrorState` (ui.tsx:369) — turns any error into a localized panel;
  because the backend writes server errors as i18n keys, this is where
  `localizeServerText` runs.
- `Markdown` (Markdown.tsx:32) — `memo`ized so streaming delta updates do not
  re-parse the whole transcript every token; fenced-code detection decides
  inline vs block + copy button.
- `nav.ts:NAV` (nav.ts:40) — the source of truth for the menu, its order and
  its routes; the section that is "its own entry" (`/implantar`, deploy) is a
  deliberate first-class navigation item, not a leaf under the shards section.
- `PipelineRail.pipelinePeers` (PipelineRail.tsx:40) — execution order: peers
  on the active model sorted by start layer (falls back to all peers); both
  Dashboard and DeployModel derive "the pipeline" from it.
- `PipelineRail.trackOf` (PipelineRail.tsx:37) — the per-index color cycle used
  to keep a peer's color consistent across rail, ribbon, and deploy rows.
- `ModelNetworkCanvas.edgeStyle` (ModelNetworkCanvas.tsx:23) — encodes the
  three edge states (principal = aqua arrow, paralela = solid iris, inativa =
  dashed faint) that make replicas legible.
- `ModelNetworkCanvas.beginDrag`/`drag` (ModelNetworkCanvas.tsx:129) — pointer
  capture drag with zoom-aware offsets and clamping to the canvas; positions are
  persisted on every move (`writePositions`).
- `Sidebar.sectionHoldsPath` (Sidebar.tsx:49) — decides whether a section is
  "where you are" (`end` exact match vs prefix) so the active section opens
  itself and the current page is never hidden.
- `RemoveNode.layersLeftUncovered` (RemoveNode.tsx:95) / `AddNodeDialog.rangeOverlap`
  (AddNodeDialog.tsx:136) — the two shared range-arithmetic helpers; the
  confirmation wording in both dialogs depends on them.
- `Confirm.tsx:Modal` (Confirm.tsx:49) - the `createPortal(document.body)` call
  that keeps `position: fixed` honest for every dialog and confirm in the app,
  not just the ones opened from `backdrop-blur` ancestors.

Links to other folders: everything imports `../lib/*` — `api.ts` (HTTP client
with connection-key auth), `queries.ts` (all React Query hooks: `useNode`,
`useCluster`, `useJobs`, `useTransfers`, `useOffers`, `useAdminNodes`,
`useInvalidate`), `types.ts` (`ClusterView`, `PeerView`, `SystemLoad`),
`format.ts` (`isServing`, `layerRange`, `serverStateLabel`, `since`, `bytes`,
`rate`, `duration`), `theme.tsx` (`useTheme`, used by Sidebar), `toast.tsx`
(`useToast`). i18n via `../i18n/provider.tsx` (`useI18n`, `LanguageSelect`) and
`../i18n/locales.ts` (`localizeServerText`, `formatMessage`, `getActiveLocale`).
Backend endpoints touched directly: `POST /api/ui/unload`, `POST /api/ui/leave`,
`POST /api/ui/undeploy`, `POST /api/ui/deploy/node`,
`POST /api/ui/deploy/node/remove`,
`GET/POST /api/ui/release/candidate` + `/api/ui/release`,
`GET /api/transfer/jobs`, `GET /api/transfer/offers`,
`POST /api/transfer/offers/:id/accept|reject`; the rest go through the hooks.

## Defeitos anotados

- **`ModelNetworkDemo.tsx` is dead code.** Nothing in the app imports it; it is
  a hardcoded simulation left over from building `ModelNetworkCanvas`. It keeps
  its own copy of the layout/meter logic, which will drift from the real canvas.
- **`ModelNetworkCanvas.tsx` hardcodes Portuguese.** Buttons ("Diminuir zoom",
  "Ajustar à rede", "Reorganizar nós automaticamente", "Fit") and empty states
  ("Nenhum modelo implantado", "Nenhuma capacidade de inferência anunciada")
  are literal strings, while the legend uses `t("canvas.*")` — with the EN
  locale active the legend switches but the rest does not.
- **`AddNodeDialog` never re-initializes its picks from props.** `nodeId` and
  `sliceIndex` are seeded once (`useState(peers[0]?.node_id ?? "")`) and the
  dialog is always mounted (open is controlled by Dashboard), so reopening after
  the peer list changed can submit a stale node_id that is not in the dropdown.
- **`LeaveNode` reuses the trigger label for the confirm button.** The card
  button and the window's confirm both read "Sair da rede" - the window shows
  two same-named buttons, and the test clicks `buttons[buttons.length - 1]`,
  relying on the portal putting the window's button last in the DOM. Cosmetic,
  but a user can confuse the trigger with the confirmation.
