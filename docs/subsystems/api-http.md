# Subsistema: API HTTP (gateway) e autenticação

O gateway HTTP é a superfície de rede do nó pom, montada por
`start_gateway` (`crates/node/src/backend/gateway.rs`) como um router axum
único. **Todo nó serve o mesmo conjunto de rotas na porta de gateway**
(`--gateway-port`, padrão 8080), então qualquer endereço de peer vale como
ponto de entrada - não existe "o nó do gateway". O router liga com
`axum::serve(...).tcp_nodelay(true)` (Nagle atrasava as escritas pequenas do
SSE) e tem `CompressionLayer` (que já pula `text/event-stream`, então o token
stream não é comprimido).

Portas, para situar: UDP P2P `--port` (42777), libp2p TCP = UDP + 10000 (52777),
gateway HTTP `--gateway-port` (8080), e o servidor de inferência
`--inference-port` (8181, `inference/infer_server.rs`, um router separado que
serve `/v1/prefill`, `/v1/step`, `/health` - não passa por este gateway). O
`/health` do servidor de inferência responde `model_loaded` (o que promove a
anotação da fatia para `serving`, ver `fatiamento.md`) e expõe `kv_can_shift`,
a condição de a cabeça rascunhar na decodificação especulativa.

## 1. O router e os grupos de rotas

`start_gateway` registra, nesta ordem (webapp é deliberadamente o último):

- **Base** (`gateway.rs`): `POST /api/generate` (+ alias `POST /generate`),
  `GET /peers`, `GET /health`, `POST /handshake` (`session::handle_handshake`),
  `POST /v1/relay-result` (`direct::RELAY_RESULT_PATH`, limite de corpo 1 MB).
- **OpenAI** (`openai.rs`): `GET /v1/models`, `GET /v1/models/:id`,
  `POST /v1/chat/completions` + OPTIONS, `POST /v1/completions` + OPTIONS.
- **Anthropic** (`messages.rs`): `POST /v1/messages` + OPTIONS,
  `POST /v1/messages/count_tokens` + OPTIONS.
- **Dashboard** (`webui.rs`): o plano `/api/ui/*` (ver abaixo).
- **Implantação** (`deploy.rs`): `/api/ui/deploy/*` + `POST /api/ui/undeploy`.
- **Transferência** (`transfer.rs`): `/api/transfer/*` (ver `transferencia.md`).
- **Dev update** (`devupdate.rs`): `POST /api/ui/dev/update`, `POST
  /api/ui/dev/restart` (gated por `POM_DEV_UPDATE`).
- **Operador** (`admin.rs`): `/admin/*`.
- **UI** (`webapp.rs`): `GET /` e `/*path` - serve o bundle do disco
  (`config::ui_dist_dir()`); o fallback de SPA é estreito: miss sob prefixo de
  API vira 404 JSON, miss com extensão de arquivo vira "asset não encontrado",
  o resto cai em `index.html`.

Em **modo setup** (sem `pom.json`), `main.rs` monta um router menor: só
`/api/setup/key`, `/api/setup/save` e a UI. As duas rotas de setup não estão no
router principal e não podem ser autenticadas - existem justamente antes de
haver chave.

## 2. Rotas por grupo

### Base

- **`POST /api/generate`** - `{ model?, prompt, max_tokens?, stop? }`; resposta `GenerateResponse`
  com `response`, `source` (`"pipeline:<cadeia>"`, `"local"`,
  `"peer:<node_id>"` ou `"none"`/`"error"`), `tokens_generated`,
  `tokens_per_sec`, `duration_ms`, `trace`. Corpo aceito como bytes crus (não
  exige `Content-Type: application/json`). `max_tokens` é o orçamento total da
  saída, incluindo o primeiro token que o prefill devolve; sem o campo, o
  padrão é 512. O pedido precisa ser maior que zero e não pode exceder o
  `max_new_tokens` da configuração do nó. Exceder esse limite responde 400
  com o valor pedido e o valor configurado, nunca trunca em silêncio.
- **`GET /peers`** - `{ peers: [PeerSummary], count }`. `PeerSummary` traz
  `node_id` (12 primeiros chars), `addr`, `model`, `layers`, `ollama_url`,
  `inference_url`, `load` (carga em voo).
- **`GET /health`** - `status`, `ollama_available`, `peer_count`,
  `pipeline_peers` (com `inference_port`), `default_model`.
- **`POST /handshake`** - corpo `{ node_id, pubkey, connection_key }`; 403
  `invalid_key`, `identity_mismatch` (o `node_id` não é o `SHA256` da `pubkey`,
  ou a chave é malformada) ou `blocked`; 200 com `approved` + `peer_id`.
- **`POST /v1/relay-result`** - onde a **cauda** de um pipeline entrega o
  resultado direto (`relay/v2-direct`). Sem auth de chave por desenho (ver
  abaixo); ticket vivo = entrega no `ReplyInbox`, ticket morto = `410 Gone`.
- **`GET /api/cluster/tls`** - `{ tls_required }`. É o que a catraca dos outros
  nós consulta no principal. **Exige a chave de conexão**, e é isso que impede
  um intruso de responder pelo principal e rebaixar o cluster para texto claro.
  Ver `rede-p2p.md`, seção 5.

### OpenAI (`/v1/*`)

Camada de protocolo pura sobre `gateway::generate_stream_with_stops`.
`render_prompt` achata a conversa em um único texto e **rotula todo turno,
inclusive o primeiro** - é o que faz o prompt de um turno ser prefixo exato do
prompt do seguinte, sem o que o cache de prefixo nunca acertaria na transição
1→2. Texto cru, sem rótulo, é o que `/v1/completions` e `POST /api/generate`
entregam. `usage.prompt_tokens_details.cached_tokens` informa quantos tokens de
prompt o pipeline não reprocessou; é medida do executado, não da escolha do
cache (ver `inferencia.md`, seção 3.1). `DEFAULT_MAX_TOKENS`
512, teto `HARD_MAX_TOKENS` 4096. `stream: true` vira SSE. Antes de abrir o
SSE em `/v1/chat/completions`, o adaptador faz a mesma guarda de orçamento de
prompt do gateway e responde
HTTP 413 quando o texto não cabe em `n_ctx - max_new_tokens`; isso evita que
clientes que enviam erro como evento SSE, como OpenCode, fiquem presos numa
resposta 200 sem conteúdo. Todas as respostas válidas levam a extensão `x_pom`
(fora do schema) com a prova de execução distribuída. O campo OpenAI `stop`
(string ou lista) passa pelo matcher incremental do gateway: um sufixo que
ainda pode ser prefixo de uma parada fica retido e a própria parada nunca é
enviada ao cliente. CORS `*` para harnesses
de navegador.

### Anthropic (`/v1/messages`)

Mesmo caminho de geração, próprio `render_prompt` (system/messages/tools
separados), `content_blocks` para `tool_use`/`tool_result`, `stop_reason`
(`end_turn`/`max_tokens`/`tool_use`/`stop_sequence`), SSE com os nomes de
evento Anthropic. Também rotula todo turno, pelo mesmo motivo do OpenAI, e
publica o reuso honrado em `usage.cache_read_input_tokens`. `stop_sequences` usa o mesmo matcher incremental e
`stop_sequence` retorna a string que encerrou a saída. `tool_choice` continua
parseado e ignorado. `x_pom` vai na resposta não-streaming.

### `/admin/*`

`GET /admin/nodes` (inclui nós bloqueados mas desconectados, para o painel não
ficar vazio), `POST /admin/nodes/{id}/block`, `POST /admin/nodes/{id}/unblock`,
`DELETE /admin/nodes/{id}`, `POST /admin/regenerate-key`, `GET+POST
/admin/api-keys`, `DELETE /admin/api-keys/{key}`, `GET /admin/transfer-offers`
(stub, ver Defeitos). `list_nodes` adiciona entradas para bloqueados fora da
rede.

### `/api/ui/*`

O plano de controle do dashboard, todos os mutáveis atrás de `ConnectionAuth`:
`node`, `models`, `models/delete`, `cluster`, `cluster/models`,
`peer/:node_id/runtime`, `paths`,
`plan`, `apply`, `launch`, `stop-inference`, `slice` + `slice/plan`, `jobs` +
`jobs/:id`, `unload`, `release` + `release/candidate`, `leave`, `rejoin`,
`requests` (o log de requisições, `reqlog::list_requests`), `network/rtt` (GET
autenticado, snapshot direcional deste nó), `chat/telemetry` (POST autenticado,
recebe `{ node_ids }` e devolve somente os RTTs direcionais observados entre os
peers solicitados), `dev/update` +
`dev/restart`, `network/advertise-addr`, `network/tls`, `thermal`. `leave` aceita
`LeaveRequest { node_id }` opcional e **repassa** para o peer quando é um id
alheio (o painel só fala com o próprio nó).

As rotas remotas `/api/ui/model-provider/search`, `/details`, `/storage` e
`/downloads` expõem o catálogo e os jobs do `ModelProvider` sem repassar
comandos ou tokens do provider. `/storage` informa as pastas de modelos
resolvidas da configuração. Um download escolhe a primeira pasta elegível,
registra o caminho no `Job.local_path` e aparece em `GET /api/ui/jobs`, que é a
fonte de progresso, cancelamento e retry da UI. **`GET /api/ui/model-provider/status`** exige
`ConnectionAuth` e devolve `{ cli: { installed, version, authenticated,
username } }`: CLI ausente é um estado estruturado, enquanto login necessário
e sessão autenticada ficam distinguíveis para a UI. O detalhe do modelo consulta
a licença no provider antes de a tela oferecer o download.

As rotas `POST /api/ui/deploy/plan` e `POST /api/ui/deploy` carregam
`params.inference_mode` (`auto`, `dense` ou `moe`) para os três modos de
execução: modelo inteiro, pipeline fatiado e Tensor Parallelism. O planner
também devolve `is_moe`, `inference_mode_supported` e
`inference_mode_reason`. `is_moe` vem da metadata real de experts do GGUF,
nunca do nome do arquivo. Em modelo denso a UI oculta a escolha; quando uma
combinação ainda não é suportada, a razão permanece visível e o deploy é
recusado para não apresentar uma preferência que não será respeitada.

**`GET` e `POST /api/ui/model-provider/config`** exigem `ConnectionAuth` e
controlam o token opcional do Hugging Face. O GET devolve somente
`{ configured, masked, source }`, nunca o segredo. O POST recebe `{ token }` -
`null` ou texto vazio remove o valor - grava `huggingface_token` no `pom.json`
com permissão `0600` no Unix e atualiza o provider em memória, sem reinício.
A precedência efetiva é `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`, token do
`pom.json` e, por último, o cache (`HF_TOKEN_PATH`, `HF_HOME/token` ou
`~/.cache/huggingface/token`). Assim, variáveis existentes continuam vencendo,
e a configuração explícita da UI vence o cache.

**`POST /api/ui/network/tls`** - corpo `{ tls_required: bool }`, liga ou desliga
o TLS entre peers do cluster inteiro. Responde **403 fora do nó principal**, e
essa checagem é do servidor: esconder o interruptor na tela de um nó comum é
conveniência, não controle. Não precisa de reinício e pode ser virado quantas
vezes o operador quiser - o efeito é imediato neste nó (cache de clientes,
anúncio, recusa de texto claro remoto) e chega aos demais pela catraca, em um
ciclo de gossip. `GET /api/ui/node` devolve o par `tls_required` (estado) e
`tls_authority` (se **este** nó pode mudar), além de `addr_candidates`,
`libp2p_peer_id` e `inference_frame_capabilities` para que um plano de
implantação possa reconstruir o túnel no rank que vai executar a chamada. Ver
`rede-p2p.md`, seção 5.

**`GET/POST /api/ui/thermal`** - exige `ConnectionAuth`. O GET devolve a
política local `{ limit_c, hysteresis_c, cooldown_secs }`; o POST valida que o
limite e a margem são finitos, positivos e formam uma histerese válida, grava a
política no `pom.json` e devolve o valor efetivo. O runtime aplica a alteração
sem reinício. `GET /api/ui/node` inclui a mesma política em `thermal`, junto
com `system.gpus[].temperature_c`, que é `null` quando o driver não fornece a
leitura.

### Leituras condicionais do painel

`GET /api/ui/jobs` e `GET /api/transfer/offers` respondem com `ETag` e
`x-state-version`. Quando o cliente envia `If-None-Match` com a versão atual, o
gateway responde `304 Not Modified` sem corpo. A lista de ofertas tem somente
`{ offers }`; espaço livre em disco é uma decisão interna da transferência,
não parte do contrato de leitura da UI.

`GET /api/ui/cluster` usa o mesmo contrato condicional. Cada gateway mantém por
até 2 segundos o snapshot já montado, protegido por um fingerprint do estado
conhecido de peers, runtime, configuração e implantação. Uma mudança conhecida
troca o fingerprint e força nova coleta antes do fim do TTL. O `ETag` ignora
somente `system.sampled_at`, que é ruído de amostragem; métricas e topologia
continuam invalidando a versão. `x-state-version` é monotônico para snapshots
alterados.
**`POST /api/ui/chat/telemetry`** - coleta sob demanda feita depois de uma
execução do Chat. O gateway inclui a medição local e consulta o
`GET /api/ui/network/rtt` autenticado de cada peer solicitado, aceitando o
relatório somente quando `source_node_id` coincide com o peer que respondeu.
Cada item responde `{ from_node_id, to_node_id, rtt_ms, transport }`. A medida
é da origem da aresta até o destino e não é o RTT do card que um gateway mediu
até um peer. Ausência de uma medida fica ausente na resposta; não há zero de
fallback.

## 3. Autenticação (`auth.rs`)

Duas credenciais, dois extractors:

- **`ConnectionAuth`** - lê só `authorization` (prefixo `Bearer` opcional) e
  compara com `connection_key`. É a credencial de operador: cobre `/admin/*`,
  `/api/ui/*` (mutáveis), `/api/transfer/*` e `/api/ui/deploy/*`. Sem header =
  401 `missing Authorization header`; chave errada = 403.
- **`ApiKeyAuth(pub Caller)`** - aceita `authorization` **ou** `x-api-key`
  (o cliente escolhe conforme a env var que tem: `OPENAI_API_KEY` manda
  `Bearer`, `ANTHROPIC_API_KEY` manda `x-api-key`). A chave de conexão conta
  como operador (`Caller::operator()`); senão vale uma API key mintada em
  `/admin/api-keys`, com `label` e `node_id` opcionais. O `Caller` construído
  aqui é o único lugar que vê a credencial e alimenta o log de requisições
  (`token_label`, `token_hint`, `token_node_id`).

`POST /admin/regenerate-key` troca a `connection_key` (SHA256 de um UUID);
mintar chaves **acumula** em vez de substituir - um token por nó/harness, e o
log atribui cada geração a quem pagou por ela.

**Rotas sem auth**: `/api/generate`, `/peers`, `/health` (o `/api/generate`
loga como anônimo), `/handshake` (a chave vai no corpo), os GETs de leitura do
`/api/ui` (`node`, `models`, `cluster`, `cluster/models`,
`peer/:node_id/runtime`, `paths` GET, `jobs`, `jobs/:id`), e `/v1/relay-result` (abaixo). Os `/v1` OpenAI/Anthropic **exigem**
`ApiKeyAuth`.

`POST /api/ui/chat/telemetry` fica fora da exceção de leitura: ele exige
`ConnectionAuth` porque repassa relatórios internos de RTT entre peers.

A porta interna de inferência (`inference_port`, normalmente 8181) tem outro
router e também exige credencial em `/v1/prefill`, `/v1/step`,
`/v1/session/release`, `/v1/session/rollback` e `/v1/tp/reduce`: aceita a
`connection_key` compartilhada ou uma API key ativa. Nas chamadas internas
entre peers, o nó envia a `connection_key` e, quando ela não existe, a primeira
API key ativa. `/health` continua público para sondas de readiness. O transporte
preferencial entre nós é o túnel
libp2p/Noise; o cliente direto pinado é fallback quando o túnel não está
disponível ou falha no transporte.

### `relay-result` sem auth é por desenho

A rota não tem `ConnectionAuth` porque o hop que responde é um **peer de
inferência**, que nunca teve a chave do operador. A credencial é o **ticket**:
um UUID v4 sorteado por quadro, conhecido só pela própria cadeia, removido do
`ReplyInbox` no `Drop` em qualquer caminho de saída. Uma resposta atrasada leva
`410` e nunca pode cair no token seguinte.

## 4. `x_pom.source` - a prova de execução distribuída

A mesma string que o log de requisições guarda como `source`: `"pipeline:<id1>→<id2>"`
(com `" (partial)"` quando um peer falhou no meio mas sobrou texto),
`"local"` ou `"peer:<node_id>"`. Aparece como campo `source` no
`/api/generate`, como extensão `x_pom` no `/v1` não-streaming e no último
chunk do SSE, e como `source` no `reqlog`. `"none"` marca erro/abandono. É o
único jeito de um harness saber que a inferência foi conjunta: `source`
começando com `pipeline:` e com **dois** hops é a prova.

## 5. Testes de rota - o andaime é um só (`gateway::testkit`)

`gateway::testkit` (`#[cfg(test)]` em `gateway.rs`) monta um `GatewayState`
inteiro com a tabela de peers semeada à mão: `peer(node_id, hostname,
gateway_port)` constrói um `PeerInfo` de loopback, `state_with(node_id, peers)`
monta o estado (com um `Network` real em porta UDP efêmera, mas **nada fala
UDP** - um teste nunca anuncia na LAN nem cai no cluster de verdade), e
`body_of(response)` extrai o JSON da resposta. É o único jeito de testar rota de
controle sem transmissão; `transfer`, `deploy` e `webui` o usam. Para subir um
nó de verdade sem sair anunciando, o equivalente em produção é `--no-discovery`.

## 6. Defeitos anotados

- **DEFEITO ANOTADO: `/admin/transfer-offers` é um stub que sempre retorna
  vazio** (`admin.rs`, `transfer_offers` hardcoda `{ "offers": [] }`). As
  ofertas reais vivem em `GET /api/transfer/offers` (`transfer.rs::list_offers`);
  a rota `/admin` existe mas é morta, então qualquer consumidor dela nunca vê
  uma oferta.
- **Em aberto (decisão de protocolo): o handshake amarra `node_id` à `pubkey`,
  mas não prova posse da chave privada.** `session::node_id_matches_pubkey`
  exige `node_id == SHA256(pubkey)` e uma chave ed25519 bem formada, o que já
  impede quem tem a chave de conexão de se passar por outra máquina (o
  `node_id` é a chave de `blocked_nodes`). Provar posse exige desafio assinado,
  ou seja, mudança de protocolo.
- **DEFEITO ANOTADO (exposição): os GETs de leitura do `/api/ui` são abertos** -
  `node`, `models`, `cluster`, `cluster/models`, `peer/:node_id/runtime`,
  `paths` (GET), `jobs` e `jobs/:id` não têm `ConnectionAuth` (só os mutáveis exigem). `GET
  /api/ui/models` devolve os caminhos locais completos dos modelos, e
  `/api/ui/cluster` a tabela de peers, para qualquer cliente da rede sem
  credencial. Aceitável em LAN confiável, exposto fora dela não.
- **DEFEITO ANOTADO (documentado, não um bug): `/api/generate`, `/peers`,
  `/health` e `/handshake` não têm auth** - `/api/generate` loga o chamador como
  anônimo, e o `/handshake` leva a chave no corpo. Coerente com a regra da casa
  ("LAN confiável"), mas na internet qualquer nó pode gerar texto no cluster.
