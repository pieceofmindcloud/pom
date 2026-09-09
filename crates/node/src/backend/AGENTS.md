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
the `connection_key` remains a separate node-to-node credential. The client
inference routes (`/api/generate`, `/v1/models`, `/v1/chat/completions`,
`/v1/completions` and `/v1/messages`) accept only active inference API keys.
The gateway callbacks `/v1/relay-result` and `/v1/remote-stream-result` use
the per-frame `Ticket` in the body and no client API key. The separate
inference listener accepts only `connection_key` plus the internal marker and
remains dual-stack. `/health`, `/peers` and `/handshake` remain dual-stack.

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

O gateway registra atividade por peer em `reqlog::RequestActivity` e o gateway
de geração chama `reload_model` quando encontra peers ociosos no plano. A rota
interna autenticada carrega o runtime remoto sem colocar uma conexão síncrona
extra no hop normal; o descarregamento é avaliado pelo monitor já existente.

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
  with the real peer address, a non-empty constant-time compared token, a
  server-side in-memory session cookie, or the explicit internal-node channel.
  Sessions use a cryptographically random identifier, bind to the current
  token digest, expire after the single `ADMIN_SESSION_TTL` value and never
  persist in the browser or on disk. `ConnectionAuth` (connection-key bearer
  only), `InternalNodeAuth` (that key plus an internal marker), `ApiKeyAuth`
  (active inference API key only, from `authorization: Bearer` or `x-api-key`),
  `AuthState = Arc<RwLock<Config>>`.
  When both headers are present they are checked independently, so an invalid
  one cannot hide a valid one; a valid API key still supplies its non-secret
  `Caller`. Uma chave desconhecida é consultada no principal pelo canal interno
  autenticado e falha fechada em timeout ou erro. A porta de inferência não
  usa esse fluxo: ela exige a `connection_key` compartilhada e o marcador
  interno, enquanto os callbacks de resultado validam o `Ticket` por quadro.
- `gateway/` - módulo dividido em estado/seleção, budgets e tipos de geração,
  geração, snapshots/planejamento, HTTP e testes: `GatewayState`, `start_gateway`, `generate_stream_with_stops` /
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
  and the `testkit` test scaffold (peer/state_with/body_of).
- `db.rs` - único armazenamento SQLite local: schema criado no arranque,
  WAL/timeout, implantações, API keys e revogações, preferências térmicas,
  segredo do Hugging Face, histórico durável do catálogo e histórico de
  conversas e linhas do log de processo por `deploy_id`. Os logs preservam a
  ordem de chegada no principal, guardam o timestamp de origem só como dado
  informativo e acumulam descartes da fila limitada. O histórico identifica
  modelos inteiros pelo caminho local, sem
  hashear os pesos durante o registro do catálogo; hashes de conteúdo ficam
  nas fronteiras explícitas de implantação, fatiamento e transferência.
  `PomDb` mantém uma conexão configurada no arranque, reutilizada
  por todas as operações; um mutex explícito serializa escritas e a mesma
  conexão também faz leituras curtas em ordem, eliminando abertura e DDL por
  operação. O caminho vem de `config::database_path()`; `POM_DB_DIR` aponta
  para um volume e, sem ele, o banco fica na raiz do projeto.
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
  storage paths, thermal policy (`GET/POST /api/ui/thermal`), cluster process
  logs (`GET /api/ui/cluster-logs`, `POST /api/ui/network/logs`), slice retention
  e CRUD autenticado do histórico de conversas (`/api/ui/chat/history`). A
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
  `GET /api/ui/infer/heads?model=...` é a única leitura dedicada ao perfil
  infer: entrega apenas as candidatas de layer zero e a escolha do planejador,
  sem peers gerais nem ações de controle. A carga de cada candidata vem do
  próprio `/api/ui/node` e fica `null` quando o relatório não chega, para que a
  UI mostre indisponível em vez de zero. A validade do RTT é de cinco minutos;
  valores vencidos aparecem como diagnóstico, mas não entram na seleção da
  geração.
  `NodeInfo` carries the same thermal policy used by the runtime. `slice/plan` and `slice` accept the
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
  Endereços IPv6 temporários rodam sozinhos: ver `docs/subsystems/rede-p2p.md`,
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
  caminho de apply/launch dos demais ajustes globais; `NodeInfo` e
  `DeployPlan.capabilities.hidden_state_f8` expõem a capability de peer usada
  para bloquear uma implantação f8 incompatível. O redeploy consulta
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
- `POST /api/ui/deploy/node/options` revarre o catálogo do `target_node_id` e
  devolve as faixas fatiadas, o GGUF inteiro disponível nessa máquina e os
  defaults locais de GPU/KV. `POST /api/ui/deploy/node` valida novamente essa
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
  transfer offer and acceptance state, monotonic replay records, and full-chain
  reconciliation. A profile set to `admin` without a valid local chain remains
  without administrative capability. `is_anchored`/`anchor_file_exists` are the
  anchored-versus-not judgement the UI shows; `parse_transported`,
  `anchor_fingerprint` and `fingerprint_matches` are the single manual check
  shared by `pom admin-anchor` and the screen (it normalises only human
  transcription - spaces, colons, case - never the value); `adopt_anchor`
  writes a transported chain onto a node that has none and refuses a node that
  already has one, because replacing a root is manual re-anchoring on the
  machine (`--replace`), not an interface gesture.
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
- `transfer.rs` (1542) - slice push/pull over HTTP `Range` with resume and MD5;
  offers carry only a `stat`, the digest is computed by the receiver. Pushes
  accept only a catalog basename in `filename`, resolve it server-side and
  require `.gguf`. Manifestos v1 são mesclados por `pom_id` no peer receptor,
  preservando `parts` e `tp_parts` locais. **O veredito volta para quem
  ofereceu** (`receive_answer`), e todo desfecho passa por
  `TransferState::finish`, que carimba `finished_at`. Um job carrega o rótulo
  da fatia (`SliceLabel`: modelo + faixa), lido do `.pom` - hash não é nome.
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
  injeta o marcador de contagem regressiva no prompt; ver `api-http.md`.
  **Os argumentos de uma chamada saem em fragmentos** (`CallArgsStreamer`),
  conforme o modelo gera: `eager_input_streaming` por ferramenta no
  `/v1/messages`, sempre no `/v1/chat/completions` porque a OpenAI não tem
  interruptor. A concatenação dos fragmentos é sempre a string que o caminho
  não-streaming produziria, e o prefixo transmitido só cresce - não existe
  desenviar um fragmento SSE.
  A capacidade publicada em `/v1/models` e usada por
  `request_output_limit_for_model` é a menor `RuntimeCapacity` entre todos os
  peers vivos do modelo/implantação. O runtime só anuncia esse valor depois do
  `/health` efetivo; apply, erro e unload retiram a capacidade durante restart.
- `messages.rs` - Anthropic Messages API (`/v1/messages`), same output-limit
  and finish-reason rules, with `x_pom.output_limits` and
  `x_pom.continuation_legs` on blocking responses and the final streaming
  `message_delta`.
- `continuation_lab.rs` (`#[cfg(test)]`) - bancada do laço de continuação: dois
  hops falsos, as rotas `/v1` de verdade num socket TCP e `reqwest` do outro
  lado, inclusive lendo o SSE byte a byte. Prova que a saída volta inteira, que
  a costura não aparece como `finish_reason` intermediário nem como segundo
  `[DONE]`, que a chamada de ferramenta chega com JSON completo, e que o teto
  absoluto para um modelo em laço.
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
- `reqlog.rs` - persistent SQLite audit trail. `AuditLog` enqueues best-effort
  writes on a dedicated bounded thread, executadas pelo `PomDb` compartilhado;
  `AuditDb` cria o schema e o índice FTS5 somente no adaptador de teste,
  abre requests em `in_progress`, completa a mesma linha, marca registros
  antigos como `incomplete` e serve consultas autenticadas paginadas mais
  estatísticas no servidor. Falha de enfileiramento ou gravação registra
  sempre a operação e o request id. Pedidos, conversas e auditoria ficam no
  mesmo `pom.db`; permissões do banco e dos sidecars WAL/SHM são aplicadas no
  arranque.
- **Toda chamada a peer sai por `GatewayState.peer_http`** (`PeerClients`), que
  fixa o certificado do destino pelo node_id. Não existe mais cliente
  compartilhado sem pinning no estado: com TLS ligado ele não verifica ninguém,
  e um chamador que o pegasse por hábito falhava calado. Loopback e Ollama usam
  `peer_http.plain()`.
- `serve.rs` - o laço de accept: espia o primeiro byte, guarda o `PeerAddr` no
  request e serve TLS ou texto claro na mesma porta. `TlsServer` (certificado, modo estrito, duração do
  último handshake). Ver `docs/subsystems/rede-p2p.md`, seção 5.
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
com API key, enquanto `POST /api/ui/network/chat` é o interruptor exclusivo do
principal. A rota de política entre nós é `GET /api/cluster/network-chat`.
O backend não retransmite pela HTTP UI: a ação entra na fila limitada do
`NetworkChat` e o swarm valida e encaminha por `discovery.rs`. O snapshot não
repete bytes de arquivo em cada polling; `GET /api/ui/network-chat/file/:message_id`
faz o download autenticado do blob persistido.

- `gateway/` - `GatewayState` (network, ollama, default_model,
  local_assignment, loads, http, runtime, gateway_port, auth, transfers,
  reply_inbox); `FromRef<GatewayState> for AuthState`; `peers_visible_to_cluster`
  filters blocked nodes; `ready_for_pipeline` health-checks each peer's
  `inference_url()/health` before planning, with the `ADMISSION_PROBE_TIMEOUT`
  of 300ms and an `AdmissionHealth` quarantine for whoever fails - a single
  unreachable announcer used to cost every inference 2,005ms, the
  `peer_client::CONNECT_TIMEOUT`, because `join_all` waits for the slowest; `all_peers` appends
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
  whole process on CUDA OOM (ksia, 2026-09-08), see `inferencia.md` §9; `generate_stream_with_stops`
  builds a `crate::direct::DirectReply` from `reply_inbox` +
  `self_reply_urls(self_reply_hosts(), gateway_port)` and hands `direct` to the
  prefill and decode executors, so the tail can answer this node directly
  (log says `resposta=direta|anel|hub`: direta when `direct` is Some and the
  plan supports direct reply, anel for `relay/v1-chain`, hub as the fallback);
  prefill in hub and direct paths runs as an ordered pipeline of configurable
  depth (`prefill_max_inflight_chunks`, effective 1..8, default 8, also
  `--prefill-max-inflight-chunks`), bounded also by a 512 MiB hidden-state byte
  budget; a chunk that fails after earlier chunks were accepted is rebuilt from
  chunk 0 because the lost response makes partial KV removal unsafe, while the
  ring stays serialized by its nested response contract; the request trace
  reports pipeline depth, prefix reuse and transport per traversal;
  MTP on a sliced plan (`mtp_enabled` + `plan.supports_mtp()` + >= 2 hops):
  the tail produces `next_draft` and the remote head installs it in the next
  internal `step` metadata before continuing its owned loop. The existing
  callback carries that draft and explicit `speculated`, `speculation_active`
  and `speculation_disabled_reason` fields in the same frame; the gateway uses
  the state for telemetry and does not reinject a draft in remote-stream mode.
  The legacy direct path keeps the gateway-side store and injection. A remote
  stream stops MTP only when `speculation_active` is false, while the legacy
  path stops when the head refuses a sent draft or the tail returns no draft;
  both paths continue with `prefill::step` and a named reason;
  the gateway still chooses the pipeline replica per session. Continuous
  batching is owned by the inference process at that replica, not by a global
  gateway queue, so changing replicas mid-session would lose the local KV;
  emits `GenEvent::Token`/`Done(GenSummary { source: "pipeline:hop_chain", .. })`
  /`Failed`; `GenSummary` also carries `GenerationTimings` aggregated over the
  full pipeline: TTFT, prompt-token prefill speed, output-token generation
  speed from the first token, and end-to-end total. The summary trace carries
  the same `trace_id` used by the audit, local gateway spans, separate
  outside-node total/accounted/unaccounted values, cold/warm state, connection
  reuse counts and first-token boundaries. The historical
  `tokens_per_sec` remains the total-rate alias. `generate_blocking_with_stops` collapses it. The terminal event
  (`Done`/`Failed`) is held back until the end-of-turn bookkeeping
  (`rewind_sessions` or `release_sessions`) has finished, because a chat sends
  the next turn the instant the stream closes and used to arrive in the middle
  of it; token events still go out live. `release_sessions` tries every
  `inference_candidates` route, remembers which route worked, and remains best
  effort so a stale address cannot prevent cleanup. A prefill that fails with
  the prefix enabled is retried once from scratch - reusing is an optimisation, never a
  reason for the request to fail. A peer that reports an unknown session also
  triggers that same single cold retry, even when the original request did not
  use prefix reuse, because a later chunk cannot be reconstructed from hidden
  state alone. When the stream receiver closes,
  `generate_stream_with_stops` cancels the prefill's local HTTP future and prevents the
  next retry, hop or chunk; already accepted peer frames are cleaned up by the
  normal session release. `start_gateway` registers
  `POST /api/generate` (+ alias `POST /generate`), `GET /peers`, `GET /health`,
  `POST /handshake` (session::handle_handshake), `POST /v1/relay-result` and
  `POST /v1/remote-stream-result`
  (`crate::direct::RELAY_RESULT_PATH`, 1 MB body limit, no client API-key auth -
  the per-frame `Ticket` is the credential; handler `handle_relay_result`
  delivers into `reply_inbox`); it serves with
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
  `total` é o teto da **requisição**: `min(pedido, n_ctx restante)`. Esgotar a
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
  `audit_status_for_summary` registra `length` como `incomplete`, e reserva
  `partial` para falha de transporte com texto salvo.
  O detector de ciclos (`inference/cycle.rs`) roda depois de cada peça no
  gateway. Ele exige três janelas canônicas, trata sufixos numéricos de
  identificadores como a mesma estrutura e preserva números de dados. Quando
  confirma, termina sem pedir outra ficha, libera a sessão e publica
  `finish_reason: "cycle"` e `partial: true`; o limite pode ser desligado em
  `RuntimeParams` ou com `--no-cycle-detection`.
  **O total é inviolável.** Nada o ultrapassa, nem uma chamada de ferramenta
  aberta no token do corte - houve aqui uma folga que ultrapassava o teto, e
  ela foi removida porque nenhum dos dois
  provedores transborda. Quando o corte cai dentro de uma chamada, o adaptador
  a devolve estruturada e incompleta (`TagScanner::finish_truncated`), com os
  argumentos numa string JSON inacabada - possível porque o scanner segura o
  corpo da chamada até a tag fechar, então nada vazou. Ver
  `docs/subsystems/inferencia.md` (9.1) e a bancada `continuation_lab.rs`.
- `openai.rs` - the shared `DEFAULT_MAX_TOKENS=512` is used only when the
  client omits its output budget; there is no backend hard cap. The effective
  request budget is the minimum of the client request and the remaining `n_ctx`
  after the prompt - the deployment's `max_new_tokens` bounds one leg of the
  continuation loop, not the request;
  `render_prompt` flattens the conversation into one user message, labelling
  **every** turn including the first, so one turn's prompt is an exact prefix of
  the next one's - the property the prefix cache rests on; raw, unlabelled text
  is what `/v1/completions` and `/api/generate` are for.
  `conversation_cache_key` is what the prefix cache keys on (the message array,
  not the rendered text) and travels as `GenerationInput::cache_key`; a versão
  Anthropic, `conversation_cache_key_with_budget`, prefixa a identidade do
  orçamento de tarefa para que uma entrada sem orçamento ou com marcador
  diferente não seja reutilizada; o marcador final é injetado a cada pedido,
  como na semântica do provedor.
  `usage.prompt_tokens_details.cached_tokens` reports the prompt tokens the
  pipeline did not reprocess, read from the trace - the executed number, not the
  cache's choice; `approx_tokens` = chars/4; `model_ids` comes only from live
  peer announcements (never the stale CLI default); model objects include both
  OpenAI and Anthropic discovery fields; `resolve_model` maps unknown names to
  the active model;
  `chat_completions`/`completions` -> `blocking_response` or `sse_stream`;
  `chat_message_json` (flat content + `tool_calls` array),
  `apply_scan_events_openai` (tool_call deltas), `chunk_json` (+
  `x_pom` extension with source/tps/duration/partial/trace/finish_reason/stop_sequence),
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
  `chat/telemetry` (POST autenticado, coleta os relatórios direcionais dos
  peers selecionados para o trace e não altera o planner), `requests` (GET ->
  `crate::reqlog::list_requests`; `chat/history` (GET/POST/DELETE autenticado,
  transcriptos normalizados em `pom.db`); `requests/stats` (GET ->
  `crate::reqlog::stats_requests`). `leave_network` now takes an optional
  `LeaveRequest { node_id }`: absent (or this node's own id) keeps the old
  behavior - this node detaches; a peer's id is **forwarded** to that peer's
  same route (`forward_leave`, same pattern as `dev/update` - the panel only
  talks to its own node, and only the owner of a process can detach it). Also
  the job store (`Job`, `JOB_RETENTION=600s`,
  `jobs()`/`new_job`/`set_job`/`set_job_progress`),
  catalog caching (`CachedCatalog`, `warm_catalog` called at startup,
  stale-while-revalidate). A primeira resposta do catálogo publica o inventário
  rápido de arquivos e `metadata_complete: false`; os cabeçalhos GGUF são lidos
  em uma tarefa `spawn_blocking` e publicados quando terminam, mesmo que o
  refresh tenha trocado o `Arc` do inventário: a reconciliação usa caminho,
  tamanho e mtime do arquivo. O cache instala um placeholder antes da primeira
  varredura, ordena as raízes e mantém single-flight para scans e enriquecimentos;
  F5 ou seleção repetida não empilha trabalho. Uma configuração nova fica em
  fila para o scan atual terminar. O enriquecimento registra início e conclusão
  (inclusive descarte), e uma falha terminal marca `metadata_error` no item e
  não impede os demais, então a passagem termina com `metadata_complete: true`.
  O resultado publicado é reutilizado nas requisições da interface; modelo cujo
  caminho, tamanho e mtime não mudaram não é relido. Nova leitura só ocorre em
  refresh por TTL ou invalidação. O survey de implantação só recusa uma
  seleção que ainda não tem suas faixas resolvidas; em TP ele verifica cada
  rank selecionado. Enriquecimento de outro modelo, peer silencioso ou campo
  ausente em peer antigo não bloqueia. `merge_parts`
  (arquivo temporário com sufixo único, `read_layout` validado antes do rename
  e de `finalize_run`; reuses an already-complete output, preserves the source
  `pom_id`, and invalidates the catalog),
  `run_apply`/`run_slice`/`release_slice`, and `get_setup_key`/`save_setup`
  which are deliberately NOT registered here - the setup mode router in
  `main.rs` serves them only while there is no `pom.json` (they cannot be
  authenticated and would rotate the connection key on a configured node).
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
  `docs/subsystems/implantacao.md` §1.1 e §1.2.
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
  `RuntimeParams.inference_mode` acompanha o pedido nos três tipos de deploy.
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
  `docs/subsystems/inferencia.md` §3.1.1 and `api-http.md`).
  `list_nodes` appends blocked-but-disconnected nodes so the panel is not empty.
  The trust root is reached by `GET/POST /api/ui/network/trust-anchor`
  (`webui/trust.rs`), outside the admin-only scope on purpose.
  The authority contract adds `GET /api/ui/network/admin-authority`,
  `POST /admin/authority/resign`, `POST
  /admin/authority/secondaries/:id/demote`, and the profile transition routes
  `/api/ui/network/request-node` and `/api/ui/network/relinquish-node`.
  Internal profile mutations carry signed authority commands; a promotion
  delivers and validates the complete trust document at the target.
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
  optional), compares to `connection_key`; `InternalNodeAuth` adds the private
  marker used by admin synchronization. `ApiKeyAuth` accepts `authorization`
  or `x-api-key`, but only for an active API key, and `api_key_identity`
  supplies the stable non-secret id used by the audit trail. `internal_node_token`
  chooses the shared `connection_key` for internal inference calls; client API
  keys never cross that boundary.
  `AuthRejection`: MissingHeader=401, InvalidKey=403.
- `reqlog.rs` - `AuditLog::begin` records the exact prompt, request body,
  resolved model, entry node, API-key identity and arrival time before
  generation. `spawn_logged_generation_with_stops` later calls `complete` on
  that id with result, telemetry, timings, token counts, hop chain and error.
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
  (`Config`, `ApiKey`, `ui_dist_dir()`, `shard_dir_path()`, `model_dir_paths()`),
  `crate::models`/`crate::catalog`/`crate::gguf`/`crate::plan`
  (`Catalog`, `GgufLayout`, `PeerBudget`, `pom::PomManifest`), `crate::splitter`
  (`MergePart`, `merge`), `crate::sysmon`, `crate::route` (`RouteTable` for the
  cluster view), `crate::pkarr_node` (`detect_local_ip`), `crate::prefill`
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
- Who calls it: `crate::main` (`start_gateway` spawns the router, `warm_catalog`
  runs at startup, setup mode mounts only `webui::get_setup_key`/`save_setup`);
  the frontend bundle in `apps/frontend/dist` hits `/api/ui/*`; harnesses hit
  `/v1/*` (OpenAI/Anthropic) and `POST /api/generate`; peers call each other's
  gateway HTTP for transfers, deploy forwarding, and cancel notices.

### Visão na implantação e no chat

`deploy::Plan.mmproj_path` vem do catálogo do modelo inteiro e
`DeployRequest.vision` só habilita o projetor na cabeça da implantação.
`webui::ApplyRequest.use_mmproj` persiste a escolha em `NodeConfig` e o
`openai` extrai imagens base64 dos pedidos multimodais. A recusa de imagem sem
projetor e a mensagem para o usuário precisam permanecer alinhadas.
O Chat consulta `GET /api/ui/chat/capabilities`, uma rota comum que agrega o
modelo ativo, a cobertura e o `mmproj_path` da cabeça da implantação sem expor
topologia ao perfil `infer`. A resposta usa `supported`, `unsupported` ou
`unavailable` para visão e cobertura completa, incompleta ou indisponível;
recusa de autorização nunca é convertida em vazio. Visão é propriedade do
modelo servido, não privilégio do perfil.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
