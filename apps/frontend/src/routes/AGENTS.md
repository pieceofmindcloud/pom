# routes/

Each screen of the panel, one file per route, wired up in `../App.tsx`. Every
screen is backed by React Query hooks from `../lib/queries.ts` and speaks to the
node's HTTP gateway (`src/webui.rs`, `src/gateway.rs`); none of them keep
cluster state in the browser. All copy goes through the i18n provider
(`../i18n/provider.tsx`); several screens additionally pass server-side error
strings through `localizeServerText` (`../i18n/locales.ts`), which the backend
writes as translation keys.

Per-file list:

- `SetupWizard.tsx` - Protótipo navegável do primeiro acesso, mantido na rota
  `/setup` mesmo depois da configuração inicial. Renderiza **fora da moldura**,
  como o `Setup.tsx` do primeiro arranque: sem Sidebar e sem Topbar (o
  `isWizard` do `App.tsx` devolve antes do shell). O assistente guia a
  configuração do zero, e o menu ao lado convidava a sair pelo meio para uma
  tela que ainda não faz sentido. Usa `setupWizardTypes.ts` para
  o estado central e `setupWizardMocks.ts` para hardware, nós, modelos,
  capabilities, redes públicas e saúde simulados; os seis caminhos terminam
  no mesmo resumo e não fazem chamadas ao backend; ver `TODO.md`, item 22. O `Setup.tsx` continua
  sendo a tela real de bootstrap quando `/api/ui/node` retorna 404.

- `Setup.tsx` - Wizard real de primeiro acesso, na identidade do portão de
  acesso (painel escuro com a marca e a trilha de passos animada como os nós
  do site). Três passos para o administrador (Início, Rede, Convite) e dois
  para nó e inferência (Início, Entrar), definidos em `setupWizardFlow.ts`.
  Pergunta só papel, nome da máquina, nome e visibilidade da rede (privada ou
  pública; país e cidade só na pública) e, para quem entra, a raiz e a chave -
  trazidas pela descoberta local (`LocalDiscoveryPanel`) ou coladas em
  `TrustFields`, que só monta quando o bloco manual é aberto. No convite, cada
  computador que procura a rede aparece em `DiscoveredMachineCard` com nome,
  sistema, IP (da LAN, só no wizard local), CPU, RAM e GPU. The admin's
  preferred address is optional and never pre-filled: the network is found by
  the principal's key, and neither the trust root nor joining from Public
  networks brings an IP bootstrap. Pastas, disco, reservas, senha (`admin`) e
  workspace (`~/pom_workspace`) são padrões ajustados depois. As telas conversam
  com `/api/setup/*`; não confundir com o protótipo `SetupWizard.tsx`. Ver
  `docs/subsystems/web-ui.md`, "Wizard de primeiro acesso".

- `PublicNetworks.tsx` - Diretório full-bleed de redes públicas, no mesmo
  padrão de Logs: tabela rolável com nome público, organização, localização,
  label geo-hierárquico, alcance, nós, latência, VRAM, modelos e ação de
  entrada. Usa `/api/public/networks`. **No IP is shown**: reachability comes
  from `usePublicLocator` (`lib/publicLocator.ts`), which asks the node
  (`/api/public/networks/locate?key=<publisher_key>`) whether the publisher key
  has a direct route, only a relay or no pkarr record at all; the details show
  the publisher key and the `relay_hints`. Métricas agregadas só aparecem quando o cluster local
  corresponde à rede exibida, pois o catálogo público atual não as fornece.
  Os filtros de país e cidade são derivados do próprio catálogo; ao escolher um
  país, o combo de cidade mostra apenas as cidades daquele país.
  O nome de cada rede abre um `Modal` com todo o registro público, incluindo
  descrição, regras e o campo único de canais Discord ou Telegram renderizados
  como Markdown; a confirmação de entrada continua sendo uma ação separada.
  O documento assinado fica em `sessionStorage` só durante a transição; o nó sai
  da rede atual e o wizard real retoma com a rede pública pronta para revisão.

- `NetworkSettings.tsx` - Mantém o retrato operacional da rede separado das
  preferências locais **e** da identidade desta máquina, que vive em
  `MyMachine.tsx` (rota `/config/minha-maquina`). Node e infer recebem uma
  leitura somente leitura com nome, descrição, regras, organização, canais,
  visibilidade e localização; um administrador autorizado pode editar o bloco
  completo. A edição grava no nó, propaga o envelope assinado aos peers e
  atualiza a descoberta pública. País e cidade usam a referência de geografia
  do nó em selects, sem expor o código do país. `TraversalPanel` mostra o campo
  `traversal` de `/api/ui/cluster` - estado do AutoNAT, endereços diretos e
  circuitos publicados, reservas de relé e hole punches - porque dois nós atrás
  de NAT que não se enxergam falham num de três degraus e antes disso descobrir
  qual exigia log de depuração na máquina certa; nó antigo não manda o campo e
  o painel não aparece. A tela também agrega nós,
  latência média, VRAM e modelos conhecidos. A lista de participantes foi
  movida para a rota `/config/rede/participantes`, apenas para `admin` e
  `node`. O painel de informações também oferece `Esquecer rede`, que pede
  confirmação e chama `POST /api/ui/network/leave` para limpar a configuração
  local e reiniciar no setup - continua aqui porque é sobre a rede, não sobre
  esta máquina. No Windows, quando suportado, a mesma tela mostra o estado das
  quatro regras de firewall derivadas da porta P2P efetiva e oferece `Tentar
  novamente` após uma recusa do UAC; essa ação é independente da implantação.
  Descrição, regras e o campo único de canais Discord ou Telegram são editados
  como Markdown com prévia ao vivo e, quando a tela é somente leitura, usam o
  mesmo renderer Markdown compartilhado pelo Chat.
- `Plugins.tsx` - `/plugins` combines the public catalog at
  `https://license.pieceofmind.cloud/v1/plugins` with local state from
  `GET /api/ui/plugins`, using `code` as the identity. The public client lives
  in `lib/pluginCatalog.ts`, sends no node credentials and accepts plugins
  with a compatible commercial release. Missing image data uses placeholders;
  up to four screenshots open in an Escape-dismissible lightbox that restores
  focus. Descriptions use Brazilian Portuguese when available and otherwise
  fall back to English. License validity, granted plugins, activation
  candidates and usage by `plugin_code` come from the node's authenticated
  state, never the public catalog. The full-bleed screen follows the header and
  inner container used by `/modelos`. `state` represents license validity;
  `active` is true only after the host completes its handshake and is ready.
  Local state may publish `installation_error`, shown on the card with an
  actionable detail; a failed activation refreshes status before the screen
  exits. Active local plugins remain visible even when absent from the public
  catalog.

  Deactivation requires confirmation. The license-key form appears whenever
  any public entry requiring a license is not active locally, even if another
  plugin already has a license or context. It also appears when local status is
  missing or expired/revoked, and is hidden when all known licensed plugins
  are active. Purchase redirects licensed plugins to the public store with an
  encoded `plugin=<code>` parameter via `lib/pluginPurchase.ts`. Cards are
  compact, two per row from `xl`, and grouped by `plugin_code`: the commercial
  catalog, GitHub repository and local state for the same code share one card.
  The right column contains the "Add from GitHub" card (public repository URL)
  above license activation. A repository card shows its manifest description
  and screenshots (`raw.githubusercontent.com`), refresh/remove icon actions,
  and a selector containing only releases compatible with this node
  (`available_for_node`), defaulting to the latest. Its action is Install,
  Update (when the public installation came from that repository), or disabled
  Installed (when the selected tag is already current). Install/update requires
  confirmation; this source does not check `requires_license` or contact the
  license server. The card does not show features or the last update check, and
  the commercial release row appears only when the catalog has a release for
  the node's target. Activation, installation, update and deactivation
  invalidate `keys.pluginUi` so the plugin menu updates immediately; 30-second
  polling covers changes made outside this tab. One form activates every
  `plugin_code` granted by the server in a batch (`POST
  /api/ui/plugins/activate`, response `{activated}`); updates and deactivation
  remain per-plugin. Preferences open in a modal from the card's settings
  icon, only when the plugin is active and its installed release has a schema.
  The modal displays typed boolean/directory defaults, sends values only to the
  node's local route and shows the node's reason on failure.

  Updating the `pom` binary is separate in `About.tsx` (see
  `crates/node/src/pom_core_update.rs`). With profile `admin` and
  `NodeInfo.pom_desenv=true`, the screen queries local sources and allows
  building and installing even inactive plugins. A local build is separate
  from license state, grants no licensed features and does not activate a
  license; an existing licensed/GitHub installation keeps its entitlement.
  The action calls the AdminOnly route and shows success or the build
  stage/exit code without compiler output, and does not use the GitHub release
  route. The page has its own Settings > Plugins menu entry. Commercial release
  data is selected for the local node's `system.os`/`system.arch`; if the
  catalog does not publish that target, the screen shows no release rather than
  suggesting one for another platform.
- `MyMachine.tsx` - "Minha Máquina" (rota `/config/minha-maquina`, entrada
  própria no menu Rede, acessível também ao `infer`). Contém exatamente o que
  saiu de `NetworkSettings.tsx`: o painel "Esta máquina" (hostname, `node_id`,
  portas, perfil, autoridade efetiva, e `Sair da rede`/`Voltar para a rede` via
  `POST /api/ui/leave`/`/api/ui/rejoin` - abdicação, não privilégio, disponível
  a todo perfil) e "Endereço anunciado" (`POST /api/ui/network/advertise-addr`,
  mostra o endereço efetivo usado pelos peers). `Sair da rede` aqui é diferente
  de `Esquecer rede` em `NetworkSettings.tsx`: um desliga a máquina do cluster,
  o outro apaga a configuração local e reinicia no setup.

- `Chat.tsx` - The chat: `POST /v1/chat/completions` with `stream: true`,
  exactly what a harness gets. Imports `../lib/chatStore` (sessions, localStorage
  and the shared active-session controller) and `../components/ChatSessionList`,
  `../lib/chatStream` (leitura de deltas SSE, erros e métricas), `../lib/chatScroll`
  (`isNearBottom`), `../lib/api` (`streamSse`, `ApiError`), `../lib/queries`
  (`useCluster`, `useNode`, `usePeerRuntimes`, `useChatCapabilities`,
  `useChatTelemetryNetwork`, `useServableModels`, `useInferenceHeads`),
  `../components/Markdown`,
  `../components/ui`, `../lib/toast`. The `/v1` call is signed with the
  connection key by `api`. When the stream finishes, it captures the cluster
  snapshot, directional RTT for each adjacent execution pair and each peer's
  inference health. It also measures browser dispatch-to-first-token TTFT
  separately from the server-reported pipeline TTFT and stores it only in the
  client-side timing view. `ExecutionEnvironment` keeps requested configuration
  separate from effective layer placement and shows `POM_PROFILE_SYNC` only
  from the explicit per-peer health field; health shows the selected path,
  while the request trace shows the executed engine and mixed/mismatch flags.
  The runtime record keeps stable identity between unchanged polls so the
  telemetry snapshot cannot trigger a render loop that freezes navigation.
  At the end of a stream, `telemetryCapture` carries the trace, timings and
  source to `Bubble` while the immutable transcript patch is settling;
  `telemetrySnapshots` keeps that complete capture keyed by message id, and
  `message.trace`/`message.timings` remain the preferred values once available.
  The quick timing summary stays outside the closed details card, so every
  assistant message keeps its summary and full telemetry without a reload.
  Each hop's backend telemetry separates the llama.cpp evaluation call from
  explicit backend synchronization.
  O perfil `infer` oculta desta tela a origem, os resumos de tempo e
  throughput e o painel detalhado de telemetria; isso é apresentação, não
  autorização.
  No mesmo composer, `InferenceHeadChoice` mostra por qual cabeça de camada 0
  a próxima inferência entrará para qualquer perfil com modelo selecionado,
  mas só aparece quando há mais de uma candidata. Deixa explícito quando não
  há candidata ou quando a seleção caiu no fallback sem RTT fresco.
  A escolha do combo é controlada pelo `ChatScreen` e segue na próxima chamada
  como `inference_head`; o backend valida o `node_id` e preserva a cabeça no
  plano e no `x_pom.source` da telemetria.
  O botão `Think` só aparece quando `/api/ui/chat/capabilities` retorna
  `thinking: "supported"`. Seu valor é salvo em `ChatSession` e enviado como
  `thinking_enabled` a cada mensagem, persistindo até nova ação do usuário.
  Missing snapshots stay visible as missing data. The environment card also
  renders the effective MoE executor and its concrete reason from `/health`.
  Phase rows also render hidden-state bytes sent/received and the effective
  f32/f16/f8/i4 dtype. The conversion row separates peer and client f32/f16
  encode/decode work from the general frame timings. Its picker uses
  `../lib/chatModels` to hide deployment-qualified
  ids from chat presentation only; deployment screens and backend model
  resolution retain those ids. Um frame de erro compatível com OpenAI dentro
  do stream fica junto de qualquer resposta parcial e mostra a mensagem exata
  do servidor; o texto genérico de resposta vazia só é usado quando nenhum
  erro foi enviado. Erros longos quebram linha, continuam selecionáveis e têm
  um controle de cópia localizado.
  A rota `/admin-ui/chat` reutiliza a mesma tela sem o shell administrativo para
  caber no popover nativo de 390 x 844 pontos. `OpenNativeChatWindowButton` é
  deliberadamente invisível no navegador comum e só publica `open` na ponte
  WebKit `pomOpenWindow`, que abre a janela maior sem criar um fluxo paralelo de
  autenticação.
- `NetworkChat.tsx` - Página própria `/network-chat`, com o mesmo padrão visual
  do Chat de IA. It reads the
  authenticated snapshot and sends text, file and typing actions through the
  node's libp2p-backed gateway. It resolves `node_id` to the node hostname/name
  map used by the dashboard, downloads persisted file blobs on demand, and
  polls typing state without touching the AI session store. Message and typing
  ids come from `lib/id.ts`, so sending over plain HTTP does not call the
  secure-context-only `crypto.randomUUID` API. The message viewport follows new
  messages while it is near the bottom, and the page uses the full-bleed
  `PageHeader` pattern instead of a rounded outer panel. No
  celular, participantes ficam ocultos até o botão `Ver online` ser acionado;
  a entrada da Sidebar recebe o contador de mensagens não lidas e a abertura
  da rota marca o snapshot atual como lido.
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
  - **Mobile is two screens sharing one address.** Below `sm:` the nav column
    takes the full width and the reader is hidden (`mobileReaderOpen` state);
    picking a guide/doc entry opens the reader full-screen with a `Voltar`
    button pinned at its top (`sm:hidden`, only mounted while open). `Voltar`
    only flips that local state - it never touches `?doc=`/`?guia=`, so the
    selection survives the round trip. A deep link present at mount
    (`?doc=`/`?guia=` already in the URL) counts as that same selection and
    opens straight into the reader. From `sm:` up both columns render side by
    side regardless of the flag, exactly like before.
  Search filters label, path *and* body, and replaces the tree with a flat
  result list whose hint is the section name, not the file. Test file
  `Documentation.test.tsx` covers the contract: guides first (a person must not
  land on agent markdown), the guide->reference deep link, no `.md` or
  `AGENTS.md` in the list, section header opens the overview, collapse works
  while holding the selection, the two scroll containers, `?doc=`/`?guia=` deep
  links, search + clear, empty state, and the mobile nav/reader/Voltar
  contract. Loading the embedded set requires
  `src/docs/` to exist, which `npm test`'s `pretest` hook guarantees.
- `About.tsx` - Tela informativa em `/config/sobre`, última folha de
  Configurações. Mostra o `LICENSE` AGPL-3.0 do repositório e a página oficial
  GNU, o aviso de risco/LGPD e links para o site (`pieceofmind.cloud`), contato
  (`contact.html`) e repositórios oficiais. É a ÚNICA tela que trata da atualização do binário
  `pom` em si: verifica
  sob demanda pela rota autenticada `/api/ui/pom-update/check`
  (`usePomCoreUpdateStatus`), e quando há release disponível, mostra e aplica
  ali mesmo (`/api/ui/pom-update/apply`) - não delega mais para a tela de
  Plugins. Mostra também, à parte, o estado do plugin `enterprise`
  (`useEnterpriseStatus`) como contexto de por que a atualização do pom pode
  estar indisponível (o mecanismo hoje empresta o lease desse plugin - ver
  `crates/node/src/pom_core_update.rs`); ativar/desativar plugins é feito em
  `/plugins`, nunca aqui. Também lista as dependências diretas do frontend e
  do workspace Rust agrupadas pela licença declarada, com versão, ecossistema,
  filtro por texto e link do repositório. O catálogo é
  `../lib/licenses.generated.ts`, gerado por `scripts/build-licenses.mjs`; o
  teste em `About.test.tsx` cobre a licença, o aviso e links, os dois
  ecossistemas, o filtro e a verificação/aplicação sob demanda da atualização.
- `Dashboard.tsx` — Cluster overview: the admin sees cluster
  coverage/machines/model/in-flight stats; node and infer see local model,
  slice and in-flight requests instead. Admins see the Add button only when
  `ClusterView.active_model` is nonempty. With no active model, Details is the
  initial view; with an active model, Flow remains the initial view. Manual
  view selection overrides that default. For administrators, the deployment
  properties inspector selects the `is_default` row for the active model,
  preferring a serving default and falling back to the live or latest local
  record so old deployment history cannot hide the map toggle. A graph view
  ("minima",
  `ModelNetworkCanvas`) or a details grid ("estendida") is used where the
  profile permits it.
  The model graph receives directional transport measurements and
  `pipeline_routes` for every complete deployed route. It renders the
  distributed cycle as Gateway -> peers -> Gateway, orders route cards by
  the backend's per-token `total_ms` (network in the chosen return mode plus
  compute; never the raw sum of legs) with node ID as a stable tie-breaker, and uses the least-cost
  route for the headline while keeping alternatives visible. An observed
  `pipeline_cost` from a completed chat execution remains available when every
  execution leg and compute measurement is known.
  O detalhe local diferencia `idle` de `serving`: o primeiro mantém o nó
  ativo, mas informa que o modelo foi descarregado e será recarregado no
  próximo pedido.
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
  ("Nenhuma máquina na rede"). Derives the active `deploy_id` from
  `useDeployments` and passes it to the add/remove dialogs. The add dialog
  fetches the selected target's catalog and cache options from
  `POST /api/ui/deploy/node/options`; it no longer uses a fixed cluster plan.
  The add trigger, dialog and `UndeployPipeline` are
  rendered only for `profile: "admin"` (from `useProfile()`, the one source -
  the context is fed by `/api/ui/node`). The details view itself is gated the
  same way, by `canSeePeerDetails` (`../lib/access`): outside `admin` the view
  is forced to the Flow **and the `ViewSwitch` is not rendered at all**, because
  a single available view is not a choice. Uses
  `../components/{LayerRibbon,PipelineRail,PeerDetailCard,UndeployPipeline,AddNodeDialog,ModelNetworkCanvas,ui}`,
  `../lib/api`, `../lib/networkPeers`, `../lib/queries`, `../lib/types`
  (`PeerView`, `DeployPlan`). The per-machine actions that used to live here
  moved to `PeerDetailCard`: `RemoveNode` (out of the model, only when serving
  a slice) and `LeaveNode` (out of the whole network, always). The
  development-only update-and-restart action is not rendered in the card. Next to
  "Adicionar nó" there is a `UndeployPipeline` button for an administrator
  when a model is live - `undeployTargets` is the list the confirmation shows
  (peers on `group` with `total_layers > 0`, sorted by start layer); the node
  re-scans the cluster before acting, so the list is just the preview. The flow
  canvas receives the
  directional RTT measurements for its adjacent routing edges and complete
  pipeline routes from `useNetworkRtt`, with the same badge presentation used
  by deployment. A whole-model deployment with one head still queries this
  endpoint even though its graph has no routing edge.
  A versão ativa do deployment também pode ser aberta num inspetor de leitura
  à direita, usando o mesmo layout e os mesmos valores configurados no Deploy
  Studio. No perfil `admin`, `DeploymentLogDock` fica recolhido no rodapé e
  expande para cima com os logs em tempo real e auto-scroll.
- In `Dashboard`, node and infer profiles do not reuse the administrator's
  cluster verdict: they show the local model, its real layer slice and local
  in-flight request count. A missing `/api/ui/node` response is rendered as
  unavailable, never as an empty model, incomplete pipeline or zero.
- `DeployModel.tsx` — "Implantar na rede". Full-bleed Deploy Studio with a
  top mode bar, separate technical model and operational deployment name,
  central peer map and compact property inspector. Tensor mode, Simple/Complete
  strategy and "Voltar ao plano do nó" are visible only when `pom_desenv` is true.
  (`../components/DeploymentInspector`, o mesmo componente que o Histórico usa
  em somente leitura) and draggable bottom peer strip. O que continua no
  `DeployModel` e não migrou para o inspetor são as seções que só a tela de
  implantar tem: grupo TP, tensor paralelo e a aba da máquina. Three shapes, chosen by the
  mode bar at the top and carried to the node as `kind`: **a set of shards**
  (below), **a whole model** (one model picker from `../lib/wholeModels`, with
  the deployment map as its only machine choice and no coverage
  ribbon/"included"/merge), or **tensor parallel** (`tensor_parallel`): when the catalog
  has `tp_parts`, the picker groups the round by TP width and the inspector
  shows one-based ranks such as `1of2` and `2of2`; otherwise it uses the
  whole-model runtime path. Canvas membership is canonical: rows plus preferred
  peers, with every live peer added by the Complete strategy except in whole
  mode, which remains limited to one machine. Tensor selectors
  reuse those map cards; pre-sliced TP accepts any mapped peer because the node
  can transfer the needed rank, while whole-model TP filters mapped cards to
  peers that hold the local GGUF. Eligible Tensor cards expose the accessible
  primary toggle, which removes that node from workers.
  `size` is derived from the selected TP group or `workers.length + 1`, never a
  typed number, and the node derives the reduce URL from the primary's
  inference port (the operator no longer types `rank0_url`). `tp` is in the
  plan queryKey so changing primary/workers re-runs validation. A plan with no `kind`
  is read as `"shards"` (`kindOf`), so an older peer mid-upgrade does not
  silence the screen. Deploy memory is namespaced `whole:<name>`, because a
  shard set and a model routinely share a name. Tests:
  `DeployModel.whole.test.tsx`, `../lib/wholeModels.test.ts`.
  Em modelo inteiro, o inspetor não repete os fatos do GGUF nem oferece um
  seletor de máquina: o mapa é a única escolha. Ao trocar de fatias para esse
  modo, a tela limpa o mapa e deixa o operador adicionar a máquina desejada;
  depois disso, tentar incluir uma segunda máquina mostra o aviso e preserva
  a escolha única.
  On the first tensor request, `tpPayload` uses the first rank-0 holder as a
  provisional primary so the map can load; workers remain opt-in map members.
  A lista de modelos pode aparecer antes do fim da leitura dos cabeçalhos GGUF.
  Esse estado global não desabilita a tela: o plano é que decide se a seleção
  atual tem os metadados necessários. Se uma faixa selecionada ainda não pode
  ser resolvida, o backend devolve um único `blocked` específico para essa
  seleção; um modelo não escolhido, peer ausente ou peer sem o campo
  `metadata_complete` não bloqueia a implantação.
  For a real MoE model, identified by `DeployPlan.is_moe` from GGUF metadata,
  the Globais inspector shows the inference engine selector and sends
  `RuntimeParams.inference_mode` in whole-model, sliced and TP requests. Dense
  models hide it. A MoE Tensor Parallelism plan keeps the selector visible but
  is blocked with the backend's explicit unsupported reason, so the UI cannot
  imply that the request will use another engine silently.
- `DeployModel.tsx` - the slice shape: picks a slice set (`group`), lets the
  planner place ranges on the central map, and exposes a 19rem collapsible
  inspector. Its colored global section includes context, batching, sampling,
  repetition, cycle detection, fit-memory, hidden-state dtype, execution toggles and the installation-wide KV cache
  types. The hidden-state selector appears only for a sliced pipeline crossing
  peer boundaries and offers f32/f16/f8/i4, with f16 as the default; the route
  test checks that f32 reaches the deploy request. The two K/V cache combos apply changes immediately through
  `POST /api/ui/cache-types`; the save button, subtitle and `global` badge are
  intentionally absent. The MTP toggle seeds `mtp_min_probability` at `0.75`; the operator can
  then edit that probability before deploying. The machine section includes
  The MTP section also exposes the safe-by-default RAM fallback switch, so the
  operator can refuse CPU drafting after measuring it. The machine section includes
  GPU/KV overrides, machine capacity and the available layer ranges that can
  be assigned to the selected peer. Choosing a range replaces that peer's
  active range; the same range on a second peer remains a replica. Coverage is
  a compact footer indicator. The inspector uses Global/Machine tab bars.
  O inspetor começa diretamente pelas abas Globais e Máquina, sem o título
  anterior e sem padding externo; alertas de capacidade aparecem como ícones
  âmbar ao lado do controle e abrem a mensagem inteira por hover, foco ou
  clique, inclusive quando a origem contém identificadores sem espaços.
  `fit_memory` is on by default for shards and whole-model deployment, and the
  screen disables it for tensor parallelism because llama.cpp cannot auto-fit
  `split-mode tensor`.
  Cada botão numérico do inspetor carrega um `InfoTip` (`[i]` no hover/foco/
  toque) com o que o parâmetro faz e como afeta a inferência; os controles que
  usavam `<label>` viraram `<div>` com `aria-label` no `<input>` porque um
  `<button>` dentro de `<label>` quebra o nome acessível.
  Os cards do mapa são um **preview do pós-deploy**, não do estado atual: com
  `machine.capacity` no plano, mostram VRAM projetada por GPU que se recalcula
  a cada ajuste de contexto/lote/sessões (mesmo debounce). Três estados:
  verde (cabe), amber (`degraded` - a conta crua não cabe, mas o `fit_memory`
  pode aparar o pedido; é heurística), rósea (`!fits` - a conta não cabe sem
  ajuste automático). Amber e
  rósea abrem um `Confirm` (`capacityAsked`, separado do de cobertura parcial)
  nomeando as máquinas e a razão, com título/texto conforme o caso;
  `canDeploy` não muda - é aviso, não bloqueio.
  Topology edits are debounced 260 ms and validated by
  `POST /api/ui/deploy/plan`. Runtime parameters, per-machine GPU/KV overrides
  and the operational name stay local while editing: their changes do not
  restart that cluster-wide survey or reuse its capacity projection. The plan
  query key contains only layer ranges and target machines, while the deploy
  request carries the current overrides. `POST /api/ui/deploy` re-surveys the cluster and
  validates the current parameters before acting, returning a job shown through
  `useJobs`. A failed preflight keeps the backend's reason visible in the rose
  job banner, including the peer that stayed occupied.
  Restores the last deploy from localStorage via
  `../lib/deployMemory` (`recallDeploy`/`rememberDeploy`/`usableSelection`),
  after the first plan, pruned against what still exists.
- The TP inspector never offers pipeline layer buttons. It lists available and
  selected ranks, while `DeployPlan.tp_targets` is the backend source of truth
  for the exact pre-sliced file and rank destination.
- `DeployModel` sends `strategy` and `preferred_node_ids` to the planner;
  `deployment_name` and `RuntimeParams` are sent with the deploy request. The
  map is explanatory: capacity/load come from the live
  cluster and RTT comes from the directional survey in the plan;
  adding a peer without a range only adds its card and keeps existing positions;
  in whole-model mode, the map has one machine only and adding another peer is
  refused with a warning;
  in pre-sliced tensor mode, adding a peer makes it eligible for a rank even
  without a local rank file, while whole-model tensor mode still requires the
  local GGUF; neither path adds an off-map peer implicitly;
  assigning a range is what validates a new selection. Choosing a range removes
  an empty placeholder for that same range, while an already assigned copy on
  another machine remains as a replica. Removing a card deletes
  that peer's routes and returns it to the bottom strip. `DeployNetworkCanvas` reutiliza os controles do
  canvas do Painel, desenha nós por faixa, pontes somente entre estágios
  consecutivos e alternativas com RTT direcional, orienta a curva pelos lados
  enfrentados dos cards, mantém métricas de GPU/RAM/carga atualizadas pelo cluster e não
  repete no rodapé peers já desenhados. Em viewport estreito, o mapa ganha
  altura mínima e rolagem interna para não cortar cards nem criar overflow na
  página. Arrastar ou dar duplo clique adiciona
  um peer disponível ao plano; a atribuição de uma faixa disponível edita
  somente aquela seleção, preserva as faixas existentes no peer como réplicas
  explícitas e volta ao backend para validação.
- A lista de rodadas de fatias usa `ShardSet.group` como chave técnica e
  `model_label` como texto; quando há duas rodadas do mesmo modelo, o seletor
  mostra o prefixo do `pom_id` para desambiguar sem trocar o grupo enviado ao
  backend.
- As opções globais condicionadas pelo modelo usam `DeployPlan.capabilities`:
  `unsupported` fica oculto; `unknown` permanece visível, desabilitado e
  explica a fonte e o motivo. `no_embeddings` é a exceção de produto:
  permanece visível, e o backend valida atomicamente a topologia por peer antes
  de aplicar o controle.
- O fechamento do inspetor é uma barra visual de 3px em altura total, com uma
  área de clique e foco maior e invisível. Ela fica fora do scroll do conteúdo,
  então continua ancorada na altura da tela durante rolagem e resize; ao fechar,
  o controle de reabertura permanece na coluna compacta existente.
- `Deployments.tsx` - lista as implantações registradas no cluster via
  `useDeployments`, diferencia "no ar" de "padrão", ordena o histórico pela
  última ativação (`deployment.activated_at`) e mostra a coluna "Última
  ativação". Tela cheia, como a de Logs:
  cabeçalho fixo, tabela rolando por dentro e, quando há versão escolhida, um
  inspetor à direita (`lg:grid-cols-[minmax(0,1fr)_minmax(19rem,24rem)]`).
  Uma implantação ativa com todos os peers vivos ociosos aparece como
  `idle`, separada de `noAir`.
  Clicar na linha de uma versão abre o `DeploymentInspector` em `readOnly` com
  `version.params` - o mesmo inspetor da tela de implantar, sem duplicar a
  lista de propriedades. O cache KV **não** entra ali: é da instalação e do
  momento, não da versão, e mostrar o valor de hoje ao lado do que subiu ontem
  seria inventar um registro que ninguém guardou. Cada linha expande
  (`VersionList`) e mostra `deployment.versions` da mais nova para a mais
  antiga, com a badge "Ativa" em `active_version` e um botão "Reimplantar esta
  versão" (`redeployDeployment(id, { version })`). O botão "Reimplantar com
  novos parâmetros" abre o `RedeployDialog`, que semeia do
  `versions[active].params` (não do `nodes[0].config`) e no submit manda só
  `params` + `per_node` - o backend decide reusar ou anexar versão. Também
  ativa uma implantação e a desimplanta pelo `deploy_id`. O painel do job de
  reimplantação mostra `job.outputs` (`"<host>: <motivo>"`) abaixo da mensagem.
  Teste: `Deployments.test.tsx` (expansão, badge da ativa, `{version}` na API).
- `DeploymentLogs.tsx` - consola full-bleed do log de processo do cluster em
  `/implantacoes/logs`. Consulta a réplica local do admin, atualiza a cada
  segundo, mantém a ordem de chegada devolvida pela API e pode filtrar por
  `deploy_id`; mantém apenas o terminal em preto, acompanha o fim enquanto o
  leitor está no fim, pausa o acompanhamento ao rolar para cima, oferece um
  botão de estado visível para reativá-lo e copia as últimas 500 linhas. Não é a
  auditoria de requisições, que é do plugin `enterprise`.
- `Keys.tsx` - Two screens in one file. `TokenKeys` delega a tabela full-bleed
  `ApiKeysTable`: per-node API tokens for
  harnesses - `GET/POST /admin/api-keys`, `DELETE /admin/api-keys/:id`. The
  list carries only `key_hint` and the tracking id; the secret appears once,
  in the `IssuedSecret` window right after creation.
  No perfil `infer`, mostra só a chave ativa deste nó, esconde o seletor de nó
  e, ao gerar uma chave nova, revoga a anterior. As rotas de API key são
  `infer+node+admin`: são a credencial de quem atende `/v1`, e todo perfil
  atende. `ConnectionKeys`: the cluster operator key, read from
  `NodeInfo.connection_key`. Ver a chave é de quem opera esta máquina - ela já
  está no `pom.json` dela e a tela só existe atrás da fronteira administrativa.
  **Trocá-la** (`POST /admin/regenerate-key`) é outra coisa: a chave é a mesma
  no cluster inteiro e girá-la corta todos os outros nós, então o botão só
  aparece com `authority_level` presente. O administrador escolhe o nó ao
  criar e vê os próprios tokens completos, enquanto os tokens de outros nós
  ficam mascarados; node e infer veem e criam somente os seus.
- Logs de Requisições e Estatísticas **não existem aqui**: são telas do plugin
  `enterprise`, entregues por `pom-plugin-logs` pelo mecanismo de UI de plugin
  (`docs/subsystems/plugins-ui.md`). `App.tsx` mostra o fallback
  `../components/EnterpriseFeatureShowcase.tsx` quando o plugin não está ativo.
- `ModelPaths.tsx` - edits `model_dirs`, `shard_dir` and `shard_max_gb` via
  `POST /api/ui/paths`. The Armazenamento screen rejects a slice folder equal to
  or inside a whole-model folder, shows current usage and the slice limit, and
  keeps `/config/pastas` as an alias.
- `NetworkSettings.tsx` - This node's membership and network information
  screen. Its `NetworkParticipants` export reuses the known-node area at
  `/config/rede/participantes`; the route is allowed only to `admin` and
  `node`, and block/unblock mutations remain admin-only. Each peer row shows
  the route actually in use (`AdminNode.route`: `RouteBadge` + `routeAddress`),
  never the advertised `addr`, which only goes into the row's `title`. A
  relayed route names the relay kind (`relay_kind`: dedicated relay or member
  relay, the last rung of the ladder).
  (`POST /api/ui/leave` / `/api/ui/rejoin`, reachable by every profile: leaving
  is abdication, not a privilege) and the known-node list from
  `GET /admin/nodes` (`node+admin`: reading the roster is what this screen
  shows). Block/unblock (`POST /admin/nodes/:id/block|unblock`) stays
  `admin-only` and the buttons are rendered only when `authority_level` is
  present - `profile: admin` alone grants nothing. The "Papel" row was replaced
  by the effective authority: the old `NodeInfo.role` left `/api/ui/node` with
  the trust model and was rendering an empty cell.
  O perfil `infer` também abre `/config/rede`, mas o backend devolve somente a
  própria máquina e oferece `POST /api/ui/network/request-node`; o pedido
  aparece no sino para aprovação de qualquer admin. Um `node` pode descer para
  infer por `POST /api/ui/network/relinquish-node`: novas inferências saem do
  encaminhamento, as atuais drenam e só então a fatia é liberada. O
  administrador principal pode promover um nó via
  `POST /admin/nodes/:id/promote-admin`.
- `AdminAuthority.tsx` - Admin authority screen backed by
  `GET /api/ui/network/admin-authority`. It shows the principal, secondary
  administrators, promotion chain and resignation eligibility. Its mutations
  are isolated in `lib/adminAuthority.ts`; pending principal-transfer and
  infer-to-node offers are answered by `Notifications`.
- `TrustAnchor.tsx` - a raiz de confiança da máquina e os dois lados de ancorar
  a autoridade, atrás de `GET/POST /api/ui/network/trust-anchor`. Mostra a
  impressão digital da âncora em grupos de oito com botão de cópia, e o estado
  **ancorado** ou **por ancorar** num relance. Num nó ainda sem raiz, mostra o
  formulário que cola a cadeia assinada, calcula a impressão digital e mantém a
  confirmação explícita da conferência pelo canal confiável. Fora do loopback o formulário dá lugar à
  explicação; a decisão continua sendo do servidor, que devolve um `reason`
  estável (`not_loopback`, `already_anchored`, `invalid_document`,
  `fingerprint_mismatch`, `install_failed`) para a tela traduzir a recusa em
  vez de repetir o texto do servidor. Vive fora do gate de perfil - ver o
  cabeçalho do arquivo e `lib/access.ts`. Teste: `TrustAnchor.test.tsx`.
- `Preferences.tsx` - Local settings screen under Configurações: owns the
  administrative exposure mode and gateway port, accepts a password only in
  network mode, and saves its Argon2id hash to `pom.json` through
  `POST /api/ui/access`. It also mounts `TlsPanel`, which moves the cluster TLS
  choice out of Network and explains when it is useful, plus `ThermalPanel` and
  `HuggingFaceSettingsPanel`. Também monta o interruptor de logs do cluster,
  disponível apenas ao principal e aplicado em tempo real, e `QuicPanel`, que
  alterna a preferência HTTP/3 em `POST /api/ui/network/quic` com o mesmo
  controle de autoridade. The access password is a human credential and is not the
  cluster `connection_key`. A rota `/config/preferencias/admin` renderiza
  somente TLS, QUIC, logs de processo entre nós e chat da rede, além do
  interruptor de descoberta local, desligado por padrão; a rota normal mantém
  exposição, Hugging Face e proteção térmica; Armazenamento só tem entrada no menu.
- `NotFound.tsx` — 404 fallback with a link back to `/`.
- `Forbidden.tsx` — a tela que o perfil ativo não abre. Não é `NotFound`: o
  endereço existe, o que falta é o perfil, então o texto nomeia o perfil ativo e
  manda usar o nó autorizado em vez de mandar procurar defeito de rota. Quem a
  escolhe é o guarda em `../App.tsx`, pela tabela de `../lib/access.ts`.
- `SendFile.tsx` - offers a `.gguf` from the local catalog via
  `POST /api/transfer/send` (`{to_node_id, filename}`). The UI displays the
  friendly name and full path, but sends only the basename; the backend resolves
  it again and the receiver sees only an opaque grant. Full-bleed shell
  (`FullBleedHeader` + scrolling body), same contract as `Deployments`/
  `PublicNetworks`.
- `Shards.tsx` - "Fatias": every local slice always listed (the Nodes screen
  only shows the difference), with who is missing it. Pipeline `parts` and TP
  `tp_parts` are separate sections; TP groups are ordered by group size and
  rows use the one-based labels `1of2`, `2of2`, and so on, with the zero-based
  rank shown alongside. Sends via `POST /api/transfer/send`; deletes via
  `../lib/deleteModel`. `uniqueName` disambiguates same-hostname peers with
  the node_id prefix. Full-bleed shell; each `ShardCard` shows the set's
  `mmproj_path` basename (`../lib/format::optionalBasename`) next to the coverage bar,
  or "no projector" when the set has none - never invented.
- `SliceModels.tsx` - "Fatiar": full-bleed shell. Pipeline and Tensor
  Parallelism are a segmented tab pair (`role="tablist"`), not a checkbox -
  each mode owns its own controls (Pipeline: size list, one explicit layer
  range, or Tensor Parallelism's rank count). The Tensor Parallelism tab only
  mounts when `useNode().data.pom_desenv === true` (the same
  `POM_DESENV=1`-gated pattern as `DeploymentInspector`/`RuntimeParamsForm`);
  without it the screen never offers or sends `mode: "tensor_parallel"`, and an
  effect snaps a stale selection back to `"pipeline"` if the flag turns out
  false after the first render. Pipeline builds a by-size plan
  (target GB per slice, empty = "the rest"); a single range uses the GGUF
  architectural starts and ends; TP chooses 2..64 ranks and previews one full
  layer range per rank, labeled `1ofN`. The modes use the debounced
  `POST /api/ui/slice/plan` and start through `POST /api/ui/slice`; TP sends
  `mode: "tensor_parallel"` and `tp_size`, while pipeline sends `slices` or
  `range`. `SlicingJobsPanel` keeps running, completed and failed jobs visible,
  including named phase, bytes and partial output paths.
  For architectures with a required non-final head, the plan includes
  `required_head_bytes`, and the preview identifies the real payload repeated
  in each intermediate slice. `asPlan` keeps this field optional for older
  nodes and for tensor-parallel plans.
  `asPlan` validates both response shapes. Only whole models (`isWholeModel`)
  can be sliced. Multipart models show the summed physical size, every
  declared part and an explicit instruction to keep the set together before
  POM slicing; incomplete sets surface the backend reason and leave the start
  action disabled.
- `Transfers.tsx` — Shows remote model downloads from `useModelDownloads` in
  the same in-progress screen as pending offers (accept/reject via
  `POST /api/transfer/offers/:id/accept|reject`), active transfers (bytes +
  speed + ETA + cancel via `POST /api/transfer/jobs/:id/cancel`), and finished
  ones. Model downloads reuse `ModelDownloadsPanel`, so their backend progress,
  configured destination path and cancel/retry actions stay visible here.
  State classification from `../lib/transferStatus`. `cancelled` é
  mostrada como **recusada**, em âmbar, e não como falha vermelha: foi decisão
  de alguém, não erro. Fatias aparecem por modelo e faixa de camadas
  (`../lib/transferLabel`), com o hash curto ao lado. Full-bleed shell; ofertas,
  ativas e terminadas são cada uma uma tabela de largura total em vez da lista
  de cartões anterior, com cabeçalho fixo e ações na última coluna.
- `WholeModels.tsx` — Full-model catalog from `useLocalCatalog`, one full-width
  table row (`ModelRow`) per model with actions: slice (links to
  `/modelos/fatiar?path=`), send (`/transferencias/enviar?path=`), delete
  (`../lib/deleteModel`). A dedicated "Vision (mmproj)" column shows the
  projector's basename (`../lib/format::optionalBasename`) when
  `DiscoveredModel.mmproj_path` is set, or "no projector" otherwise - the
  catalog already resolves this file from GGUF identity and projection dimensions,
  the screen only renders it. `ScannedRoots`
  lists the scanned folders and per-folder file counts. Full-bleed shell.
- `RemoteModels.tsx` - mostra modelos GGUF em alta pela API pública do Hugging
  Face ao abrir, sem credenciais; a busca autenticada usa debounce em
  `/api/ui/model-provider/search`, com filtro, ordenação, paginação por URL,
  detalhes lazy e confirmação de download. O status de login fica compacto
  dentro da barra de busca, com ação explícita de Buscar. O
  detalhe resolve a licença pelo provider e marca modelos gated antes do
  download. O diálogo mostra as pastas de modelos resolvidas pelo backend. Os
  jobs aparecem no `ModelDownloadsPanel`, também em Transferências, então trocar
  de rota ou recarregar não perde uma transferência. Full-bleed shell; os
  resultados são uma tabela de largura total (nome/tags, estado, downloads,
  curtidas, atualizado, ação), não mais um grid de cartões - `RemoteModelCard`
  foi removido por ficar sem uso.

Key logic carriers:

- `ChatSessionProvider.startStream` (`lib/chatStore.ts`) - snapshots turns
  before the placeholder reply, streams deltas into `consumeChatDelta` (keeps
  reasoning/content split), merges `x_pom` metrics via `finishChatStream`,
  measures browser-local first chunk/token and delivery timing, and persists
  the partial session on a 50 ms scheduler while the stream is active. The
  final or interrupted state is flushed immediately. Route unmounts do not
  abort it; `stopStream` and a new message do.
- `components/TelemetryLine.tsx` plus the `chat.telemetry` slot - each answer
  shows the basic one-line telemetry; when an active plugin claims the slot,
  `Bubble` mounts its component at the end of that line with the props of
  `lib/chatTelemetry.ts` (trace, timings, source, captured environment,
  `onAsk`). Only then does `ChatScreen` fetch the per-node network RTT and
  runtimes for the environment. `onAsk` sends the plugin's question as a turn
  whose visible text differs from the request content.
- `DeployModel` and `Deployments` use `RuntimeParamsForm` for the shared runtime
  knobs, including `prefill_max_inflight_chunks` (prefill pipeline depth, 1..8)
  and `fit_memory_margin_mb` (per-device VRAM safety reserve).
  As duas telas leem `NodeInfo.pom_desenv` de `useNode`; o Deploy Studio, o
  inspetor do histórico e a caixa de reimplantação falham fechados. A flag só
  esconde os controles de desenvolvimento locais; o estado de planejamento,
  lançamento e reimplantação mantém os valores recebidos da implantação,
  mesmo quando este nó não pode editá-los.
- `DeployModel.rowsOf`/`sameRows` (DeployModel.tsx) — the selection model:
  merged steps expand back into per-slice rows and the planner answer is compared
  with the displayed rows after sorting by layer.
- `DeployModel.validated`/`syncing`/`answered`/`whyNot` (DeployModel.tsx) - why
  the button is grey, and who is allowed to say so. `sameRows` compares both
  sides **sorted by layer**: the screen keeps its rows in layer order and the
  node builds its answer its own way, and comparing position by position made a
  plan that described exactly these rows look like a different plan. The
  explanation panels hang off `answered` (the node has replied about *these*
  rows), never off `validated`, so every planner answer can explain the disabled
  button. `whyNot` puts the one-line reason next to
  the disabled button, including the two states with no panel at all (nothing
  selected, answer still in flight). Regression test: `DeployModel.test.tsx`.
- `DeployModel.deploy` (DeployModel.tsx:602) — posts the current rows/settings
  (from `rows`/`eff`, not the nullable edit state), remembers them via
  `rememberDeploy`, and tracks the returned job id.
- `SliceModels.asPlan` (SliceModels.tsx:56) - shape guard on pipeline and TP
  plan responses; the one screen rendering straight off a POST body.
- `Setup.save` (Setup.tsx) - posts the complete first-access payload, including
  trust document, typed fingerprint, resource policy, storage and network
  visibility, then reloads after the node respawns.
- `ModelPaths.save` (ModelPaths.tsx:55) — trims/filters the folder list, posts,
  and re-syncs state from the server's answer (which is authoritative).

Backend endpoints used (see `src/webui.rs` / `src/gateway.rs` / `src/openai.rs`
in the repo root): `/v1/chat/completions`, `/v1/models`, `/api/ui/cluster`,
`/api/ui/cluster/models`, `/api/ui/node`, `/api/ui/models`, `/api/ui/paths`,
`/api/ui/jobs`, `/api/ui/requests`, `/api/ui/requests/stats`, `/api/ui/deploy/plan`, `/api/ui/deploy`,
`/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/leave`,
`/api/ui/rejoin`, `/api/ui/release`, `/api/ui/release/candidate`,
`/api/ui/unload`, `/api/ui/undeploy`, `/api/ui/dev/update`, `/api/ui/slice/plan`, `/api/ui/slice`,
`/api/transfer/send`, `/api/transfer/jobs`, `/api/transfer/jobs/:id/cancel`,
`/api/transfer/offers`, `/api/transfer/offers/:id/accept|reject`,
`/admin/nodes`, `/admin/nodes/:id/block|unblock`, `/admin/api-keys`,
`/admin/regenerate-key`, `/api/setup/hardware`, `/api/setup/models`,
`/api/setup/admin-anchor/preview`, `/api/setup/health`, `/api/setup/save`,
`/api/public/networks`, `/api/public/network`,
`/api/ui/model-provider/search`, `/api/ui/model-provider/details`,
`/api/ui/model-provider/storage`, `/api/ui/model-provider/downloads` e
`/api/ui/model-provider/downloads/:id/{cancel,retry}`.

### Deploy e chat multimodais

`DeployModel.tsx` exibe a opção de visão para modelo inteiro e para conjunto de
fatias quando o plano expõe `mmproj_candidates`. Once the plan has answered,
the options are **only** those of the layer-0 machine
(`currentPlan.mmproj_candidates` and `mmproj_path`); the catalog, which mixes
every machine's paths, only fills the list while the first plan loads, and a
selection that is not among the head's options is replaced. The list is
preselected with the plan's path and lets the operator pick another local
format at deploy time. Without candidates, it shows `mmproj_reason` or a
localized hint. The plan request carries no projector; the deployment sends
`vision` and, only with vision on, `mmproj_path`, and the route redoes the
survey and the plan before validating the path chosen for the head.
`Chat.tsx` usa um único
campo de ficheiro sem filtro de extensão, aceita arrastar e largar com retorno
visual e redimensiona imagens antes de enviá-las ao endpoint OpenAI. Imagens
exigem visão disponível; outros tipos são convertidos para texto no pedido.
A imagem recebe o orçamento efetivo do modelo, usando `context_length` de
`/v1/models` ou o `n_ctx` local como fallback, com quatro bytes por token e
margem de segurança de 10%. Qualidade JPEG e resolução descem até 512 px,
preservando a proporção; a etiqueta `Redimensionada automaticamente` fica no
anexo quando isso ocorreu. A visão e a
cobertura do Chat vêm de
`GET /api/ui/chat/capabilities`, uma leitura agregada da implantação que também
funciona nos perfis `node` e `infer`; recusa ou falha fica como indisponível,
nunca como modelo vazio ou cobertura zero. O mesmo endpoint informa se o
thinking global permite renderizar o botão `Think`; a escolha da sessão vai no
`thinking_enabled` de cada chamada e permanece entre mensagens. Imagens enviadas ficam inteiras na
bolha da mensagem, com legenda e texto alternativo. O botão de ditado só é
renderizado quando o navegador expõe Web Speech e usa o locale da interface.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
