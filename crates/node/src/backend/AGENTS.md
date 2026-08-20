# AGENTS.md - crates/node/src/backend/

## What this folder is

The HTTP backend of the `pom` node: every axum router and handler the
binary serves. It owns the single generation path (`gateway::generate_stream`,
which routes to the **distributed pipeline** - there is no fallback to a local
or peer Ollama, a missing plan fails the request), the gateway state all
handlers share, the two protocol adapters (OpenAI and
Anthropic), the operator control plane (`/admin/*`, `/api/ui/*`, `/api/ui/deploy/*`,
`/api/ui/undeploy`),
slice transfer over HTTP `Range`, and the in-memory request log. No P2P, no
inference, no model loading lives here - those are `network/`, `inference/`,
`models/`. All state is `crate::gateway::GatewayState` (or `crate::auth::AuthState`
where a handler only needs the config); handlers are axum functions, most
behind an auth extractor.

## Files

- `auth.rs` (132) - auth extractors. `ConnectionAuth` (connection-key bearer
  only), `ApiKeyAuth(pub Caller)` (connection key or minted API key, either
  `authorization: Bearer` or `x-api-key`), `AuthState = Arc<RwLock<Config>>`.
  `is_valid_inference_token` aplica a mesma regra na porta 8181: a
  `connection_key` compartilhada ou uma API key ativa.
- `gateway.rs` (1542) - `GatewayState`, `start_gateway`, `generate_stream` /
  `generate_blocking` / `spawn_logged_generation`, prompt-budget math, the
  direct-reply relay route (`POST /v1/relay-result`), and the `testkit` test
  scaffold (peer/state_with/body_of).
- `webapp.rs` (214) - serves the frontend bundle from disk with a narrow SPA
  fallback (`config::ui_dist_dir()`); nothing is compiled into the binary.
- `webui.rs` (2974) - the `/api/ui/*` control API behind the dashboard:
  catalog, cluster view, slicing, apply/launch, merge, jobs, release, leave,
  storage paths and slice retention.
  (local or forwarded to a peer).
- `deploy.rs` - `/api/ui/deploy/*` + `POST /api/ui/undeploy`: survey de
  capacidade, estratégias simples/completa, RTT direcional, cobertura, carga projetada,
  escolha de máquinas e sequenciamento das cópias; re-surveys o cluster no
  deploy e mantém `group` técnico separado de `deployment_name` operacional.
  `route_options` liga somente máquinas selecionadas em estágios consecutivos;
  réplicas cruzam apenas para o próximo estágio, nunca para uma camada posterior.
  `undeploy` tira uma pipeline inteira do ar.
- `transfer.rs` (1542) - slice push/pull over HTTP `Range` with resume and MD5;
  offers carry only a `stat`, the digest is computed by the receiver. Pushes
  accept only a catalog basename in `filename`, resolve it server-side and
  require `.gguf`. Manifestos v2 são mesclados por `pom_id` no peer receptor,
  preservando `parts` e `tp_parts` locais.
- `openai.rs` (1253) - OpenAI-compatible `/v1/*` on top of the generation path.
- `messages.rs` (1038) - Anthropic Messages API (`/v1/messages`), same path.
- `tool_call.rs` (725) - tool-call text rendering + `TagScanner` output scan.
- `devupdate.rs` (647) - env-gated remote `git pull` + rebuild + self-restart.
- `admin.rs` (185) - operator control plane: nodes, block/unblock, keys.
- `reqlog.rs` (205) - bounded in-memory request log (500 entries), `Caller`/`Entry`.
- **Toda chamada a peer sai por `GatewayState.peer_http`** (`PeerClients`), que
  fixa o certificado do destino pelo node_id. Não existe mais cliente
  compartilhado sem pinning no estado: com TLS ligado ele não verifica ninguém,
  e um chamador que o pegasse por hábito falhava calado. Loopback e Ollama usam
  `peer_http.plain()`.
- `serve.rs` - o laço de accept: espia o primeiro byte e serve TLS ou texto
  claro na mesma porta. `TlsServer` (certificado, modo estrito, duração do
  último handshake). Ver `docs/subsystems/rede-p2p.md`, seção 5.
- `session.rs` (111) - `POST /handshake` (connection key + blocked-nodes check).

## Key functions and routes

- `gateway.rs` - `GatewayState` (network, ollama, default_model,
  local_assignment, loads, http, runtime, gateway_port, auth, transfers,
  reply_inbox); `FromRef<GatewayState> for AuthState`; `peers_visible_to_cluster`
  filters blocked nodes; `ready_for_pipeline` health-checks each peer's
  `inference_url()/health` (10s timeout) before planning; `all_peers` appends
  the local node with `self_dial_host` (advertised > local IP > loopback) and
  advertises `relay/v2-direct` in the capability list alongside
  `frame/v1-f16`, `frame/v2-zstd-f16`, `relay/v1-chain` and
  `speculate/v1-lookup`; `speculate/v1-mtp-sliced` goes out unconditionally and
  `speculate/v1-mtp` only when `mtp_ready` (the node has the `nextn` block);
  `self_reply_hosts()` lists the addresses by which a
  peer reaches this node (`self_dial_host` first, then `self_addr_candidates`);
  `prompt_budget_of(n_ctx, max_sessions, max_new_tokens)` - the KV cache is
  split across sessions, so the window is `n_ctx / max_sessions`, and `n_batch`
  deliberately does NOT appear (prefill chunks the prompt); `generate_stream`
  builds a `crate::direct::DirectReply` from `reply_inbox` +
  `self_reply_urls(self_reply_hosts(), gateway_port)` and hands `direct` to the
  prefill and decode executors, so the tail can answer this node directly
  (log says `resposta=direta|anel|hub`: direta when `direct` is Some and the
  plan supports direct reply, anel for `relay/v1-chain`, hub as the fallback);
  MTP on a sliced plan (`mtp_enabled` + `plan.supports_mtp()` + >= 2 hops):
  the tail drafts on its own reply (`next_draft`), the gateway stores it and
  injects it as `speculate.draft` on the next round, and the head only
  decodes; `especulando` stops when the head refused a ready draft
  (`draft_sent && !step.speculated`) or the tail returned no `next_draft`
  (`!draft_sent && step.next_draft.is_none()`), then the gateway calls
  `prefill::step` (the direct reply) for the rest of the request - insisting
  would pin the chain to the bubbled ring (2N hops per token);
  emits `GenEvent::Token`/`Done(GenSummary { source: "pipeline:hop_chain", .. })`
  /`Failed`; `generate_blocking` collapses it. `start_gateway` registers
  `POST /api/generate` (+ alias `POST /generate`), `GET /peers`, `GET /health`,
  `POST /handshake` (session::handle_handshake), and `POST /v1/relay-result`
  (`crate::direct::RELAY_RESULT_PATH`, 1 MB body limit, deliberately no auth -
  the per-frame `Ticket` is the credential; handler `handle_relay_result`
  delivers into `reply_inbox`); it serves with
  `axum::serve(...).tcp_nodelay(true)` because Nagle delayed the small SSE
  writes. `#[cfg(test)] mod testkit` is the shared scaffold for route tests - a
  whole `GatewayState` with the peer table seeded by hand, no UDP, nothing
  touches the LAN: `peer(node_id, hostname, gateway_port)` builds a loopback
  `PeerInfo`, `state_with(node_id, peers)` the state, `body_of(response)` the
  JSON body (used by the transfer, deploy and webui tests).
- `openai.rs` - `DEFAULT_MAX_TOKENS=512`, `HARD_MAX_TOKENS=4096`;
  `render_prompt` flattens the conversation into one user message (single
  lone user message with no tools passes through byte-identical to
  `/api/generate`); `approx_tokens` = chars/4; `model_ids` from peers + active
  model; `resolve_model` maps unknown names to the active model;
  `chat_completions`/`completions` -> `blocking_response` or `sse_stream`;
  `chat_message_json` (flat content + `tool_calls` array),
  `apply_scan_events_openai` (tool_call deltas), `chunk_json` (+
  `x_pom` extension with source/tps/duration/partial/trace),
  `cors_headers` (allow `*`). Routes: `GET /v1/models`, `GET /v1/models/:id`,
  `POST /v1/chat/completions` + OPTIONS, `POST /v1/completions` + OPTIONS.
- `messages.rs` - Anthropic `POST /v1/messages` (+ OPTIONS) and
  `POST /v1/messages/count_tokens`; its own `render_prompt` (system/messages/
  tools split), `content_blocks` for tool_use/tool_result, `stop_reason`
  (end_turn / max_tokens / tool_use), `blocking_response` and `stream_response`
  with the Anthropic SSE event names. `tool_choice` and `stop_sequences` are
  parsed-and-ignored (serde drops unknown fields).
- `webui.rs` - `CATALOG_TTL=20s`, `pub(crate) PEER_TIMEOUT=4s` (reused by
  deploy.rs). Registers the `/api/ui/*` routes: `node` (GET node_info),
  `models` (GET), `models/delete` (POST), `cluster` (GET), `cluster/models`
  (GET), `paths` (GET+POST), `plan` (POST), `apply` (POST), `launch` (POST),
  `stop-inference` (POST), `slice` + `slice/plan` (POST), `jobs` (GET) +
  `jobs/:id` (GET), `unload` (POST), `release` (POST) + `release/candidate`
  (GET), `leave` (POST), `rejoin` (POST), `network/rtt` (GET autenticado),
  `requests` (GET ->
  `crate::reqlog::list_requests`). `leave_network` now takes an optional
  `LeaveRequest { node_id }`: absent (or this node's own id) keeps the old
  behavior - this node detaches; a peer's id is **forwarded** to that peer's
  same route (`forward_leave`, same pattern as `dev/update` - the panel only
  talks to its own node, and only the owner of a process can detach it). Also
  the job store (`Job`, `JOB_RETENTION=600s`,
  `jobs()`/`new_job`/`set_job`/`set_job_progress`),
  catalog caching (`CachedCatalog`, `warm_catalog` called at startup,
  stale-while-revalidate), `merge_parts` (temp file with unique suffix, rename
  at the end; reuses an already-complete output, preserves the source
  `pom_id`, and invalidates the catalog),
  `run_apply`/`run_slice`/`release_slice`, and `get_setup_key`/`save_setup`
  which are deliberately NOT registered here - the setup mode router in
  `main.rs` serves them only while there is no `pom.json` (they cannot be
  authenticated and would rotate the connection key on a configured node).
- `deploy.rs` - `DeployKind` (`shards` | `whole` | `tensor_parallel`) escolhe
  qual inventário o `survey` lê: `shard_sets`, ou os modelos inteiros via
  `parse_whole`, que lê um GGUF não fatiado como a faixa `0..total-1`. O
  `tensor_parallel` (caminho C) nomeia o grupo (primary + workers); a v1 da
  varredura lê o modelo do primary, e a descoberta de workers fica junto do
  fork do llama.cpp. O pedido carrega `TpLaunch {size, primary_node_id,
  worker_node_ids}` - o operador escolhe primary/workers por node_id e o nó
  deriva a `rank0_url` da `Machine.inference_url` (`tp_identity`). Validação em
  duas camadas antes do deploy: `tp_group_checks` (tamanho, presença no
  cluster, modelo inteiro, sem repetição) dentro do `plan`, e
  `tp_shardability_check` (rota `POST /api/ui/tp/check`, handler
  `webui::tp_check` -> `splitter::validate_tensor_parallel`) que recusa MoE/
  SSM/weight-tied na tela. Essa rota é **autenticada** (`ConnectionAuth`), então
  a chamada leva `bearer_auth(connection_key)`: `peer_http.for_peer` resolve só
  o TLS, credencial é por requisição. Sem ela o primary responde 401 e nenhuma
  implantação TP passa, em máquina nenhuma. A leitura da resposta vive em
  `tp_check_verdict`, separada para ser testável e para repetir o `error` do
  peer em vez de chamar toda resposta sem `ok` de binário velho. Daí em diante é o mesmo planner, o mesmo
  `launch_targets` e o mesmo `launch` - não há segundo caminho. Só as recusas
  mudam de substantivo (`DeployKind::unit`). Ver
  `docs/subsystems/implantacao.md` §1.1 e §1.2.
  O `survey` preserva o `PeerId` libp2p e a capability do túnel de cada
  máquina; `tp_members` transfere esses dados para cada rank, permitindo que
  `broadcast_to` e o reduce usem primeiro o túnel Noise, com cliente direto
  pinado como fallback quando o túnel não está disponível.
  O `register_tp_reduce` também recebe o tamanho do grupo. O nó publica esse
  tamanho e o snapshot de coletivas no campo `node_timings.tp`, sem misturar
  bytes ou esperas TP com a telemetria de pipeline.
  O mesmo `survey` agrega os relatórios autenticados de `/api/ui/network/rtt`:
  cada origem publica somente suas medições diretas, e `route_options` usa a
  chave `(from_node_id, to_node_id)`. A latência do gateway não é reutilizada
  para uma aresta entre dois peers remotos.
- `deploy.rs` - `launch_targets` é a fase 2 extraída de `run`: os passos do
  plano resolvidos contra a varredura pós-cópia, que é a única parte do deploy
  testável sem cluster (`a_whole_model_is_launched_as_one_plain_target`).
- `deploy.rs` - `RuntimeParams.fit_memory` fica ligado por padrão em shards e
  modelos inteiros para deixar o `llama.cpp` ajustar camadas à memória livre;
  `effective_params_for_kind` e o `apply` desligam o ajuste no tensor
  parallelism, que não suporta o fit automático em `split-mode tensor`.
- `deploy.rs` - `overlapping_layers`, `split_position`. Routes: `POST
  /api/ui/deploy/plan` (read-only, cheap), `POST /api/ui/deploy` (re-surveys
  the cluster, refuses on `plan.blocked` or empty steps), `POST
  /api/ui/deploy/node`, `POST /api/ui/deploy/node/remove`, `POST
  /api/ui/undeploy` (take a whole pipeline out of the air). The planner is a
  suggestion; the operator's `selection` in the request is authoritative.
  Rules from the architecture: pipeline order = layer order (layer 0 takes
  every prompt); one distinct range per node unless `merge` (adjacent ranges
  joined via `splitter::merge`); the exact same range on different nodes is a
  replica and a valid parallel route; the same whole model may likewise be
  selected on multiple nodes for balancing; distinct overlapping cuts are refused,
  and a gap only warns.
  **`steps` always comes back in layer order**, blocked plans included:
  `position` only means "place in the pipeline" in that order, `run` reads the
  launch order straight off the list, and the screen lines the plan up against
  rows it keeps sorted by layer. The grouping loop walks by target machine, so
  the sort is applied once at the end - without it, a machine holding two
  ranges emitted its second one ahead of everything in between and the deploy
  screen fell silent (2026-08-16). For the same reason an **empty
  `target_node_id` never groups**: "no machine" is not a machine, and lumping
  every unplaced range under one empty id produced a refusal naming a machine
  with no name.
  `undeploy_endpoint` re-surveys the cluster (never trusts the screen),
  resolves `UndeployRequest.group` via `resolve_group` (empty picks the only
  pipeline in the air; two pipelines is a 400 - never guess), then calls
  `release_one` per serving machine in parallel and returns 200 with an
  `UndeployOutcome` line per machine, always: a silent peer is
  `released: false` with the reason in `error`, never counted as success.
  Nothing touches disk - slices stay, only memory returns, through the peers'
  own `/api/ui/unload` (same route as the single-machine button; the local
  machine uses `runtime.stop()`). `undeploy_authority(role)` gates who may
  undeploy - today it lets every role through (see Defeitos anotados).
- `transfer.rs` - `CHUNK=1MB`, `DISK_HEADROOM_MB=512`, `GGUF_MAGIC=b"GGUF"`;
  `/api/transfer/send` receives `{to_node_id, filename}`, never a path.
  Routes: `GET /api/transfer/manifest`, `GET /api/transfer/download`,
  `POST /api/transfer/pull`, `POST /api/transfer/offer`, `GET
  /api/transfer/offers`, `POST /api/transfer/offers/:id/accept` +
  `/:id/reject`, `GET /api/transfer/jobs`, `POST
  /api/transfer/jobs/:id/cancel`, `POST /api/transfer/send`. Path safety is
  directional: `safe_model_path` confines the *pull* direction to `models/`
  (`servable_path` canonicalizes and checks `starts_with`), while
  `local_send_path` lets the *operator's own* screen offer any readable GGUF
  via an opaque `grant` id (`TransferState::grants`) so a path never travels.
  `ensure_gguf` (magic + extension, conjunction) gates every exit. **An offer
  carries no digest**: `send_to_peer` posts only a `stat` (name + real
  `size_bytes`, `md5` empty - the field still travels because an older peer
  requires it); hashing the whole file is what used to hold the bell on the
  other side for minutes (3 min 23 s for a 4.2 GB slice on the SD card). The
  receiver computes the MD5 itself, via `/api/transfer/manifest`, after
  accepting. **Accepting returns before the other side answers**:
  `spawn_download` registers the job right away (`bytes_total` 0, indeterminate
  bar, detail "conferindo o arquivo na origem"), then in a background task
  `fetch_manifest` asks for size + MD5 (`MANIFEST_TIMEOUT` = 30 min - a digest
  is a full read on a slow volume - and runs the disk-space check there too),
  then `run_download` writes a `.part` next to its destination - the model's
  folder for a **part** (`<shard_dir>/<modelo>-<id da rodada>/<sha256>.gguf`,
  `.pom` written first - the run id comes from the manifest, so two slicings of
  one model never share a folder), `<shard_dir>/recebidos/` for anything that belongs to no manifest -
  resumes with `Range`, verifies the sha256 it asked for (a part) or the MD5
  (anything else) before the rename, and cancels symmetrically on both
  sides (`cancel`/`notify_peer_cancel`, idempotent, works for jobs not yet
  known). A manifest/download error becomes a job `failed` with the reason,
  never overwriting an operator's `cancel`.
  `DEV_AUTO_ACCEPT_ENV` (`POM_DEV_AUTO_ACCEPT_TRANSFERS`, read once at
  startup via `dev_auto_accept`, the receiver's own variable) is OFF unless the
  receiver sets it; a sender cannot ask for it.
- `admin.rs` - `admin_routes()`: `GET /admin/nodes`, `POST
  /admin/nodes/{id}/block`, `POST /admin/nodes/{id}/unblock`, `DELETE
  /admin/nodes/{id}`, `POST /admin/regenerate-key`, `GET+POST /admin/api-keys`,
  `DELETE /admin/api-keys/{key}`, `GET /admin/transfer-offers` (same offers as
  `transfer.rs::list_offers`).
  `list_nodes` appends blocked-but-disconnected nodes so the panel is not empty.
- `devupdate.rs` - `ENV_GATE=POM_DEV_UPDATE`, `ENV_FEATURES=
  POM_BUILD_FEATURES`; `enabled()` = env `== "1"`; `gate_rejection` 403
  when closed. Routes `POST /api/ui/dev/update` (preflight: refuses non-main
  branch, detached HEAD, dirty tree; then `git pull --ff-only` + `cargo build
  --release`; no destructive commands) and `POST /api/ui/dev/restart`
  (`RESTART_DELAY=300ms` so the HTTP body clears before the process dies).
  Each node restarts itself; the panel forwards to a peer, the gate is checked
  at the destination. **It no longer writes the last-slice annotation** before
  restarting - `runtime.rs` owns that lifecycle (apply writes `Pending`, the
  `/health` watcher promotes to `Serving`); a `devupdate` write could only
  lie.
- `auth.rs` - `ConnectionAuth` reads only `authorization` (Bearer prefix
  optional), compares to `connection_key`; `ApiKeyAuth` accepts `authorization`
  or `x-api-key`, and the connection key counts as operator
  (`Caller::operator()`). `AuthRejection`: MissingHeader=401, InvalidKey=403.
- `reqlog.rs` - `CAPACITY=500` in a `OnceLock<RwLock<VecDeque<Entry>>>`;
  `Caller { token_label, token_hint, token_node_id }` (built by the auth
  extractor, the only place that sees the credential); `Entry` carries
  route/model/token/entry_node_id/hops/source/tokens/duration/status/error;
  `record` drops the oldest; `list_requests` filters by node_id/token and
  returns by-token/by-node tallies plus an error count.
- `tool_call.rs` - `render_tools_block` (empty when no tools),
  `render_tool_call` (`<tool_call><function=NAME><parameter=P>...</parameter>`,
  string args raw, others re-serialized). `TagScanner` incrementally splits a
  token stream into text / tool-call spans tolerating tags split across chunk
  boundaries (`longest_prefix_overlap`); `CallKind::Wrapped` (the real
  `<tool_call>` convention) vs `CallKind::BareFunction` (a tolerated
  wrapper-less `<function>NAME</function>` fallback, name-only, whitespace
  guard). `parse_call_body` accepts the nested `<function=...>` XML form or
  the legacy Hermes JSON-object form; malformed spans degrade to literal text.
  `parse_output` = scanner over the whole text -> `Vec<OutputPart>`
  (`Text` | `ToolCall { name, arguments }`), adjacent text merged.
- `webapp.rs` - `API_PREFIXES = ["api/", "v1/", "admin/", "peers", "health",
  "generate"]`; `routes<S>()` serves `GET /` (index) and `GET /*path`. A miss
  under an API prefix is a JSON 404, a miss with a file extension is "asset
  não encontrado", anything else falls back to `index.html`. `resolve`
  rejects `..`, absolute and prefixed components. Cache: index.html no-cache,
  hashed assets immutable (1 year). Missing bundle -> 500 explaining the
  `npm run build` / `POM_UI_DIST` remedy.
- `session.rs` - `handle_handshake` compares `connection_key` (403 invalid_key)
  then `blocked_nodes` (403 blocked), else approves with `peer_id`.

## Links to other folders

- This folder calls: `crate::network` (`Network`, `PeerInfo` - discovery,
  `alive_peers`, `advertised_host`, `self_addr_candidates`), `crate::pipeline`
  (`plan_pipeline`, `LoadTracker`, `PipelinePlan` - the distributed path),
  `crate::ollama` (`OllamaClient` - model resolution and `/health` only, no
  generation fallback), `crate::runtime`
  (`NodeRuntime`, `NodeConfig` - model/slice lifecycle), `crate::config`
  (`Config`, `ApiKey`, `ui_dist_dir()`, `shard_dir_path()`, `model_dir_paths()`),
  `crate::models`/`crate::catalog`/`crate::gguf`/`crate::plan`
  (`Catalog`, `GgufLayout`, `PeerBudget`, `pom::PomManifest`), `crate::splitter`
  (`MergePart`, `merge`), `crate::sysmon`, `crate::route` (`RouteTable` for the
  cluster view), `crate::pkarr_node` (`detect_local_ip`), `crate::prefill`
  (`RequestTrace` in the summaries), `crate::direct` (`ReplyInbox`,
  `DirectReply`, `Ticket`, `self_reply_urls`, `handle_relay_result`,
  `RELAY_RESULT_PATH` - the direct-reply relay that lets the tail answer the
  originator instead of bubbling through every hop), and
  `routing_contract` capability types.
- Who calls it: `crate::main` (`start_gateway` spawns the router, `warm_catalog`
  runs at startup, setup mode mounts only `webui::get_setup_key`/`save_setup`);
  the frontend bundle in `apps/frontend/dist` hits `/api/ui/*`; harnesses hit
  `/v1/*` (OpenAI/Anthropic) and `POST /api/generate`; peers call each other's
  gateway HTTP for transfers, deploy forwarding, and cancel notices.

## Defeitos anotados

- **The handshake binds `node_id` to `pubkey`, but does not prove possession of
  the private key** (`session::node_id_matches_pubkey`). `node_id` must be
  `SHA256(pubkey)` and the key must be a well-formed ed25519 point, so a caller
  holding the connection key can no longer claim another machine's `node_id`
  (which is the `blocked_nodes` key). A possession proof would need a signed
  challenge, i.e. a protocol change; open for decision.
- **`undeploy_authority` lets any role undeploy the whole pipeline**
  (`deploy.rs:1794` returns `Ok(())` for every role). Deliberate for now - the
  test `any_node_may_undeploy_for_now` (`deploy.rs:2242`) documents the open
  gate on purpose, so closing it is a conscious edit: replace the function
  body (the doc comment spells out the Principal-only check) and update the
  test. The route still needs the connection key (`ConnectionAuth`), so the
  port is operator-gated, not anonymous.
