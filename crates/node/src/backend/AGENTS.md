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

## Files

- `auth.rs` (132) - auth extractors. `ConnectionAuth` (connection-key bearer
  only), `ApiKeyAuth(pub Caller)` (connection key or minted API key, either
  `authorization: Bearer` or `x-api-key`), `AuthState = Arc<RwLock<Config>>`.
  `is_valid_inference_token` aplica a mesma regra na porta 8181: a
  `connection_key` compartilhada ou uma API key ativa.
- `gateway.rs` - `GatewayState`, `start_gateway`, `generate_stream_with_stops` /
  `generate_blocking_with_stops` / `spawn_logged_generation_with_stops`, o `StopMatcher` incremental,
  prompt-budget math, the
  direct-reply relay route (`POST /v1/relay-result`), and the `testkit` test
  scaffold (peer/state_with/body_of).
- `webapp.rs` (214) - serves the frontend bundle from disk with a narrow SPA
  fallback (`config::ui_dist_dir()`); nothing is compiled into the binary.
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
  fonte efetiva do token, e o POST salva o segredo no `pom.json` com modo
  restrito e atualiza o `HfTokenStore` sem reinício.
  O handler clona o `ModelFile` antes de criar a task `'static`, não mantém
  empréstimos de respostas do provider em `tokio::spawn`, e as buscas de
  arquivos locais emprestam o nome a cada iteração.
- `webui.rs` (3100) - the `/api/ui/*` control API behind the dashboard:
  catalog, cluster view, slicing, apply/launch, merge, jobs, release, leave,
  storage paths, thermal policy (`GET/POST /api/ui/thermal`) and slice retention.
  `NodeInfo` carries the same thermal policy used by the runtime. `slice/plan` and `slice` accept the
  explicit `tensor_parallel` mode with `tp_size`; the job extracts every rank
  and seals `tp_parts` in the same source-hash manifest used by pipeline
  `parts`. Jobs and the cluster view use conditional JSON responses:
  matching `If-None-Match` returns 304 without a body, and the cluster keeps
  a 2s fingerprinted snapshot cache that bypasses its TTL on known state
  changes.
  `plan` reads the GGUF header from the node that owns `model_path` and, when
  calculating budget-driven ranges, includes the configured context, sessions
  and K/V cache types in the byte arithmetic. The response exposes weight,
  KV and total bytes per shard; absent KV metadata is an explicit bad request.
  `RuntimeParams.prefill_max_inflight_chunks` is persisted with the other
  inference knobs and sets the prefill pipeline depth (1..8, default 8).
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
- `deploy.rs` - `/api/ui/deploy/*` + `POST /api/ui/undeploy`: survey de
  capacidade, estratégias simples/completa, RTT direcional, cobertura, carga projetada,
  escolha de máquinas e sequenciamento das cópias; re-surveys o cluster no
  deploy e mantém `group` técnico separado de `deployment_name` operacional.
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
  `max_tokens` uses the selected deployment's `NodeConfig.max_new_tokens` and
  the remaining `n_ctx` after the prompt, with `DEFAULT_MAX_TOKENS=512` only as
  the absent-request fallback. A reduction is logged and returned in the
  `x_pom.output_limits` extension. Streaming and blocking paths use the same
  finish-reason ordering, so an exhausted budget is `length` even when a tool
  call was parsed; the audit row for that result is `incomplete`, while a
  transport failure remains `partial`. `/v1/models` exposes `context_length`
  and the OpenRouter-compatible `top_provider.max_completion_tokens` from the
  live deployment.
- `messages.rs` - Anthropic Messages API (`/v1/messages`), same output-limit
  and finish-reason rules, with `x_pom.output_limits` on blocking responses and
  the final streaming `message_delta`.
- `tool_call.rs` (725) - tool-call text rendering + `TagScanner` output scan.
- `devupdate.rs` (647) - env-gated remote `git pull` + rebuild + self-restart.
- `admin.rs` (185) - operator control plane: nodes, block/unblock, keys.
- `reqlog.rs` - persistent SQLite audit trail. `AuditLog` enqueues best-effort
  writes on a dedicated bounded thread; `AuditDb` creates the schema and FTS5
  prompt index, opens requests in `in_progress`, completes the same row, marks
  stale rows as `incomplete`, and serves authenticated queries.
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
  `inference/frame-v1-batch`, `relay/v1-chain` and `speculate/v1-lookup`;
  `speculate/v1-mtp-sliced` goes out unconditionally and
  `speculate/v1-mtp` only when `mtp_ready` (the node has the `nextn` block);
  `inference/frame-v1-batch` only when `network.batching_ready()` confirms
  that the loaded local inference server has the multi-item scheduler active;
  `self_reply_hosts()` lists the addresses by which a
  peer reaches this node (`self_dial_host` first, then `self_addr_candidates`);
  `prompt_budget_of(n_ctx, max_sessions, max_new_tokens)` - `n_ctx` is the
  complete window for each session; `max_sessions` multiplies the runtime KV
  reservation, and `n_batch` deliberately does NOT appear (prefill chunks the
  prompt); `generate_stream_with_stops`
  builds a `crate::direct::DirectReply` from `reply_inbox` +
  `self_reply_urls(self_reply_hosts(), gateway_port)` and hands `direct` to the
  prefill and decode executors, so the tail can answer this node directly
  (log says `resposta=direta|anel|hub`: direta when `direct` is Some and the
  plan supports direct reply, anel for `relay/v1-chain`, hub as the fallback);
  prefill in hub and direct paths runs as an ordered pipeline of configurable
  depth (`prefill_max_inflight_chunks`, effective 1..8, default 8, also
  `--prefill-max-inflight-chunks`), bounded also by a 512 MiB hidden-state byte
  budget; a chunk that fails past chunk 0 is resumed from that chunk rather than
  re-sending the whole prompt, while the ring stays serialized by its nested
  response contract; the request trace reports pipeline depth, prefix reuse and
  transport per traversal;
  MTP on a sliced plan (`mtp_enabled` + `plan.supports_mtp()` + >= 2 hops):
  the tail drafts on its own reply (`next_draft`), the gateway stores it and
  injects it as `speculate.draft` on the next round, and the head only
  decodes; `especulando` stops when the head refused a ready draft
  (`draft_sent && !step.speculated`) or the tail returned no `next_draft`
  (`!draft_sent && step.next_draft.is_none()`), then the gateway calls
  `prefill::step` (the direct reply) for the rest of the request - insisting
  would pin the chain to the bubbled ring (2N hops per token);
  the gateway still chooses the pipeline replica per session. Continuous
  batching is owned by the inference process at that replica, not by a global
  gateway queue, so changing replicas mid-session would lose the local KV;
  emits `GenEvent::Token`/`Done(GenSummary { source: "pipeline:hop_chain", .. })`
  /`Failed`; `GenSummary` also carries `GenerationTimings` aggregated over the
  full pipeline: TTFT, prompt-token prefill speed, output-token generation
  speed from the first token, and end-to-end total. The historical
  `tokens_per_sec` remains the total-rate alias. `generate_blocking_with_stops` collapses it. The terminal event
  (`Done`/`Failed`) is held back until the end-of-turn bookkeeping
  (`rewind_sessions` or `release_sessions`) has finished, because a chat sends
  the next turn the instant the stream closes and used to arrive in the middle
  of it; token events still go out live. A prefill that fails with the prefix
  enabled is retried once from scratch - reusing is an optimisation, never a
  reason for the request to fail. A peer that reports an unknown session also
  triggers that same single cold retry, even when the original request did not
  use prefix reuse, because a later chunk cannot be reconstructed from hidden
  state alone. When the stream receiver closes,
  `generate_stream_with_stops` cancels the prefill's local HTTP future and prevents the
  next retry, hop or chunk; already accepted peer frames are cleaned up by the
  normal session release. `start_gateway` registers
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
- `gateway.rs` - `/api/generate` aceita `max_tokens` opcional (padrão 512),
  valida-o contra `NodeConfig.max_new_tokens` e conta o primeiro token do
  prefill no orçamento total, sem o overshoot histórico de 513 tokens; a
  resposta separa TTFT, velocidade do prefill, velocidade da geração e total
  em `timings`. `generation_finish_reason` testa o orçamento antes do EOS em
  cada ramo de decodificação; `audit_status_for_summary` registra `length` como
  `incomplete`, e reserva `partial` para falha de transporte com texto salvo.
- `openai.rs` - the shared `DEFAULT_MAX_TOKENS=512` is used only when the
  client omits its output budget; there is no backend hard cap. The effective
  request budget is the minimum of the client request, deployment
  `max_new_tokens` and the remaining `n_ctx` after the prompt;
  `render_prompt` flattens the conversation into one user message, labelling
  **every** turn including the first, so one turn's prompt is an exact prefix of
  the next one's - the property the prefix cache rests on; raw, unlabelled text
  is what `/v1/completions` and `/api/generate` are for.
  `conversation_cache_key` is what the prefix cache keys on (the message array,
  not the rendered text) and travels as `GenerationInput::cache_key`.
  `usage.prompt_tokens_details.cached_tokens` reports the prompt tokens the
  pipeline did not reprocess, read from the trace - the executed number, not the
  cache's choice; `approx_tokens` = chars/4; `model_ids` from peers + active
  model; `resolve_model` maps unknown names to the active model;
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
- `webui.rs` - `CATALOG_TTL=20s`, `CLUSTER_CACHE_TTL=2s` and
  `pub(crate) PEER_TIMEOUT=4s` (reused by
  deploy.rs). Registers the `/api/ui/*` routes: `node` (GET node_info),
  `model-identity` (POST, prova um arquivo já catalogado por caminho canônico
  e opcionalmente calcula seu SHA-256),
  `models` (GET, `hostname` = o nome anunciado pelo nó, **não** o do SO),
  `models/delete` (POST), `cluster` (GET), `cluster/models`
  (GET), `peer/:node_id/runtime` (GET), `paths` (GET+POST), `plan` (POST), `apply` (POST), `launch` (POST),
  `model-layer-options` (POST, cortes reais do GGUF) e `slice-ranges` (POST,
  materialização local de faixas escolhidas a partir de um modelo inteiro),
  `stop-inference` (POST), `slice` + `slice/plan` (POST), `jobs` (GET) +
  `jobs/:id` (GET), `unload` (POST), `release` (POST) + `release/candidate`
  (GET), `leave` (POST), `rejoin` (POST), `network/rtt` (GET autenticado),
  `network/rtt` (GET autenticado, relatório direcional deste nó),
  `chat/telemetry` (POST autenticado, coleta os relatórios direcionais dos
  peers selecionados para o trace e não altera o planner), `requests` (GET ->
  `crate::reqlog::list_requests`). `leave_network` now takes an optional
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
  (arquivo temporário com sufixo único, rename
  at the end; reuses an already-complete output, preserves the source
  `pom_id`, and invalidates the catalog),
  `run_apply`/`run_slice`/`release_slice`, and `get_setup_key`/`save_setup`
  which are deliberately NOT registered here - the setup mode router in
  `main.rs` serves them only while there is no `pom.json` (they cannot be
  authenticated and would rotate the connection key on a configured node).
  Os testes que chamam `apply_cluster_policy` também seguram
  `peer_client::POLITICA_GLOBAL`, porque o esquema TLS global é compartilhado
  por toda a suíte.
- `deploy.rs` - `DeployKind` (`shards` | `whole` | `tensor_parallel`) escolhe
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
- `deploy.rs` - `launch_targets` é a fase 2 extraída de `run`: os passos do
  plano resolvidos contra a varredura pós-cópia, que é a única parte do deploy
  testável sem cluster (`a_whole_model_is_launched_as_one_plain_target`).
- `deploy.rs` - `RuntimeParams.fit_memory` fica ligado por padrão em shards e
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
- `deploy.rs` - `overlapping_layers`, `split_position`. Routes: `POST
  /api/ui/deploy/plan` (read-only, cheap), `POST /api/ui/deploy` (re-surveys
  the cluster, refuses on `plan.blocked` or empty steps), `POST
  /api/ui/deploy/node`, `POST /api/ui/deploy/node/remove`, `POST
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
  machine uses `runtime.stop()`). `undeploy_authority(role)` gates who may
  undeploy - today it lets every role through (see Defeitos anotados).
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
  (`Caller::operator()`). `api_key_identity` supplies the stable non-secret API
  key id used by the audit trail. `inference_token` chooses the shared connection key,
  falling back to the first active API key for internal inference calls.
  `AuthRejection`: MissingHeader=401, InvalidKey=403.
- `reqlog.rs` - `AuditLog::begin` records the exact prompt, request body,
  resolved model, entry node, API-key identity and arrival time before
  generation. `spawn_logged_generation_with_stops` later calls `complete` on
  that id with result, telemetry, timings, token counts, hop chain and error.
  The request body contains no headers or secrets. `list_requests` requires
  `ConnectionAuth`, supports node, API-key identity, prompt FTS and time-range
  filters, and returns newest first with incomplete/error tallies.
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

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
