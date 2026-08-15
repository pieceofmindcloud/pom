# src/lib

Shared browser-side logic for the rdma-p2p web UI: the HTTP client, all
react-query reads, localStorage persistence (chat history, last deploy, canvas
layout, theme, connection key), the streaming chat state machine, formatting,
toasts and every type the panel exchanges with the node.

This folder is pure frontend. Nothing here talks to peers directly - the node's
gateway serves the API and the UI on the same port, so every fetch uses a
relative path (`api.ts`) and the browser never learns a peer's address.

## Per-file

- `api.ts` - fetch wrapper over relative paths with connection-key auth; also the SSE reader.
- `chatScroll.ts` - whether the chat viewport is near the bottom (auto-scroll policy).
- `chatStore.ts` - conversation sessions persisted in `localStorage`, cross-tab synced.
- `chatStream.ts` - incremental parse of streaming deltas; `<think>`/`</think>` splitter and request trace types.
- `deleteModel.ts` - two-step delete of a GGUF with a warnings/force round trip.
- `deployMemory.ts` - last deploy per model remembered in `localStorage` for the Deploy screen.
- `format.ts` - number/time formatting in the active locale plus node-state labels.
- `networkGraph.ts` - flattens `ClusterView` into per-model layer-range lists for the canvas.
- `networkLayout.ts` - canvas node positions persisted per deployment fingerprint.
- `networkPeers.ts` - how the panel reads the peer list: who serves the active model (pipeline order) vs who is on the network without a slice.
- `queries.ts` - every read the panel does, as react-query hooks with polling intervals.
- `theme.tsx` - dark/light theme provider backed by `data-theme` + `localStorage`.
- `toast.tsx` - toast context (success/error/warning/info) with auto-dismiss.
- `transferStatus.ts` - predicates over `TransferJob["state"]` for the transfers UI.
- `types.ts` - all API shapes served by the node, one authoritative source.
- `*.test.ts(x)` - unit tests exist for chatScroll, chatStream, deleteModel, deployMemory, networkGraph, networkLayout, networkPeers, transferStatus (run with `npm test`).

## Key functions

### api.ts
- `request<T>(path, opts)` - the single fetch path: JSON body, optional `Authorization: Bearer <connectionKey>`, throws `ApiError` on non-2xx. Everything in the app goes through it.
- `class ApiError` - carries `status` and the decoded JSON `body` so 409-style refusals (e.g. delete warnings list) survive to the caller.
- `setConnectionKey` / `getConnectionKey` - module-level credential persisted in `localStorage` key `codai.connectionKey`; seeded by `useNode()` from `GET /api/ui/node`.
- `streamSse(path, body, signal)` - async generator yielding each `data:` payload of an SSE stream (blank-line framing); used by Chat for `/v1/chat/completions`.
- `api.get/post/del` - typed conveniences over `request`.
- `devUpdate(nodeId)` - update+restart a machine, repassing to a peer when `node_id` is not this node (`src/devupdate.rs`).
- `addNode` / `removeNode` - add/remove one machine to a deployed model (`/api/ui/deploy/node{,/remove}`).
- `undeployPipeline(group)` - take the whole pipeline out of service: `POST /api/ui/undeploy` asks every machine serving the model to release RAM/VRAM at once and returns an `UndeployReport` (machine by machine; `released` only when that node confirmed). Nothing is deleted from disk.
- `leaveNetwork(nodeId?)` - `POST /api/ui/leave`. Without `nodeId` this node leaves; with a peer's `node_id` this node forwards the request to that peer (`webui::forward_leave`) - the panel only uses relative paths, so acting on another machine is always a relay through the local node.

### types.ts
- Authoritative shapes with pointers to their Rust origin (comments cite `src/webui.rs`, `src/sysmon.rs`, `src/runtime.rs`, `src/transfer.rs`, `src/admin.rs`, `src/reqlog.rs`, `src/plan.rs`, `src/deploy.rs`).
- `ServerState` - `"disabled" | "starting" | "ready" | "failed"` only, no `| string` escape hatch; the doc comment records that the loose first version made every machine look idle.
- `ClusterView` / `PeerView` / `ModelCapability` - the network/dashboard model.
- `DeploySelection`, `DeployStep`, `DeployPlan`, `RuntimeParams`, `DeployCandidate`, `DeployMachine` - deploy screen shapes; `DeployStep.parts` carries merged-slice steps, `primary` marks whoever holds layer 0.
- `isWholeModel(m)` - a whole `.gguf` vs a slice (`shard_range == null`).
- `SizedRange` / `SizePlan` / `PlannedShard` / `SplitPlan` - size- and budget-driven slicing plans.
- `UndeployOutcome` / `UndeployReport` - per-machine result of an undeploy: `released` is `true` only when that machine confirmed, `error` carries the reason otherwise.
- `TransferOffer.md5` is documented as always empty: verifying the file is the receiver's job after accepting (`/api/transfer/manifest`), because computing the digest to offer held the notification up for the whole file read (3 min 23 s of MD5 on a 4.2 GB slice).

### queries.ts
- `useIsSetupMode()` - one-shot probe (`retry:false`, `staleTime:Infinity`): 404 on `/api/ui/node` means the node runs only the setup router. Its own query so the shell does not flip between setup and dashboard on poll errors.
- `useNode()` - polls `/api/ui/node` every 4s, stores `connection_key`.
- `useCluster()` 5s, `useLocalCatalog()` 30s (stale 15s), `useClusterCatalog(enabled)` 60s, `usePaths()` (stale 30s), `useJobs()` / `useTransfers()` 2s, `useOffers()` 2s, `useAdminNodes()` 8s, `useRequestLog({node_id, token})` 3s, `useServableModels()` 30s.
- `optional(promise, fallback)` - swallows only 404 (an older node lacking a route is not an error to surface); used by jobs/transfers/offers/request log.
- `useOffers()` polls at 2s (was 5s): an offer is a question waiting on a human, not a progress bar, and with the sender no longer hashing the file before offering (the digest moved to the receiver, after accept) an offer now lands in milliseconds - a 5s cadence only added waiting on top of a 5s poll.
- `invalidateAfterAction(qc)` / `useInvalidate()` - after a mutation, refetch node, cluster, jobs, admin-nodes and models at once.

### chatStore.ts
- `useChatSessions()` - `localStorage` list (`codai.chat.sessions`) capped at `MAX_SESSIONS` 50, kept in sync across tabs via the `storage` event; returns `{sessions, upsert, remove, clear}`.
- `turnsOnly(messages)` - drops `role: "event"` entries (model-change marks) before the transcript travels in a request.
- `titleFrom(text)` / `emptySession(model)` / `newId()` - defaults and ids; `titleFrom` stops the first message from overwriting a `titled` session's chosen name.

### chatStream.ts
- `consumeChatDelta(previous, delta)` - pure state machine that separates `<think>` reasoning from visible content without leaking a split delimiter across chunk boundaries (partial suffix held in `pending`).
- `finishChatStream(previous, metrics)` - flushes an incomplete trailing delimiter as literal text and records tokens/sec, duration and trace.
- `isPhasedHop(hop)` - discriminates `PhasedHopTrace` (split prefill/decode) from `LegacyHopTrace` so the telemetry table can render both shapes.
- Types `RequestTrace`, `PhaseTrace`, `HopTrace` - the per-hop telemetry contract from `inference_server.py`/gateway.

### deployMemory.ts
- `rememberDeploy(group, memory)` / `recallDeploy(group)` - last deploy (selection, params, merge) per model group in `localStorage` `codai.deploy.last`, capped at 20 models, oldest dropped by `at`.
- `usableSelection(remembered, ranges, machineIds)` - prunes remembered rows against what the cluster still offers (deleted slices, left machines); returns `null` when nothing survives so the caller keeps the node's own suggestion.

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

### transferStatus.ts
- `isFinishedTransfer` / `isActiveTransfer` / `canCancel` - state predicates; only `running`/`verifying` can be cancelled.

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
- `components/Notifications.tsx` and `routes/Transfers.tsx` use `transferStatus`.
- `components/Sidebar.tsx` uses `theme`.
- `i18n/locales.ts` is imported by `api.ts` and `chatStore.ts` (`getActiveLocale`) for non-React localized strings.
- Backend endpoints hit from this folder: `/api/ui/node`, `/api/ui/cluster`, `/api/ui/models`, `/api/ui/cluster/models`, `/api/ui/paths`, `/api/ui/jobs`, `/api/ui/requests`, `/api/ui/models/delete`, `/api/ui/deploy/node`, `/api/ui/deploy/node/remove`, `/api/ui/undeploy`, `/api/ui/leave`, `/api/ui/dev/update`, `/api/transfer/jobs`, `/api/transfer/offers`, `/admin/nodes`, `/admin/api-keys`, `/v1/models`.

## Defeitos anotados

- `deleteModel.ts` hardcodes pt-BR confirm strings ("Apagar ... do disco desta máquina?", "Atenção:", "Apagar mesmo assim?") instead of going through `i18n/locales`; with the UI in English the delete prompt still reads Portuguese. Same pattern in `toast.tsx` (`aria-label="Fechar aviso"`).
- `networkLayout.ts` `writePositions` and `clearPositions` write `localStorage` without a try/catch, unlike every other storage helper in this folder (quota/security exceptions would throw from the canvas drag handler).
- `transferStatus.ts` exports a `TransferState` union type that nothing outside the module uses (the predicates all type on `TransferJob["state"]` directly); the export is dead.
