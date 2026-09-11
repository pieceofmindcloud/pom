# components/

Reusable UI building blocks, split into two kinds. The small primitives live in
`ui.tsx` (the design system: panels, buttons, inputs, badges, meters, states,
formatters) and everything is composed from them. The bigger pieces are the
app chrome (`Sidebar`/`Topbar`/`Notifications`/`nav.ts`), chat surface
navigation (`ChatSurfaceTabs`), the cluster
visualizations (`PipelineRail`, `ModelNetworkCanvas`, `DeployNetworkCanvas`), and the dialogs/guards
for destructive or remote actions (`Confirm`/`Modal`, `ErrorBoundary`,
`AddNodeDialog`, `ReleaseSlice`, `RemoveNode`, `LeaveNode`,
`UndeployPipeline`), and `PeerDetailCard`, the one card for a machine shown in
the Dashboard details tab and in the window the Flow canvas opens. Test files
present: `AddNodeDialog.test.tsx`, `Confirm.test.tsx`, `GpuTemperature.test.tsx`, `LeaveNode.test.tsx`,
`UndeployPipeline.test.tsx`, `ModelNetworkCanvas.test.tsx`,
`PeerDetailCard.test.tsx`. Fake peers for these tests come from
`../test/fixtures.ts` and must have the *full* shape the node serves. A partial
`node` can slip past one component's `?.` and crash the next; the maintenance
pendência está em `TODO.md`, item 23.

- `InferenceHeadChoice.tsx` - no Chat, em qualquer perfil, não mostra nada
  quando existe uma única cabeça disponível; com duas ou mais, usa o mesmo combo nativo do seletor
  de modelo para inspecionar as candidatas layer-zero. As opções carregam nome,
  id, RTT e frescura, carga em voo, faixa, capacidade e a marca da escolhida; o
  critério, fallback e pipeline incompleto ficam nas opções desativadas do menu.
  Carga desconhecida aparece como "indisponível", nunca zero. Sem candidata,
  mantém um aviso claro. O componente não apresenta peers fora desse recorte.
  A escolha é controlada pelo `ChatScreen`: `onChange` devolve o `node_id`
  selecionado e a próxima chamada envia esse valor em `inference_head`.

- `setup-wizard/` - Componentes visuais reutilizáveis do wizard de primeiro
  acesso: layout com progresso e ações, cards de intenção, hardware, nó,
  modelo e rede, além das telas de cada etapa. A navegação e o estado ficam em
  `../routes/SetupWizard.tsx`; os componentes só recebem estado e callbacks.

Per-file list:

- `GpuTemperature.tsx` - shared GPU temperature badge used by Flow and machine
  details. Uma máquina pode ter mais de uma placa: os dois cards mostram **uma
  linha por GPU**, nomeada por `gpuDisplayName` (que acrescenta `#índice`
  quando duas placas têm o mesmo nome, o caso das duas RTX 5060 Ti da ksia).
  It uses `SystemLoad.gpu_kind` as the source of truth: `none` returns
  no markup, while a real GPU with a missing reading shows only the unavailable
  sensor message. Flow and machine details pass through this component so the
  no-GPU guard cannot be forgotten by a caller. Normal, attention, critical and
  unavailable-sensor readings use the existing design-system tones.

- `ui.tsx` — The design system in one file on purpose. `Panel`/`PanelHeader`/
  `PageHeader` (surfaces), `Button` (4 variants, sizes, loading spinner),
  `Input`/`Select`/`Field`, `Badge`/`Dot`/`Progress`/`Meter` (the shared `Tone`
  palette: neutral/iris/aqua/amber/rose/sky/lime), `Empty`/`Skeleton`/
  `ErrorState` (renders `localizeServerText` on the error message),
  `Mono`/`ScrollX`/`Footnote`/`KeyValue`. Everything is token-based class names
  (e.g. `bg-iris`, `border-line`) with `clsx`, no runtime styling.
- `AlertTooltip` (ui.tsx) - `compact` deixa o ícone do tamanho do `[i]`, sem
  moldura, para caber na coluna de ícones do `DeploymentInspector`; `Meter` e
  `KeyValue` ganharam `compact` pelo mesmo motivo no card de máquina. É o aviso
  compacto usado no inspetor: o ícone âmbar é
  focável e acionável por foco ou clique, e o texto completo é renderizado em
  portal fixo, com quebra explícita de palavras para mensagens longas e ids sem
  espaços. `InfoTip` é a versão neutra (ícone `[i]`, sem peso de aviso) para
  explicar um parâmetro no hover/foco/toque; os dois dividem
  `usePositionedPopover` (estado + posicionamento do balão de portal). `Field`
  ganhou `tip?: string` - renderiza um `InfoTip` colado à linha do rótulo,
  **fora** do `<label>` (um `<button>` dentro de `<label>` quebra o nome
  acessível do controle), então o `<label>` continua embrulhando só texto e
  controle. `hint` (texto visível) segue existindo para as outras telas.
- `RequestInspector.tsx` - inspetor da tela Logs. A aba ENVIADO vem antes de
  REQUEST e separa prompt novo e contexto anterior em duas superfícies de
  Markdown renderizado; Resposta usa o mesmo visualizador. `MDEditor.Markdown`
  de `@uiw/react-md-editor/nohighlight` é somente leitura, sem barra de edição,
  sem cópia interna, com HTML desabilitado, sanitização de links e sem imagens;
  cada superfície é limitada a 64 KiB para manter a tela responsiva. O toggle
  global entre Markdown renderizado e JSON cru persiste em
  `storageKey("logs.inspector.format")`. O cabeçalho repete os detalhes da
  linha, reúne cópia da aba ativa, toggle e fechar, e a telemetria continua
  usando JSON, caminho da requisição e o cartão compartilhado
  `ExecutionTelemetry`. Cada área Markdown tem um botão de expansão no canto
  superior direito que abre uma janela `Modal` grande; ela acompanha o toggle
  global e usa a mesma sanitização e censura de credenciais.
  A cópia sempre redige credenciais.
- `CodeViewer.tsx` - superfície de leitura flexível com realce JSON por
  `highlight.js`. É somente leitura, mas permanece selecionável e copiável pelo
  navegador, e é usada nas abas REQUEST e TELEMETRIA do inspetor.
- `ExecutionTelemetry.tsx` - cartão compartilhado do trace de execução usado
  pelo Chat e pelo inspetor de Logs. Mantém a tabela de fases, tempos por hop,
  rede, ambiente, diagnóstico e demais detalhes em um único componente. Cruza
  também os timings ponta a ponta com o processamento medido no nó, para não
  chamar de gargalo do backend uma espera que ocorreu fora dele. A seção
  distribuída mostra estado de coleta, cobertura de peers/spans, caminho
  crítico, compute/transporte acumulados, overlap somente quando medido e os
  histogramas agregados do `remote-stream`; não deduz `outside-node` por
  subtração do wall-clock. Mostra também
  o executor MoE efetivo e o motivo publicado pelo runtime quando há fallback
  CPU ou indisponibilidade. O cartão distingue o caminho selecionado no
  snapshot atual de `/health` do caminho observado no trace desta requisição;
  cada fase também mostra seu `selected_path`, e uma divergência entre os dois
  escopos é sinalizada. A timeline também mostra spans medidos, a decomposição
  de `outside_node`, TTFT do servidor/motor/cliente, primeiro token, stream,
  conexões e tokens por chamada; fronteiras ausentes permanecem não medidas.
  O resumo exibe sempre a contagem e as razões do fallback HTTPS e, quando há
  amostras, os quantis do intervalo de entrega de tokens do `remote-stream`,
  separando produção, downstream, reserva do canal, fila e callback anterior;
  o detalhe por nó também mostra o custo síncrono do draft MTP quando presente.
  O resumo por transporte diferencia RTT de execução, que inclui processamento
  remoto, do RTT de rede medido no limite do transporte; a linha QUIC usa a
  medição da conexão quando disponível.
  Cobertura de decode só é exibida quando há denominador esperado; traces
  diretos sem essa medição mostram o estado como não medido, e uma coleta de
  transporte incompleta continua visível mesmo quando a resposta terminou.
- `Markdown.tsx` — memoized assistant-answer renderer: `ReactMarkdown` with
  `remark-gfm` + `rehype-highlight` (auto-detect language), inline vs fenced
  code split, every fenced block gets a copy button (answers here are mostly
  commands). Used by `Chat`.
- `ChatSessionList.tsx` - shared conversation history list. The Chat screen
  uses the wide variant; the `infer` Sidebar uses the compact variant. The
  list owns the bounded `overflow-y-auto` region, active `aria-pressed` state,
  delete action and localized new-conversation button.
- `ApiKeysTable.tsx` - tabela full-bleed de API keys. O administrador escolhe o
  nó na criação, vê seus próprios tokens completos e os demais mascarados;
  node e infer ficam restritos às chaves do próprio nó.
- `DeploymentLogDock.tsx` - barra recolhida do Dashboard para o admin, que
  expande para cima até metade da área e acompanha o log de implantação em
  tempo real com auto-scroll.
- `OpenNativeChatWindowButton.tsx` - botão macOS usado somente quando a ponte
  WebKit `pomOpenWindow` está disponível; fica na toolbar do `Topbar`, não no
  corpo do Chat. É um controle icon-only: o nome completo permanece no nome
  acessível e no tooltip.
- `ChatSurfaceTabs.tsx` - abas de rota compartilhadas por `Chat` e
  `NetworkChat`. Mantém `/chat` e `/network-chat` acessíveis nos dois sentidos,
  com o estado ativo derivado do pathname.
- `nav.ts` - `NAV`: the menu as data. A seção Implantações usa `Rocket` (era
  `Boxes`, o mesmo ícone de Modelos), o Histórico usa `History` e Logs possui
  folhas separadas para requisições e implantação; Network Chat é uma entrada
  própria. O submenu não
  desenha ícone de folha, então `Rocket` na seção e na folha `/implantar` não
  colidem. A tabela em si é an ordered array of `NavSection`
  (icon + `to` or `children: NavLeaf[]`), each leaf with `labelKey`, route and
  `end` (whether the sidebar should match child routes too). The sidebar and the
  App router both consume it; the routes themselves are declared in `../App.tsx`.
  `navForProfile(profile)` devolve o menu já podado pelo perfil ativo, lendo a
  mesma tabela (`../lib/access.ts`) que o guarda de rota do `App.tsx` consulta -
  uma seção com filhas só some quando **nenhuma** filha sobra. Ele copia a seção
  antes de filtrar; reescrever `NAV` deixaria o outro perfil com o menu podado.
  Estatísticas é uma entrada de primeiro nível depois de Logs, mantendo a
  sequência operacional de consultar eventos antes de seus agregados. A mesma
  tabela marca as entradas que vão para o rodapé do `infer` e promove a rede
  para o menu principal; `navForProfile` e `navFooterForProfile` são suas duas
  projeções. Testes em `nav.test.ts`.
  A seção principal Rede contém Informações, Participantes, Administradores,
  Compartilhamento e API Keys. Configurações contém somente Preferências,
  Armazenamento e Preferências Administrativas; Compartilhamento é a tela única
  que reutiliza `TrustAnchor` para raiz de confiança, esta máquina e chave de
  conexão.
- `Sidebar.tsx` — `Sidebar` (desktop aside) and `SidebarContent` (reused by the
  mobile drawer in `Topbar`). `SidebarContent` lê `useProfile()` e renderiza
  `navForProfile(profile)`, não `NAV` cru. `Section`/`Leaf` render it with an animated
  active pill; a section holding the current path stays open. The desktop
  sidebar can be minimized and persists that choice in `localStorage`; the
  minimized state renders icon-only links and hover/focus flyout menus for
  sections with children. `SidebarBrand` is the header and, on desktop, the
  lockup itself is the accessible expand/collapse button: only an explicit click
  or touch toggles it.
  In the mobile drawer it remains a passive lockup. The menu below it keeps a
  5px top gap so the first item does not touch that line - the hash sits **on the identity** because in
  a multi-machine cluster "which build is that?" is unanswerable without a
  shell, and a peer on an old binary looks healthy until inference.
  `SidebarUtilityControls` is the footer, and the node's `build_commit` lives
  there now, beside the controls: the bare hash in mono, no "build" label in
  front of it, with the explanation in a `title` tooltip; o hash fica oculto
  para `infer`. `SidebarUtilityControls` shows language as **flags**
  (`brand.flags[locale]`, from `brand.json`) rather than "EN"/"PT-BR", since a
  text label only helps someone who already knows what the code means and the
  target here is whoever opened the panel in the wrong language - the language
  name stays in `aria-label`/`title`, because a flag is not a language to a
  screen reader. Plus the dark/light toggle (`../lib/theme`). `TlsIndicator.tsx`
  keeps the TLS policy link beside those flags only for `admin`: icon-only, with different icon
  and tone for encrypted versus cleartext and the meaning in
  `aria-label`/`title`. Para `infer`, o rodapé também renderiza documentação e
  API keys, enquanto a coluna principal é substituída por `ChatSessionList` e
  o link da rede. O histórico mantém o botão de nova conversa e a rede fora da
  área rolável, para uma lista longa não empurrar nenhuma ação para fora.
  Como esse menu é visto de toda rota, escolher uma conversa ou começar uma
  nova também **navega para `/chat`**: sem isso, o clique feito das
  configurações ou da documentação só trocava a sessão ativa e deixava a tela
  onde estava. Quem já está no chat não é reenviado.
- `SlicingJobsPanel.tsx` - Embedded slicing activity panel used by
  `SliceModels`. It reads `useJobs` (`GET /api/ui/jobs`) and renders one card
  per job with state, named phase, byte heartbeat, rate/ETA when available and
  output paths. Completed and failed history remains on the slicing screen so
  a failure cannot look like an eternal spinner; partial outputs are shown.
- `AdminAuthorityPanel.tsx` - Readable authority roster for the principal,
  secondary admins and promotion chain. It exposes promotion, demotion and
  succession controls only when the presentation contract says this viewer is
  the principal; refusal states remain explicit.
- `Topbar.tsx` - Sticky header: `NodeChip` (hostname, short_id, peer count,
  coverage), o selo i18n do perfil persistido do nó, o botão macOS de janela
  maior, the bell (`Notifications`) and `NodeActions` for unloading this
  node's slice. Mobile drawer renders `SidebarContent` inside an animated
  panel. A ação de sair da rede não fica exposta no menu.
- `Notifications.tsx` — The activity bell: counts running slicing jobs
  (`useJobs`), transfers worth notifying (`useTransfers` +
  `isWorthNotifying`) and pending offers (`useOffers`), and shows progress bars
  inside the popover. Offers can be accepted/rejected right there
  (`POST /api/transfer/offers/:id/accept|reject`). All state is read back from
  the node, so a reload in the middle of a transfer shows the same bar.
  It also renders principal-transfer and infer-to-node offers from the same
  `useOffers()` response. Node-to-infer changes are amber notices without
  action buttons. Authority offers keep their pending state in the bell and
  link to `/config/administracao` for the roster.
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
  of nodes (range cards with VRAM/RAM/load/temperature meters when a GPU exists) and curved edges, driven by
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
  under it. Exports `edgeStyle` and `isControlTarget`. That gesture is gated by
  `canSeePeerDetails` (`../lib/access`): outside `admin` the card takes no
  `onOpen`, so it neither opens the window nor advertises a double-click in its
  `title` - the same rule that removes the Details tab and its view switch from
  the Dashboard. The graph also renders directional RTT badges using the same
  label component as the deploy map. The cluster coverage badge is an admin
  summary; node and infer profiles use their own model and slice data instead.
  A missing peer load stays visibly unknown in the card meter and is never
  rendered as zero. The selected pipeline is drawn only forward; a virtual
  `Gateway` is placed after the last layer stage and receives the return-leg
  arrow. Its card shows the complete cycle cost only when every directional
  leg and peer compute measurement is present, never treating an absent leg as
  zero.
  When the Dashboard has an active deployment, its properties toggle is a
  compact icon button inside the map's upper-right control area, next to the
  zoom and Fit controls; the dashboard toolbar remains reserved for cluster
  status and actions.
- `RouteRttLabel.tsx` - selo visual de RTT direcional compartilhado pelo mapa
  do Painel e pelo mapa de implantação. No Painel ele também identifica o
  transporte efetivo (`HTTP`, `HTTPS` ou `QUIC`) observado na execução; quando
  a medição ainda não existe, conserva `RTT -` em vez de usar a sonda libp2p.
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
  slice it has not announced yet. `LoadParamsSection` renders the llama.cpp
  context parameters from `health.load_params` (Flash Attention,
  `n_batch`/`n_ubatch`, context, KV sequences, threads, GPU layers, KV cache
  type); it is skipped entirely when the peer's `/health` carries no such
  field, and `flash_attn: null` reads as "não sei", never as off.
  Peer ilegível tem **dois** estados, e o card os separa: `unreachable` é o par
  que atendeu no endereço anunciado e a leitura quebrou (beco sem saída, ponto
  rosa), `rediscovering` é o par cujos endereços todos morreram e cujo endereço
  atual o nó já foi perguntar (trabalho em curso, ponto âmbar a pulsar,
  `dashboard.rediscovering`). Endereços IPv6 temporários rodam sozinhos, então o
  segundo estado é o comum e resolve-se na volta seguinte do gossip - mostrá-lo
  como beco sem saída mandava o utilizador procurar um defeito inexistente. Ver
  `docs/subsystems/rede-p2p.md`, "Endereço que deixou de responder".
  `PipelineRail` e `ModelNetworkCanvas` fazem a mesma distinção.
  `UpdateNode` (update+restart, dev-only via
  `POM_DEV_UPDATE`), `RemoveNode`, `LeaveNode` and `ReleaseSlice` all
  render from here now - e todos numa **barra de ferramentas no topo do
  card**, junto do distintivo de MTP, em vez de espalhados pelo pé: reiniciar
  uma máquina era rolar até o fim de tudo o que ela reporta. O card é denso de
  propósito (`compact` em `Meter`/`KeyValue`, seções `px-3 py-2.5`) para caber
  numa tela; o que é útil sem precisar estar sempre visível vive num `InfoTip`
  (a razão de "sem fatia", o `gpu_note`, a nota do fit, caminho do modelo, pid
  e reinícios), e "Parâmetros de carga do llama.cpp" e "Processos na GPU" são
  `<details>` fechados. Os discos são **um medidor por partição montada**
  (`system.disks`), com recuo para `system.disk` num nó que ainda não publique
  a lista. `PeerPlacement` (interno) é a única parte do card que
  faz uma chamada extra: monta só com a máquina servindo, e o `enabled` do
  `usePeerRuntime` é o que impede um `/health` por card a cada ciclo do Painel.
  Quando a configuração pediu MTP, o mesmo card também mostra
  `mtp_ready`/`mtp_allocation`/`mtp_unavailable_reason` do runtime, incluindo o
  local medido do contexto e a razão da falha de anexação. A seção térmica mostra uma linha por GPU e declara temperatura
  desconhecida quando o driver não fornece leitura.
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
  cluster). `POST /api/ui/deploy/node/options` is re-read for the selected
  target, so its own slices and whole GGUF are offered instead of a fixed
  cluster plan. The same response supplies that machine's GPU layers, KV
  placement and K/V cache choices, which are sent with the add request.
  Overlap is the point - it creates a parallel route - so it is flagged, not
  blocked. The dialog can add a whole-model head to a sliced deployment and
  vice versa. Exports `rangeOverlap` (shared layers between a chosen range and
  existing ones).
  `Dashboard` mounts this dialog and its trigger only for the `admin` profile;
  the server repeats that authorization and returns a clear localized refusal
  for other profiles.
  Re-seeding the two selects happens on the *open* transition only (`wasOpen`
  ref): `Dashboard` rebuilds `peers` on every cluster poll, so an effect keyed
  on those props threw the operator's choice away mid-edit. A second effect
  only replaces a choice that disappeared from the list. The catalog request
  is aborted when the machine changes, and the option select is keyed by kind
  plus `start-end` (`keyOf`), not by array index, so a reordered list cannot
  silently point at another range.
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
  `Dashboard` renders it only for the `admin` profile, while the backend is the
  final authority and reports the refusal through `localizeServerText`.
  Renders both a `Confirm` and a result `Modal` (no cancel). `RemoveNode` stays
  separate and keeps the per-machine button available to every profile.
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
  mesma coluna. Em modelo inteiro, o mapa aceita uma única máquina e recusa uma
  segunda com aviso; em Tensor, o mapa limita os ranks disponíveis. A carga em
  voo do card vem do relatório vivo da própria máquina; quando esse relatório
  não chega, o medidor mostra indisponível e não inventa zero.
  O `DeployModel` calcula `mapNodeIds` uma vez e passa essa mesma população
  para os cards e para os seletores primary/workers; TP pré-fatiado aceita
  worker mapeado sem o rank local, enquanto TP de modelo inteiro exige o GGUF
  local. O card Tensor expõe a ação acessível de marcar primary, removendo o
  mesmo nó dos workers.
  Uma GPU ausente mostra uma trilha estática com o texto `sem GPU`, sem barra
  indeterminada. Quando o plano traz `machine.capacity` (projeção pós-deploy
  vinda de `/api/ui/deploy/plan`), o card troca a barra única de GPU por uma
  `GpuProjectionBar` por dispositivo: preenchimento = uso projetado
  (`vram_projected_mb / vram_total_mb`), tracinho = uso de agora, róseo quando
  a projeção passa da placa, com um `AlertTooltip` listando os números quando
  `!fits`. Sem `capacity` e com faixa atribuída, a barra diz "projeção após a
  cópia". A hidratação de posições preserva cards existentes quando um
  peer novo entra sem faixa, então a topologia não se reorganiza por causa de
  uma inclusão ainda não configurada. Em viewport estreito, o mapa usa
  rolagem interna para preservar o tamanho dos cards e alinhar a curva aos
  endpoints mesmo quando o conteúdo é mais largo que o painel. A separação existe porque este grafo
  representa uma implantação ainda não aplicada.
- `DeploymentInspector.tsx` - o inspetor de propriedades de uma implantação, e
  **um só** para os dois ecrãs: a tela de implantar o usa editável, o Histórico
  o usa com `readOnly` para mostrar com que parâmetros uma versão subiu. Em
  somente leitura não há `<input>` desabilitado: há o valor escrito. Três
  categorias, e a fronteira entre elas é de quem é o parâmetro - *Configurações
  padrões* (o que o llama.cpp já tem, incluindo os combos de cache KV da
  instalação), *Configurações POM* (motor, ajuste de memória, chunks em voo,
  fusão de fatias, precisão do estado oculto) e *Configurações POM (DESENV)*
  (detector de ciclo, recuo do MTP para a RAM e descarregamento ocioso por
  peer). O `[i]` vem **antes** do nome,
  numa coluna reta: depois de nomes de larguras diferentes ele saía serrilhado.
  O combo do motor não oferece "Automático" e assume `dense` ao montar - é
  apresentação, o backend continua aceitando os três modos. Números de ponto
  flutuante passam por `showNumber`, senão a temperatura sai como
  `0.699999988079071`. Os controles de cache KV, fusão de fatias e precisão do
  estado oculto só aparecem quando quem monta os passa. `VisionControl` segue
  a mesma fronteira: exibe e alterna o mmproj resolvido para modelos inteiros
  e fatias, ou mostra a indisponibilidade localizada quando a rodada fatiada
  não tem projetor.
- `RuntimeParamsForm.tsx` - formulário compartilhado dos parâmetros globais de
  inferência usados pelo Deploy Studio e pela tela de implantações registradas;
  inclui a profundidade do pipeline de prefill (`prefill_max_inflight_chunks`,
  1..8) e a reserva configurável do fit (`fit_memory_margin_mb`, em MiB), e
  mantém os ajustes por máquina fora do bloco global. Também expõe o detector de
  ciclos, seus limites de janela e o número de repetições. Todo campo numérico
  passa `tip` para o `Field` (dica no `[i]`, sem texto fixo no layout);
  `deploy.knob.<chave>Hint` no `locales.ts` é a fonte, e todas as chaves de
  `KNOBS` agora têm uma. `CapabilitySwitch` usa o
  estado tri-state retornado pelo plano: oculta incompatíveis, mantém
  indeterminados visíveis, acionáveis e com alerta de risco, e preserva sempre o
  controle `no_embeddings`.
  O seletor `hidden_state_dtype` mantém f16 como padrão e oferece f8 somente
  como escolha explícita; uma capability de peer comprovadamente incompatível
  desabilita a opção e exibe a razão devolvida pelo servidor.
- `PeerDetailCard.cardRange` (PeerDetailCard.tsx:102) — the range a card may
  show, or `null` when the machine serves nothing; never invents a "0-0".
- `PeerDetailCard.ContextReservationNotice` (PeerDetailCard.tsx) - when the
  live `/health` reports multiple sessions with KV measured on a device, the
  card shows the effective total and per-session context and warns that the KV
  reservation can raise per-call latency under tight VRAM. A single session or
  an unmeasured/CPU KV cache does not create this warning.
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
- `RemoteModelCard.tsx` / `RemoteModelDetails.tsx` - card comparável e janela
  sob demanda para modelos do provider, com prioridade para GGUF, quantização,
  tamanho, licença, revisão e estados instalado/em download.
- `RemoteModelProviderStatus.tsx` - estado estruturado do CLI do Hugging Face:
  indisponível, login necessário ou autenticado, com mensagem localizada; its
  internal route link uses React Router so the `/admin-ui` basename is kept.
- `HuggingFaceSettingsPanel.tsx` - campo de senha para o token opcional. O
  valor salvo nunca é pré-preenchido nem renderizado: a API fornece só máscara
  e fonte efetiva, e o painel invalida status/configuração depois de salvar.
- `DownloadModelDialog.tsx` - confirmação do download remoto; chama somente a
  API estruturada, mostra espaço disponível e as pastas de modelos resolvidas
  pelo backend, sem aceitar caminho do navegador.
- `ModelDownloadsPanel.tsx` - progresso, destino local, cancelamento e retry de
  jobs `model_download` vindos de `useModelDownloads`; é reutilizado em Modelos
  remotos e Transferências. A fonte de verdade é o nó e o painel não cria
  downloader local.

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
