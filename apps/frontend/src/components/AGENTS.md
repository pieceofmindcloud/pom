# components/

Reusable UI building blocks, split into two kinds. The small primitives live in
`ui.tsx` (the design system: panels, buttons, inputs, badges, meters, states,
formatters) and everything is composed from them. The bigger pieces are the
app chrome (`Sidebar`/`Topbar`/`Notifications`/`nav.ts`), the cluster
visualizations (`PipelineRail`, `ModelNetworkCanvas`, `DeployNetworkCanvas`), and the dialogs/guards
for destructive or remote actions (`Confirm`/`Modal`, `ErrorBoundary`,
`AddNodeDialog`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
`UndeployPipeline`), and `PeerDetailCard`, the one card for a machine shown in
the Dashboard details tab and in the window the Flow canvas opens. Test files
present: `AddNodeDialog.test.tsx`, `Confirm.test.tsx`, `LeaveNode.test.tsx`,
`UndeployPipeline.test.tsx`, `ModelNetworkCanvas.test.tsx`,
`PeerDetailCard.test.tsx`, `ProfileSwitcher.test.tsx`. Fake peers for these tests come from
`../test/fixtures.ts` and must have the *full* shape the node serves - a
partial `node` (e.g. one without `status`) slips past one component's `?.` and
crashes the next.

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
  `navForProfile(profile)` devolve o menu já podado pelo perfil ativo, lendo a
  mesma tabela (`../lib/access.ts`) que o guarda de rota do `App.tsx` consulta -
  uma seção com filhas só some quando **nenhuma** filha sobra. Ele copia a seção
  antes de filtrar; reescrever `NAV` deixaria o outro perfil com o menu podado.
  Testes em `nav.test.ts`.
- `Sidebar.tsx` — `Sidebar` (desktop aside) and `SidebarContent` (reused by the
  mobile drawer in `Topbar`). `SidebarContent` lê `useProfile()` e renderiza
  `navForProfile(profile)`, não `NAV` cru. `Section`/`Leaf` render it with an animated
  active pill; a section holding the current path stays open. The desktop
  sidebar can be minimized and persists that choice in `localStorage`; the
  minimized state renders icon-only links and hover/focus flyout menus for
  sections with children. `SidebarBrand` is the header and, on desktop, the
  lockup itself is the accessible expand/collapse button: click or touch toggles
  it, while a deliberate mouse hover closes an open sidebar after a short delay.
  In the mobile drawer it remains a passive lockup. The menu below it keeps a
  5px top gap so the first item does not touch that line - the hash sits **on the identity** because in
  a multi-machine cluster "which build is that?" is unanswerable without a
  shell, and a peer on an old binary looks healthy until inference.
  `SidebarUtilityControls` is the footer, and the node's `build_commit` lives
  there now, beside the controls: the bare hash in mono, no "build" label in
  front of it, with the explanation in a `title` tooltip. `SidebarUtilityControls` shows language as **flags**
  (`brand.flags[locale]`, from `brand.json`) rather than "EN"/"PT-BR", since a
  text label only helps someone who already knows what the code means and the
  target here is whoever opened the panel in the wrong language - the language
  name stays in `aria-label`/`title`, because a flag is not a language to a
  screen reader. Plus the dark/light toggle (`../lib/theme`).
- `ProfileSwitcher.tsx` — **Andaime de desenvolvimento.** Alterna o perfil ativo
  (`admin`/`node`) direto na barra superior para conferir a tela pelos olhos de
  cada perfil. Borda tracejada e âmbar de propósito: não é controle de operação
  e não concede permissão nenhuma - quem manda é `../lib/profile.tsx`.
  `PROFILE_SWITCHER_ENABLED` false faz o componente renderizar `null`; quando o
  perfil passar a chegar do nó, este arquivo sai inteiro.
- `SlicingJobsPanel.tsx` - Embedded slicing activity panel used by
  `SliceModels`. It reads `useJobs` (`GET /api/ui/jobs`) and renders one card
  per job with state, progress and output paths only while at least one job is
  running. Completed history stays available to the notification system, but
  does not leave an empty activity card on the slicing screen.
- `Topbar.tsx` — Sticky header: `NodeChip` (hostname, short_id, peer count,
  coverage), o `ProfileSwitcher`, the bell (`Notifications`), and `NodeActions` — a menu to unload
  this node's slice (`POST /api/ui/unload`) or leave the network
  (`POST /api/ui/leave`), each behind a `Confirm`. Mobile drawer renders
  `SidebarContent` inside an animated panel.
- `Notifications.tsx` — The activity bell: counts running slicing jobs
  (`useJobs`), transfers worth notifying (`useTransfers` +
  `isWorthNotifying`) and pending offers (`useOffers`), and shows progress bars
  inside the popover. Offers can be accepted/rejected right there
  (`POST /api/transfer/offers/:id/accept|reject`). All state is read back from
  the node, so a reload in the middle of a transfer shows the same bar.
  Lista os **dois sentidos** — ícone de subida para `sending`, de descida para
  `receiving` — e também o **desfecho recente**: concluída, falhada e recusada
  ficam no sino por `RECENTLY_FINISHED_MS`. Antes ela mostrava só o que estava
  em andamento, e o resultado sumia no instante em que acontecia: quem recebia
  via a barra andar e depois nada, quem enviava não via aviso de aceite nem de
  recusa. Uma transferência terminada não desenha barra (uma barra parada em
  100% sugere que ainda anda), e o `detail` do nó passa por
  `localizeServerText`. O título de uma fatia é **modelo + faixa de camadas**
  (`transferLabel`), com o sha256 curto como identificação secundária ao lado:
  a notificação mostrava o hash, que não diz de que modelo é nem que pedaço
  traz. Numa oferta, o que se oferece é o título e quem oferece é o subtítulo —
  ao contrário, a faixa de camadas era justamente o que a reticência comia.
- `PipelineRail.tsx` — The cluster as the path a request takes: `PipelineRail`
  draws peer cards in execution order connected by `Conduit` wires labelled
  with the layer boundary the hidden state crosses; `LayerRibbon` is one tick
  per layer colored by owning peer, exposing coverage holes. `pipelinePeers`
  lived here until 5423852 and now comes from `../lib/networkPeers` (the rail
  imports it); only `trackOf` (the repeating color palette, `TRACKS`) is still
  exported, reused by `PeerDetailCard`, `Dashboard` and `DeployModel`.
  Currently rendered hidden on Dashboard.
- `ModelNetworkCanvas.tsx` — The primary operational view: a pan/zoom SVG graph
  of nodes (range cards with VRAM/RAM/load meters) and curved edges, driven by
  `graphFromCluster` (`../lib/networkGraph`). Edges have three states via
  `edgeStyle` (principal / paralela / inativa) from `cluster.routing_edges` and
  `cluster.candidate_edges` (replicas = ready alternate routes). Positions are
  persisted per model-deployment key through `../lib/networkLayout`
  (`readPositions`/`writePositions`/`clearPositions`/`fallbackPosition`/
  `deploymentLayoutKey`), drag-restrained to the canvas. Double-clicking a card
  opens the machine's details in a window: `NetworkNode` guards the gesture
  with `isControlTarget` (a click that lands on a `button`/`a`/`input`/etc is
  not a click on the card), and `detailNodeId` stores the `node_id`, not a peer
  copy, so the window shows fresh numbers when the cluster list refreshes
  under it. Exports `edgeStyle` and `isControlTarget`.
- `PeerDetailCard.tsx` — The card of one machine, in the same drawing for every
  machine. It lives outside `Dashboard` because it shows in two places: the
  details tab stacks one card per machine (`networkPeers` splits the whole
  network into the active deployment first, in layer order, then the rest), and
  the double-click on a Flow canvas card opens the same card in a
  `PeerDetailDialog` window over the `Modal` shell (`size="lg"`). One component
  is what keeps the two surfaces from diverging. `cardRange` returns `null`
  when the machine serves nothing - `hasSlice` reads `model` empty with zero
  `total_layers` as absence, never the range "0-0" - and the card then shows a
  "sem fatia" badge plus `dashboard.noSliceDetail` instead of a range, and does
  not offer "Remover" (`RemoveNode` renders only for `serving && group &&
  range`). `cardRange` falls back to `node.config` when the peer has loaded a
  slice it has not announced yet. `UpdateNode` (update+restart, dev-only via
  `POM_DEV_UPDATE`), `RemoveNode`, `LeaveNode` and `ReleaseSlice` all
  render from here now. `PeerPlacement` (interno) é a única parte do card que
  faz uma chamada extra: monta só com a máquina servindo, e o `enabled` do
  `usePeerRuntime` é o que impede um `/health` por card a cada ciclo do Painel.
- `ModelPlacement.tsx` - a seção "Como o modelo foi carregado": a tira de
  camadas (uma célula por bloco, verde na GPU, âmbar dividida, cinza na CPU),
  os totais de peso por lado, onde o cache KV ficou e a cabeça de saída.
  Exporta `placementOf`, `isCpuBuffer` e `kvCacheBuffers`. A regra que importa
  está em `placementOf`: uma camada é classificada pelos **bytes** dela em cada
  buffer, nunca pelo `device` do bloco - com override de buffer type (experts
  de um MoE na CPU) os dois divergem, e é justamente esse caso que a tela
  existe para mostrar. Sem `placement` a seção diz que não sabe, em vez de
  derivar de `n_gpu_layers`.
- `AddNodeDialog.tsx` — "Add a machine to a live model" dialog:
  `POST /api/ui/deploy/node` applies to the target only (no redeploy of the
  cluster). Overlap is the point — it creates a parallel route — so it is
  flagged, not blocked. Exports `rangeOverlap` (shared layers between a chosen
  range and existing ones). Slices come from `Dashboard`'s `deploy/plan`.
  Re-seeding the two selects happens on the *open* transition only (`wasOpen`
  ref): `Dashboard` rebuilds `peers`/`slices` on every cluster poll, so an
  effect keyed on those props threw the operator's choice away mid-edit. A
  second effect only replaces a choice that disappeared from the list. The
  slice select is keyed by `start-end` (`keyOf`), not by array index, so a
  reordered list cannot silently point at another range.
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
  reuses the shell and has no "cancelar". A window can open another window (the
  "Sair da rede" confirmation is born inside the details window), and all of
  them listen for Escape on the `document`, so `Modal` keeps a module-level
  stack of the open ones (`openModals`, keyed by `useId`) and only the top one
  answers Escape; without it one Escape closed both. `size` picks `md` (a
  paragraph) or `lg` (a whole panel, for `PeerDetailDialog`); the panel is a
  flex column capped at `max-h-[calc(100vh-2rem)]` whose body scrolls
  (`min-h-0 flex-1 overflow-y-auto`) instead of the old `max-h-[70vh]`.
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
- `Logo.tsx` — `LogoFull` draws the lockup from `brand.logo[theme]`: two arts,
  one per theme, which is why it needs **no plate behind it** - each theme gets
  art drawn for its own ground. The dark one is derived from the light one by
  `scripts/derive-dark-logo.py` (luminance of the flat teal wordmark mirrored,
  brain illustration untouched) until hand-drawn art exists; replacing
  `assets/brand/pom-logo-dark.png` is the whole change. `variant` is only about
  filling: `"card"` pads (setup screen), `"band"` fills edge to edge (the
  sidebar header, which is the band). `LogoMark` (the real artwork, `<img>` from `/brand/`, served as a static asset because it is a detailed raster shown small) + `Logo` with wordmark. Name, tagline and image URL all come from `lib/brand` (`brand.json` at the repository root), never from literals here; callers size it through `size` only, so sidebar, setup screen and tab icon follow one source. The mark carries its own glow on transparency, so it gets no plate behind it.
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
- `networkPeers.pipelinePeers` (networkPeers.ts:13) — execution order: peers
  on the active model sorted by start layer (falls back to all peers);
  `Dashboard`, `PipelineRail` and `ModelNetworkCanvas` derive "the pipeline"
  from it. Moved here from `PipelineRail` in 5423852.
- `PipelineRail.trackOf` (PipelineRail.tsx:38) — the per-index color cycle used
  to keep a peer's color consistent across rail, ribbon, and deploy rows.
- `ModelNetworkCanvas.edgeStyle` (ModelNetworkCanvas.tsx:25) — encodes the
  three edge states (principal = aqua arrow, paralela = solid iris, inativa =
  dashed faint) that make replicas legible.
- `ModelNetworkCanvas.isControlTarget` (ModelNetworkCanvas.tsx:69) — the
  double-click guard: a gesture that landed on a control is not a gesture on
  the card, so opening details never becomes a trap for a future button.
- `ModelNetworkCanvas.beginDrag`/`drag` (ModelNetworkCanvas.tsx:152) — pointer
  capture drag with zoom-aware offsets and clamping to the canvas; positions are
  persisted on every move (`writePositions`).
- `DeployNetworkCanvas.tsx` — The compact, deploy-time topology view. It
  reutiliza o contrato visual do `ModelNetworkCanvas`: fundo pontilhado, cards de
  máquina, `autoLayout`, posições persistidas por fingerprint, arraste com
  ponteiro, zoom, Fit, Organizar e Redefinir. Cards já presentes no mapa não
  repetem a faixa inferior; arrastar ou dar duplo clique num peer disponível
  adiciona-o ao conjunto preferido e faz o card aparecer no mapa. O plano ainda é a fonte de
  verdade: estágios são identificados pela faixa, não pelo nó, e `route_options`
  desenha somente pontes entre estágios consecutivos, com caminhos
  principal/paralelo/inativo e RTT direcional; a geometria ancora a origem e o
  receptor nos lados que se encaram, inclusive quando o receptor está à esquerda;
  o canvas também filtra
  respostas antigas que tentem saltar um estágio. A faixa inferior continua
  aceitando peers arrastados para recalcular a sugestão. O `X` de um card remove todas as faixas daquele peer da seleção
  e o devolve à faixa inferior. Se o backend não publicar uma aresta, o canvas
  deixa o RTT da aresta ausente em vez de copiar a medição do nó de destino; o
  card ainda pode mostrar o RTT local anunciado pela própria máquina. Réplicas exatas podem aparecer empilhadas na
  mesma coluna. Em modelo inteiro, incluir um peer no mapa acrescenta a réplica
  explícita da faixa completa; em Tensor, o mapa limita os ranks disponíveis.
  O `DeployModel` calcula `mapNodeIds` uma vez e passa essa mesma população
  para os cards e para os seletores primary/workers; TP pré-fatiado aceita
  worker mapeado sem o rank local, enquanto TP de modelo inteiro exige o GGUF
  local. O card Tensor expõe a ação acessível de marcar primary, removendo o
  mesmo nó dos workers.
  Uma GPU ausente mostra uma trilha estática com o texto `sem GPU`, sem barra
  indeterminada. A hidratação de posições preserva cards existentes quando um
  peer novo entra sem faixa, então a topologia não se reorganiza por causa de
  uma inclusão ainda não configurada. Em viewport estreito, o mapa usa
  rolagem interna para preservar o tamanho dos cards e alinhar a curva aos
  endpoints mesmo quando o conteúdo é mais largo que o painel. A separação existe porque este grafo
  representa uma implantação ainda não aplicada.
- `RuntimeParamsForm.tsx` - formulário compartilhado dos parâmetros globais de
  inferência usados pelo Deploy Studio e pela tela de implantações registradas;
  mantém os ajustes por máquina fora do bloco global.
- `PeerDetailCard.cardRange` (PeerDetailCard.tsx:102) — the range a card may
  show, or `null` when the machine serves nothing; never invents a "0-0".
- `ModelPlacement.placementOf` (ModelPlacement.tsx:39) — "gpu" / "cpu" /
  "mixed" / "unknown" pelos bytes de peso em cada buffer; `isCpuBuffer` é quem
  reconhece o host (`CPU`, `CPU_Mapped`), e é por nome porque é assim que o
  ggml batiza o buffer de cada backend.
- `Sidebar.sectionHoldsPath` (Sidebar.tsx:49) — decides whether a section is
  "where you are" (`end` exact match vs prefix) so the active section opens
  itself and the current page is never hidden.
- `RemoveNode.layersLeftUncovered` (RemoveNode.tsx:95) / `AddNodeDialog.rangeOverlap`
  (AddNodeDialog.tsx:136) — the two shared range-arithmetic helpers; the
  confirmation wording in both dialogs depends on them.
- `Confirm.tsx:Modal` (Confirm.tsx:71) - the `createPortal(document.body)` call
  that keeps `position: fixed` honest for every dialog and confirm in the app,
  not just the ones opened from `backdrop-blur` ancestors. The `openModals`
  stack (Confirm.tsx:27) is the companion rule: only the top window answers
  Escape.

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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
