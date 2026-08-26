# src/lib

Shared browser-side logic for the pom web UI: the HTTP client, all
react-query reads, localStorage persistence (chat history, last deploy, canvas
layout, theme, connection key), the streaming chat state machine, formatting,
toasts and every type the panel exchanges with the node.

This folder is pure frontend. Nothing here talks to peers directly - the node's
gateway serves the API and the UI on the same port, so every fetch uses a
relative path (`api.ts`) and the browser never learns a peer's address.

## Per-file

- `api.ts` - fetch wrapper over relative paths with connection-key auth,
  conditional JSON GETs with ETag caching, the SSE reader and the deployment
  registry actions (`listDeployments`, `redeployDeployment`,
  `activateDeployment`), além do status autenticado do provider remoto.
- `chatScroll.ts` - whether the chat viewport is near the bottom (auto-scroll policy).
- `chatModels.ts` - presentation-only options for the test chat; hides deployment-qualified ids containing `@d-` while leaving the backend catalog unchanged.
- `chatStore.ts` - conversation sessions persisted in `localStorage`, cross-tab synced; keeps the initial model mark and private request content for telemetry analysis.
- `chatStream.ts` - incremental parse of streaming deltas; `<think>`/`</think>` splitter and request trace types, including observed phase `round_trip_ms`, selected path, executed engine, mixed/mismatch flags, hidden-state bytes/dtype and optional TP telemetry. Backend completion timing must not be used to infer `POM_PROFILE_SYNC` or the engine.
- `telemetryAnalysis.ts` - pure diagnosis of TP, pipeline transport, engine queue and backend cost; derives the observed execution RTT, adjacent pipeline pairs and effective GPU/CPU/mixed layer counts from actual buffers.
- `deleteModel.ts` - two-step delete of a GGUF with a warnings/force round trip.
- `deployMemory.ts` - last deploy per model remembered in `localStorage` for the Deploy screen, including the operational name and strategy when available. Keys are namespaced by kind (`whole:<name>` for a whole model), because a shard set and a model routinely share a name.
- `wholeModels.ts` - `wholeModelsOf` names each un-sliced model of the cluster catalog once, collecting its holders. `tensorParallelModelsOf` groups `ShardSet.tp_parts` by source group and TP width, preserving rank holders for the Deploy screen. The menu the Deploy screen needs before it has a plan; a model whose GGUF header gave no layer count stays in the list on purpose, so the node can say what is wrong with the file.
- `format.ts` - number/time formatting in the active locale plus node-state labels.
- `thermal.ts` - pure temperature-band and highest-known-reading helpers used by
  the GPU badge; missing or non-finite readings stay explicitly unknown.
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
  `/api/ui/cache-types`; saving it is an authenticated mutation owned by the
  deployment inspector, not a per-machine launch parameter.
  `useChatTelemetryNetwork` asks the authenticated gateway for the directional
  RTT snapshot after a chat stream, and `usePeerRuntimes` reads `/health` for
  the peers in that trace without polling the whole cluster.
- `profile.tsx` - o perfil de quem olha a tela (`admin`/`node`): tipo, provider,
  `useProfile()` e a constante `PROFILE_SWITCHER_ENABLED`, que liga o seletor de
  desenvolvimento da barra superior. **Não é autorização** - esconde e bloqueia
  telas no navegador e nada mais; a credencial continua sendo a chave de conexão
  verificada no nó.
- `access.ts` - que rota cada perfil abre. `RESTRICTED` mapeia prefixo de rota
  para a lista de perfis; rota ausente é livre. `canAccessRoute(path, profile)` é
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
- `types.ts` - all API shapes served by the node, one authoritative source.
  `ShardSet.shards` is the pipeline `parts` array and `ShardSet.tp_parts` is
  the tensor-parallel array. TP `DiscoveredModel` entries carry `tp_rank` and
  `tp_size`, which the UI turns into one-based labels such as `1of4`.
  `WholeLayerOptions` and `LayerCut` carry the GGUF-derived cut points used by
  the whole-model deploy slider; `WholeSliceSelection` identifies the source,
  destination and accepted prefix end sent back to the node.
  `tpPartsOf` normalizes a missing optional wire field to an empty list before
  catalog consumers iterate it. DiscoveredModel.multipart_parts carries the
  ordered physical files and multipart_error carries the concrete reason a
  declared set cannot be sliced; the UI uses metadata_complete to avoid
  displaying a first-part byte count while enrichment is pending.
  `RuntimeParams.hidden_state_dtype` is the deployment-wide f16/f8 choice;
  `PhaseTrace` carries the effective dtype and hidden-state bytes per hop.
- `*.test.ts(x)` - unit tests exist for chatScroll, chatStream, deleteModel, deployMemory, wholeModels, networkGraph, networkLayout, networkPeers, transferStatus (run with `npm test`).

## Key functions

### api.ts
- `request<T>(path, opts)` - the single ordinary fetch path: JSON body,
  optional `Authorization: Bearer <connectionKey>`, throws `ApiError` on
  non-2xx. Everything in the app goes through it.
- `requestConditional<T>(path, opts)` / `api.getConditional` - GET with an
  in-memory ETag cache. It sends `If-None-Match`, returns the cached object
  without parsing a 304 body, and falls back to an unconditional GET if a new
  tab receives a bare 304.
- `clearConditionalCache()` - clears conditional snapshots; changing the
  connection key does this automatically so a new node cannot reuse an old
  node's state.
- `class ApiError` - carries `status` and the decoded JSON `body` so 409-style refusals (e.g. delete warnings list) survive to the caller.
- `brand.ts` - the product identity on the screen side: `brand` (name, tagline, slug, logo/mark/favicon URLs) from the generated `src/brand/brand.json`, same source the binary embeds. `storageKey(name)` prefixes every `localStorage` key with the slug (`pom.theme`, `pom.locale`, `pom.connectionKey`, `pom.chat.sessions`, `pom.deploy.last`, `pom.network-layout`), which is what makes renaming the product one edit - and why a rename drops local preferences, deliberately: they belong to another product's keys.
- `setConnectionKey` / `getConnectionKey` - module-level credential persisted in `localStorage` key `codai.connectionKey`; seeded by `useNode()` from `GET /api/ui/node`. A changed key also clears conditional GET snapshots.
- `streamSse(path, body, signal)` - async generator yielding each `data:` payload of an SSE stream (blank-line framing); used by Chat for `/v1/chat/completions`.
- `api.get/post/del` - typed conveniences over `request`.
- `devUpdate(nodeId)` - update+restart a machine, repassing to a peer when `node_id` is not this node (`src/devupdate.rs`).
- `addNode` / `removeNode` - add/remove one machine to a deployed model (`/api/ui/deploy/node{,/remove}`).
- `undeployPipeline(group)` - take the whole pipeline out of service: `POST /api/ui/undeploy` asks every machine serving the model to release RAM/VRAM at once and returns an `UndeployReport` (machine by machine; `released` only when that node confirmed). Nothing is deleted from disk.
- `leaveNetwork(nodeId?)` - `POST /api/ui/leave`. Without `nodeId` this node leaves; with a peer's `node_id` this node forwards the request to that peer (`webui::forward_leave`) - the panel only uses relative paths, so acting on another machine is always a relay through the local node.

### types.ts
- Authoritative shapes with pointers to their Rust origin (comments cite `src/webui.rs`, `src/sysmon.rs`, `src/runtime.rs`, `src/transfer.rs`, `src/admin.rs`, `src/reqlog.rs`, `src/plan.rs`, `src/deploy.rs`).
- `ServerState` - `"disabled" | "starting" | "ready" | "failed" | "thermal"`; the thermal state is distinct from an unreachable or failed peer.
- `ClusterView` / `PeerView` / `ModelCapability` - the network/dashboard model.
- `ShardSet` identifies a slicing round with the technical `group`, readable
  `model_label` and `pom_id`; `DeploySelection`, `DeployStep`, `DeployTpTarget`, `DeployPlan`,
  `RuntimeParams`, `DeployCandidate`, `DeployMachine` are the deploy screen
  shapes. `DeployPlan` also carries the operational name, strategy, RTT route
  alternatives and projected load, while `DeployMachine` carries GPU/RAM
  capacity plus optional live GPU/RAM usage percentages. `DeployStep.parts`
  carries merged-slice steps, `primary` marks whoever holds layer 0, and the
  optional `exec_start`/`exec_end` pair identifies the sub-range actually
  executed by that step. `start_layer`/`end_layer` remain the physical range
  loaded on disk, so old responses without the pair fall back to that range.
  `DeployPlan.tp_targets` describes pre-sliced TP ranks separately from
  pipeline steps, including the source hash/path used by a transfer.
  `RuntimeParams.fit_memory` enables llama.cpp memory fitting and is normalized
  off for tensor parallel deployment. `RuntimeParams.inference_mode` is the
  persisted `auto`/`dense`/`moe` preference. `DeployPlan.is_moe` comes from the
  node's GGUF expert metadata; the deploy screen must not infer MoE from a
  filename. `inference_mode_supported` and `inference_mode_reason` keep an
  unsupported path visible instead of presenting a misleading selection.
  `DeployPlan.capabilities` is the backend tri-state contract for conditional
  global controls, including source and reason. The UI does not infer model or
  topology support from names; unknown remains visible but disabled.
- `isWholeModel(m)` - a whole `.gguf` vs a slice (`shard_range == null`).
- `SizedRange` / `SizePlan` / `TpSlicePlan` / `PlannedShard` / `SplitPlan` -
  size-, tensor- and budget-driven slicing plans. Budget-driven shards expose
  `weight_bytes`, `kv_bytes` and `total_bytes`; those are the byte arithmetic
  returned by the node planner, not display-only estimates.
- `UndeployOutcome` / `UndeployReport` - per-machine result of an undeploy: `released` is `true` only when that machine confirmed, `error` carries the reason otherwise.
- `TransferOffer.md5` is documented as always empty: verifying the file is the receiver's job after accepting (`/api/transfer/manifest`), because computing the digest to offer held the notification up for the whole file read (3 min 23 s of MD5 on a 4.2 GB slice).

### queries.ts
- `useIsSetupMode()` - one-shot probe (`retry:false`, `staleTime:Infinity`): 404 on `/api/ui/node` means the node runs only the setup router. Its own query so the shell does not flip between setup and dashboard on poll errors.
- `useNode()` - polls `/api/ui/node` every 4s, stores `connection_key`.
- `usePeerRuntime(nodeId, enabled)` 15s (stale 10s) - o `/health` daquela
  máquina, por `optional` (um nó sem a rota devolve 404 e a seção some). O
  `enabled` só é ligado com o card de uma máquina servindo montado: perguntar
  sempre seria um fan-out por peer a cada ciclo do Painel, por uma seção quase
  sempre fechada.
- `useCluster()` 5s with conditional GET, `useLocalCatalog()` 30s (stale 15s), `useClusterCatalog(enabled)` 60s, `usePaths()` (stale 30s), `useCacheTypes()` (stale 30s), `useJobs()` 2s while running or 10s idle, `useTransfers()` 2s, `useOffers()` 2s while waiting or 10s idle, `useAdminNodes()` 8s, `useRequestLog({node_id, api_key, prompt, from, to})` 3s, `useServableModels()` 30s.
- `useCluster()` 5s with conditional GET, `useLocalCatalog()` 30s (stale 15s), `useClusterCatalog(enabled)` 60s, `usePaths()` (stale 30s), `useCacheTypes()` (stale 30s), `useJobs()` 2s while running or 10s idle, `useTransfers()` 2s, `useOffers()` 2s while waiting or 10s idle, `useAdminNodes()` 8s, `useRequestLog({node_id, api_key, prompt, from, to})` 3s, `useServableModels()` 30s. Os dois catálogos mudam para polling de 2s enquanto `metadata_complete` é falso, então o inventário rápido é substituído pelos cabeçalhos sem esperar o intervalo normal.
- `optional(promise, fallback)` - swallows only 404 (an older node lacking a route is not an error to surface); used by jobs/transfers/offers/request log.
- `useOffers()` uses 2s while an offer is waiting and 10s when empty. An offer
  is a question waiting on a human, not a progress bar. With the sender no
  longer hashing the file before offering, the active cadence keeps that
  question responsive without charging an idle tab the same rate.
- `invalidateAfterAction(qc)` / `useInvalidate()` - after a mutation, refetch node, cluster, jobs, admin-nodes and models at once.

### chatStore.ts
- `useChatSessions()` - `localStorage` list (`codai.chat.sessions`) capped at `MAX_SESSIONS` 50, kept in sync across tabs via the `storage` event; returns `{sessions, upsert, remove, clear}`.
- `turnsOnly(messages)` - drops `role: "event"` entries (model-change marks) before the transcript travels in a request.
- `turnsOnly(messages)` - drops event marks and substitutes `requestContent` when a visible chat message carries private telemetry-analysis content.
- `titleFrom(text)` / `emptySession(model)` / `newId()` - defaults and ids; `titleFrom` stops the first message from overwriting a `titled` session's chosen name.

### chatStream.ts
- `consumeChatDelta(previous, delta)` - pure state machine that separates `<think>` reasoning from visible content without leaking a split delimiter across chunk boundaries (partial suffix held in `pending`).
- `finishChatStream(previous, metrics)` - flushes an incomplete trailing delimiter as literal text and records tokens/sec, duration and trace.
- `isPhasedHop(hop)` - discriminates `PhasedHopTrace` (split prefill/decode) from `LegacyHopTrace` so the telemetry table can render both shapes.
- Types `RequestTrace`, `PhaseTrace`, `HopTrace` - the per-hop telemetry
  contract from `inference/prefill.rs`/gateway. `PhaseTrace.selected_path` is
  the GGUF selection, while `executed_engine` and `executed_engines` are the
  backend facts (`llama.cpp`, `moe` or unknown when absent). The mismatch and
  mixed flags are evidence from the request, not a UI guess.
  `RequestTrace.cached_tokens` is optional on purpose: a node that does not
  report prefix reuse means "unknown", and the chat shows nothing rather than
  claiming savings it cannot prove.

### deployMemory.ts
- `rememberDeploy(group, memory)` / `recallDeploy(group)` - last deploy (selection, params, merge) per model group in `localStorage` `codai.deploy.last`, capped at 20 models, oldest dropped by `at`.
- `usableSelection(remembered, ranges, machineIds)` - prunes remembered rows against what the cluster still offers (deleted slices, left machines); returns `null` when nothing survives so the caller keeps the node's own suggestion.

### profile.tsx / access.ts
- `PROFILES` / `Profile` - `["admin", "node"]` e a união literal.
- `DEFAULT_PROFILE` - `admin`. Abrir como `node` esconderia telas de quem sempre
  as teve, sem a pessoa saber que existe perfil para trocar.
- `ProfileProvider({ children, initialProfile })` - estado inicial de
  `initialProfile` (usado pelos testes) ou do `localStorage` `pom.profile`;
  valor desconhecido guardado cai no padrão. Escreve `<html data-profile>`.
- `routeProfiles(path)` - os perfis que entram, ou `null` quando a rota é livre.
  Casa por segmento (`/logs` cobre `/logs/ao-vivo`, e **não** `/logs-antigos`) e
  a chave mais específica vence.

### theme.tsx
- `ThemeProvider` / `useTheme` - reads the theme from `document.documentElement.dataset.theme` (index.html stamped it pre-paint), writes `data-theme` + `localStorage` `codai.theme`. Dark is the default; the system preference does not override.

### toast.tsx
- `ToastProvider` / `useToast` - `push(kind, title, detail)`; error/warning linger 9s, others 4.2s; keeps the last 4 toasts; `fromError` surfaces `ApiError.message` verbatim.

### format.ts
- `bytes/megabytes/pct/num/duration/ms/rate` - `Intl.NumberFormat` in the active locale; null/non-finite render as a placeholder dash.
- `since(iso)` / `clock(iso)` - relative recency ("há 4 min" / "4 min ago") and clock time.
- `isServing(state)` / `serverStateLabel(state)` / `serverStateTone(state)` - the one place `ServerState` is compared/labeled, so screens never hand-roll the comparison.

### deleteModel.ts
- `deleteModel(path, name, confirm)` - confirm, `POST /api/ui/models/delete`, and on 409 with a `warnings` list re-confirms the list then retries with `force: true`. `confirm` is injected so screens choose how to ask (and tests need no `window`).

### networkGraph.ts
- `graphFromCluster(cluster)` - groups each peer's `capabilities` by `model_id` into `GraphRange[]` sorted by `start_layer`, for the model network canvas.

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
- `routes/Chat.tsx` is the only consumer of `chatStream`, `chatStore` and `chatScroll`, plus `streamSse` against `/v1/chat/completions`.
- `routes/DeployModel.tsx` is the only consumer of `deployMemory`.
- `components/ModelNetworkCanvas.tsx` is the only consumer of `networkGraph` + `networkLayout`.
- `networkPeers` is used by `components/ModelNetworkCanvas.tsx`,
  `components/PipelineRail.tsx` and `routes/Dashboard.tsx` (`pipelinePeers`),
  and by `components/PeerDetailCard.tsx` (`hasSlice`).
- `routes/WholeModels.tsx` and `routes/Shards.tsx` both use `deleteModel`.
- `components/Notifications.tsx` and `routes/Transfers.tsx` use `transferStatus` and `transferLabel`.
- `components/Sidebar.tsx` uses `theme`.
- `i18n/locales.ts` is imported by `api.ts` and `chatStore.ts` (`getActiveLocale`) for non-React localized strings.
- Backend endpoints hit from this folder: `/api/ui/node`, `/api/ui/cluster`, `/api/ui/models`, `/api/ui/cluster/models`, `/api/ui/peer/:node_id/runtime`, `/api/ui/paths`, `/api/ui/jobs`, `/api/ui/requests`, `/api/ui/models/delete`, `/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/undeploy`, `/api/ui/leave`, `/api/ui/dev/update`, `/api/transfer/jobs`, `/api/transfer/offers`, `/admin/nodes`, `/admin/api-keys`, `/v1/models`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
