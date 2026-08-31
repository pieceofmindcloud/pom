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
  `../lib/chatStream` (leitura de deltas SSE, erros e métricas), `../lib/chatScroll`
  (`isNearBottom`), `../lib/api` (`streamSse`, `ApiError`), `../lib/queries`
  (`useCluster`, `useNode`, `usePeerRuntimes`, `useChatTelemetryNetwork`,
  `useServableModels`), `../components/Markdown`,
  `../components/ui`, `../lib/toast`. The `/v1` call is signed with the
  connection key by `api`. When the stream finishes, it captures the cluster
  snapshot, directional RTT for each adjacent execution pair and each peer's
  inference health. `ExecutionEnvironment` keeps requested configuration
  separate from effective layer placement and shows `POM_PROFILE_SYNC` only
  from the explicit per-peer health field; health shows the selected path,
  while the request trace shows the executed engine and mixed/mismatch flags.
  The runtime record keeps stable identity between unchanged polls so the
  telemetry snapshot cannot trigger a render loop that freezes navigation.
  Missing snapshots stay visible as missing data. Phase rows also render
  hidden-state bytes sent/received and the effective f16/f8 dtype. Its picker uses `../lib/chatModels` to hide deployment-qualified
  ids from chat presentation only; deployment screens and backend model
  resolution retain those ids. Um frame de erro compatível com OpenAI dentro
  do stream fica junto de qualquer resposta parcial e mostra a mensagem exata
  do servidor; o texto genérico de resposta vazia só é usado quando nenhum
  erro foi enviado. Erros longos quebram linha, continuam selecionáveis e têm
  um controle de cópia localizado.
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
  before acting, so the list is just the preview. The flow canvas receives the
  directional RTT measurements for its adjacent routing edges from
  `useNetworkRtt`, with the same badge presentation used by deployment.
- `DeployModel.tsx` — "Implantar na rede". Full-bleed Deploy Studio with a
  top mode bar, separate technical model and operational deployment name,
  Simple/Complete strategy, central peer map, compact property inspector and
  draggable bottom peer strip. Three shapes, chosen by the
  mode bar at the top and carried to the node as `kind`: **a set of shards**
  (below), **a whole model** (one model picker from `../lib/wholeModels`, one
  machine picker, no coverage ribbon/"included"/merge - a pipeline of one has a
  single answer), or **tensor parallel** (`tensor_parallel`): when the catalog
  has `tp_parts`, the picker groups the round by TP width and the inspector
  shows one-based ranks such as `1of2` and `2of2`; otherwise it uses the
  whole-model runtime path. Canvas membership is canonical: rows plus preferred
  peers, with every live peer added by the Complete strategy. Tensor selectors
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
  Quando a máquina selecionada não tem uma fatia, o inspetor de máquina expõe
  um slider indexado pelos cortes que o backend leu do GGUF. O índice não é uma
  camada cravada: em modelos densos os cortes podem ser camada a camada, e em
  modelos híbridos saltam pelas fronteiras arquiteturais reais. A seleção envia
  `whole_slice`; o backend materializa prefixo e restante na máquina que tem o
  modelo inteiro e só então usa a transferência existente. Se o destino já tem
  a faixa, a lista "Camadas disponíveis neste peer" continua sendo o controle e
  o slider fica oculto; falha na leitura aparece no inspetor com o motivo.
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
  repetition, fit-memory, hidden-state dtype, execution toggles and the installation-wide KV cache
  types. The MTP toggle seeds `mtp_min_probability` at `0.75`; the operator can
  then edit that probability before deploying. The machine section includes
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
  Plan edits are debounced 260 ms and validated by `POST /api/ui/deploy/plan`;
  the deploy itself re-surveys the cluster via `POST /api/ui/deploy`, returning
  a job shown through `useJobs`. A failed preflight keeps the backend's reason
  visible in the rose job banner, including the peer that stayed occupied.
  Restores the last deploy from localStorage via
  `../lib/deployMemory` (`recallDeploy`/`rememberDeploy`/`usableSelection`),
  after the first plan, pruned against what still exists.
- The TP inspector never offers pipeline layer buttons. It lists available and
  selected ranks, while `DeployPlan.tp_targets` is the backend source of truth
  for the exact pre-sliced file and rank destination.
- `DeployModel` sends `deployment_name`, `strategy` and `preferred_node_ids` to
  the planner. The map is explanatory: capacity/load come from the live
  cluster and RTT comes from the directional survey in the plan;
  adding a peer without a range only adds its card and keeps existing positions;
  in whole-model mode, adding a peer also appends an explicit full-model replica;
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
  criação mais recente e mostra a coluna "Criada". Permite reimplantar só
  parâmetros, ativar uma implantação e desimplantá-la pelo `deploy_id`.
- `Keys.tsx` — Two screens in one file. `TokenKeys`: per-node API tokens for
  harnesses — `GET/POST /admin/api-keys`, `DELETE /admin/api-keys/:key`.
  `ConnectionKeys`: the cluster operator key — `POST /admin/regenerate-key`.
- `LogsByToken.tsx` - Audit inspector for the persistent request trail. It reads
  `GET /api/ui/requests` with API-key identity, prompt FTS, node and time-range
  filters, shows exact prompt/request body/result/telemetry in collapsible
  details, and labels `in_progress` and `incomplete` separately from success.
  Reuses `HopChain`, `statusLabel` and `statusTone` in `LogsLive`.
- `LogsLive.tsx` - Same authenticated SQLite audit source (polled at 3 s),
  rendered as an animated newest-first stream with pause/resume and a frozen
  snapshot. It displays the non-secret API-key label and identity.
- `ModelPaths.tsx` - edits `model_dirs`, `shard_dir` and `shard_max_gb` via
  `POST /api/ui/paths`. The Armazenamento screen rejects a slice folder equal to
  or inside a whole-model folder, shows current usage and the slice limit, and
  keeps `/config/pastas` as an alias.
- `NetworkSettings.tsx` — Admin view: this node's membership
  (`POST /api/ui/leave` / `/api/ui/rejoin`) and the known-node list from
  `GET /admin/nodes` with block/unblock (`POST /admin/nodes/:id/block|unblock`).
  Também monta o `HuggingFaceSettingsPanel`, que salva o token local sem
  receber o segredo de volta. O `ThermalPanel` grava a política local em
  `POST /api/ui/thermal`, incluindo limite, margem de histerese e cooldown.
- `NotFound.tsx` — 404 fallback with a link back to `/`.
- `Forbidden.tsx` — a tela que o perfil ativo não abre. Não é `NotFound`: o
  endereço existe, o que falta é o perfil, então o texto nomeia o perfil ativo e
  manda trocar de perfil em vez de mandar procurar defeito de rota. Quem a
  escolhe é o guarda em `../App.tsx`, pela tabela de `../lib/access.ts`.
- `SendFile.tsx` - offers a `.gguf` from the local catalog via
  `POST /api/transfer/send` (`{to_node_id, filename}`). The UI displays the
  friendly name and full path, but sends only the basename; the backend resolves
  it again and the receiver sees only an opaque grant.
- `Setup.tsx` — First run (no `pom.json`): choose Principal (mints key via
  `GET /api/setup/key`) or Nó (paste key + `ip:port` bootstrap), then
  `POST /api/setup/save`; the node writes config, respawns, and the screen
  reloads itself after 4 s. Uses `../components/Logo` and `LanguageSelect` from
  the i18n provider.
- `Shards.tsx` - "Fatias": every local slice always listed (the Nodes screen
  only shows the difference), with who is missing it. Pipeline `parts` and TP
  `tp_parts` are separate sections; TP groups are ordered by group size and
  rows use the one-based labels `1of2`, `2of2`, and so on, with the zero-based
  rank shown alongside. Sends via `POST /api/transfer/send`; deletes via
  `../lib/deleteModel`. `uniqueName` disambiguates same-hostname peers with
  the node_id prefix.
- `SliceModels.tsx` - "Fatiar": the operator chooses pipeline slicing, one
  explicit layer range, or Tensor Parallelism. Pipeline builds a by-size plan
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
  (`../lib/transferLabel`), com o hash curto ao lado.
- `WholeModels.tsx` — Full-model catalog from `useLocalCatalog`, one `ModelRow`
  per model with actions: slice (links to `/modelos/fatiar?path=`), send
  (`/transferencias/enviar?path=`), delete (`../lib/deleteModel`). `ScannedRoots`
  lists the scanned folders and per-folder file counts.
- `RemoteModels.tsx` - mostra primeiro o status do CLI e da autenticação,
  depois busca com debounce em `/api/ui/model-provider/search`, filtro GGUF,
  ordenação, paginação por URL, detalhes lazy e confirmação de download. O
  detalhe resolve a licença pelo provider e marca modelos gated antes do
  download. O diálogo mostra as pastas de modelos resolvidas pelo backend. Os
  jobs aparecem no `ModelDownloadsPanel`, também em Transferências, então trocar
  de rota ou recarregar não perde uma transferência.

Key logic carriers:

- `Chat.send` (Chat.tsx:469) — the streaming loop: snapshots turns before the
  placeholder reply, streams deltas into `consumeChatDelta` (keeps
  reasoning/content split), merges `x_pom` metrics via `finishChatStream`,
  aborts via `AbortController`, persists the session on finish. The only screen
  that talks to the OpenAI-compatible surface.
- `Chat.ExecutionTelemetry` (Chat.tsx:128) + `PhaseRow`/`LegacyHopRows` - the
  closed-by-default telemetry table: per-hop prefill/decode phases, selected
  path versus backend-reported executed engine, mixed/mismatch evidence,
  gateway vs backend vs residual vs transport timing, bytes per hop; renders the legacy (pre-phase-split) hop shape separately, an
  optional TP collective table, aggregate transport identity/RTT distribution/
  bytes with fallback and switch counts, prefix reuse counters, overlap counters and a pure diagnosis card. The request summary also shows TTFT, prompt-token prefill rate, output-token generation rate and end-to-end total, including an explicit unavailable state when a rate has no measurable interval. The AI action sends
  both tables as private request content, while the initial model is shown as
  a non-turn mark before chat messages.
- `DeployModel` and `Deployments` use `RuntimeParamsForm` for the shared runtime
  knobs, including `prefill_max_inflight_chunks` (prefill pipeline depth, 1..8)
  and `fit_memory_margin_mb` (per-device VRAM safety reserve).
- `DeployModel.rowsOf`/`sameRows` (DeployModel.tsx) — the selection model:
  merged steps expand back into per-slice rows and the planner answer is compared
  with the displayed rows after sorting by layer.
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
- `SliceModels.asPlan` (SliceModels.tsx:56) - shape guard on pipeline and TP
  plan responses; the one screen rendering straight off a POST body.
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
`/admin/regenerate-key`, `/api/setup/key`, `/api/setup/save`,
`/api/ui/model-provider/search`, `/api/ui/model-provider/details`,
`/api/ui/model-provider/storage`, `/api/ui/model-provider/downloads` e
`/api/ui/model-provider/downloads/:id/{cancel,retry}`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
