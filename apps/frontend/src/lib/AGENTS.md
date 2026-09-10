# src/lib

Shared browser-side logic for the pom web UI: the HTTP client, all
react-query reads, localStorage persistence (chat history, last deploy, canvas
layout, theme, connection key), the streaming chat state machine, formatting,
toasts and every type the panel exchanges with the node.

This folder is pure frontend. Nothing here talks to peers directly - the node's
gateway serves the API and the UI on the same port, so every fetch uses a
relative path (`api.ts`) and the browser never learns a peer's address.

## Per-file

- `api.ts` - fetch wrapper over relative paths with same-origin credentials,
  connection-key auth, administrative-token auth only for `/api/ui/**` and
  `/admin/**`, conditional JSON GETs with ETag caching, the SSE reader and the deployment
  registry actions (`listDeployments`, `redeployDeployment` - `{ params }` ou
  `{ version }` para reimplantar uma versão registrada -, `activateDeployment`),
  além do status autenticado do provider remoto. `addNodeOptions` consulta o
  catálogo e a configuração da máquina alvo para o diálogo incremental;
  `addNode` envia o tipo whole/shards e os overrides por máquina, enquanto
  `removeNode` recebe o `deploy_id` para versionar a topologia correta.
  `listDeployments` também preserva o estado `idle` quando os peers continuam
  vivos, mas descarregaram o modelo.
- `postNetworkChat` - obtains the node-owned API key and includes it in every
  network-chat action body. The gateway still authenticates the browser with
  the connection key separately.
- `downloadNetworkChatFile` - fetches a persisted network-chat file only when
  the operator asks to download it, so regular history polling carries metadata
  but not repeated file bytes.
- `id.ts` - gera ids compactos no formato UUID sem depender de
  `crypto.randomUUID`, que não existe no contexto HTTP da LAN; usa
  `getRandomValues` quando disponível e mantém um fallback para ids não
  secretos em navegadores antigos.
- `chatScroll.ts` - whether the chat viewport is near the bottom (auto-scroll policy).
- `chatModels.ts` - presentation-only options for the chat; hides deployment-qualified ids containing `@d-` while leaving the backend catalog unchanged.
- `chatStore.ts` - conversation sessions persisted in `localStorage`, cross-tab
  synced; keeps the initial model mark, attachment structure and private request
  content for telemetry analysis. `ChatSessionProvider` shares the active
  session and selection/new/delete actions between `Chat` and the infer Sidebar,
  and owns the active stream so route unmounts preserve live partial replies.
  Direct Chat consumers get an isolated provider wrapper.
- `chatStream.ts` - incremental parse of streaming deltas; `<think>`/`</think>` splitter, request-level `GenerationTimings` and request trace types, including observed phase `round_trip_ms`, selected path, executed engine, mixed/mismatch flags, MoE executor and reason, hidden-state bytes/dtype, bounded `transport_summary` (including opportunistic `remote-stream` callbacks) and optional TP telemetry. It also carries trace spans, outside-node accounting, first-token boundaries, connection counts, decode counts and browser-local stream observations. Backend completion timing must not be used to infer `POM_PROFILE_SYNC` or the engine.
  `chatStream.ts` also carries trace spans, outside-node accounting,
  first-token boundaries, connection counts, decode counts, browser-local stream
  observations and distributed `TelemetrySummary`/`TelemetrySpan`. TTFT keeps
  separate pipeline/server/client meanings; backend completion timing must not
  be used to infer `POM_PROFILE_SYNC` or the engine.
- `telemetryAnalysis.ts` - pure diagnosis of TP, pipeline transport, engine queue,
  backend cost and time outside the node; compara timings ponta a ponta com o
  processamento medido no nó antes de nomear o gargalo. It refuses to label
  a distributed gap as `outside-node` while telemetry is incomplete and keeps
  the fixed/per-token regression as an analysis helper only. Derives the observed
  execution RTT, adjacent pipeline pairs and effective GPU/CPU/mixed layer
  counts from actual buffers.
- `deleteModel.ts` - two-step delete of a GGUF with a warnings/force round trip.
- `deployMemory.ts` - last deploy per model remembered in `localStorage` for the Deploy screen, including the operational name and strategy when available. Keys are namespaced by kind (`whole:<name>` for a whole model), because a shard set and a model routinely share a name.
- `wholeModels.ts` - `wholeModelsOf` names each un-sliced model of the cluster catalog once, collecting its holders. `tensorParallelModelsOf` groups `ShardSet.tp_parts` by source group and TP width, preserving rank holders for the Deploy screen. The menu the Deploy screen needs before it has a plan; a model whose GGUF header gave no layer count stays in the list on purpose, so the node can say what is wrong with the file.
- `format.ts` - number/time formatting in the active locale plus node-state
  labels, incluindo o estado `idle` distinto de `serving` e `noAir`.
- `thermal.ts` - pure temperature-band and highest-known-reading helpers used by
  the GPU badge; missing or non-finite readings stay explicitly unknown.
- `gpu.ts` - `sumVram`/`vramUsedPct`: a VRAM somada das placas que a reportam,
  `null` quando nenhuma reporta (memória unificada, placa sem leitura). É o que
  o card em miniatura do Fluxo mostra - antes ele lia só `gpus[0]` e metade do
  hardware de uma caixa com duas placas sumia da tela. Testes em `gpu.test.ts`.
- `networkGraph.ts` - flattens `ClusterView` into per-model layer-range lists for the canvas.
- `networkLayout.ts` - canvas node positions persisted per deployment fingerprint.
- `networkPeers.ts` - how the panel reads the peer list: who serves the active model (pipeline order) vs who is on the network without a slice.
- `polling.ts` - the fast 2s and idle 10s cadences used by jobs and offers.
- `queries.ts` - every read the panel does, as react-query hooks with polling
  intervals. `useCluster`, `useJobs` and `useOffers` use conditional GETs;
  jobs and offers slow to 10s when idle and return to 2s while active.
  `useModelDownloads` derives the `model_download` jobs from `/api/ui/jobs` and
  is shared by the remote-model and transfers screens.
  `useCacheTypes` reads the installation-wide K/V cache representation from
  `/api/ui/cache-types`; the deployment inspector applies combo changes through
  the same endpoint. The add-machine dialog reads and updates the selected
  peer's representation through the target-aware deploy flow, so cache choices
  are applied on that machine before launch.
  `useChatTelemetryNetwork` asks the authenticated gateway for the directional
  transport and RTT observed by the exact chat execution, and
  `usePeerRuntimes` reads `/health` for the peers in that trace without polling
  the whole cluster. `useNetworkRtt` asks the same route for the most recent
  completed execution matching the Dashboard edges and refreshes it every 15 s;
  without a matching execution the edge remains unknown instead of displaying
  a libp2p probe as inference telemetry.
  `useInferenceHeads` polls the model head view every 15 s for every profile;
  this is a capability slice, not a general peer list.
  `useClusterLogs` lê o log de processo do principal, opcionalmente filtrado
  por `deploy_id`, e atualiza a consola a cada segundo.
  `useAdminSurface` is the one-shot shell probe: it distinguishes setup,
  available, token-required and local-only states without exposing the token.
- `stableRecord.ts` - preserves the identity of a shallow record while its
  values remain unchanged. It prevents effects that capture `useQueries` data
  from writing state on every render; the Chat regression test covers this
  navigation freeze.
- `profile.tsx` - o perfil persistido que o nó entrega em `/api/ui/node`
  (`admin`/`node`/`infer`), o provider e `useProfile()`. Não há setter nem
  `localStorage`: o navegador só apresenta o valor e deriva dele o menu/guarda
  de rota. **Não é autorização** - o backend continua sendo a fronteira.
- `access.ts` - que rota cada perfil abre. `RESTRICTED` mapeia prefixo de rota
  para a lista de perfis; para `infer`, `/chat`, `/documentacao`, `/config/keys/api`
  e `/config/rede` são visíveis. No Chat, esse perfil também esconde os resumos
  e o painel de telemetria.
  `canAccessRoute(path, profile)` é
  lida pelo guarda em `../App.tsx` **e** por `navForProfile` em
  `../components/nav.ts`, de propósito: duas listas dariam menu que oferece o que
  a rota recusa. Testes em `access.test.ts`.
- `theme.tsx` - dark/light theme provider backed by `data-theme` + `localStorage`.
- `toast.tsx` - toast context (success/error/warning/info) with auto-dismiss.
- `transferStatus.ts` - predicates over `TransferJob` for the transfers UI and for the bell.
- `transferLabel.ts` - como uma fatia se nomeia na tela (modelo + camadas, hash de lado).
- `modelProvider.ts` - serializa a busca remota, filtra jobs de download,
  calcula a cadência derivada e só calcula percentual quando o backend conhece
  o total de bytes. `useRemoteModelProviderStatus` em `queries.ts` lê o estado
  estruturado do CLI e da sessão Hugging Face antes da busca remota.
- `api.ts` e `queries.ts` também expõem `getHuggingFaceTokenConfig`,
  `saveHuggingFaceToken` e `useHuggingFaceTokenConfig`; o tipo de resposta
  contém apenas `configured`, `masked` e `source`, nunca o token bruto.
- `peerName.ts` - como uma **máquina** se nomeia na tela: `peerDisplayName` (nome anunciado, id curto só como último recurso) e `uniquePeerName` (desempate por id quando dois nós dividem hostname). Usado por `routes/Shards.tsx`.
- `types.ts` - all API shapes served by the node, one authoritative source,
  incluindo `ClusterLogEntry` e o estado de autoridade do interruptor de logs.
  `ShardSet.shards` is the pipeline `parts` array and `ShardSet.tp_parts` is
  the tensor-parallel array. TP `DiscoveredModel` entries carry `tp_rank` and
  `tp_size`, which the UI turns into one-based labels such as `1of4`.
  O modo modelo inteiro não expõe cortes de GGUF no inspetor: o mapa escolhe
  uma única máquina e o frontend não envia seleção de prefixo.
  `SliceLayerOptions` carries the starts and architectural options for the
  single-range slicing controls, while `SizePlan` and `TpSlicePlan` carry the
  destination disk-space estimate shown before writing.
  `tpPartsOf` normalizes a missing optional wire field to an empty list before
  catalog consumers iterate it. DiscoveredModel.multipart_parts carries the
  ordered physical files and multipart_error carries the concrete reason a
  declared set cannot be sliced; the UI uses metadata_complete to avoid
  displaying a first-part byte count while enrichment is pending.
  `RuntimeParams.hidden_state_dtype` is the deployment-wide f16/f8 choice;
  `RuntimeParams.max_new_tokens` is only the size of one backend leg. The
  cycle detector settings in `RuntimeParams` control the gateway's bounded
  repetition safety stop and can be disabled explicitly. The total output
  capacity comes from live runtime context and `/v1/models`, not from a second deployment field. `PhaseTrace` carries the effective dtype and
  hidden-state bytes per hop. `RequestTrace` carries the server trace id, local
  spans, outside-node accounting, first-token boundaries, connection reuse
  counts and stream measurements when the backend publishes them.
- `*.test.ts(x)` - unit tests exist for chatScroll, chatStream, deleteModel, deployMemory, wholeModels, networkGraph, networkLayout, networkPeers, transferStatus (run with `npm test`).

## Key functions

### api.ts
- `request<T>(path, opts)` - the single ordinary fetch path: JSON body,
  same-origin credentials for the server session, connection-key auth for
  administration, and the dedicated `pom-[node_id]` API key for `/v1/*` and
  `/api/generate`; a 401/403 replaces that key once and retries.
  Everything in the app goes through it; the human token remains in memory only.
- `requestConditional<T>(path, opts)` / `api.getConditional` - GET with an
  in-memory ETag cache. It sends `If-None-Match`, returns the cached object
  without parsing a 304 body, and falls back to an unconditional GET if a new
  tab receives a bare 304.
- `clearConditionalCache()` - clears conditional snapshots; changing the
  connection key does this automatically so a new node cannot reuse an old
  node's state.
- `class ApiError` - carries `status` and the decoded JSON `body` so 409-style refusals (e.g. delete warnings list) survive to the caller.
- `onAdminSessionLost(handler)` - subscribe to "the administrative session is
  gone". Every refusal path checks the node's `x-pom-admin-error` header
  (`backend/auth.rs`): `missing-token`/`invalid-token` on an administrative
  path drops the in-memory token and notifies. The header, not the sentence -
  the body text is for the person and changes with the locale. A profile 403
  (`perfil sem autorização para esta rota`) is deliberately not this: sending
  an authenticated operator back to the gate would only make them re-enter to
  see the same refusal.
- `brand.ts` - the product identity on the screen side: `brand` (name, tagline, slug, logo/mark/favicon URLs) from the generated `src/brand/brand.json`, same source the binary embeds. `storageKey(name)` prefixes every `localStorage` key with the slug (`pom.theme`, `pom.locale`, `pom.connectionKey`, `pom.chat.sessions`, `pom.deploy.last`, `pom.network-layout`), which is what makes renaming the product one edit - and why a rename drops local preferences, deliberately: they belong to another product's keys.
- `setConnectionKey` / `getConnectionKey` - module-level credential persisted in `localStorage` key `codai.connectionKey`; seeded by `useNode()` from `GET /api/ui/node`. A changed key also clears conditional GET snapshots.
- `ensureChatApiKey` / `forgetChatApiKey` - keep the UI-only `pom-[node_id]`
  key in memory and recreate it after revocation without exposing its value.
- `streamSse(path, body, signal)` - async generator yielding each `data:`
  payload of an SSE stream (blank-line framing); used by Chat for
  `/v1/chat/completions`, with one replacement-key retry.
- `api.get/post/del` - typed conveniences over `request`.
- `devUpdate(nodeId)` - update+restart a machine, repassing to a peer when `node_id` is not this node (`src/devupdate.rs`).
- `addNodeOptions` / `addNode` / `removeNode` - query the selected machine's
  local catalog and cache settings, then add/remove one machine to a deployed
  model (`/api/ui/deploy/node{,/remove}`). Add and remove responses carry the
  deployment version when a persisted deployment is being edited.
- `undeployPipeline(group)` - take the whole pipeline out of service: `POST /api/ui/undeploy` asks every machine serving the model to release RAM/VRAM at once and returns an `UndeployReport` (machine by machine; `released` only when that node confirmed). Nothing is deleted from disk.
- `leaveNetwork(nodeId?)` - `POST /api/ui/leave`. Without `nodeId` this node leaves; with a peer's `node_id` this node forwards the request to that peer (`webui::forward_leave`) - the panel only uses relative paths, so acting on another machine is always a relay through the local node.

### types.ts
- `NetworkChatRequest`, `NetworkChatSnapshot` and related types mirror the
  network-chat action and snapshot contract. `NetworkChatEvent` preserves
  `node_id` as the identity key instead of using hostname as a key.
- Authoritative shapes with pointers to their Rust origin (comments cite `src/webui.rs`, `src/sysmon.rs`, `src/runtime.rs`, `src/transfer.rs`, `src/admin.rs`, `src/reqlog.rs`, `src/plan.rs`, `src/deploy.rs`).
- `ServerState` - `"disabled" | "starting" | "ready" | "idle" | "failed" | "thermal"`; `idle` means a live peer unloaded its model, while thermal is distinct from an unreachable or failed peer.
- `ClusterView` / `PeerView` / `ModelCapability` - the network/dashboard model.
  `PeerView.rediscovering` distingue "endereço morreu, a rede já foi perguntada"
  de `unreachable` ("atendeu e a leitura quebrou"); `ModelCoverage.unreadable_nodes`
  nomeia as máquinas de que a cobertura depende sem conseguir ler, e é o que
  impede o Painel de pintar "completo" em verde nesse estado. Ver
  `docs/subsystems/rede-p2p.md`, "Endereço que deixou de responder". A carga
  em voo de `PeerView` é `null` quando o relatório vivo do peer não chegou;
  zero só representa um peer que de fato reportou estar ocioso.
- `ShardSet` identifies a slicing round with the technical `group`, readable
  `model_label`, `pom_id` and optional local `mmproj_path`; `DeploySelection`, `DeployStep`, `DeployTpTarget`, `DeployPlan`,
  `RuntimeParams`, `DeployCandidate`, `DeployMachine` are the deploy screen
  shapes. `DeployPlan` also carries the operational name, strategy, RTT route
  alternatives and projected load, while `DeployMachine` carries GPU/RAM
  capacity plus optional live GPU/RAM usage percentages. Its live load and
  route load distribution are nullable when the survey could not read the
  peer, so the deploy screen can say that the value is unavailable.
  `DeployStep.parts`
  carries merged-slice steps, `primary` marks whoever holds layer 0, and the
  optional `exec_start`/`exec_end` pair identifies the sub-range actually
  executed by that step. `start_layer`/`end_layer` remain the physical range
  loaded on disk, so old responses without the pair fall back to that range.
  `DeployPlan.tp_targets` describes pre-sliced TP ranks separately from
  pipeline steps, including the source hash/path used by a transfer.
  `RuntimeParams.fit_memory` enables llama.cpp memory fitting and is normalized
  off for tensor parallel deployment. `RuntimeParams.fit_memory_margin_mb` is
  the per-device safety reserve, defaulting to 256 MiB. `RuntimeParams.inference_mode` is the
  persisted `auto`/`dense`/`moe` preference. `DeployPlan.is_moe` comes from the
  node's GGUF expert metadata; the deploy screen must not infer MoE from a
  filename. `inference_mode_supported` and `inference_mode_reason` keep an
  unsupported path visible instead of presenting a misleading selection.
  The capacity preview is an analytic planning estimate, not the effective
  placement reported by the real load; the deploy screen keeps that distinction
  visible in its capacity warning.
  `DeployPlan.capabilities` is the backend tri-state contract for conditional
  global controls, including source and reason. The UI does not infer model or
  topology support from names; unknown remains visible but disabled.
  `PeerRuntimeHealth` carries the selected path and effective MoE ownership
  (`moe_requested`, `moe_executor`, dtypes and `moe_executor_reason`) alongside
  `mtp_ready` and an optional `mtp_allocation` and `mtp_unavailable_reason`;
  the dashboard detail card reads these fields from `usePeerRuntime` so a
  requested-but-unused executor, actual VRAM/RAM placement and a failed MTP
  attach are visible.
- `isWholeModel(m)` - a whole `.gguf` vs a slice (`shard_range == null`).
- `SizedRange` / `SizePlan` / `TpSlicePlan` / `PlannedShard` / `SplitPlan` -
  size-, tensor- and budget-driven slicing plans. Budget-driven shards expose
  `weight_bytes`, `kv_bytes` and `total_bytes`; those are the byte arithmetic
  returned by the node planner, not display-only estimates.
- `UndeployOutcome` / `UndeployReport` - per-machine result of an undeploy: `released` is `true` only when that machine confirmed, `error` carries the reason otherwise.
- `TransferOffer.md5` is documented as always empty: verifying the file is the receiver's job after accepting (`/api/transfer/manifest`), because computing the digest to offer held the notification up for the whole file read (3 min 23 s of MD5 on a 4.2 GB slice).
- `AdminAuthorityView` and the authority offer types describe the principal,
  secondary admins, promotion chain, resignation eligibility and notification
  states. `adminAuthority.ts` keeps mutations behind a contract and does not
  encode key, signature or grant-storage decisions.
- `TrustAnchorView` e `TrustAnchorRefusal` descrevem a raiz de confiança: se o
  nó está ancorado, a impressão digital pública, o cluster e a máquina da raiz,
  se a ligação é loopback e se instalar é possível agora. `trustAnchorApi`
  (também em `adminAuthority.ts`) envia a cadeia colada e a impressão digital
  digitada; a tela nunca deriva a segunda da primeira, e `can_install` é
  apresentação - quem recusa é o servidor.

### queries.ts
- `useAdminSurface()` - one-shot shell probe (`retry:false`, no focus refetch):
  404 means setup, a valid server session means available, a localhost policy
  refusal means the local-only state, and other refusals keep the token gate.
  It subscribes to `onAdminSessionLost` and re-probes: the administrative
  session lives in the node's memory, so a restart or the 8h expiry kills it
  with the tab open, and a once-per-load probe would leave every screen
  printing the server's raw 401 with no way back to the gate.
- `useIsSetupMode()` - one-shot probe (`retry:false`, `staleTime:Infinity`): 404 on `/api/ui/node` means the node runs only the setup router. Its own query so the shell does not flip between setup and dashboard on poll errors.
- `useAdminAccess()` - public one-shot metadata for the administrative token
  gate; it never receives the token.
- `useAdminAccessInfo()` - authenticated administrative settings, including the
  token only for the Preferences screen.
- `useNode()` - polls `/api/ui/node` every 4s after the admin gate, stores
  `connection_key`. That field is not decoration: it is what authenticates
  every later call that needs `ConnectionAuth` (`/admin/*`, `/api/ui/leave`).
  It also carries `authority_level`, the **effective** administrative
  authority, which is what decides whether a screen offers an administrative
  act - `profile: admin` alone grants nothing.
- `usePeerRuntime(nodeId, enabled)` 15s (stale 10s) - o `/health` daquela
  máquina, por `optional` (um nó sem a rota devolve 404 e a seção some). O
  `enabled` só é ligado com o card de uma máquina servindo montado: perguntar
  sempre seria um fan-out por peer a cada ciclo do Painel, por uma seção quase
  sempre fechada.
- `useCluster()` 5s with conditional GET, `useChatCapabilities()` 5s from
  `/api/ui/chat/capabilities` (vision and coverage aggregated for the active
  model), `useLocalCatalog()` 30s (stale 15s), `useClusterCatalog(enabled)`
  60s, `usePaths()` (stale 30s), `useCacheTypes()` (stale 30s), `useJobs()` 2s
  while running or 10s idle, `useTransfers()` 2s, `useOffers()` 2s while
  waiting or 10s idle, `useAdminNodes()` 8s, `useRequestLog({node_id, api_key,
  prompt, from, to, limit, offset})` 3s, `useRequestStats({node_id, api_key,
  prompt, from, to}, enabled)` 30s while the statistics tab is open,
  `useServableModels()` 30s. The latter preserves `context_length` and
  `max_input_tokens` from `/v1/models` for the Chat attachment guard. A refusal
  or transport failure in the capability read becomes the explicit
  `unavailable` state, never an empty capability.
- `useCluster()` 5s with conditional GET, `useLocalCatalog()` 30s (stale 15s), `useClusterCatalog(enabled)` 60s, `usePaths()` (stale 30s), `useCacheTypes()` (stale 30s), `useJobs()` 2s while running or 10s idle, `useTransfers()` 2s, `useOffers()` 2s while waiting or 10s idle, `useAdminNodes()` 8s, `useRequestLog({node_id, api_key, prompt, from, to, limit, offset})` 3s, `useRequestStats({node_id, api_key, prompt, from, to}, enabled)` 30s while the statistics tab is open, `useServableModels()` 30s. Os dois catálogos mudam para polling de 2s enquanto `metadata_complete` é falso, então o inventário rápido é substituído pelos cabeçalhos sem esperar o intervalo normal.
- `optional(promise, fallback)` - swallows only 404 (an older node lacking a route is not an error to surface); used by jobs/transfers/offers/request log.
- `useOffers()` uses 2s while an offer is waiting and 10s when empty. An offer
  is a question waiting on a human, not a progress bar. With the sender no
  longer hashing the file before offering, the active cadence keeps that
  question responsive without charging an idle tab the same rate.
- `useTrustAnchor()` reads the anchoring state once, sem polling: uma raiz de
  confiança muda por gesto do operador, e a tela refaz a leitura ao instalar.
- `useAdminAuthority()` reads the authority presentation contract every 8s;
  `useOffers()` normalizes legacy slice offers and also carries principal
  transfer, infer-to-node approval and node-to-infer notice states for the
  activity bell.
- `invalidateAfterAction(qc)` / `useInvalidate()` - after a mutation, refetch node, cluster, jobs, admin-nodes and models at once.

### chatStore.ts
- `useChatSessions()` - `localStorage` list (`codai.chat.sessions`) capped at `MAX_SESSIONS` 50, kept in sync across tabs via the `storage` event; returns `{sessions, upsert, remove, clear}`.
- `ChatSessionProvider` / `useChatSessionController()` - one active session
  and the store actions shared by the Chat screen and `Sidebar`. The controller
  invokes the registered pre-change hook before a menu selection, new session or
  deletion so the Chat clears transient state; changing routes does not abort a
  live stream. `startStream` aborts the previous request when a new message is
  sent, while `stopStream` is the explicit user cancellation. Deltas are
  persisted in the session as they arrive, and deleting a session prevents a
  finishing stream from recreating it.
  `startStream` also carries the optional Chat `inference_head` selection in
  the OpenAI-compatible request body.
- `turnsOnly(messages)` - drops `role: "event"` entries (model-change marks) before the transcript travels in a request.
- `turnsOnly(messages)` - drops event marks and substitutes `requestContent` when a visible chat message carries private telemetry-analysis content.
- `titleFrom(text)` / `emptySession(model)` / `newId()` - defaults and ids; `titleFrom` stops the first message from overwriting a `titled` session's chosen name.

### Anexos

`imageResize.ts` reduz imagens para no máximo 2048 px no lado maior e preserva
a proporção antes de montar o pedido multimodal. `chatStore` preserva as partes
estruturadas do pedido para que anexos de imagem e de outros tipos sobrevivam ao
histórico; o Chat aplica o limite de contexto antes de aceitar o lote.

### chatStream.ts
- `consumeChatDelta(previous, delta)` - pure state machine that separates `<think>` reasoning from visible content without leaking a split delimiter across chunk boundaries (partial suffix held in `pending`).
- `streamErrorMessage(payload)` / `consumeChatError(previous, error)` - leem a mensagem real de um frame de erro compatível com OpenAI dentro do stream SSE e a mantêm junto do conteúdo já renderizado.
- `finishChatStream(previous, metrics)` - flushes an incomplete trailing delimiter as literal text and records the legacy total rate, separated TTFT/prefill/generation/total timings and trace.
- `isPhasedHop(hop)` - discriminates `PhasedHopTrace` (split prefill/decode) from `LegacyHopTrace` so the telemetry table can render both shapes.
- Types `RequestTrace`, `PhaseTrace`, `HopTrace` - the per-hop telemetry
  contract from `inference/prefill.rs`/gateway. `PhaseTrace.selected_path` is
  the GGUF selection, while `executed_engine` and `executed_engines` are the
  backend facts (`llama.cpp`, `moe` or unknown when absent). The mismatch and
  mixed flags are evidence from the request, not a UI guess.
  `RequestTrace.cached_tokens` is optional on purpose: a node that does not
  report prefix reuse means "unknown", and the chat shows nothing rather than
  claiming savings it cannot prove. The trace also carries prefix candidate and
  transmitted-token counts, reuse status, bounded prefill-overlap counters, and
  per-transport traversal distributions with fallback and switch counts. The
  transport contract is aggregate by default, never one serialized entry per
  decode call. `RequestTrace` also exposes explicit HTTPS fallback counts and
  reasons, normal-mode token-cycle quantiles, and remote-stream delivery
  interval quantiles; the remote-stream contract also separates producer wait
  from queue wait, and profiling spans remain opt-in.

### deployMemory.ts
- `rememberDeploy(group, memory)` / `recallDeploy(group)` - last deploy (selection, params, merge) per model group in `localStorage` `codai.deploy.last`, capped at 20 models, oldest dropped by `at`.
- `usableSelection(remembered, ranges, machineIds)` - prunes remembered rows against what the cluster still offers (deleted slices, left machines); returns `null` when nothing survives so the caller keeps the node's own suggestion.

### profile.tsx / access.ts
- `PROFILES` / `Profile` - `["admin", "node", "infer"]` e a união literal.
- `DEFAULT_PROFILE` - `admin` somente enquanto `/api/ui/node` ainda não respondeu.
  A resposta do nó substitui o fallback assim que chega.
- `ProfileProvider({ children, initialProfile })` - lê o perfil de `useNode()`;
  `initialProfile` existe somente para testes de apresentação. Escreve
  `<html data-profile>` e não persiste estado no navegador.
- `homeRoute(profile)` - a tela inicial do perfil: `/` para quem abre o Painel,
  `/chat` para quem não abre. O `App.tsx` redireciona com ela em vez de mostrar
  a recusa, senão o `infer` abriria a interface num beco - e a recusa mandava
  trocar de perfil, controle que deixou de existir.
- `routeProfiles(path)` - os perfis que entram, ou `null` quando a rota é livre.
  Casa por segmento (`/logs` cobre futuras filhas, e **não** `/logs-antigos`) e
  a chave mais específica vence.
- `canAccessRoute(path, profile)` - para o `infer`, permite somente a vista de
  conversa, documentação, API keys e rede local. É apresentação, não escopo de credencial; a fronteira
  real ficou para `rdma-perfis-escopo-backend`. `components/nav.ts` usa o mesmo
  resultado para manter a rede no menu principal do infer e mover documentação
  e API keys para o rodapé.

### theme.tsx
- `ThemeProvider` / `useTheme` - reads the theme from `document.documentElement.dataset.theme` (index.html stamped it pre-paint), writes `data-theme` + `localStorage` `codai.theme`. Dark is the default; the system preference does not override.

### toast.tsx
- `ToastProvider` / `useToast` - `push(kind, title, detail)`; error/warning linger 9s, others 4.2s; keeps the last 4 toasts; `fromError` surfaces `ApiError.message` verbatim; clears auto-dismiss timers when the provider unmounts.

### format.ts
- `bytes/megabytes/pct/num/duration/ms/rate` - `Intl.NumberFormat` in the active locale; null/non-finite render as a placeholder dash.
- `since(iso)` / `clock(iso)` - relative recency ("há 4 min" / "4 min ago") and clock time.
- `deployModelDisplayName(name)` - removes only a final `.gguf` and a final
  eight-character hexadecimal round hash from deployment labels; request
  values and model identities remain unchanged.
- `isServing(state)` / `serverStateLabel(state)` / `serverStateTone(state)` - the one place `ServerState` is compared/labeled, so screens never hand-roll the comparison.

### deleteModel.ts
- `deleteModel(path, name, confirm)` - confirm, `POST /api/ui/models/delete`, and on 409 with a `warnings` list re-confirms the list then retries with `force: true`. `confirm` is injected so screens choose how to ask (and tests need no `window`).

### networkGraph.ts
- `graphFromCluster(cluster)` - groups each peer's `capabilities` by `model_id` into `GraphRange[]` sorted by `start_layer`, for the model network canvas.

### network information

`types.ts` define o retrato compartilhado `NetworkSettings` e o envelope de
leitura `NetworkInfoView`. `useNetworkInfo` em `queries.ts` consulta
`/api/ui/network/info` para todos os perfis e `invalidateAfterAction` invalida
esse retrato após mutações de configuração. A UI nunca guarda a chave de
conexão no cache: os materiais de compartilhamento são buscados pelo admin
somente em `/api/ui/network/trust-share`.

### networkPeers.ts
The panel asks two questions, and the screen needs both: "who serves the
active model, in execution order" and "who is on the network" - the second
includes a machine connected without a slice, which is exactly the one that
shows nowhere when only the pipeline is looked at, and is where the next slice
would go.

- `pipelinePeers(cluster)` - peers on the active model sorted by start layer,
  falling back to all peers when none matches. Moved here from
  `components/PipelineRail` in 5423852; `Dashboard`, `PipelineRail` and
  `ModelNetworkCanvas` all consume it.
- `hasSlice(peer)` - whether the machine announces a slice: `model` empty with
  `total_layers` zero is what the node sends for a connected machine with
  nothing deployed (`webui::cluster`), and it is *not* the range "0-0" - it is
  the absence of a range. Used by `PeerDetailCard` (`cardRange`).
- `networkPeers(cluster)` - the whole network in two lists: `pipeline` keeps
  execution order, `others` (machines not in the pipeline) sorted by hostname.
  The guard against double-listing is the `pipelinePeers` fallback - when no
  peer matches the active model, `pipeline` already holds everyone and
  `others` must stay empty.

### networkLayout.ts
- `deploymentLayoutKey(model, peers)` - per-deployment fingerprint key (node id + range + `changed_at`), so hand-arranged layouts do not leak into a new deploy.
- `fallbackPosition(index)` / `readPositions` / `writePositions` / `clearPositions` - canvas persistence in `localStorage`.
- `mergePositions(current, persisted, arrangement, nodeIds)` - keeps positions already visible when a new peer is added and uses the automatic arrangement only for nodes without a position.

### transferStatus.ts
- `isFinishedTransfer` / `isActiveTransfer` / `canCancel` - state predicates; only `running`/`verifying` can be cancelled.
- `isRecentlyFinished(job, now?)` / `isWorthNotifying(job, now?)` / `RECENTLY_FINISHED_MS` (5 min) - o que o **sino** mostra: o que anda mais o que acabou de terminar. Sem isto o desfecho sumia da tela no instante em que acontecia, e só sobrevivia na tela de Transferências. `finished_at` ausente (nó de versão anterior) nunca conta como recente: supor "agora" encheria o sino de transferências velhas a cada carregamento.

### transferLabel.ts
- `transferTitle(item)` / `transferHash(name)` - como uma transferência ou uma oferta se apresenta: "qwen3:8b · L0–15" no título, sha256 curto ao lado. O nome de uma fatia em disco é o hash, e hash não identifica nada que se leia; o rótulo vem do `.pom`, que o nó manda junto com o job. Um modelo inteiro não tem faixa e continua se chamando pelo nome do arquivo, e um nó de versão anterior (que não manda nada disso) cai no nome também, sem quebrar a tela.

### chatScroll.ts
- `isNearBottom(metrics, threshold = 32)` - decides whether streaming keeps the chat pinned to the bottom.

## Links to the rest of the app

- Routes/components importing `lib/*` (counted by import): `queries` (19), `format` (17), `toast` (16), `api` (16), `types` (12).
- `routes/Chat.tsx` renders the composer/transcript and consumes `chatStore` and
  `chatScroll`; `chatStore` owns the `chatStream` parsing and `streamSse` call
  against `/v1/chat/completions` so the stream survives route unmounts.
- `routes/DeployModel.tsx` is the only consumer of `deployMemory`.
- `components/ModelNetworkCanvas.tsx` is the only consumer of `networkGraph` + `networkLayout`.
- `networkPeers` is used by `components/ModelNetworkCanvas.tsx`,
  `components/PipelineRail.tsx` and `routes/Dashboard.tsx` (`pipelinePeers`),
  and by `components/PeerDetailCard.tsx` (`hasSlice`).
- `routes/WholeModels.tsx` and `routes/Shards.tsx` both use `deleteModel`.
- `components/Notifications.tsx` and `routes/Transfers.tsx` use `transferStatus` and `transferLabel`.
- `components/Sidebar.tsx` uses `theme`.
- `i18n/locales.ts` is imported by `api.ts` and `chatStore.ts` (`getActiveLocale`) for non-React localized strings.
- Backend endpoints hit from this folder: `/api/ui/node`, `/api/ui/cluster`, `/api/ui/models`, `/api/ui/cluster/models`, `/api/ui/peer/:node_id/runtime`, `/api/ui/paths`, `/api/ui/jobs`, `/api/ui/requests`, `/api/ui/requests/stats`, `/api/ui/models/delete`, `/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/undeploy`, `/api/ui/leave`, `/api/ui/dev/update`, `/api/transfer/jobs`, `/api/transfer/offers`, `/admin/nodes`, `/admin/api-keys`, `/v1/models`.
- O Chat de qualquer perfil que tenha um modelo selecionado lê
  `/api/ui/infer/heads?model=...`, que é autorizado por `InferNodeAdmin` e
  devolve somente as candidatas de camada zero, o nó escolhido, carga e RTT
  com idade. A carga é `null` se o relatório vivo do candidato não chegar, e a
  UI mostra "indisponível" nesse caso. RTT fora da validade continua
  identificável na resposta, mas não entra no mapa usado para escolher.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
