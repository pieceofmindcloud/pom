# AGENTS.md - crates/node/src/backend/

## What this folder is

The HTTP backend of the `pom` node: every axum router and handler the
binary serves. It owns the single generation path (`gateway::generate_stream_with_stops`,
which routes to the **distributed pipeline** - there is no fallback to a local
or peer Ollama, a missing plan fails the request), the gateway state all
handlers share, the two protocol adapters (OpenAI and
Anthropic), the operator control plane (`/admin/*`, `/api/ui/*`, `/api/ui/deploy/*`,
`/api/ui/undeploy`),
slice transfer over HTTP `Range`, and the persistent request audit. No P2P, no
inference, no model loading lives here - those are `network/`, `inference/`,
`models/`. All state is `crate::gateway::GatewayState` (or `crate::auth::AuthState`
where a handler only needs the config); handlers are axum functions, most
behind an auth extractor.

The gateway keeps the pipeline transport separate from the administrative
surface. `/api/ui/**` and `/admin/**` plus the static UI surface pass through
the same boundary: localhost mode checks the real peer address, network mode
checks a server-side `HttpOnly` session minted from `X-POM-Access-Token`, and
the `connection_key` remains the private-network transport credential. Public
network control calls carry an Ed25519-signed `x-pom-control-signature`
envelope whose issuer must be a current authority in the trust chain. The client
inference routes (`/api/generate`, `/v1/models`, `/v1/chat/completions`,
`/v1/completions` and `/v1/messages`) accept only active inference API keys.
The gateway callbacks `/v1/relay-result` and `/v1/remote-stream-result` use
the per-frame `Ticket` in the body and no client API key. The gateway also
serves `/v1/link` for the persistent full-duplex return of `remote-stream`;
its callback frame is authenticated per envelope and falls back to
`/v1/remote-stream-result` only after an explicit transport failure. The separate
inference listener accepts the connection key on private networks; public
networks use TLS transport without a shared secret. A public node keeps a
local `connection_key` for the browser/admin boundary, but it is never
shared with peers or used for public admission. Inference frames are not
wrapped in control signatures. `/health`, `/peers` and `/handshake` remain
dual-stack.

Before that exposure check, `gateway::require_admin_for_surface` consults the
single `webui/routes/mod.rs` data table with the request method and the
persisted `Config.profile`. A rule may name a method, and the most specific
wins - longest prefix first, then the one that names a method - which is what
lets `GET /admin/nodes` open to `node` while blocking, removing and promoting a
machine stay `AdminOnly` under the same prefix.
A browser profile mismatch returns HTTP 403 with a JSON error, path and profile
before the handler runs. A valid `InternalNodeAuth` is the explicit exception
for node-to-node fan-out, so a receiver's local profile cannot reject a
legitimate pipeline control call; the handler still verifies the signed admin
mutation where authority changes. Setup and the separate inference listener
are marked out of scope in the table and are not silently treated as gateway
routes.
Os testes do gateway cobrem essa diferença por método: `GET /api/ui/deployments`
é leitura `NodeAdmin`, enquanto as ações de implantação e parada permanecem
`AdminOnly`; chamadas internas autenticadas continuam independentes do perfil
local do receptor.

O gateway registra atividade por peer em `reqlog::RequestActivity` e o gateway
de geração dispara `reload_model` em paralelo para os peers ociosos da pipeline
planejada. The event is idempotent and answers at once: `ready`, or `loading`
when the peer was idle (the call starts the load) or already loading. The
gateway repeats it every second until every planned peer is `ready`, with a
15 min ceiling, instead of holding one call open for the whole model load;
a 503 carries the reason a load cannot happen and ends the wait. After the
wake it waits for the peers' announcement and plans again
(`generate_stream_inner`, span `pipeline_wake`). O descarregamento continua
sendo avaliado pelo monitor já existente.

O `serve_with_quic` encerra o endpoint HTTP/3 com um canal one-shot, para que
um pedido de parada emitido antes do primeiro poll da task não seja perdido.
Depois da drenagem TCP, ele aguarda o endpoint QUIC por no máximo 2 s e aborta
a task se o transporte não fechar; assim o supervisor do runtime não fica
preso em `endpoint.wait_idle()` durante um reload. O supervisor tem 5 s no
total para concluir a geração anterior.

`POST /api/ui/network/advertise-addr` é `InferNodeAdmin` porque atualiza
somente o endereço anunciado deste processo e não recebe alvo nem repassa a
chamada. As rotas administrativas com parâmetro de nó usam a sintaxe `:id`
do Axum 0.7. A webapp serve a SPA por um fallback do router, então um handler
concreto de POST ou DELETE vence e um caminho mutável desconhecido, depois da
autorização, devolve 404, não o 405 produzido por um wildcard registrado apenas
para GET. Perfis sem escopo continuam recusados antes do fallback.

Two decisions the table cannot express, because they depend on the body rather
than the path, live in their handlers: `POST /api/ui/leave` (leaving is
abdication and belongs to every profile, but forwarding the request to
*another* machine needs effective administrative authority - the forward
travels with the internal credential, which the target does not judge by the
caller's profile) and the trust anchor. In both, `profile: admin` alone grants
nothing; `admin_trust::effective_authority` decides.

Every administrative refusal names itself in the `x-pom-admin-error` header
(`ADMIN_ERROR_*` in `auth.rs`): `missing-token`, `invalid-token`,
`no-peer-address`, `local-only`. The body sentence is for the person and
changes with the locale; the header is the stable contract the browser reads to
know its session died and return to the access gate, instead of printing the
raw 401 inside whatever screen was open.

The `webui` route registry is organized under `webui/routes/`: `common.rs`
contains routes shared by every profile, `infer.rs` contains inference-profile
actions, `node.rs` contains local node/model operations, and `admin.rs`
contains administrative and lifecycle routes. `routes/mod.rs` is the only
composition point. This organization is for legibility; the gateway's central
profile policy remains the effective authorization boundary.

## Files

- `auth.rs` - auth extractors. `AdminAuth` protects the administrative surface
  with the real peer address, an Argon2id-verified human password, a
  server-side in-memory session cookie, or the explicit internal-node channel.
  Passwords are never persisted in plaintext; sessions use a cryptographically
  random identifier, bind to the current password hash, expire after the single
  `ADMIN_SESSION_TTL` value and never
  persist in the browser or on disk. `ConnectionAuth` (connection-key bearer
  only), `InternalNodeAuth` (that key plus an internal marker), `ApiKeyAuth`
  (active inference API key only, from `authorization: Bearer` or `x-api-key`),
  `AuthState = Arc<RwLock<Config>>`.
  When both headers are present they are checked independently, so an invalid
  one cannot hide a valid one; a valid API key still supplies its non-secret
  `Caller`. Uma chave desconhecida é consultada no principal pelo canal interno
  autenticado, enviando só o hash, e falha fechada em timeout ou erro. A porta de inferência não
  usa a `connection_key` compartilhada somente em redes privadas; em redes
  públicas exige transporte TLS e o marcador interno, enquanto os callbacks de
  resultado validam o `Ticket` por quadro.
- `/handshake` é a prova de admissão da descoberta privada. A comparação exige
  que a chave configurada e a recebida sejam ambas não vazias; chave vazia
  nunca transforma um processo de setup em membro do enxame.
- `gateway/` - módulo dividido em estado/seleção, budgets e tipos de geração,
  geração, snapshots/planejamento, HTTP e testes: `GatewayState`,
  `start_gateway_with_ready`, `generate_stream_with_stops` /
  `generate_blocking_with_stops` / `spawn_logged_generation_with_stops`, o `StopMatcher` incremental,
  prompt-budget math, the
  direct-reply relay route (`POST /v1/relay-result`), the bounded remote-token
  callback route (`POST /v1/remote-stream-result`), the deployment-wide KV
  snapshot fan-out (`resolve_plan_for_model`, `snapshot_deployment`,
  `restore_deployment`, `erase_snapshot` — all-or-nothing across hops, mirroring
  `rewind_sessions`/`release_sessions`; used by `/admin/slots`), the persistent
  prefix cache (Phase 3, `slot_persist_prefix_cache`): `build_prefix_disk_index`
  builds `GatewayState.prefix_disk`, `generate_stream_with_stops` spills each
  retained turn to disk in a detached task and, on a memory miss, restores the
  longest strict-prefix snapshot before prefill (`prefix_reuse_status = "disk"`),
  the adaptive chunk tuner fits each hop and link as fixed + per-token cost
  from completed prefill traces for each model/route and simulates the chunked
  pipeline for the next prompt's size to pick the chunk (see
  `docs/subsystems/inference.md`, "Adaptive chunk"), capped by
  `prefill_chunk_tokens` and `n_batch`, and the `testkit`
  test scaffold (peer/state_with/body_of).
- `db.rs` - único armazenamento SQLite local, interno ao crate: schema criado no arranque,
  WAL/timeout, implantações, API keys e revogações, preferências térmicas,
  segredo do Hugging Face, histórico durável do catálogo e histórico de
  conversas e linhas do log de processo por `deploy_id`. Cada admin efetivo
  mantém sua própria réplica dos logs e da auditoria. Os logs preservam a
  ordem de chegada nesta réplica, guardam o timestamp de origem só como dado
  informativo e acumulam descartes da fila limitada. `try_prune_cluster_logs`
  apaga somente `cluster_logs`, em transações `BEGIN IMMEDIATE` de até 256
  linhas numa conexão com timeout zero; contenda encerra a rodada e o WAL é
  truncado depois de uma exclusão. O histórico identifica
  modelos inteiros pelo caminho local, sem
  hashear os pesos durante o registro do catálogo; hashes de conteúdo ficam
  nas fronteiras explícitas de implantação, fatiamento e transferência.
  `PomDb` mantém uma conexão configurada no arranque, reutilizada
  por todas as operações; um mutex explícito serializa escritas e a mesma
  conexão também faz leituras curtas em ordem, eliminando abertura e DDL por
  operação. O caminho vem de `config::database_path()`; `POM_DB_DIR` aponta
  para um volume e, sem ele, o banco fica na raiz do projeto.
  `local_setting_keys_with_prefix` permite que o runtime restaure dinamicamente
  os códigos de plugins persistidos sem manter uma lista compilada no binário.
- `webapp.rs` - serves the frontend bundle from disk at `/admin-ui/`; `/`
  redirects there and the static surface is covered by the same local-only
  boundary, while nothing is compiled into the binary.
- `model_provider.rs` - API agnóstica de status do CLI, autenticação, busca,
  licença, detalhes, espaço e downloads remotos. Injeta `DynModelProvider`, usa
  `webui::Job` como estado persistente, `CancellationToken` por download e
  semáforo de dois processos; não monta comandos nem expõe token ou stderr.
  Cada download escolhe a primeira pasta de modelos não vazia resolvida da
  configuração, passa essa pasta ao provider e registra o destino no job. A
  resposta de storage expõe as pastas elegíveis. O status estruturado em
  `GET /api/ui/model-provider/status` permite à UI distinguir CLI ausente,
  login necessário e sessão autenticada.
  `GET` e `POST /api/ui/model-provider/config` expõem somente a máscara e a
  fonte efetiva do token, e o POST salva o segredo em `pom.db` com modo
  restrito e atualiza o `HfTokenStore` sem reinício. O segredo nunca aparece
  em log, erro ou resposta.
  O handler clona o `ModelFile` antes de criar a task `'static`, não mantém
  empréstimos de respostas do provider em `tokio::spawn`, e as buscas de
  arquivos locais emprestam o nome a cada iteração.
- `webui/` - the `/api/ui/*` control API behind the dashboard, split into
  `mod.rs`, `routes/` (common, infer, node and admin), `catalog.rs`, `cluster.rs`, `params.rs`, `jobs.rs`,
  `jobs_apply.rs`, `slicing.rs`, `lifecycle.rs`, `setup.rs`, `trust.rs` and test fragments:
  catalog, cluster view, slicing, apply/launch, merge, jobs, release, leave,
  storage paths, thermal policy (`GET/POST /api/ui/thermal`), local auto-discovery
  preference (`GET/POST /api/ui/network/auto-discovery`), cluster process
  logs (`GET /api/ui/cluster-logs`, `POST /api/ui/network/logs`), slice retention
  e CRUD autenticado do histórico de conversas (`/api/ui/chat/history`).
  `GET /api/ui/plugins` (all profiles) lists local state for independent
  plugins restored or seen by the runtime (`crate::plugins`), each with its
  own features, validity, authenticated usage and updates.
  `POST /api/ui/plugins/activate` activates every `plugin_code` granted by the
  server in one batch; `POST /api/ui/plugins/:code/refresh|deactivate|update/check|update/apply`
  acts on one plugin. GitHub repositories are local to the node and use the
  AdminOnly routes `GET/POST /api/ui/plugins/github-repositories`,
  `POST /api/ui/plugins/github-repositories/:id/refresh`,
  `DELETE /api/ui/plugins/github-repositories/:id` and
  `POST /api/ui/plugins/github-repositories/:id/install` with
  `{release_id}`. POM queries GitHub directly and does not ask the license
  server whether a plugin requires a license. Sources are stored in
  `plugin_github_repositories`; installations use
  `plugin_public_install:<code>` without a lease or grant. Typed preferences
  stay local at `/api/ui/plugins/:code/preferences`; these routes do not
  activate a license. With `POM_DESENV=1`, `GET
  /api/ui/plugins/dev-sources` lists mounted sources and
  `POST /api/ui/plugins/dev-sources/:repository/rebuild-install` builds and
  installs or rebuilds a local plugin without prior activation. Both routes
  are AdminOnly; the handler accepts mounted direct-child `pom-plugin-*`
  sources only after validating their manifests and plugin codes, uses fixed
  Cargo commands and never exposes compiler output. A development-only installation persists in
  `plugin_dev_install:<code>` and starts the host without granting licensed
  features. If a license or GitHub installation already exists, its
  entitlement is preserved. The browser reads the commercial catalog without
  credentials. Purchases redirect the operator to the public store with the
  plugin code. Updating the `pom_core` release is a separate boundary in
  `crate::pom_core_update`: `GET /api/ui/pom-update/status`,
  `POST /api/ui/pom-update/check` forces an on-demand check, and
  `POST /api/ui/pom-update/apply` requires explicit confirmation before
  downloading the release and restarting. The cluster view transports
  `SystemLoad.gpus[].power_watts` as optional telemetry and keeps it `null`
  when the system does not report board power. The
  `/api/ui/network/geo-reference` route returns the SQLite country and city
  reference for UI selectors and uses `InferNodeAdmin`; it does not create or
  change network data. `GET /api/ui/deployments` é `NodeAdmin` para permitir
  que node veja o inspetor somente leitura, enquanto mutações e ações de
  implantação continuam sob a política administrativa existente.
  superfície de autoridade expõe o contrato de apresentação em
  `/api/ui/network/admin-authority`; pedidos infer-to-node e descida
  node-to-infer usam `/api/ui/network/request-node` e
  `/api/ui/network/relinquish-node`. Ofertas de autoridade compartilham o feed
  de ofertas para que o sino do browser não escolha o mecanismo de concessão ou
  assinatura.
  **A raiz de confiança é anterior a tudo isso** e vive em `webui/trust.rs`:
  `GET/POST /api/ui/network/trust-anchor` mostra a impressão digital da âncora
  e o estado ancorado/por ancorar, e instala a cadeia que o operador
  transportou. A rota é `CommonOrInternal` de propósito - um `profile: admin`
  por ancorar não passa em `AdminOnly`, que é exatamente o estado em que o
  operador precisa dela - e não exige a chave de conexão do cluster, pelo
  motivo registrado no cabeçalho do arquivo. As três travas da instalação são
  do servidor: loopback medido no `serve::PeerAddr` (nunca `admin_exposure`,
  que pode estar aberta à rede, e nunca `X-Forwarded-For`), nó ainda sem
  âncora, e a impressão digital digitada pelo operador conferida contra a
  cadeia colada. A validação e a conferência são as de `admin_trust`
  (`parse_transported`, `fingerprint_matches`, `adopt_anchor`), as mesmas que
  `pom admin-anchor` usa: não existe segunda política.
  `GET /api/ui/network/trust-share` serializa a cadeia local, inclusive o
  bloco `setup` assinado pelo wizard. Se uma cadeia primária antiga não tiver
  esse bloco, a rota recupera os dados persistidos em `pom.json`/`pom.db`,
  assina-os com o principal local e grava a cadeia antes do download; uma
  autoridade secundária não pode fabricar esses metadados.
  `POST /api/ui/chat-key` é permitido a `node` porque cria somente a chave
  local `pom-<node_id>` e a registra no principal pela rota interna assinada
  (só o hash vai; o segredo fica em `local_secrets`, `PomDb::node_api_key`, e
  é trocado quando não bate mais com uma chave ativa);
  a exceção não permite criar, revogar ou validar chaves de terceiros.
  A leitura `/api/cluster/network-settings` é interna e somente leitura: um
  nó adota do principal o documento completo de `NetworkSettings`, inclusive
  visibilidade e metadados, depois do primeiro `Announce` autenticado.
  `GET /api/ui/infer/heads?model=...` é a única leitura dedicada das cabeças
  de layer zero: entrega as candidatas e a escolha do planejador, sem peers
  gerais nem ações de controle. A rota é `InferNodeAdmin`, portanto admin e
  infer podem lê-la. A carga de cada candidata vem do próprio `/api/ui/node` e
  fica `null` quando o relatório não chega, para que a UI mostre indisponível
  em vez de zero. A validade do RTT é de cinco minutos; valores vencidos
  aparecem como diagnóstico, mas não entram na seleção da geração.
  `NodeInfo` carries the same thermal policy used by the runtime. It also
  carries `pom_desenv`, which is true only for the exact startup environment
  value `POM_DESENV=1`; this flag controls only local UI visibility, so every
  deploy boundary preserves development-only runtime fields received in the
  request. A missing inference mode uses `dense` (`llama.cpp`) by default.
  `slice/plan` and `slice` accept the
  explicit `tensor_parallel` mode with `tp_size`; the job extracts every rank
  and seals `tp_parts` in the same source-hash manifest used by pipeline
  `parts`. Jobs and the cluster view use conditional JSON responses:
  matching `If-None-Match` returns 304 without a body, and the cluster keeps
  a 2s fingerprinted snapshot cache that bypasses its TTL on known state
  changes.
  A leitura de cada peer no `/api/ui/cluster` percorre `gateway_candidates()`,
  não um endereço só, e alimenta a memória de falha do `RouteTable`
  (`mark_failure`/`mark_success`) - a mesma do caminho de inferência. Sem
  candidato utilizável ela chama `Network::request_peer_addresses` e devolve
  `rediscovering: true` em vez de `unreachable`, e `model_inventory[]` nomeia em
  `unreadable_nodes` os pares de quem a cobertura depende sem conseguir ler.
  O `PeerView.load` é a carga em voo reportada pelo próprio peer: para o nó
  local vem do `LoadTracker` local, e para peers remotos vem do JSON de
  `/api/ui/node`. Leitura remota ausente ou inválida permanece `null`, porque
  o zero do tracker local significa apenas que nenhuma requisição passou por
  este gateway.
  `model_inventory[].complete` e `/api/ui/chat/capabilities` só confirmam uma
  pipeline quando cada peer contribuinte tem `runtime_capacity` confirmado
  pelo `/health` ou está `idle`; capacidades de gossip durante o boot ficam
  visíveis como cobertura, mas não são apresentadas como prontas.
  Endereços IPv6 temporários rodam sozinhos: ver `docs/subsystems/p2p-network.md`,
  "Endereço que deixou de responder".
  `plan` reads the GGUF header from the node that owns `model_path` and, when
  calculating budget-driven ranges, includes the configured context, sessions
  and K/V cache types in the byte arithmetic. The response exposes weight,
  KV and total bytes per shard; absent KV metadata is an explicit bad request.
  `POST /api/ui/deploy/capacity-preview` (`capacity_preview_endpoint`,
  `ConnectionAuth`) is the deploy-time, per-step, peer-local version: it
  resolves a `NodeConfig`/`EngineConfig` for a locally-present slice, reads
  the GGUF layout and live `SystemLoad.gpus[]`, and returns projected VRAM per
  GPU. Weight and KV are calculated analytically; the endpoint never calls
  `llama_backend_init`, `llama_rs_fit_params` or the real-load `fit_plan`.
  `fits` is therefore an estimate. When the raw projection does not fit,
  `degraded` is true only when `fit_memory` is enabled and the requested layer
  count is the default all-layers value, which signals that the real load may
  trim the request; it does not claim a native result. The response note tells
  the operator that the real load confirms the final layout. KV quantization is in the byte math
  (`cache_type_k/v` from the installation); KV only counts toward VRAM when
  `offload_kqv` is on. A process admits four previews at once and returns
  `429` immediately when all slots are occupied. `split_projection` always
  divides projected weight+KV bytes proportionally to current free VRAM. The
  compute buffer is deliberately left out because the estimate is a planning
  signal, not a replacement for the real load.
  `RuntimeParams.prefill_max_inflight_chunks` is persisted with the other
  inference knobs and sets the prefill pipeline depth (1..8, default 8).
  `RuntimeParams.fit_memory_margin_mb` is also persisted and controls the
  per-device safety reserve used by llama.cpp's automatic layer fit (default
  256 MiB).
  `RuntimeParams.unload_if_idle` and `idle_minutes` are persisted with each
  deployment; the runtime applies them independently on each peer and the
  registry reports `idle` while a live peer has released its model.
  `/api/ui/peer/:node_id/runtime` forwards the inference `/health` unchanged,
  including `mtp_ready`, `mtp_allocation` and `mtp_unavailable_reason`, so the
  dashboard can distinguish MTP in VRAM, MTP in RAM, a model without `nextn`,
  and a memory failure.
  GGUF multipart sources are read through models::multipart::GgufSource:
  slice plans and extraction use all tensor parts, and model identity hashes
  the complete ordered set. An incomplete set is a concrete bad request, so
  the UI cannot start a one-file slice by accident.
  `POST /api/ui/slice/plan` and `/api/ui/slice` also accept one `range` with
  explicit start/end layers. The handler validates architectural boundaries,
  estimates the real output against the configured destination volume, and
  appends the resulting part to the source-hash manifest. Existing ranges are
  preserved and a matching file is reused only after SHA-256 verification.
  The same preflight checks architecture-specific output contracts before a
  plan or job is accepted. `deepseek4` requires its complete output head in a
  final slice; non-final ranges repeat the real required head weights, and the
  plan exposes their per-slice disk cost. If the source is missing any tensor
  that must be copied, the refusal names it before writing any payload.
  Slicing does not invoke automatic retention, so it cannot silently remove an
  older range. Jobs report named phases, source bytes read, current file, rate,
  ETA when measurable, terminal errors and partial outputs.
  (local or forwarded to a peer).
- `RuntimeParams.hidden_state_dtype` is persistido e encaminhado pelo mesmo
  caminho de apply/launch dos demais ajustes globais. f16 continua padrão, f32
  é a opção sem perda sem negociação extra; `DeployPlan.capabilities` para
  f8/i4 bloqueia os formatos com perda quando algum peer não anuncia suporte. O redeploy consulta
  `/api/ui/node` de todos os participantes antes de reconfigurar qualquer nó,
  evitando uma implantação parcialmente misturada.
- **Implantações são versionadas** (`Deployment`, `DEPLOYMENT_VERSION = 2`,
  `models/deployment.rs`): `versions` guarda cada `RuntimeParams` que já rodou
  sob o mesmo `deploy_id`, `active_version` aponta a que está no ar e `nodes` é
  a lista achatada da versão ativa. `POST /api/ui/deploy` (`deploy_endpoint`),
  antes de cunhar um id, procura no `pom.db` local uma implantação de mesma
  forma (`deployment::matching_shape`: `kind` + `DeploymentIdentity` provada +
  conjunto de `(node_id, faixa, tp_rank)`); achando, passa
  `RecordPlan::ReuseVersion` para `run`, que chama `upsert_version` (reusa a
  versão de `params` iguais, senão anexa `max+1`). Sem prova de identidade,
  `RecordPlan::New` com id novo. A resposta carrega `deploy_id` e `reused`. Toda
  leitura de registro (`PomDb`, `find_record`, `list_deployments`) roda
  `ensure_versions` para sintetizar a v1 de um registro v1 legado. `find_record`
  reúne a cópia local e as cópias dos peers participantes e escolhe, por
  `activated_at` e depois `active_version`, a versão corrente antes de
  `redeploy`/`activate` a usarem.
- `POST /api/ui/deployments/:id/redeploy` (`redeploy_endpoint` →
  `run_redeploy`, `deploy/`): mesmo `deploy_id`, mesmas máquinas e faixas.
  `RedeployRequest` carrega `params` (opcional), `version` (opcional) e
  `per_node`; informe um dos dois (400 quando faltam os dois ou `version` não
  existe). `RedeployRequest::resolve` roda antes do desvio TP: `version`
  reativa uma versão registrada sem anexar; `params` novos anexam - ou
  reativam - pela mesma comparação do `deploy_endpoint`; muta o registro e
  devolve os parâmetros base. A subida usa `no.config` da versão ativa (os
  parâmetros com os ajustes por máquina embutidos), então reimplantar uma
  versão devolve a cada máquina o que ela rodava. Fotografa o `NodeConfig`
  vivo de cada nó, **descarrega** todo peer que anuncia o `deploy_id`
  (`release_one` em paralelo), e **sobe uma máquina de cada vez** esperando
  cada uma reportar `ready` (`wait_for_peer_serving_with_timeout`). Funciona
  mesmo se um nó não estava servindo: `/api/ui/reconfigure` aceita um
  `ReconfigureSlice` opcional (`webui/`) e `reconfigure_local` reconstrói o
  `NodeConfig` dele. O arquivo é resolvido em três degraus: o `model_path`
  fotografado antes do unload; o `part_sha256` do registro
  (`transfer::find_part`); e, quando o registro não tem hash de parte (modelo
  inteiro, ou fatia de rodada antiga), o catálogo local casando rótulo do
  modelo + faixa de camadas (`resolve_deployment_slice_file`). Só quando os
  três falham é erro com o nome da faixa. `DeploymentNode.source_model_path`
  (`models/deployment.rs`, `#[serde(default)]`) grava o arquivo que cada nó
  carregou, para o redeploy achá-lo mesmo com o nó fora do ar. `finish_redeploy`
  grava por máquina o que foi aceito, sincroniza o snapshot da versão ativa
  (`commit_active_nodes`) e publica; `job.outputs` leva o resultado por
  máquina. Tensor Parallelism ainda usa o caminho antigo
  (`run_redeploy_in_place` → `reconfigure_one`, sem unload).
- `POST /api/ui/deploy/node/options` revarre o catálogo real do
  `target_node_id` e devolve somente as faixas fatiadas e o GGUF inteiro
  materializados nessa máquina, além dos defaults locais de GPU/KV. A tela não
  usa o plano antigo como fallback quando já conhece o grupo do modelo.
  `POST /api/ui/deploy/node` valida novamente essa
  opção no alvo, aplica `cache_type_k`/`cache_type_v` pela API local ou remota e
  lança somente a máquina escolhida. Quando há `deploy_id`, a operação anexa
  uma versão de topologia e muda `kind` para `mixed` quando uma cabeça whole e
  uma fatiada passam a coexistir. `POST /api/ui/deploy/node/remove` descarrega
  somente o alvo e anexa uma versão com as máquinas restantes; remoção parcial
  mantém o registro servindo, enquanto a última grava uma versão vazia e o
  deixa fora da seleção de deployments ativos. O snapshot vazio é encaminhado
  aos peers que já conheceram o deployment para retirar a máquina removida sem
  tocar a inferência dos demais.
- `gateway/generation.rs` - `log_early_outcome` garante que toda requisição
  aceita deixe **exatamente uma** linha de desfecho. As saídas antecipadas de
  `generate_stream_inner` (cancelamento antes do início, orçamento de prompt,
  nenhum pipeline válido, visão sem projetor) eram mudas: o registo mostrava o
  pedido a chegar e o ajuste do limite de saída, e depois nada - nem
  `Pipeline planejado`, nem erro. Para o cliente isso é um pedido pendurado, e
  foi o que pôs um agente em recuo exponencial a repetir o mesmo pedido em
  produção a 2026-09-08.
- `profile.rs` - `NodeProfile` is the durable product profile and the one source
  for network visibility and profile transitions. The web UI serializes it
  through `GET /api/ui/node`. Infer nodes can read a local-only network view
  and request node access; any active admin can authorize that signed request.
  A secondary admin keeps management access but cannot promote another admin.
- `admin_trust.rs` - the protected, durable Ed25519 authority chain. It stores
  the manually anchored root, one-level secondary grants, signed demotions,
  transfer offer and acceptance state, and full-chain reconciliation. Consumed
  nonces live in a local replay ledger beside the chain
  (`replay_path_for`: `<chain>.replay.json`), never in the shared document:
  control-envelope entries expire with the envelope's `CONTROL_WINDOW_SECS`
  (5 min) window, admin-mutation entries (grants, demotions, transfers, infer
  promotion) have no clock and are kept. `ControlEnvelope.issued_at` is signed
  and checked against that window on both verify paths. A profile set to `admin` without a valid local chain remains
  without administrative capability. `is_anchored`/`anchor_file_exists` are the
  anchored-versus-not judgement the UI shows; `parse_transported`,
  `anchor_fingerprint` and `fingerprint_matches` are the single manual check
  shared by `pom admin-anchor` and the screen (it normalises only human
  transcription - spaces, colons, case - never the value); `adopt_anchor`
  writes a transported chain onto a node that has none and refuses a node that
  already has one, because replacing a root is manual re-anchoring on the
  machine (`--replace`), not an interface gesture. A chain created by first
  access also carries a signed `setup` block with the network name, visibility,
  access policy, geographic label, `relay_hints` (dedicated relay `pk:` keys,
  never addresses) and federated-directory replicas, so a joining node does not
  retype network identity data. The block carries no IP: a joining node finds
  the principal by its key through pkarr (`main::spawn_pkarr_bootstrap`), and
  setup copies `relay_hints` into its `relay_peers`. `ensure_setup_metadata` repairs only a missing setup block
  from the local administrator configuration, and only when the local
  principal is the current primary.
- `deploy/` - `/api/ui/deploy/*` + `POST /api/ui/undeploy`, dividido em
  `mod.rs`, `routes.rs`, `types.rs`, `survey.rs`, `planning.rs`, `preview.rs`,
  `launch.rs`, `lifecycle.rs`, `node.rs` e fragmentos de testes: survey de
  capacidade, estratégias simples/completa, RTT direcional, cobertura, carga projetada,
  escolha de máquinas e sequenciamento das cópias; re-surveys o cluster no
  deploy e mantém `group` técnico separado de `deployment_name` operacional.
  A carga em voo do survey vem do `/api/ui/node` de cada máquina. A struct
  interna mantém zero apenas para que o planner ordene máquinas sem leitura,
  mas `load_known` impede que esse fallback chegue à `MachineView` ou à UI;
  nesse caso a carga publicada é `null`.
  Depois de montar os passos (e só com `blocked` vazio), `plan_endpoint` chama
  `enrich_capacity_previews`: uma requisição `capacity-preview` por `Step`
  materializado localmente (não `merged`, `target_path` presente), agregada
  por máquina em `MachineView.capacity` (soma por GPU). O fan-out usa no máximo
  quatro chamadas simultâneas; TP e passos ainda por copiar/fundir ficam sem
  projeção. É aviso, nunca bloqueio: a tela mostra a máquina estourada em
  rósea e pede um `Confirm` antes de implantar.

  Quando um modelo inteiro selecionado ainda não tem `total_layers` no
  `/api/ui/models`, o survey chama `model-metadata` somente para esse caminho;
  `metadata_complete` dos vizinhos continua podendo ser falso sem bloquear a
  implantação. O layout completo só é pedido quando há um corte de prefixo
  explícito.
  Antes de transferir ou lançar, prova a identidade por `pom_id` ou hash do
  GGUF, libera somente a implantação comprovadamente igual e aguarda cada peer
  ficar ocioso dentro de um prazo; recusações de identidade, falhas de unload e
  timeouts ficam no job com o peer e o motivo.
  Em `tensor_parallel`, prefere `shard_sets[].tp_parts`, monta `TpTarget` por
  rank e transfere pelo hash do arquivo original; o `apply` usa
  `skip_split: true` para não fatiar novamente. Sem `tp_parts`, mantém o caminho
  de runtime que extrai cada rank de um modelo inteiro.
  `route_options` forma um grafo das máquinas selecionadas cujas sub-faixas de
  execução são contíguas; ele preserva mais de uma entrada quando há caminhos
  alternativos e nunca inventa salto para uma camada posterior. Faixas
  carregadas sobrepostas são permitidas quando pertencem a caminhos distintos;
  a cobertura é validada pelo caminho executável, não pela união dos arquivos.
  `undeploy` tira uma pipeline inteira do ar.
- `webui/sidecars.rs` - `GET/POST /api/ui/model-sidecar-links`: the
  principal's model -> projector / MTP draft links (`model_sidecar_links` in
  `pom.db`). A secondary reads them from the principal and keeps the last copy
  (`stale`), and forwards a save there. `load_sidecar_links` is what the deploy
  preview and launch read.
- `deploy/sidecars.rs` - `attach_sidecars` fills `Plan.sidecars`: the
  compatible files of the network with every holder, the head's projector and
  the tail's MTP draft (screen choice, else the link), whether each is already
  on its target or copied from a holder, and the refusal when nobody has it. A
  compatible external draft turns `capabilities.mtp` supported for a model
  without `nextn`. `lifecycle::run` adds the copies to its `pulls` and sends
  `mmproj_file` / `mtp_draft_file` so the target resolves the path on its own
  disk (`jobs_apply`). `transfer::servable_path` serves a path outside the
  shard folder only when it is in the node's sidecar inventory, and a finished
  download invalidates the receiver's catalog.
- `webui/kv_cache.rs` - this machine's KV cache for the dashboard:
  `kv_cache_disk` measures the snapshot directory (`NodeInfo.kv_cache`) and
  `POST /api/ui/kv-cache/clear` (AdminOnly, forwarded by `node_id` like
  `/api/ui/leave`) deletes its `.bin`/`.json` files, resets
  `GatewayState.prefix_disk` and clears the inference server's host-memory
  cache. See `http-api.md`.
- `transfer.rs` (1542) - slice push/pull over HTTP `Range` with resume and MD5;
  offers carry only a `stat`, the digest is computed by the receiver. Pushes
  accept only a catalog basename in `filename`, resolve it server-side and
  require `.gguf` or no extension. Manifestos v1 são mesclados por `pom_id` no peer receptor,
  preservando `parts` e `tp_parts` locais. **O veredito volta para quem
  ofereceu** (`receive_answer`), e todo desfecho passa por
  `TransferState::finish`, que carimba `finished_at`. Um job carrega o rótulo
  da fatia (`SliceLabel`: modelo + faixa), lido do `.pom` - hash não é nome.
- `decisions.rs` - typed decisions, `POST /v1/systemone` and `/v1/decisions`
  (Jev API shape). Each question is one blocking generation with
  `max_tokens=1`, thinking off and a `prefill::DecisionProbe`: the last peer
  returns the probability of each single-token label (`A`.. for choice,
  `0`.. for score, `Yes`/`No` for noul) and this file turns them into typed
  answers and confidences. The prompt is ordered fixed system turn ->
  question head (instructions + answer format) -> compact state -> options;
  the text up to the state is the prefix-cache key and the head's retain
  boundary (`retain_prefix_bytes`), so repeated questions over new states
  reuse the question's KV. See `docs/subsystems/inference.md` 3.9.
- `openai.rs` - OpenAI-compatible `/v1/*` on top of the generation path;
  `max_tokens` da requisição é `min(pedido, n_ctx restante)`, com
  `DEFAULT_MAX_TOKENS=512` só como padrão de quem não pede. O
  `max_new_tokens` da implantação **não** entra nessa conta: ele é o teto de
  uma **perna** do laço de continuação (ver `gateway/` abaixo). A redução é
  registrada e devolvida em `x_pom.output_limits`
  (`requested`/`effective`/`deployment`/`context_available`/`leg`),
  e `x_pom.continuation_legs` diz quantas pernas a resposta costurou.
  Streaming e blocking usam a mesma ordem de finish-reason, e ela não tem
  exceção: orçamento **total** esgotado é `length`/`max_tokens` mesmo com
  chamada de ferramenta lida, porque atingir o teto é parada terminal; perna
  esgotada nunca é. Quando o corte cai **dentro** de uma chamada, ela volta
  estruturada e incompleta (`x_pom.truncated_tool_call: true`), que é o que os
  dois provedores devolvem - nunca markup cru, nunca descarte. A linha de
  auditoria desse resultado é `incomplete`,
  enquanto falha de transporte continua `partial`. `/v1/models` expõe
  `context_length` e o `top_provider.max_completion_tokens` compatível com
  OpenRouter, que é a capacidade da **requisição** derivada do `n_ctx`, não o
  tamanho de uma perna. O contexto disponível reduz o orçamento quando o
  prompt já ocupa parte da janela.
  `messages.rs` aceita `output_config.task_budget` (advisory, nunca corta) e
  injeta o marcador de contagem regressiva no prompt; ver `http-api.md`.
  **Os argumentos de uma chamada saem em fragmentos** (`CallArgsStreamer`),
  conforme o modelo gera: `eager_input_streaming` por ferramenta no
  `/v1/messages`, sempre no `/v1/chat/completions` porque a OpenAI não tem
  interruptor. A concatenação dos fragmentos é sempre a string que o caminho
  não-streaming produziria, e o prefixo transmitido só cresce - não existe
  desenviar um fragmento SSE.
  O Chat pode enviar `inference_head` com um `node_id` de layer 0; o gateway
  valida a candidata viva, fixa essa cabeça no planejador e a repete em
  `x_pom.source`/trace, sem reescolher por RTT ou carga.
  A capacidade publicada em `/v1/models` e usada por
  `request_output_limit_for_model` é a menor `RuntimeCapacity` entre os peers
  da pipeline escolhida para o modelo/implantação. Um peer ocioso nessa
  pipeline é acordado pelo evento `reload_model` em paralelo aos demais peers
  ociosos antes do primeiro prefill; a barreira espera `/health` por até 15
  minutos e falha explicitamente se o peer não voltar. Um peer vivo de rota
  alternativa não bloqueia a requisição. O runtime só
  anuncia esse valor depois do `/health` efetivo; apply, erro e unload retiram
  a capacidade durante restart.
- `messages.rs` - Anthropic Messages API (`/v1/messages`), same output-limit
  and finish-reason rules, with `x_pom.output_limits` and
  `x_pom.continuation_legs` on blocking responses and the final streaming
  `message_delta`.
- `continuation_lab.rs` (`#[cfg(test)]`) - bancada do laço de continuação: dois
  hops falsos, as rotas `/v1` de verdade num socket TCP e `reqwest` do outro
  lado, inclusive lendo o SSE byte a byte. Prova que a saída volta inteira, que
  a costura não aparece como `finish_reason` intermediário nem como segundo
  `[DONE]`, que a chamada de ferramenta chega com JSON completo, e que o teto
  absoluto para um modelo em laço. A prova de fragmentação não depende de uma
  fração fixa do relógio de parede, pois o scheduler e o agrupamento do socket
  não fazem parte do contrato do SSE.
- `tool_call.rs` (725) - tool-call text rendering + `TagScanner` output scan.
- `devupdate.rs` (647) - env-gated remote `git pull` + rebuild + self-restart.
- `admin.rs` - control plane do operador: nós, bloqueio, chaves e autoridade.
  O pedido `infer -> node` assina a solicitação no nó requerente, procura um
  admin vigente e envia `/api/internal/profile/authorize-node`; o admin assina
  o comando e o envia a `/api/internal/profile/accept-node`. As duas pernas
  percorrem `PeerInfo::gateway_candidates()` até obter resposta, tratando o
  primeiro endereço anunciado como cache que pode expirar. A ida usa um prazo
  maior que uma tentativa de retorno, porque o admin também precisa executar a
  segunda perna; retries automáticos da mesma mutação assinada são evitados para
  não transformar timeout em falso replay administrativo. O testkit de admin
  sobe dois listeners TCP locais e prova promoção e recusa ponta a ponta.
- `reqlog.rs` - adaptador de auditoria e fallback de migração. Com o plugin
  ativo, o host grava em `enterprise.db`; independentemente desse destino, o
  `AuditLog` do POM replica `begin` e `complete` aos admins efetivos por libp2p.
  O receptor ordena os eventos numa fila limitada e os entrega ao próprio host
  ativo; se ele não os aceitar, persiste no `pom.db` legado. Eventos recebidos
  nunca são retransmitidos. `AuditDb` cria o schema e o índice FTS5 somente no
  adaptador de teste, abre requests em `in_progress`, completa a mesma linha,
  marca registros antigos como `incomplete` e serve consultas autenticadas
  paginadas mais estatísticas no servidor. As consultas aceitam `node_id`,
  identidade de API key, rótulo de usuário, modelo, prompt e intervalo de
  entrada; o usuário é o rótulo humano da API key porque a auditoria não
  armazena uma conta separada. Falha de enfileiramento ou gravação registra
  sempre a operação e o request id. Pedidos e conversas ficam no `pom.db`; a
  auditoria Enterprise fica no `enterprise.db` isolado, enquanto o adaptador
  legado permanece como fallback. Permissões dos bancos e dos sidecars WAL/SHM
  são aplicadas no arranque. Filtros de usuário e modelo vazios são ignorados
  antes da consulta SQL, preservando a busca ampla quando o campo não foi
  preenchido. As rotas `/api/ui/requests` e `/api/ui/requests/stats` são
  Enterprise e falham fechado com `enterprise_required` sem entitlement. Com
  entitlement ativo, os handlers encaminham listagem e estatísticas ao processo
  `pom-plugin-host`.
- **Toda chamada a peer sai por `GatewayState.peer_http`** (`PeerClients`), que
  fixa o certificado do destino pelo node_id. Não existe mais cliente
  compartilhado sem pinning no estado: com TLS ligado ele não verifica ninguém,
  e um chamador que o pegasse por hábito falhava calado. Loopback e Ollama usam
  `peer_http.plain()`. Chamada de controle a outro nó usa
  `GatewayState::control_get`/`control_post_json`/`control_post_empty`: assina
  uma vez, tenta cada endereço anunciado - na ordem de `gateway_bases`, o host
  que já respondeu primeiro - e cai no túnel libp2p
  (`peer_client::send_to_gateway_over`), que é o caminho para um peer atrás de
  NAT simétrico. A signed command that changes state is sent to **one** delivered address: after a timeout on an established connection it is not repeated elsewhere, because the same envelope would be refused as a replay (409) while the peer still runs the first copy. Calls that stop a server (`/api/ui/unload`, `/api/ui/stop-inference`, `/api/ui/leave`) use `webui::PEER_STOP_TIMEOUT` (60 s), not `PEER_TIMEOUT`. `control_get_via_tunnel` é o último recurso de quem já
  percorreu os endereços com a memória de falha do `RouteTable` (a leitura do
  `/api/ui/cluster`). `admin.rs` sync with the principal (`sync_json`,
  `post_internal_json`, `validate_api_key_remote_parts`) uses the same
  `send_to_gateway_over`: direct addresses and, last, the tunnel; `sync_json`
  signs a new envelope on every attempt. Only weight transfer stays outside
  the tunnel.
- `serve.rs` - o laço de accept: espia o primeiro byte, guarda o `PeerAddr` no
  request (`request_origin`: um pedido do loopback com a marca do túnel libp2p
  vira `TUNNELED_ORIGIN`, não-loopback, e o cabeçalho da marca sai de todo
  pedido - ver `p2p-network.md`) e serve TLS ou texto claro na mesma porta TCP, com upgrades de
  conexão habilitados (o WebSocket do proxy de plugins,
  `crate::plugin_proxy`). O endpoint HTTP/3
  recebe configuração QUIC separada e sobe na porta UDP dedicada anunciada,
  mesmo quando o TCP está em HTTP puro; `TlsServer` cobre somente o certificado
  TCP, modo estrito e duração do último handshake. Ver
  `docs/subsystems/p2p-network.md`, seção 5.
- `models::OllamaClient::is_available` é apenas uma sonda opcional do
  `/health`: devolve o último valor e agenda em background uma checagem de
  500 ms no máximo a cada 15 s. Não herda os 300 s da geração, e um Ollama frio
  ou ausente nunca segura a resposta do gateway.
- `session.rs` - `POST /handshake` usa um desafio Ed25519 em duas etapas. Uma
  requisição sem `nonce` e `signature` é recusada com HTTP 403
  `challenge_required` e recebe um nonce hexadecimal aleatório de 32 bytes. O
  verificador guarda esse nonce por 30 segundos e o consome antes de conferir a
  assinatura enviada; somente a assinatura do nonce bruto pela chave cuja
  `SHA256` é o `node_id` recebe `approved: true`. Um nó que usa o corpo antigo
  vê a resposta 403 do desafio e nunca entra até falar o formato assinado novo.
  Reuso, expiração, divergência de chave e assinatura ausente são recusados.

## Key functions and routes

### Chat da rede

`GatewayState.network_chat` expõe o serviço libp2p do Chat da rede. `webui`
serve `GET/POST /api/ui/network-chat` para a interface, persistência e ações
com API key. `POST /api/ui/network/chat` grava no principal real; quando a
interface está num admin secundário, a rota encaminha a mutação autenticada
para esse principal. A rota de política entre nós é
`GET /api/cluster/network-chat`, e o probe só trata como principal a
`authority_pubkey` confirmada pelo `AdminTrust`, não o perfil anunciado.
Os GETs de política (`/api/cluster/tls`, `/api/cluster/logs` e
`/api/cluster/network-chat`) exigem `InternalNodeAuth`: `connection_key` mais
`x-pom-internal-node: 1`; não são superfície de token humano.
O backend não retransmite pela HTTP UI: a ação entra na fila limitada do
`NetworkChat` e o swarm valida e encaminha por `discovery.rs`. O snapshot não
repete bytes de arquivo em cada polling; `GET /api/ui/network-chat/file/:message_id`
faz o download autenticado do blob persistido.

- `gateway/` - `GatewayState` (network, ollama, default_model,
  local_assignment, loads, http, runtime, gateway_port, auth, transfers,
  reply_inbox); `FromRef<GatewayState> for AuthState`; `peers_visible_to_cluster`
  filters blocked nodes; `ready_for_pipeline` health-checks each peer's
  inference `/health` before planning, trying every inference address in
  `Network::inference_bases` order (the host that already answered first) with
  the `ADMISSION_PROBE_TIMEOUT` of 100ms each, then `/health` over the libp2p
  tunnel (`infer_tunnel::health_via_tunnel`, 3 s). The peer answers `/health`
  without waiting for its engine (a busy peer returns its last snapshot with
  `busy: true`), so a peer mid-prefill stays in the plan. Session lifecycle
  calls use `SESSION_LIFECYCLE_TIMEOUT` (60 s), because the peer's handler
  needs the engine lock. Probing only the first
  announced address left a peer behind an unreachable IPv6 out of every plan
  ("Gap at layer 0"). A direct success is remembered with
  `RouteTable::mark_success`. The probe follows the pinned route
  (`bases_by_route_plan`): a tunnel route goes straight to the tunnel, an
  HTTP/QUIC route puts the measured address first. Each successful body also
  feeds `AdmissionHealth::note_perf` (`fastest::PeerPerf`), which the
  delivery-time planner (`generation::plan_by_delivery_time`) and the
  dashboard telemetry (`catalog::model_peer_perf`) read. Whoever fails gets an `AdmissionHealth`
  quarantine - a single
  unreachable announcer used to cost every inference 2,005ms, the
  `peer_client::CONNECT_TIMEOUT`, because `join_all` waits for the slowest. A
  successful health response also refreshes the model label, deployment id and
  layer range from the inference process, so a P2P announcement still in flight
  cannot make a newly loaded pipeline look incomplete; `all_peers` appends
  the local node with `self_dial_host` (advertised > local IP > loopback) and
  advertises `relay/v2-direct` in the capability list alongside
  `inference/frame-v1-batch`, `transport/link-v1`, `relay/v1-chain` and
  `speculate/v1-lookup`;
  `speculate/v1-mtp-sliced` goes out unconditionally and
  `speculate/v1-mtp` only when `mtp_ready` (the node has the `nextn` block);
  `inference/frame-v1-batch` only when `network.batching_ready()` confirms
  that the loaded local inference server has the multi-item scheduler active;
  `self_reply_hosts()` lists the addresses by which a
  peer reaches this node (`self_dial_host` first, then `self_addr_candidates`);
  `prompt_budget_of(n_ctx, max_sessions, max_new_tokens)` - `n_ctx` is the
  complete window for each session; `max_sessions` multiplies the runtime KV
  reservation, and `n_batch` deliberately does NOT appear (prefill chunks the
  prompt); `prompt_budget_error_of(n_ctx, max_new_tokens, prompt)` is the pure
  refusal shared by both admission paths, and a gateway with no local model
  admits against the peer's announced `RuntimeCapacity` before any health probe
  - without it a 30k-char prompt reached the peer's decode and aborted the
  whole process on CUDA OOM (servidor CUDA, 2026-09-08), see `inference.md` §9; `generate_stream_with_stops`
  builds a `crate::direct::DirectReply` from `reply_inbox` +
  `self_reply_urls(self_reply_hosts(), gateway_port)` and hands `direct` to the
  prefill and decode executors, so the tail can answer this node directly
  (log says `resposta=direta|anel|hub`: direta when `direct` is Some and the
  plan supports direct reply, anel for `relay/v1-chain`, hub as the fallback);
  prefill in hub and direct paths runs as an ordered pipeline of configurable
  depth (`prefill_max_inflight_chunks`, effective 1..8, default 8), bounded also by a 512 MiB hidden-state byte
  budget; a chunk that fails after earlier chunks were accepted is rebuilt from
  chunk 0 because the lost response makes partial KV removal unsafe, while the
  ring stays serialized by its nested response contract; the request trace
  reports pipeline depth, prefix reuse and transport per traversal. It also
  carries distributed telemetry status, peer/span coverage and bounded
  `remote-stream` frame aggregates. The normal stream return uses the
  persistent `/v1/link` channel; a transport failure falls back to the HTTP
  callback and is counted with an explicit reason. A response may reach the client while
  transport receipts remain pending; this is represented as
  `response_complete` until the retained trace is complete, not as an
  invented `unaccounted` duration;
  MTP on a sliced plan (`mtp_enabled` + `plan.supports_mtp()` + >= 2 hops):
  the remote-stream selector asks the remote head for MTP: the tail drafts
  `nextn` after sampling and returns `next_draft` in the same reply, and the
  head installs it in its next step, so a round yields up to `mtp_k` tokens
  for one head-to-tail round trip; the legacy direct path keeps the
  gateway-side store and injection. The callback carries explicit
  `speculated`, `speculation_active` and `speculation_disabled_reason` fields. Both paths continue with `prefill::step` and a
  named reason when speculation is unavailable;
  the gateway still chooses the pipeline replica per session. Continuous
  batching is owned by the inference process at that replica, not by a global
  gateway queue, so changing replicas mid-session would lose the local KV;
  emits `GenEvent::Token`/`Done(GenSummary { source: "pipeline:hop_chain", .. })`
  /`Failed`; `GenSummary` also carries `GenerationTimings` aggregated over the
  full pipeline: TTFT, prompt-token prefill speed, output-token generation
  speed from the first token, and end-to-end total. The summary trace carries
  the same `trace_id` used by the audit, local gateway spans, separate
  optional outside-node total/accounted/unaccounted values only when an
  explicit residual is measured, cold/warm state, connection reuse counts and
  first-token boundaries. The historical
  `tokens_per_sec` remains the total-rate alias. `generate_blocking_with_stops`
  collapses it. Failed prefill events carry the trace observed by the prefill
  pipeline into the audit completion; timings stay null unless a successful
  summary measured them. The terminal event
  (`Done`/`Failed`) is held back until the end-of-turn bookkeeping
  (`rewind_sessions` or `release_sessions`) has finished, because a chat sends
  the next turn the instant the stream closes and used to arrive in the middle
  of it; token events still go out live. `release_sessions` tries every
  `inference_candidates` route, remembers which route worked, and remains best
  effort so a stale address cannot prevent cleanup. When every transport proves
  a prefill chunk was not delivered, the gateway resumes at that absolute
  position with the same live KV and prefix. It rebuilds the session only for
  an unknown session, ambiguous delivery, downstream state mismatch or missing
  absolute progress, and logs the reason. An unknown session also triggers one
  cold rebuild without prefix reuse, because a later chunk cannot be
  reconstructed from hidden state alone. When the stream receiver closes, the
  event pump observes
  `Sender::closed()` even though the generation keeps a sender clone for
  end-of-turn bookkeeping, and cancels the local HTTP future before the next
  retry, hop or chunk. The remote-stream callback propagates an asynchronous
  `410 Gone` ACK as `GatewayGone`, so an abandoned remote producer stops too;
  already accepted peer frames are cleaned up by the normal session release.
  Logs `prefix gateway` registram hit/miss e tamanho do prefixo; `prefix rewind`
  registra a confirmação ou recusa por peer e sessão, sem conteúdo do prompt.
  `start_gateway_with_ready` faz o bind do listener antes de sinalizar ao shell
  Windows que o gateway está pronto, e então registra
  `POST /api/generate` (+ alias `POST /generate`), `GET /peers`, `GET /health`,
  `POST /handshake` (session::handle_handshake), `POST /v1/relay-result`,
  `POST /v1/remote-stream-result` and `POST /v1/link`
  (`crate::direct::RELAY_RESULT_PATH`, 1 MB body limit, no client API-key auth -
  the per-frame `Ticket` is the credential; the callback handlers deliver into
  `reply_inbox`, while `/v1/link` keeps the response body streaming and checks
  the internal node credential for each frame); it serves with
  `axum::serve(...).tcp_nodelay(true)` because Nagle delayed the small SSE
  writes. `#[cfg(test)] mod testkit` is the shared scaffold for route tests - a
  whole `GatewayState` with the peer table seeded by hand; its network socket
  is loopback-only and nothing touches the LAN: `peer(node_id, hostname, gateway_port)` builds a loopback
  `PeerInfo`, `state_with(node_id, peers)` the state, and
  `state_with_secondary_admin(node_id, peers)` a state whose trust document
  names another node as primary and carries one signed secondary grant;
  `body_of(response)` extracts the JSON body (used by the transfer, deploy and
  webui tests).
- `gateway/` - **os dois orçamentos** (`OutputBudget`), que nunca são o mesmo
  número. `leg` é o `max_new_tokens` da implantação: o teto de **uma** geração
  de backend, escolha do operador, que a decisão do produto proíbe levantar.
  `total` é o teto da **requisição**: `min(pedido, n_ctx restante)`, recomputed
  after prefill with the real prompt (`OutputBudget::within_session_window`,
  against `n_ctx_per_session`); reaching the window also ends with `length`.
  Esgotar a
  perna não encerra a resposta -
  `note_leg_boundary` conta a perna e o laço segue decodificando na **mesma**
  sessão de KV, no mesmo fluxo SSE, sem reprefill e sem o cliente ver nada
  (`GenSummary::legs` é o que torna a costura inspecionável). O
  `max_new_tokens` nunca foi limite do backend - o `infer_server` não o conhece;
  sempre foi um corte do gateway, e cortar aí calado era o defeito. Só `total`
  produz `finish_reason: "length"`. `/api/generate` aceita `max_tokens`
  opcional (padrão 512), exige valor maior que zero e conta o primeiro token do
  prefill no orçamento, sem o overshoot
  histórico de 513 tokens; a resposta separa TTFT, velocidade do prefill,
  velocidade da geração e total em `timings`. `generation_finish_reason` testa o
  orçamento total antes do EOS em cada ramo de decodificação;
  `audit_status_for_summary` registra `length` como `incomplete` - exceto
  numa decisão tipada (trace com `jev`/`probe`), que sempre para no seu único
  token e por isso é `ok` -, e reserva `partial` para falha de transporte com
  texto salvo.
  **O total é inviolável.** Nada o ultrapassa, nem uma chamada de ferramenta
  aberta no token do corte - houve aqui uma folga que ultrapassava o teto, e
  ela foi removida porque nenhum dos dois
  provedores transborda. Quando o corte cai dentro de uma chamada, o adaptador
  a devolve estruturada e incompleta (`TagScanner::finish_truncated`), com os
  argumentos numa string JSON inacabada - possível porque o scanner segura o
  corpo da chamada até a tag fechar, então nada vazou. Ver
  `docs/subsystems/inference.md` (9.1) e a bancada `continuation_lab.rs`.
- `openai.rs` - the shared `DEFAULT_MAX_TOKENS=512` is used only when the
  client omits its output budget; there is no backend hard cap. The effective
  request budget is the minimum of the client request and the remaining `n_ctx`
  after the prompt - the deployment's `max_new_tokens` bounds one leg of the
  continuation loop, not the request;
  `render_prompt` flattens the conversation into one user message, labelling
  **every** turn including the first, so one turn's prompt is an exact prefix of
  the next one's - the property the prefix cache rests on; raw, unlabelled text
  is what `/v1/completions` and `/api/generate` are for.
  `conversation_cache_key` é o prompt renderizado canônico que a cabeça
  tokeniza e viaja como `GenerationInput::cache_key`; isso mantém a ordenação
  de system/developer e ferramentas idêntica à execução e evita declarar hit
  quando essas partes mudam no início do prompt. O gateway registra o motivo
  local do miss e o tamanho efetivo do chunk no trace. A versão
  Anthropic, `conversation_cache_key_with_budget`, prefixa a identidade do
  orçamento de tarefa para que uma entrada sem orçamento ou com marcador
  diferente não seja reutilizada; o marcador final é injetado a cada pedido,
  como na semântica do provedor.
  `usage.prompt_tokens_details.cached_tokens` reports the prompt tokens the
  pipeline did not reprocess, read from the trace - the executed number, not the
  cache's choice; `approx_tokens` = chars/4; `model_ids` comes only from live
  peer announcements (never the stale CLI default), and keeps an `idle` deployment
  selectable while its weights are unloaded; an unready non-idle peer without
  `runtime_capacity` still hides the model until `/health` confirms it; model objects include both
  OpenAI and Anthropic discovery fields; `resolve_model` only accepts a name a
  peer actually announces (`model_ids`) or an explicit `<modelo>@<deploy_id>`
  selector still pending gossip - anything else is refused with HTTP 404
  before generation starts, never silently swapped for the active model.
  A request that omits `model` still uses `active_model()`,
  which is the only remaining implicit default;
  `chat_completions`/`completions` -> `blocking_response` or `sse_stream`;
  `chat_message_json` (flat content + `tool_calls` array),
  `apply_scan_events_openai` (tool_call deltas), `chunk_json` (+
  `x_pom` extension with source/tps/duration/partial/trace/finish_reason/stop_sequence),
  including semantic pipeline/server/client TTFT and distributed telemetry
  completeness when the backend provides them,
  `cors_headers` (allow `*`). Routes: `GET /v1/models`, `GET /v1/models/:id`,
  `POST /v1/chat/completions` + OPTIONS, `POST /v1/completions` + OPTIONS.
- `messages.rs` - Anthropic `POST /v1/messages` (+ OPTIONS) and
  `POST /v1/messages/count_tokens`; its own `render_prompt` (system/messages/
  tools split, every turn labelled, same reason as `openai.rs`) and
  `conversation_cache_key`; `usage.cache_read_input_tokens` carries the honoured
  prefix reuse; `content_blocks` for tool_use/tool_result, `stop_reason`
  (end_turn / max_tokens / tool_use / stop_sequence), `blocking_response` and
  `stream_response` with the Anthropic SSE event names. `tool_choice` is
  parsed-and-ignored; `stop_sequences` is applied by the gateway matcher.
- `webui/` - `CATALOG_TTL=20s`, `CLUSTER_CACHE_TTL=2s` and
  `pub(crate) PEER_TIMEOUT=4s` (reused by
  deploy/). Registers the `/api/ui/*` routes: `node` (GET node_info),
  `model-identity` (POST, prova um arquivo já catalogado por caminho canônico
  e opcionalmente calcula seu SHA-256),
  `model-metadata` (POST, lê e publica somente o cabeçalho do modelo selecionado
  enquanto o restante do catálogo ainda está em background),
  `models` (GET, `hostname` = o nome anunciado pelo nó, **não** o do SO),
  `models/delete` (POST), `cluster` (GET), `chat/capabilities` (GET, modelo
  ativo, visão e cobertura do Chat), `cluster/models`
  (GET), `peer/:node_id/runtime` (GET), `paths` (GET+POST), `plan` (POST), `apply` (POST), `launch` (POST),
  `model-layer-options` (POST, cortes reais do GGUF) e `slice-ranges` (POST,
  materialização local de faixas escolhidas a partir de um modelo inteiro),
  `stop-inference` (POST), `slice` + `slice/plan` (POST), `jobs` (GET) +
  `jobs/:id` (GET), `unload` (POST), `release` (POST) + `release/candidate`
  (GET), `leave` (POST), `rejoin` (POST), `network/rtt` (GET autenticado),
  `network/rtt` (GET autenticado, relatório direcional deste nó),
  `chat/telemetry` (POST autenticado, deriva transporte e RTT das pernas
  direcionais de `pipeline_cycle` no trace da execução identificada, ou da
  última execução concluída no Painel; sem `execution_id`, também enumera todas
  as rotas completas do modelo ativo, soma Gateway -> cabeça -> ... -> cauda ->
  Gateway e ordena por custo agregado e `node_id`; uma perna observada no trace
  recente usa primeiro seu RTT de fio (`net_rtt_median_ms`), enquanto as
  sondas direcionais frescas do gateway e dos peers ficam como fallback; também
  devolve `pipeline_cost` quando a decomposição observada de rede e compute
  existe; traces antigos recuam para o RTT de execução),
  `requests` (GET ->
  `crate::reqlog::list_requests`; `chat/history` (GET/POST/DELETE autenticado,
  transcriptos normalizados em `pom.db`); `requests/stats` (GET ->
  `crate::reqlog::stats_requests`). `leave_network` now takes an optional
  `LeaveRequest { node_id }`: absent (or this node's own id) keeps the old
  behavior - this node detaches; a peer's id is **forwarded** to that peer's
  same route (`forward_leave`, same pattern as `dev/update` - the panel only
  talks to its own node, and only the owner of a process can detach it). The
  local leave stops the runtime and detaches the network without consulting
  model config, health or readiness, so a node can use it after repeated load
  failures. Also
  the job store (`Job`, `JOB_RETENTION=600s`,
  `jobs()`/`new_job`/`set_job`/`set_job_progress`),
  catalog caching (`CachedCatalog`, `warm_catalog` called at startup,
  stale-while-revalidate). A primeira resposta do catálogo publica o inventário
  rápido de arquivos e `metadata_complete: false`; os cabeçalhos GGUF são lidos
  em uma tarefa `spawn_blocking` e publicados quando terminam, mesmo que o
  refresh tenha trocado o `Arc` do inventário: a reconciliação usa caminho,
  tamanho e mtime do arquivo. O cache instala um placeholder antes da primeira
  varredura, ordena as raízes e mantém single-flight para scans e enriquecimentos;
  as rotas de seleção de modelo distinguem esse placeholder de um arquivo
  ausente: quando `metadata_complete` é falso, devolvem que a descoberta ainda
  está em andamento e não a mensagem de que o arquivo não pertence ao peer;
  a varredura rápida não calcula SHA-256 de modelos inteiros para montar
  `shard_sets`, então um volume grande publica seus caminhos e raízes antes da
  leitura dos headers GGUF usados para sugerir projetores. A escolha do
  projetor não calcula SHA-256 do payload; a validação estrutural do caminho
  escolhido fica no apply do peer que vai carregá-lo.
  F5 ou seleção repetida não empilha trabalho. Uma configuração nova fica em
  fila para o scan atual terminar. O enriquecimento registra início e conclusão
  (inclusive descarte), e uma falha terminal marca `metadata_error` no item e
  não impede os demais, então a passagem termina com `metadata_complete: true`.
  Um arquivo que o enriquecimento descarta (sidecar que só o header revela,
  como um draft MTP sem o prefixo `mtp-` ou sem extensão) sai também do
  inventário atual (`drop_discarded`): pendente para sempre, ele reiniciava o
  enriquecimento em laço, centenas de vezes por segundo.
  O resultado publicado é reutilizado nas requisições da interface; modelo cujo
  caminho, tamanho e mtime não mudaram não é relido. Nova leitura só ocorre em
  refresh por TTL ou invalidação. O survey de implantação só recusa uma
  seleção que ainda não tem suas faixas resolvidas; em TP ele verifica cada
  rank selecionado. Enriquecimento de outro modelo, peer silencioso ou campo
  ausente em peer antigo não bloqueia. `merge_parts`
  (arquivo temporário com sufixo único, `read_layout` validado antes do rename
  e de `finalize_run`; reuses an already-complete output, preserves the source
  `pom_id`, and invalidates the catalog),
  `run_apply`/`run_slice`/`release_slice`, and the setup handlers for hardware,
  models, trust-anchor, health and save, which are deliberately NOT registered
  here - the setup mode router in `main.rs` serves them only while there is no
  `pom.json`.
  Os testes que chamam `apply_cluster_policy` também seguram
  `peer_client::POLITICA_GLOBAL`, porque o esquema TLS global é compartilhado
  por toda a suíte.
- `deploy/` - `DeployKind` (`shards` | `whole` | `tensor_parallel`) escolhe
  qual inventário o `survey` lê: `shard_sets`, ou os modelos inteiros via
  `parse_whole`, que lê um GGUF não fatiado como a faixa `0..total-1`. O
  `tensor_parallel` (caminho C) nomeia o grupo (primary + workers); a v1 da
  varredura lê o modelo do primary, e a descoberta de workers fica junto do
  fork do llama.cpp. O pedido carrega `TpLaunch {size, primary_node_id,
  worker_node_ids}` - o operador escolhe primary/workers por node_id e o nó
  deriva a `rank0_url` e seus candidatos da `Machine.inference_url` e
  `Machine.inference_candidates` (`tp_identity`). Validação em
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
  `docs/subsystems/deployment.md` §1.1 e §1.2.
  O `survey` preserva o `PeerId`, os candidatos de endereço UDP e a capability
  do túnel de cada máquina; os candidatos vêm do `/api/ui/node` do próprio
  peer, e não somente do anúncio que o coordenador recebeu. `tp_members`
  transfere esses dados e todos os candidatos de URL para cada rank, permitindo
  que `broadcast_to` e o reduce reidratem o swarm no destino e tentem cada
  endereço HTTPS alcançável quando o túnel não está disponível.
  O `register_tp_reduce` também recebe o tamanho do grupo. O nó publica esse
  tamanho e o snapshot de coletivas no campo `node_timings.tp`, sem misturar
  bytes ou esperas TP com a telemetria de pipeline.
  O mesmo `survey` agrega os relatórios autenticados de `/api/ui/network/rtt`:
  cada origem publica somente suas medições diretas, e `route_options` usa a
  chave `(from_node_id, to_node_id)`. A latência do gateway não é reutilizada
  para uma aresta entre dois peers remotos.
- `deploy/` - `launch_targets` é a fase 2 extraída de `run`: os passos do
  plano resolvidos contra a varredura pós-cópia, que é a única parte do deploy
  testável sem cluster (`a_whole_model_is_launched_as_one_plain_target`).
- `deploy/` - `RuntimeParams.fit_memory` fica ligado por padrão em shards e
  modelos inteiros para deixar o `llama.cpp` ajustar camadas à memória livre;
  `effective_params_for_kind` e o `apply` desligam o ajuste no tensor
  parallelism, que não suporta o fit automático em `split-mode tensor`.
  `RuntimeParams.inference_mode` acompanha o pedido nos três tipos de deploy
  independentemente de `POM_DESENV`; `effective_params_for_kind` só aplica a
  regra estrutural de desligar `fit_memory` no Tensor Parallelism. O modo
  ausente é `dense` (`llama.cpp`); `auto` só entra quando foi pedido
  explicitamente.
  O survey leva `is_moe` do catálogo GGUF ao `DeployPlan`; somente metadata de
  experts identifica MoE. O plano expõe `inference_mode_supported` e
  `inference_mode_reason`: MoE em Tensor Parallelism é recusado explicitamente
  enquanto esse caminho não for suportado.
  O mesmo `DeployPlan` expõe `capabilities` em estado `supported`,
  `unsupported` ou `unknown`, com origem, motivo e aviso opcional: thinking vem
  do template e vocabulário GGUF, MTP primeiro do arquivo GGUF selecionado e
  depois apenas adverte sobre o runtime do peer final, lookup vem das
  capacidades runtime de todos os alvos, e fit-memory da estratégia. Ausência
  comprovada no modelo oculta; runtime limitado ou evidência indeterminada
  permanece acionável. O `no_embeddings` é avaliado por posição real na
  cadeia; quando solicitado, `launch` valida todos os peers antes de parar
  qualquer processo, recusando intermediários comprovadamente inseguros e
  permitindo topologia indeterminada somente com o controle avisado.
- `deploy/` - `overlapping_layers`, `split_position`. Routes: `POST
  /api/ui/deploy/plan` (read-only, cheap), `POST /api/ui/deploy` (re-surveys
  the cluster, refuses on `plan.blocked` or empty steps), `POST
  /api/ui/deploy/node/options`, `POST /api/ui/deploy/node`, `POST
  /api/ui/deploy/node/remove`, `POST
  /api/ui/undeploy` (take a whole pipeline out of the air). The planner is a
  suggestion; the operator's `selection` in the request is authoritative.
  Rules from the architecture: pipeline order = layer order (layer 0 takes
  every prompt); one node may serve multiple distinct ranges, and `merge` only
  joins adjacent ranges via `splitter::merge`; the exact same range on different
  nodes is a replica and a valid parallel route; the same whole model may
  likewise be selected on multiple nodes for balancing; distinct overlapping
  cuts and global gaps are refused.
  **`steps` always comes back in layer order**, blocked plans included:
  `position` only means "place in the pipeline" in that order, `run` reads the
  launch order straight off the list, and the screen lines the plan up against
  rows it keeps sorted by layer. The grouping loop walks in layer order and
  only joins consecutive entries when `merge` is enabled, so separated ranges
  on one machine remain independent steps. For the same reason an **empty
  `target_node_id` never groups**: "no machine" is not a machine, and each
  unplaced range keeps its own step and problem text.
  `undeploy_endpoint` re-surveys the cluster (never trusts the screen),
  resolves `UndeployRequest.group` via `resolve_group` (empty picks the only
  pipeline in the air; two pipelines is a 400 - never guess), then calls
  `release_one` per serving machine in parallel and returns 200 with an
  `UndeployOutcome` line per machine, always: a silent peer is
  `released: false` with the reason in `error`, never counted as success.
  Nothing touches disk - slices stay, only memory returns, through the peers'
  own `/api/ui/unload` (same route as the single-machine button; the local
  machine uses `runtime.stop()`). `deployment_authority(profile, refusal)`
  checks `ProfileAction::ManageDeployment`: only the `admin` profile may add a
  machine through `/api/ui/deploy/node` or undeploy the whole pipeline. The
  `/api/ui/deploy/node/remove` route has no such gate because it is the
  per-machine self-removal operation and remains available to every profile.
- `transfer.rs` - `CHUNK=1MB`, `DISK_HEADROOM_MB=512`, `GGUF_MAGIC=b"GGUF"`;
  `/api/transfer/send` receives `{to_node_id, filename}`, never a path.
  Routes: `GET /api/transfer/manifest`, `GET /api/transfer/download`,
  `POST /api/transfer/pull`, `POST /api/transfer/offer`, `GET
  /api/transfer/offers`, `POST /api/transfer/offers/:id/accept` +
  `/:id/reject`, `POST /api/transfer/offers/answer`, `GET
  /api/transfer/jobs`, `POST /api/transfer/jobs/:id/cancel`, `POST
  /api/transfer/send`. `GET /api/transfer/offers` returns only
  `{ offers }`, without disk telemetry, and uses ETag plus 304 when
  unchanged. Path safety is
  directional: `safe_model_path` confines the *pull* direction to `models/`
  (`servable_path` canonicalizes and checks `starts_with`), while
  `local_send_path` lets the *operator's own* screen offer any readable GGUF
  via an opaque `grant` id (`TransferState::grants`) so a path never travels.
  `ensure_gguf` (magic + GGUF file name, conjunction: `.gguf` or no extension,
  never an Ollama `sha256-` blob) gates every exit. **An offer
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
  folder for a **part** (`<shard_dir>/<modelo>-<hash curto do original>/<sha256>.gguf`,
  `<modelo>-<hash curto do original>.pom` written first - pipeline and tensor
  slicing of one model share the folder and manifest), `<shard_dir>/recebidos/`
  for anything that belongs to no manifest -
  resumes with `Range`, verifies the sha256 it asked for (a part) or the MD5
  (anything else) before the rename, and cancels symmetrically on both
  sides (`cancel`/`notify_peer_cancel`, idempotent, works for jobs not yet
  known). A manifest/download error becomes a job `failed` with the reason,
  never overwriting an operator's `cancel`.
  **Os dois lados sabem o desfecho.** Todo estado terminal passa por
  `TransferState::finish`, que grava `finished_at` e recusa reescrever um
  terminal já posto - é ele que faz "cancelado por X" sobreviver a um "falhou"
  anônimo, e é o carimbo que deixa o sino mostrar o que **acabou de** terminar.
  Do lado que envia, `served_stream` fecha o job no **último byte** e não no
  fim do stream: o hyper para de puxar quando o corpo satisfaz o
  `Content-Length` e nunca pede o `None` do fim, então o job de envio ficava
  `running` para sempre com `bytes_done` congelado. E o veredito do receptor
  volta por `notify_offerer` → `POST /api/transfer/offers/answer`
  (`receive_answer`), casado pelo `grant` guardado em `TransferState::offered`:
  aceite vira job `sending` já em curso ("aceito; aguardando o início do
  envio"), recusa vira `cancelled` com o nome de quem recusou. Um veredito de
  um `node_id` que não é o destino registrado é 403.
  **Um job diz de que modelo é.** `TransferJob` carrega `model_label`,
  `start_layer` e `end_layer`, montados por `slice_label`/`slice_label_for_path`
  a partir do `.pom` que está ao lado do arquivo - nunca deduzidos do nome, que
  é o sha256 e não afirma nada sobre camada nenhuma. O manifesto já está no
  lugar quando o job nasce nos dois sentidos: veio na oferta (push) ou foi
  buscado em `/api/transfer/pom` (pull), sempre antes do primeiro byte de peso.
  `DEV_AUTO_ACCEPT_ENV` (`POM_DEV_AUTO_ACCEPT_TRANSFERS`, read once at
  startup via `dev_auto_accept`, the receiver's own variable) is OFF unless the
  receiver sets it; a sender cannot ask for it.
- `admin.rs` - `admin_routes()`: `GET /admin/nodes`, `POST
  /admin/nodes/:id/block`, `POST /admin/nodes/:id/unblock`, `DELETE
  /admin/nodes/:id`, `POST /admin/regenerate-key`, `GET+POST /admin/api-keys`,
  `DELETE /admin/api-keys/:key`, `GET /admin/transfer-offers` (same offers as
  `transfer.rs::list_offers`), `GET /admin/slots` (local `/health` view of KV
  snapshots plus `prefix_auto_cache` index stats when Phase 3 is on),
  `POST /admin/slots/:name?action=save|restore|erase` (orchestrates
  a deployment-wide KV snapshot via `gateway::{snapshot,restore,erase}` — see
  `docs/subsystems/inference.md` §3.1.1 and `http-api.md`).
  `list_nodes` appends blocked-but-disconnected nodes so the panel is not empty,
  and gives each remote peer a `route` (`Network::peer_route`): the path this
  node actually uses to reach it - direct HTTP, direct libp2p (maybe hole
  punched) or relayed - as opposed to `addr`, which is only what it advertises.
  Setup mode exposes `/api/setup/hardware`, `/api/setup/models`,
  `/api/setup/geo-reference`,
  `/api/setup/admin-anchor/preview`, `/api/setup/trust-anchor`,
  `/api/setup/health` and `/api/setup/save` on its loopback-only router. Saving
  installs the trust document, persists network details in both `pom.json` and
  `pom.db`, and respawns. `setup_pending` keeps the router active after leaving
  a network while preserving local hardware preferences. The setup hardware
  response returns the effective P2P port used to render the optional preferred
  address (never pre-filled nor required); the anchor preview signs the network
  setup metadata into the trust document.
  Public-network setup accepts an empty federated-directory list; configured
  environment or administration entries are still copied into that metadata.
  The setup health response also reports the existing Windows firewall-rule
  check; on Mac/Linux it reports whether the POM ports are locally available
  for binding while no unified firewall API is available.
  The preview returns a private connection key only for local/private
  visibility; the administrator chooses a password only for network exposure,
  and `pom.json` persists only its Argon2id hash. Localhost-only exposure has
  no administrative password.
  `GET /api/public/network(s)` exposes public network metadata, including the
  signed trust document, Markdown description/rules/channels, the publisher
  `pk:` key and `relay_hints` for the no-secret join flow - never an address.
  `GET /api/public/networks/locate?key=pk:...` resolves a listed publisher key
  through pkarr on the node and answers only `direct|relayed|unresolved` plus a
  relay count (30 s cache; keys the node does not list get 404). `GET/POST
  /api/public/directory/records` is the federated directory
  ingress: records are checked locally for Ed25519 signature, proof of work,
  trust-root consistency and expiry before entering the bounded SQLite cache.
  A node publishes to every configured ingress and queries all of them in
  parallel. Private nodes use this outbound HTTP path but skip private-swarm
  UDP/libp2p/pkarr candidates, so a public peer cannot learn or dial private
  peers through the listing feature. Direct pkarr discovery remains available
  only while the active network is public; it still cannot enumerate arbitrary
  pkarr keys or make an unroutable IPv6 address reachable.
  `gateway::PkarrPublishHandle` is the live pkarr publisher: it rebuilds the
  record from `Network::libp2p_addrs` every 15 s (`record_addrs` turns circuits
  into relay keys and drops private addresses) and republishes on change or
  every 30 min, with or without a deployment. Failed publishes back off from
  15 s to 30 min (`PublishBackoff`) and only the 1st, 2nd, 4th, 8th...
  consecutive failure logs at `warn`. With `--no-pkarr` there is no publisher
  (`pkarr_publisher` is `None`).
  The trust root is reached by `GET/POST /api/ui/network/trust-anchor`
  (`webui/trust.rs`), outside the admin-only scope on purpose.
  The authority contract adds `GET /api/ui/network/admin-authority`,
  `POST /admin/authority/resign`, `POST
  /admin/authority/secondaries/:id/demote`, and the profile transition routes
  `/api/ui/network/request-node` and `/api/ui/network/relinquish-node`.
  Internal profile mutations carry signed authority commands; a promotion
  delivers and validates the complete trust document at the target.
- `webui/network.rs` - `GET /api/ui/network/info` entrega o bloco `network`
  salvo no banco para todos os perfis, com `can_edit` derivado da cadeia de
  autoridade. `POST /api/ui/network/settings` é exclusivo do administrador,
  grava `pom.json` e SQLite, atualiza o rótulo geo do pkarr e envia a nova
  configuração em um envelope assinado aos peers. O envelope leva a
  `connection_key` somente como segredo de transporte de redes privadas; em uma
  rede pública o valor local do admin não entra no envelope. A mutação remota
  exige a assinatura da autoridade. `GET
  /api/ui/network/trust-share` entrega ao administrador a raiz de confiança
  para cópia ou download e, quando aplicável, a chave de conexão. `GET
  /api/ui/network/geo-reference` entrega países e cidades da referência local
  para os selects da tela, sem expor uma mutação de banco.
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
- `auth.rs` - `ConnectionAuth` accepts the private connection key or a signed
  control envelope; `InternalNodeAuth` verifies envelope metadata and resolves
  its issuer against the local authority or a live peer. The gateway middleware
  hashes the actual body before a mutating handler runs. `ApiKeyAuth` accepts
  `authorization` or `x-api-key`, but only for an active API key, and
  `api_key_identity` supplies the stable non-secret id used by the audit trail.
  API keys are stored and exchanged only as `key_hash` (SHA-256 of the secret,
  `api_key_hash`) plus `key_hint` (`api_key_hint`, start and end only); the
  secret (`generate_api_key_secret`, `sk-pom-` + 32 random bytes) leaves the
  node once, in the `POST /admin/api-keys` response (`IssuedApiKey`). The
  tracking id (`ApiKey.id`, UUID v4) is what logs record. `db.rs`
  (`migrate_api_key_secrets`) converts pre-hash databases in place, keeping
  every id, and rewrites the file so no plain-text secret survives. `pom.json`
  never carries API keys.
  `internal_node_token` remains the private-network credential for inference
  transport; client API keys never cross that boundary.
  `AuthRejection`: MissingHeader=401, InvalidKey=403.
- `reqlog.rs` - `AuditLog::begin` records the exact prompt, request body,
  resolved model, entry node, API-key identity and arrival time before
  generation when the Enterprise host is unavailable; the active Enterprise
  path sends the same neutral events to `pom-plugin-host`, which persists them
  in `enterprise.db`. `spawn_logged_generation_with_stops` later calls
  `complete` on that id with result, telemetry, timings, token counts, hop
  chain and error.
  The request body contains no headers or secrets. `list_requests` requires
  `ConnectionAuth`, supports node, API-key identity, prompt FTS, time-range,
  limit and offset filters, and returns newest first with incomplete/error
  tallies. `stats_requests` uses the same auth and filters, scans the filtered
  rows on the server and returns token, node, day, model, peer, cache, terminal,
  inference and TTFT aggregates for the Logs statistics tab.
- `tool_call.rs` - `render_tools_block` (empty when no tools),
  `render_tool_call` (`<tool_call><function=NAME><parameter=P>...</parameter>`,
  string args raw, others re-serialized). `TagScanner` incrementally splits a
  token stream into text / tool-call spans tolerating tags split across chunk
  boundaries (`CALL_FORMATS` + `longest_prefix_overlap`); the registry is
  extended only with markers verified in the active GGUF's
  `tokenizer.chat_template`. `CallKind::Wrapped` (the real `<tool_call>`
  convention) vs `CallKind::BareFunction` (a tolerated wrapper-less
  `<function>NAME</function>` fallback, name-only, whitespace guard).
  `parse_call_body` accepts the nested `<function=...>` XML form or the legacy
  Hermes JSON-object form, including an extra JSON-encoded `arguments` string;
  malformed spans and incomplete calls degrade to literal text. `parse_output`
  is the scanner over the whole text -> `Vec<OutputPart>` (`Text` |
  `ToolCall { name, arguments }`), with adjacent text merged. The same scanner
  feeds the blocking OpenAI message and its streaming tool-call deltas.
- `webapp.rs` - `API_PREFIXES = ["api/", "v1/", "admin/", "peers", "health",
  "generate"]`; `routes<S>()` serves `GET /` (index) and `GET /*path`. A miss
  under an API prefix is a JSON 404, a miss with a file extension is "asset
  não encontrado", anything else falls back to `index.html`. `resolve`
  rejects `..`, absolute and prefixed components. Cache: index.html no-cache,
  hashed assets immutable (1 year). Missing bundle -> 500 explaining the
  `npm run build` / `POM_UI_DIST` remedy.
- `session.rs` - `handle_handshake` compara `connection_key` (403
  `invalid_key`), amarra `node_id` à `pubkey`, consulta `blocked_nodes`, exige o
  desafio Ed25519 de uso único e aprova com `peer_id`; as recusas do desafio
  estão descritas na entrada do arquivo acima.

## Links to other folders

- This folder calls: `crate::network` (`Network`, `PeerInfo` - discovery,
  `alive_peers`, `advertised_host`, `self_addr_candidates`), `crate::pipeline`
  (`plan_pipeline`, `LoadTracker`, `PipelinePlan` - the distributed path),
  `crate::ollama` (`OllamaClient` - model resolution and `/health` only, no
  generation fallback), `crate::runtime`
  (`NodeRuntime`, `NodeConfig` - model/slice lifecycle), `crate::config`
  (`Config`, `ApiKey`, `ui_dist_dir()`, `shard_dir_path()`,
  `model_dir_paths()`, `workspace_root_path()`),
  `crate::models`/`crate::catalog`/`crate::gguf`/`crate::plan`
  (`Catalog`, `GgufLayout`, `PeerBudget`, `pom::PomManifest`), `crate::splitter`
  (`MergePart`, `merge`), `crate::sysmon`, `crate::route` (`RouteTable` for the
  cluster view), `crate::addrs` (`outbound_local_ip`), `crate::prefill`
  (`RequestTrace` in the summaries), `crate::direct` (`ReplyInbox`,
  `DirectReply`, `Ticket`, `self_reply_urls`, `handle_relay_result`,
  `RELAY_RESULT_PATH` - the direct-reply relay that lets the tail answer the
  originator instead of bubbling through every hop),
  `REMOTE_STREAM_RESULT_PATH` - the bounded callback inbox used when a remote
  head owns the decode loop. A remote-capable head is selected by
  `generate_stream_inner` only when the gateway can receive callbacks. A
  sliced MTP plan sends the initial MTP request to that head; every hop rolls
  back its own rejected KV suffix before bubbling the existing reply, so no
  rollback request or probe frame is added per token. The local path and peers
  without the capability keep the established route. The stream degrades with
  `peer_without_sliced_mtp_wire` when a peer lacks the MTP wire capability.
  `routing_contract` capability types.
- Who calls it: `crate::main` (`start_gateway_with_ready` spawns the router, `warm_catalog`
  runs at startup, setup mode mounts only the dedicated `/api/setup/*` handlers);
  the frontend bundle in `apps/frontend/dist` hits `/api/ui/*`; harnesses hit
  `/v1/*` (OpenAI/Anthropic) and `POST /api/generate`; peers call each other's
  gateway HTTP for transfers, deploy forwarding, and cancel notices.

### Visão na implantação e no chat

`deploy::Plan.mmproj_path`, `mmproj_candidates` e `mmproj_reason` vêm do modelo
inteiro ou do `ShardSet` escolhido. A interface pré-seleciona o primeiro
candidato compatível e pode enviar `DeployRequest.mmproj_path` para escolher
outro formato na cabeça selecionada. A seleção da cabeça usa a faixa e o nome
do candidato escolhido, não o primeiro arquivo de visão da máquina.
`deploy::launch` only passes `mmproj_path` to the plan with `vision: true`, and
`planning::resolve_head_projector` resolves it against the head by path or, if
the screen sent another machine's copy, by the unique file name among the
head's candidates; with no match, the plan refuses naming the file.
`DeployRequest.vision` só habilita o projetor no alvo cujo `start_layer` é zero, a cabeça da implantação;
`webui::ApplyRequest.use_mmproj` persiste a escolha em `NodeConfig` e o
`openai` extrai imagens base64 dos pedidos multimodais. A recusa de visão devolve
`mmproj_reason`; o caminho também é validado contra o GGUF antes de o runtime
iniciar, inclusive em configurações antigas.
O Chat consulta `GET /api/ui/chat/capabilities`, uma rota comum que agrega o
modelo ativo, a cobertura, o `mmproj_path` e o `no_thinking` da cabeça da
implantação sem expor topologia ao perfil `infer`. A resposta usa `supported`,
`unsupported` ou `unavailable` para visão, thinking e cobertura completa,
incompleta ou indisponível; recusa de autorização nunca é convertida em vazio.
Visão e a disponibilidade de thinking são propriedades do modelo servido, não
privilégios do perfil. `openai::ChatRequest.thinking_enabled` segue no
`GenerationInput` e no metadata de prefill; a cabeça aplica o kill-switch
global antes de renderizar o template.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
