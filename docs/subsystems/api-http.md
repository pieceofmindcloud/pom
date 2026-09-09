# Subsistema: API HTTP (gateway) e autenticação

O gateway HTTP é a superfície de rede do nó pom, montada por
`start_gateway` (`crates/node/src/backend/gateway.rs`) como um router axum
único. **Todo nó serve o mesmo conjunto de rotas na porta de gateway**
(`--gateway-port`, padrão 8080), então qualquer endereço de peer vale como
ponto de entrada - não existe "o nó do gateway". O router liga com
`axum::serve(...).tcp_nodelay(true)` (Nagle atrasava as escritas pequenas do
SSE) e tem `CompressionLayer` (que já pula `text/event-stream`, então o token
stream não é comprimido). O listener do gateway continua dual-stack porque
também leva a reserva HTTP da pipeline; a proteção da administração é uma
fronteira de rota aplicada no servidor.

Portas, para situar: UDP P2P `--port` (42777), libp2p TCP = UDP + 10000 (52777),
gateway HTTP `--gateway-port` (8080), e o servidor de inferência
`--inference-port` (8181, `inference/infer_server.rs`, um router separado que
serve `/v1/prefill`, `/v1/step`, `/v1/step-stream`, `/v1/link`, `/health` - não
passa por este gateway). O
`/health` do servidor de inferência responde `model_loaded` (o que promove a
anotação da fatia para `serving`, ver `fatiamento.md`) e expõe `kv_can_shift`,
a condição de a cabeça rascunhar na decodificação especulativa.
As rotas internas desse listener exigem `Authorization: Bearer <connection_key>`
e `x-pom-internal-node: 1`; elas não aceitam API keys de cliente.
Essa reconferência é local e em tempo constante sobre o segredo compartilhado;
não abre uma requisição adicional. A chave pública e a assinatura Ed25519 são
usadas separadamente no handshake para provar a identidade do peer.

## 1. O router e os grupos de rotas

`start_gateway` registra, nesta ordem (webapp é deliberadamente o último):

- **Base** (`gateway.rs`): `POST /api/generate` (+ alias `POST /generate`),
  `GET /peers`, `GET /health`, `POST /handshake` (`session::handle_handshake`),
  `POST /v1/relay-result` (`direct::RELAY_RESULT_PATH`, limite de corpo 1 MB),
  `POST /v1/remote-stream-result` (`direct::REMOTE_STREAM_RESULT_PATH`, o
  callback limitado do transporte remoto).
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
- **Sincronização de API keys** (`admin.rs`): o nó principal expõe
  `/api/internal/api-keys/{upsert,revoke,revoke-origin,validate}` somente para
  chamadas nó-a-nó autenticadas pela `connection_key` e pelo marcador interno.
  O registro central guarda `origin_node_id`; notificações de criação e
  revogação são idempotentes e têm três tentativas com timeout limitado.
- **UI** (`webapp.rs`): `GET /` redireciona para `/admin-ui/`, que serve o
  bundle do disco (`config::ui_dist_dir()`); assets e rotas SPA ficam sob esse
  mount. O fallback de API continua 404 JSON, e miss com extensão vira
  "asset não encontrado".

O registro das rotas de controle da UI fica organizado em
`crates/node/src/backend/webui/routes/`: `common.rs` para rotas genéricas,
`infer.rs` para ações do perfil de inferência, `node.rs` para operações da
máquina e modelos, e `admin.rs` para administração e ciclo de vida. `mod.rs`
faz a composição; a autorização por perfil continua no middleware central do
gateway, portanto a organização dos arquivos não é uma barreira de segurança.

O mesmo `webui/routes/mod.rs` mantém a tabela de dados `ROUTE_RULES`, que é a
fonte do escopo aplicado pelo gateway:

- `admin-only`: administração, logs, desenvolvimento e ciclo de vida;
- `node+admin`: modelos, preparação de máquina, fatiamento, provider,
  transferência e a **leitura** do retrato da rede (`GET /admin/nodes`), que é
  o que a tela Rede mostra;
- `infer+node+admin`: geração, API `/v1`, histórico do chat, pedido de entrada
  de infer para nó, as API keys (`/admin/api-keys`, a credencial de quem atende
  `/v1`, e todo perfil atende), corrigir o endereço anunciado desta própria
  máquina (`POST /api/ui/network/advertise-addr`) e sair/voltar à rede
  (`/api/ui/leave`, `/api/ui/rejoin`);
- `common/identity internal`: catálogo próprio, vistas compartilhadas,
  telemetria e protocolo nó-a-nó.

Uma regra pode nomear o **método**, e a mais específica vence: primeiro o
prefixo mais longo, e no empate a que nomeia o método. É o que deixa
`GET /admin/nodes` aberto ao `node` enquanto bloquear, desbloquear, remover e
promover uma máquina - POST e DELETE sob o mesmo prefixo - continuam
`admin-only`. Sem método declarado, a regra vale para todos.

O endereço anunciado é uma descrição do próprio processo: a rota não recebe
`node_id` e não repassa chamadas a outro peer. Por isso o `infer` pode corrigi-lo
sem obter autoridade administrativa. Os endpoints de administração que recebem
um nó alvo continuam reservados pela regra do alvo e usam a sintaxe `:id` do
Axum 0.7.

O fallback da interface é aplicado somente depois de as rotas concretas serem
tentadas e depois da autorização da superfície. Assim, um POST ou DELETE
desconhecido de um chamador já autorizado devolve 404, e não 405 por casar um
wildcard registrado apenas para GET; perfis sem escopo continuam recusados
antes do fallback.

Uma decisão que a tabela **não** exprime, porque depende do corpo e não do
caminho, fica no handler: `POST /api/ui/leave` sem `node_id`, ou com o próprio,
é abdicar do próprio posto, e qualquer perfil o faz; com o `node_id` de
**outra** máquina o pedido é repassado, e aí exige autoridade administrativa
efetiva. O repasse viaja com a credencial interna, que o alvo não julga pelo
perfil de quem pediu, então sem essa trava o escopo mais largo da rota
transformaria "posso sair" em "posso expulsar qualquer um". Como sempre,
`profile: admin` sozinho não basta: quem decide é a concessão assinada
(`admin_trust::effective_authority`).

O middleware lê o `profile` persistido no `pom.json` e recusa um navegador fora
do escopo com HTTP 403 e JSON nomeando rota e perfil. Uma chamada entre nós que
prova `InternalNodeAuth` pode atravessar o perfil local do receptor para o
fan-out de controle; o handler ainda exige a assinatura administrativa quando
a operação muda autoridade. Nas rotas `common/identity internal`, as chamadas
entre nós exigem a `connection_key` correta e o marcador interno validado pelo
handler, nunca o marcador sozinho. `/api/setup/key`, `/api/setup/save` e as rotas da porta 8181
(`/v1/prefill`, `/v1/step`, `/v1/step-stream`, `/v1/link`, `/v1/session/*`,
`/v1/tp/reduce`, `/health`) ficam explicitamente fora do escopo desta política.

A tela de autoridade usa `GET /api/ui/network/admin-authority` para o contrato
de apresentação do principal, admins secundários, cadeia de promoções e
elegibilidade da sucessão. As mutações isoladas da tela são
`POST /admin/nodes/:id/promote-admin`, `POST /admin/authority/resign` e
`POST /admin/authority/secondaries/:id/demote`; o principal é a única origem
autorizada para promover, resignar e despromover. O pedido de infer para node é
`POST /api/ui/network/request-node` e a descida voluntária de node para infer é
`POST /api/ui/network/relinquish-node`. Os estados das ofertas de autoridade
compartilham `GET /api/transfer/offers` e suas respostas
`POST /api/transfer/offers/{id}/accept|reject`, para que a interface use o
mesmo sino das ofertas de fatia. Para uma oferta de sucessão, `accept` exige a
aceitação assinada do sucessor; `reject` é recusado porque a oferta só termina
com aceitação assinada ou avanço de época. Esses endpoints são contratos de
apresentação e não definem chave, assinatura ou armazenamento de concessões.

## 2.1 Autoridade administrativa

`connection_key` é apenas credencial de transporte entre nós. A capacidade de
admin vem do arquivo separado `config::admin_trust_path()`, protegido com modo
0600 e contendo a âncora manual, a concessão raiz, a cadeia de concessões
secundárias, despromoções, sucessão e registros persistidos de replay. O
`profile: admin` sozinho não cria capacidade: sem cadeia válida o nó permanece
sem acesso às rotas `admin-only` e emite um diagnóstico no arranque.

`GET /api/ui/network/admin-authority` expõe somente a vista de apresentação. O
principal promove com `POST /admin/nodes/:id/promote-admin`; a entrega leva a
cadeia completa assinada ao alvo. `POST /admin/authority/resign` abre uma oferta
assinada para o último secundário ativo, e `POST
/api/transfer/offers/{id}/accept` só conclui quando o sucessor apresenta a
aceitação assinada. `POST /admin/authority/secondaries/:id/demote` é exclusivo
do principal. O caminho de infer para node é um pedido assinado autorizado por
qualquer admin, e node para infer é uma decisão local com aviso.

O pedido de promoção é uma transação nó-a-nó em duas pernas. O `infer` envia a
solicitação assinada a `/api/internal/profile/authorize-node`; o admin autoriza
pela cadeia Ed25519 e envia o comando assinado a
`/api/internal/profile/accept-node`. Cada perna tenta os
`PeerInfo::gateway_candidates()` na ordem anunciada, então `gateway_url()` é
apenas a primeira tentativa e não a verdade permanente do peer. Resposta HTTP
é preservada como recusa explícita; somente falha de transporte leva ao próximo
endereço. O prazo da ida é maior que o de uma tentativa da volta, pois a
resposta do admin precisa conter a segunda chamada. A mesma mutação assinada
não é reenviada automaticamente depois de um timeout, evitando que a proteção
contra replay a rejeite como se fosse um novo pedido.

Os quadros de inferência não recebem assinatura administrativa por quadro; o
peer é autenticado pelo transporte libp2p e o controle permanece separado.

### Ancorar a raiz pela interface

`pom admin-anchor` continua sendo o gesto do operador na máquina. A interface
espelha os dois lados dele em `GET/POST /api/ui/network/trust-anchor`
(`crates/node/src/backend/webui/trust.rs`).

O `GET` mostra o que o operador precisa conferir: `anchored`, `fingerprint` da
chave ancorada, `cluster_id`, `anchor_node_id`, `is_anchor` (a raiz é a chave
deste nó), `loopback` e `can_install`. Tudo é público - a impressão digital
existe para ser transportada e conferida, e conhecê-la não concede nada.

O `POST` recebe `{ document, fingerprint }`: o conteúdo colado do arquivo de
confiança e a impressão digital **digitada pelo operador**. Três recusas, todas
no servidor, e nenhuma delas contornável pelo navegador:

| `reason` | HTTP | quando |
|---|---|---|
| `not_loopback` | 403 | a ligação não veio do loopback |
| `already_anchored` | 409 | o nó já tem raiz, ou já há arquivo no disco |
| `invalid_document` | 400 | o conteúdo colado não é cadeia assinada válida |
| `fingerprint_mismatch` | 400 | o valor digitado não é o da âncora colada |
| `install_failed` | 409 | a gravação falhou; nada foi alterado |

O loopback é medido no endereço real do socket (`serve::PeerAddr`), **não** em
`admin_exposure`, que pode estar deliberadamente aberta à rede, e nunca em
`X-Forwarded-For` ou afins, que o cliente escreve. Um nó já ancorado recusa
porque trocar a raiz é reancoragem manual (`pom admin-anchor --replace`), não
gesto de interface. E o arquivo não se auto-valida: a assinatura só prova
coerência interna da cadeia; quem a valida é o valor que o operador trouxe pelo
canal dele, por isso ele nunca é pré-preenchido a partir do conteúdo colado.

A rota é `CommonOrInternal` na tabela de escopo, de propósito: um `profile:
admin` **por ancorar** não passa em `AdminOnly`, que é exatamente o estado em
que ele precisa ver a impressão digital, e quem recebe a âncora costuma ser um
`node` ou um `infer`. Ela também não exige a `connection_key`: a fronteira
continua sendo `AdminAuth` (loopback em modo local, token humano em modo de
rede), e somar aquela credencial não protege contra a ameaça que a trava de
loopback contém - quem alcança o navegador alcança o `localStorage` da mesma
origem - enquanto deixaria a tela morta num navegador que ainda não a recebeu.

### Fronteira de exposição

O modo de exposição em Configurações / Preferências vale somente para a
superfície administrativa: `/api/ui/**`, `/admin/**` e o mount estático
`/admin-ui/**` (incluindo a raiz e assets antigos). Em `localhost`, o servidor
recusa essa superfície quando a conexão veio de fora de loopback e devolve uma
página HTML autônoma, sem bundle nem navegação, com status 403 e as mensagens
em inglês e português. Em `network`, a tela estática de login é pública, mas
os dados administrativos exigem `X-POM-Access-Token` na primeira chamada; o
servidor então emite uma sessão em memória identificada por cookie `HttpOnly`,
`SameSite=Strict`, com validade de 8 horas e caminhos somente administrativos.
O digest da sessão é invalidado ao regenerar o token ou voltar a localhost.
O valor do token é comparado no servidor e nunca é substituído pela
`connection_key`. Chamadas de controle entre nós usam um marcador interno junto
da `connection_key`, sem abrir esse caminho para o navegador.

Toda recusa administrativa se nomeia no cabeçalho `x-pom-admin-error`:
`missing-token` (401, não havia credencial nenhuma), `invalid-token` (403,
havia e não confere), `no-peer-address` (403) e `local-only` (403, a página
autônoma). A frase do corpo é para a pessoa e muda com o idioma; este valor é
estável e é o que a interface usa. A sessão vive só na memória do nó, então um
reinício ou o fim das 8 horas a derruba com a aba aberta: ao ver
`missing-token`/`invalid-token` numa rota administrativa, a interface descarta
a credencial morta e refaz a sonda de superfície, voltando ao portão de acesso.
Antes disso cada tela imprimia o 401 cru do servidor no lugar do seu conteúdo,
sem caminho de volta que não fosse recarregar na mão.

As rotas da pipeline não entram nessa fronteira: `/api/generate`, `/generate`,
`/v1/**`, `/health`, `/peers` e `/handshake`, além das rotas do servidor de
inferência em `--inference-port`, continuam acessíveis por IPv4 e IPv6 para
que o HTTP de reserva do túnel libp2p não seja interrompido. `GET /api/access`
é a única exceção pública da administração, pois informa apenas o modo, a
necessidade de token e a porta para a tela decidir se deve mostrar a catraca.

Em **modo setup** (sem `pom.json`), `main.rs` monta um router menor: só
`/api/setup/key`, `/api/setup/save` e a UI. As duas rotas de setup não estão no
router principal e não podem ser autenticadas - existem justamente antes de
haver chave.

## 2. Rotas por grupo

### Base

- **`POST /api/generate`** - `{ model?, prompt, max_tokens?, stop? }`; resposta `GenerateResponse`
  com `response`, `source` (`"pipeline:<cadeia>"`, `"local"`,
  `"peer:<node_id>"` ou `"none"`/`"error"`), `tokens_generated`,
  `tokens_per_sec`, `duration_ms`, `timings`, `trace`. `timings` agrega o
  pipeline inteiro e separa `ttft_ms` (fila + prefill até o primeiro token),
  `prefill_ms`/`prefill_tokens_per_sec` (tokens efetivamente processados, após
  reuso de prefixo), `generation_ms`/`generation_tokens_per_sec` (saída desde
  o primeiro token) e `total_ms`/`total_tokens_per_sec`. Uma taxa sem tokens ou
  sem intervalo mensurável é `null`; `tokens_per_sec` continua como alias do
  total para não quebrar clientes. Corpo aceito como bytes crus (não
  exige `Content-Type: application/json`). `max_tokens` é o orçamento total da
  saída, incluindo o primeiro token que o prefill devolve; sem o campo, o
  padrão é 512. O pedido precisa ser maior que zero. A capacidade efetiva vem
  do `n_ctx` confirmado pelo runtime e pelo menor valor entre os peers vivos;
  o `max_new_tokens` da implantação limita apenas uma perna do laço de
  continuação. Se o prompt ocupar todo o contexto, a rota responde 413 antes
  de abrir o SSE. `finish_reason` e `partial` também aparecem no corpo: uma
  geração interrompida pelo detector de ciclo tem `finish_reason: "cycle"` e
  `partial: true`; esse motivo não é um corte silencioso nem uma nova execução.
  A rota aceita somente `Authorization: Bearer <API key ativa>` ou
  `x-api-key: <API key ativa>`; a `connection_key`, token ausente e caller
  anônimo são recusados antes do pipeline.
  `trace` também expõe, por requisição, `mtp_draft_tokens_proposed`,
  `mtp_draft_tokens_accepted`, `mtp_draft_tokens_rejected` e
  `mtp_acceptance_rate` (`accepted / proposed`, ou `null` sem proposta MTP).
  `proposed` conta somente o draft que chegou à verificação do alvo.
- **`GET /peers`** - `{ peers: [PeerSummary], count }`. `PeerSummary` traz
  `node_id` (12 primeiros chars), `addr`, `model`, `layers`, `ollama_url`,
  `inference_url`, `load` (carga em voo).
- **`GET /health`** - `status`, `ollama_available`, `peer_count`,
  `pipeline_peers` (com `inference_port`), `default_model` e
  `runtime_capacity` (`n_ctx`, `max_new_tokens`) quando o runtime local já
  respondeu `model_loaded`. Na porta de inferência, a resposta também publica
  `n_ubatch_effective`, `mtp_ready` e `mtp_unavailable_reason`; este último
  explica por que o contexto MTP pedido não foi anexado. `load_params` traz os
  parâmetros de llama.cpp com que o contexto subiu - `n_ctx`, `n_ctx_seq`,
  `n_batch`, `n_ubatch`, `n_seq_max`, `n_threads`, `n_threads_batch`,
  `flash_attn`, `flash_attn_auto`, `kv_unified`, `embeddings`, `n_rs_seq`,
  `offload_kqv`, `cache_type_k`, `cache_type_v`, `n_gpu_layers` e
  `fit_memory_applied` -, lidos do contexto vivo e não do `EngineConfig`. Com
  `flash_attn: null` a ponte de diagnóstico não respondeu, e a interface diz
  que não sabe em vez de mostrar "desligado".
- **`POST /handshake`** - protocolo de desafio em duas mensagens. A primeira
  requisição leva `{ node_id, pubkey, connection_key }` sem `nonce` nem
  `signature`; ela é recusada com 403 `challenge_required` e devolve um nonce
  hexadecimal aleatório de 32 bytes. A segunda repete os campos e acrescenta
  `{ nonce, signature }`, onde `signature` é a assinatura Ed25519, em
  hexadecimal, do nonce bruto pela chave privada correspondente à `pubkey`.
  O nonce fica válido por 30 s e é consumido uma única vez. Só então o gateway
  devolve 200 com `approved` + `peer_id`. `invalid_key`, `identity_mismatch`
  (o `node_id` não é o `SHA256` da `pubkey`, ou a chave é malformada),
  `blocked`, `missing_nonce`, `missing_signature`, `invalid_signature`,
  `nonce_reused` e `nonce_expired` continuam sendo recusas 403. Um nó antigo,
  que envia o corpo anterior e não sabe repetir com a assinatura, vê 403 e
  nunca é admitido.
- **`POST /v1/relay-result`** - onde a **cauda** de um pipeline entrega o
  resultado direto (`relay/v2-direct`). Não usa API key de cliente: exige um
  ticket vivo no corpo; ticket morto = `410 Gone`.
- **`POST /v1/remote-stream-result`** - callback do head remoto para cada lote
  oportunista. Não usa API key de cliente: exige um ticket vivo. O ticket é
  consumido por um `mpsc` limitado a 16 lotes; quando a
  fila está cheia, o handler espera e aplica contrapressão. Ticket cancelado ou
  lote terminal encerra o stream; ticket desconhecido responde `410 Gone`.
  Além de `accepted`, `done`, tempos e `node_timings`, o lote pode trazer
  `next_draft`, `speculated`, `speculation_active` e
  `speculation_disabled_reason`. O draft viaja nesse mesmo callback, mas a
  cabeça remota o instala internamente no próximo `step`; o gateway não faz
  uma chamada de reinjeção. Peers antigos sem esses campos continuam válidos.
- **`GET /api/cluster/tls`** - `{ tls_required }`. É o que a catraca dos outros
  nós consulta no principal. **Exige a chave de conexão**, e é isso que impede
  um intruso de responder pelo principal e rebaixar o cluster para texto claro.
  Ver `rede-p2p.md`, seção 5.

### OpenAI (`/v1/*`)

Camada de protocolo pura sobre `gateway::generate_stream_with_stops`.
As rotas de inferência do cliente exigem uma API key ativa: `Authorization: Bearer
<chave>` ou `x-api-key: <chave>`. Os headers são candidatos independentes:
quando os dois chegam, uma credencial válida autentica mesmo que a outra esteja
inválida; dois inválidos continuam HTTP 403. A `connection_key` identifica o
canal nó-a-nó, mas não autentica inferência do cliente. Uma API key ativa preserva seu
`Caller` no log de auditoria, e uma chave desconhecida localmente é validada no
principal pelo canal interno autenticado; timeout, erro ou resposta inválida
falham fechados antes do dispatch.
Esse mapeamento segue a [autenticação da API Claude](https://platform.claude.com/docs/en/manage-claude/authentication): `ANTHROPIC_API_KEY` usa `x-api-key` e um token Bearer usa `Authorization`.
`render_prompt` achata a conversa em um único texto e **rotula todo turno,
inclusive o primeiro** - é o que faz o prompt de um turno ser prefixo exato do
prompt do seguinte, sem o que o cache de prefixo nunca acertaria na transição
1→2. Texto cru, sem rótulo, é o que `/v1/completions` e `POST /api/generate`
entregam. `usage.prompt_tokens_details.cached_tokens` informa quantos tokens de
prompt o pipeline não reprocessou; é medida do executado, não da escolha do
cache (ver `inferencia.md`, seção 3.1). `DEFAULT_MAX_TOKENS` 512 é apenas o
padrão quando o cliente não envia orçamento. Não há teto artificial no
adaptador: o orçamento **total** da requisição é o menor entre o pedido do
cliente e o espaço restante de `n_ctx` depois do prompt. O `max_new_tokens` da
implantação **não** entra nessa conta:
ele é o teto de uma **perna** do laço de continuação, que o gateway atravessa
sem fechar o SSE (ver `inferencia.md`, seção 9.1). Se o total for reduzido, o
gateway registra a redução e a devolve em `x_pom.output_limits`
(`requested_max_tokens`, `effective_max_tokens`, `deployment_max_tokens`,
`context_available_tokens`, `leg_max_tokens`); se o contexto não deixar nenhum
token, responde HTTP 413 antes de abrir o SSE. A capacidade publicada em
`/v1/models` é derivada do `n_ctx` efetivo, nunca de um teto absoluto oculto.
`x_pom.continuation_legs` diz quantas pernas a resposta costurou - `1` é uma
geração só. Assim, uma saída que esgota o orçamento **total** recebe
`finish_reason: "length"`, inclusive se contiver uma chamada de ferramenta já
reconhecida, e a auditoria registra esse término como `incomplete` (`partial`
fica reservado para falha de transporte com texto salvo). Uma perna esgotada
nunca produz `length`: o cliente não tem como distinguir a costura, e era
justamente esse `length` intermediário que fazia um harness largar a chamada de
ferramenta pela metade. O total, por sua vez, é **inviolável**: nada o
ultrapassa, nem uma chamada de ferramenta aberta no token do corte. Quando o
corte cai dentro de uma chamada, ela volta **estruturada e incompleta** -
`tool_calls[].function.arguments` com a string JSON inacabada na OpenAI, um
bloco `tool_use` final com o `input` incompleto na Anthropic -, e
`x_pom.truncated_tool_call` é `true`, junto de `x_pom.partial`. É a forma dos
dois provedores: o cliente lê o motivo de parada terminal e repete o pedido com
um teto maior, sem executar nada. O scanner compartilhado
reconhece os marcadores declarados em
`CALL_FORMATS`, preserva texto anterior à chamada, transforma todas as chamadas
completas em `tool_calls` e, no SSE, em deltas indexados. Um marcador incompleto
que o **modelo** deixou pela metade volta como conteúdo literal para não
desaparecer; um que o **teto** cortou volta estruturado, e é essa a diferença
entre `TagScanner::finish` e `finish_truncated`. Todas as
respostas válidas levam a extensão `x_pom` (fora do schema) com a prova de
execução distribuída e o
objeto próprio `timings`; o campo OpenAI `stop`
(string ou lista) passa pelo matcher incremental do gateway: um sufixo que
ainda pode ser prefixo de uma parada fica retido e a própria parada nunca é
enviada ao cliente. CORS `*` para harnesses
de navegador.

`GET /v1/models` e `GET /v1/models/{id}` anunciam somente modelos publicados
por peers vivos, inclusive o modelo local quando há uma atribuição ativa. O
`default_model` do CLI e o registro persistido de deployments não entram nessa
autodetecção, então um unload não deixa um modelo fantasma no catálogo. A lista
traz o envelope de paginação Anthropic (`first_id`, `last_id`, `has_more`) e
campos compatíveis com os dois clientes: `type: "model"`, `created_at`,
`display_name`, `capabilities`, `max_input_tokens`, `max_tokens`, além de
`context_length` e `top_provider.max_completion_tokens` para OpenAI. O campo
`created_at` usa o epoch RFC 3339 quando a data de release local é desconhecida.

Os limites vêm do runtime efetivo e da rede: `context_length`,
`max_input_tokens`, `max_tokens` e `top_provider.max_completion_tokens` são o
menor `n_ctx` entre os peers da pipeline planejada que servem o
modelo/implantação, a capacidade da **requisição**, não o teto de uma perna.
Anunciar o
`max_new_tokens` da implantação aqui faria o cliente pedir menos do que o nó
entrega. O mesmo mínimo de peers é usado pela validação de cada requisição.
Um peer só entra nessa capacidade depois de seu `/health` confirmar o modelo
carregado; durante restart, o anúncio anterior é retirado.
Um peer ocioso escolhido pelo plano é acordado por `reload_model` antes da
geração e a requisição espera seu `/health`; um peer vivo de rota alternativa
não bloqueia o catálogo nem a admissão. Um peer planejado não ocioso sem essa
confirmação mantém o modelo pendente, em vez de cair na capacidade local e
esconder um estágio antigo.

### Anthropic (`/v1/messages`)

Mesmo caminho de geração, próprio `render_prompt` (system/messages/tools
separados), `content_blocks` para `tool_use`/`tool_result`, `stop_reason`
(`end_turn`/`max_tokens`/`tool_use`/`stop_sequence`), SSE com os nomes de
evento Anthropic. Também rotula todo turno, pelo mesmo motivo do OpenAI, e
publica o reuso honrado em `usage.cache_read_input_tokens`. `stop_sequences` usa o mesmo matcher incremental e
`stop_sequence` retorna a string que encerrou a saída. `tool_choice` continua
parseado e ignorado. `x_pom.output_limits` informa o orçamento pedido, o
efetivo e o de uma perna; `x_pom.continuation_legs`, quantas pernas foram
costuradas; `x_pom.timings` usa a mesma convenção do OpenAI; uma saída cortada
pelo orçamento **total** usa `stop_reason: "max_tokens"`, e uma perna esgotada
nunca.

Para Claude Code, `ANTHROPIC_BASE_URL` deve apontar para a origem do gateway,
por exemplo `http://ksia:8080`; o SDK acrescenta `/v1/messages`. A variável
`ANTHROPIC_API_KEY` é enviada como `x-api-key`; `ANTHROPIC_AUTH_TOKEN` é
enviado como `Authorization: Bearer`. O valor precisa ser uma API key ativa do
`pom.db`, e nunca uma `connection_key` ou chave Anthropic externa. Se o cliente enviar os dois headers, o gateway considera os
dois independentemente, sem registrar nenhum segredo.

#### Streaming incremental dos argumentos

Os argumentos de uma chamada de ferramenta saem em **fragmentos, conforme o
modelo os gera**. Medido no ksia antes disso: 663 caracteres num pedaço só, com
o primeiro e o último quadro ambos aos 11,01 s - o leitor de tags acumulava o
corpo inteiro e só descarregava quando a chamada fechava, então o usuário
encarava uma tela parada e recebia o arquivo de supetão.

`tool_call::CallArgsStreamer` transcodifica a marcação do modelo em JSON
conforme ela chega. **A concatenação dos fragmentos é sempre exatamente a
string de argumentos que o caminho não-streaming produziria** - inclusive
quando o teto corta a chamada, caso em que ela é a string inacabada da seção
9.1 de `inferencia.md`. O que já foi transmitido nunca é repetido no
fechamento: um fragmento parcial normal e um truncamento terminal não se
confundem na saída.

O ajuste é `eager_input_streaming`, booleano **opcional na definição da
ferramenta**, sem cabeçalho beta - forma da Anthropic, verificada na
documentação viva em 2026-08-29
(`platform.claude.com/docs/en/agents-and-tools/tool-use/fine-grained-tool-streaming`):

| onde | comportamento |
|---|---|
| `/v1/messages`, `eager_input_streaming: true` na ferramenta | fragmentos conforme o modelo gera |
| `/v1/messages`, ausente ou `false` | represado **por valor de parâmetro** (o padrão deles): o parâmetro sai inteiro quando fecha |
| `/v1/chat/completions` | sempre em fragmentos - a OpenAI não tem interruptor para isso, e inventar um campo estenderia o schema dela |

**O tipo do argumento nunca é perdido.** A marcação do POM não carrega tipo -
`<parameter=limit>5</parameter>` só vira número porque `decode_parameter_value`
tenta parsear o valor completo. Por isso o modo ansioso só transmite valor que
**comprovadamente não pode ser JSON**: acima de `EAGER_MIN_VALUE_BYTES` (64) e
começando com caractere que não pode abrir um JSON válido (`{`, `[`, `"`, `-`,
`+` e dígito ficam represados; letra passa, porque `true`/`false`/`null` são
literais curtos). Um arquivo que começa em `{` fica represado - custa latência
num caso raro, e é o preço de nunca entregar um array virado em texto.

O invariante que sustenta tudo é a **monotonicidade**: o prefixo transmissível
só cresce, nunca encolhe nem diverge, porque não existe desenviar um fragmento
SSE. Dela saem o recuo de espaço em branco no fim do valor, a retenção do que
ainda pode virar `</parameter>`, e a ordem das chaves do fechamento (ordem do
documento, não a alfabética de `serde_json::Map`). `CallArgsStreamer` ainda
confere isso em tempo de execução e suprime o fragmento divergente em vez de
corromper o JSON do cliente.

**Limite declarado:** a forma Hermes (`<tool_call>{"name":…}`) não é
transcodificada ao vivo - o nome mora dentro do próprio JSON que estaria sendo
transmitido -, então ela sai inteira no fechamento, como antes.

#### Orçamento de tarefa

`output_config.task_budget` é o mecanismo que os provedores oferecem **no lugar
do transbordo** do teto: em vez de deixar a geração passar de `max_tokens`, ele
informa o modelo do orçamento para que ele se organize e termine de forma
graciosa dentro do limite. A forma é a da Anthropic, verificada na documentação
viva em 2026-08-29 (`platform.claude.com/docs/en/build-with-claude/task-budgets`):

```json
{
  "output_config": {
    "task_budget": {"type": "tokens", "total": 64000, "remaining": 42000}
  }
}
```

- `type` é sempre `"tokens"`; qualquer outro valor é HTTP 400.
- `total` é o orçamento do laço agêntico inteiro. O mínimo é
  `TASK_BUDGET_MIN_TOTAL = 20000`, o mesmo dos provedores, e abaixo dele a
  resposta é HTTP 400 - orçamento pequeno demais para a tarefa faz o modelo
  recusar ou parar cedo.
- `remaining` é opcional e vale `total` quando ausente: é o resto vindo de uma
  requisição anterior.
- Campos de `output_config` que este nó não implementa (`effort`, `format`) são
  aceitos e ignorados; recusar um campo que o cliente manda para outro modelo
  não protege ninguém.

Ele é **advisory e nunca corta**. Quem corta é `max_tokens`, que é teto imposto
e inviolável - um `total` de um milhão não estende o teto por um token, e a
bancada afirma isso. O servidor injeta na geração um marcador de contagem
regressiva que só o modelo enxerga (`messages::task_budget_marker`), no fim do
prompt renderizado, logo antes de o modelo começar a escrever. O marcador nunca
aparece na resposta.

Como o marcador faz parte do prompt, o caminho Anthropic inclui `type`, `total` e
`remaining` na chave do cache de prefixo. Mudar qualquer um desses valores
invalida o KV anterior. O marcador é final e é injetado a cada requisição, logo
não coincide entre turnos, como na semântica documentada da Anthropic.

**A forma do marcador é deste nó.** A Anthropic documenta que injeta um
marcador server-side e que ele é visível só para o modelo; o texto literal não
é publicado, então não há o que espelhar. O que é espelhado é o contrato.

**Limite declarado.** Lá o marcador atravessa o laço agêntico inteiro e
atualiza sozinho a cada turno, contando o que o modelo vê (pensamento, chamadas
de ferramenta, resultados, saída) e não o histórico reenviado. Aqui ele é
injetado **uma vez por requisição**, com o `remaining` que a requisição trouxe:
o nó não guarda estado entre requisições, então quem costura o laço é o cliente
- exatamente o caso que a Anthropic documenta para compactação, em que o
`remaining` é passado adiante a cada pedido. `x_pom.task_budget` devolve
`{total, remaining_at_turn_start}` e `x_pom.task_budget_spent_this_turn` diz o
que o turno gastou, para o cliente decrementar. Nada disso entra em `usage`: a
Anthropic não devolve orçamento restante ali, e copiar o schema dela inclui o
que ela **não** manda.

**Não verificado:** se um modelo local reage ao marcador. A bancada prova que
ele chega à geração; se o modelo servido sabe usá-lo é medição em cluster.

### `/admin/*`

`GET /admin/nodes` (inclui nós bloqueados mas desconectados, para o painel não
ficar vazio), `POST /admin/nodes/:id/block`, `POST /admin/nodes/:id/unblock`,
`POST /admin/nodes/:id/promote-admin`, `POST /admin/authority/resign`,
`POST /admin/authority/secondaries/:id/demote`, `DELETE /admin/nodes/:id`, `POST /admin/regenerate-key`, `GET+POST
/admin/api-keys`, `DELETE /admin/api-keys/{key}`, `GET /admin/transfer-offers`
(stub, ver Defeitos). `list_nodes` adiciona entradas para bloqueados fora da
rede.

`POST /admin/nodes/:id/promote-admin` só é aceito pelo nó cuja autoridade
efetiva no arquivo separado é `Primary`. O principal assina uma concessão
`secondary` pela identidade Ed25519; o alvo aceita a mudança somente após
validar a âncora manual e a cadeia completa, enquanto ainda é `node`.
`GET /api/ui/network/admin-authority` é a vista da cadeia e dos membros. A
resignação é uma oferta assinada ao último secundário ativo e só a aceitação
assinada pela chave desse sucessor muda o principal. Um secundário não pode
promover nem despromover outro.

`DELETE /admin/api-keys/:key` desativa a chave no `pom.db` sem apagar a linha e
acrescenta uma revogação com o `node_id` do operador autenticado e o horário.
Uma segunda tentativa é idempotente e não cria outro evento; a chave nunca
volta a autenticar.

#### Snapshots de KV cache (`--slot-save-path`)

`GET /admin/slots` devolve `{slot_persist_enabled, slot_count}` lidos do
`/health` da porta de inferência **local** (vista rápida deste nó), mais
`prefix_auto_cache: {enabled, index_entries, index_bytes}` quando o cache de
prefixo persistente (`slot_persist_prefix_cache`) está ligado neste gateway.

`POST /admin/slots/:name?action=save|restore|erase` orquestra um snapshot pela
implantação inteira. Corpo: `{model, session_id?, dest_session_id?}` (`model` é o
rótulo ou o `deploy_id`). O gateway resolve o pipeline com o mesmo planejador da
geração e chama `POST /v1/session/slot` em cada hop; cada peer grava/lê a fatia
dele em `slot_save_path` com o nome `"{name}__{node_id}"`. Tudo-ou-nada: um hop
sem a capability `inference/slot-persist-v1`, ou uma falha em qualquer hop,
recusa a operação inteira (os hops já gravados são apagados). `save` exige
`session_id` de uma sessão viva (vem do `x_pom` de uma resposta); `restore` cria
`dest_session_id` (ou um id novo) e confere que todos os hops restauraram a
mesma fronteira de tokens. Ver `docs/subsystems/inferencia.md`.

### `/api/ui/*`

O plano de controle do dashboard, todos os mutáveis atrás de `ConnectionAuth`:
`node`, `models`, `models/delete`, `cluster`, `cluster/models`,
`peer/:node_id/runtime`, `paths`,
`plan`, `apply`, `launch`, `stop-inference`, `slice` + `slice/plan`, `jobs` +
`jobs/:id`, `unload`, `release` + `release/candidate`, `leave`, `rejoin`,
`requests` e `requests/stats` (a auditoria persistente, respectivamente
`reqlog::list_requests` e `reqlog::stats_requests`), `network/rtt` (GET
autenticado, snapshot direcional deste nó), `chat/telemetry` (POST autenticado,
recebe `{ node_ids }` e devolve somente os RTTs direcionais observados entre os
peers solicitados), `chat/history` (GET/POST/DELETE autenticados para o histórico
durável de conversas), `dev/update` +
`dev/restart`, `network/advertise-addr`, `network/tls`, `thermal`. `leave` aceita
`LeaveRequest { node_id }` opcional e **repassa** para o peer quando é um id
alheio (o painel só fala com o próprio nó).

`GET /api/ui/infer/heads?model=<id>` é a leitura dedicada das cabeças de
inferência, autorizada para os perfis `infer` e `admin`:
retorna somente as máquinas vivas que anunciam uma faixa começando em L0 para
o modelo pedido, além da cabeça escolhida, carga em voo e RTT/idade de cada
candidata. O planejador exige cobertura completa; a cobertura vem primeiro e,
entre faixas equivalentes, um score pondera RTT fresco e carga em voo antes do
desempate por `node_id`. A medição vale cinco minutos.
`candidates[].load` vem do `/api/ui/node` da própria candidata; se essa leitura
não chegar, ele é `null`, não zero.
Sem candidata a resposta é `status: "no_candidate"`; com cabeça mas sem
cobertura completa é `status: "incomplete_pipeline"`. RTT vencido aparece
para diagnóstico, mas não entra na escolha.

`GET /api/ui/deployments` deduplica as cópias persistidas por `deploy_id` e
marca `serving` somente quando um peer vivo anuncia o deployment. O registro
pode continuar visível com `serving: false` para diagnóstico e recuperação,
mas não publica modelo para os clientes de inferência. `?local=true` responde
somente o registro local e não faz fan-out; `/v1/models` usa a lista viva do
gateway, não esse catálogo persistido.

`POST /api/ui/deploy/node/options` consulta o catálogo e a configuração da
máquina indicada em `target_node_id`. Devolve as opções de fatia e de modelo
inteiro disponíveis nessa máquina, além de defaults de GPU/KV e os tipos de
cache aceitos. A tela chama a rota novamente quando o operador troca o
dropdown, evitando reutilizar um plano de outro peer.

`POST /api/ui/deploy/node` (somar uma máquina a uma implantação) e
`POST /api/ui/undeploy` (tirar a implantação inteira do ar) exigem, além da
`ConnectionAuth`, que o `profile` persistido deste nó seja `admin`. A recusa é
403 e o JSON traz `error` com a ação nomeada: somente o administrador pode
adicionar uma máquina ou desimplantar a implantação. A interface esconde os
dois controles para os perfis `node` e `infer`, mas o servidor mantém a
autoridade. `POST /api/ui/deploy/node/remove` é diferente: libera uma única
máquina pelo botão do próprio card e continua disponível para qualquer perfil.
Adicionar ou remover com `deploy_id` anexa uma nova versão com o conjunto de
máquinas resultante. A remoção parcial mantém `serving: true`; a última grava
uma versão vazia, que continua no histórico mas não é uma implantação viva.
O `kind` do registro passa a `mixed` quando uma cabeça de modelo inteiro e uma
cabeça fatiada coexistem no mesmo deployment.

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

`GET /api/ui/requests` exige `ConnectionAuth`, pois devolve prompts e corpos de
requisição. Os filtros são `node_id`, `api_key` (id, rótulo ou nó associado),
`prompt` (busca FTS5), `from`, `to`, `limit` e `offset`; a ordenação é por
`received_at` mais recente. A UI usa `limit` e `offset` para scroll infinito,
sem carregar o histórico inteiro no primeiro pedido. O banco fica em
`pom.db`, em `POM_DB_DIR` quando definido ou em `config::project_root()` por
padrão, é criado de forma idempotente no arranque e mantém índice do prompt e
dos filtros de identidade, nó, estado e horário. WAL e timeout de 5 segundos
ficam como rede de segurança. Dentro do processo, `PomDb` reutiliza uma
conexão configurada no arranque e serializa mutações com um mutex explícito;
isso faz leituras curtas aguardarem a escrita em andamento, sem reabrir o
arquivo nem executar DDL por pedido. O
registro nasce como `in_progress` antes da inferência e a mesma linha recebe
resultado, telemetria, tempos, tokens, peers e erro ao terminar. Reinício ou
mais de 15 minutos sem conclusão marca a linha como `incomplete` com o motivo
visível. A escrita usa uma fila limitada e um escritor separado; falha de disco
ou de enfileiramento é registrada no log do servidor com a operação e o
request id perdido, e não é propagada à requisição de inferência.

`GET /api/ui/requests/stats` exige a mesma `ConnectionAuth` e aceita os filtros
`node_id`, `api_key`, `prompt`, `from` e `to`, mas não pagina. O servidor faz uma
única agregação sobre as linhas filtradas e devolve totais por token, nó, dia e
modelo, consumo por peer (`bytes_sent`, `bytes_received`, `network_ms`), média
de inferência, TTFT médio, fichas de entrada/saída, estados terminais e cache
de prefixo (`input_tokens`, `cached_tokens`, requisições elegíveis e acertos).
Essa separação evita uma varredura pesada no cliente e deixa a lista de linhas
pequena no carregamento inicial.

O registro guarda somente a identidade não secreta da API key: `id`, `label` e
`node_id`. O segredo não entra no SQLite nem na resposta. O corpo salvo é a
representação JSON dos campos aceitos, sem cabeçalhos. A política de retenção de prompts completos ainda
precisa de decisão de produto: `needs-decision: definir retenção e eventual
expurgo por idade ou volume`.

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
`null` ou texto vazio remove o valor - grava `huggingface_token` no `pom.db` com
permissão `0600` no Unix e atualiza o provider em memória, sem reinício.
A precedência efetiva é `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN`, token do
`pom.db` e, por último, o cache (`HF_TOKEN_PATH`, `HF_HOME/token` ou
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

**`GET /api/cluster/logs`** - exige `ConnectionAuth` e devolve
`{ logs_enabled }`. É a fonte autenticada que os nós consultam quando o campo
`cluster_logs_enabled` do anúncio dispara a catraca; não aceita o anúncio como
política.

**`POST /api/ui/network/logs`** - corpo `{ logs_enabled: bool }`, liga ou
desliga a captura e o envio do log de processo do cluster em tempo real. Só o
principal recebe a operação; o efeito é aplicado sem reinício e anunciado como
gatilho para a busca autenticada dos demais nós.

**`GET /api/ui/cluster-logs`** - exige `ConnectionAuth` no principal e devolve
as linhas persistidas em ordem de chegada, com `deploy_id`, timestamp de origem,
nome do nó e contador de descartes. Aceita `deploy_id` e `limit`; não reordena
por `origin_at`.

**`GET/POST /api/ui/thermal`** - exige `ConnectionAuth`. O GET devolve a
política local `{ limit_c, hysteresis_c, cooldown_secs }`; o POST valida que o
limite e a margem são finitos, positivos e formam uma histerese válida, grava a
política no `pom.db` e devolve o valor efetivo. O runtime aplica a alteração
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

No payload de `/api/ui/cluster`, `PeerView.load` é a leitura de requisições em
voo publicada pelo próprio peer. Para o peer local o gateway consulta seu
`LoadTracker`; para peers remotos usa o campo `load` do respectivo
`/api/ui/node`. Se a leitura remota não chega ou não contém um número válido,
o campo é `null`, não zero. O planner de implantação preserva a mesma
distinção em `DeployMachine.load` e na distribuição de carga.

A leitura do `/api/ui/node` de cada peer percorre `PeerInfo::gateway_candidates()`
- uma URL por endereço conhecido, o `addr` primeiro - e alimenta a memória de
falha do `RouteTable`: erro de transporte ou timeout desacreditam aquele
endereço, resposta boa o limpa. Resposta inválida e HTTP de erro **não**
desacreditam, porque quem responde 401 ou 500 está vivo ali. Cada peer traz
`unreachable` (atendeu, a leitura quebrou) e `rediscovering` (todo endereço
conhecido morreu, o nó já pediu o atual à rede) - são estados distintos, nunca
os dois ao mesmo tempo. `rediscovering` expira em 90 s: passado isso o par volta
a `unreachable` dizendo que nenhum dos endereços conhecidos respondeu. `model_inventory[].unreadable_nodes` nomeia os pares de
quem a cobertura depende sem conseguir ler; a cobertura sai do gossip, não da
leitura, então sem essa lista um modelo apareceria "completo" com metade
ilegível. Endereços IPv6 temporários rodam sozinhos e o cluster tem de aguentar
isso sem `--advertise-addr`: ver `docs/subsystems/rede-p2p.md`, "Endereço que
deixou de responder".
**`POST /api/ui/chat/telemetry`** - coleta sob demanda feita depois de uma
execução do Chat. O gateway inclui a medição local e consulta o
`GET /api/ui/network/rtt` autenticado de cada peer solicitado, aceitando o
relatório somente quando `source_node_id` coincide com o peer que respondeu.
Cada item responde `{ from_node_id, to_node_id, rtt_ms, transport }`. A medida
é da origem da aresta até o destino e não é o RTT do card que um gateway mediu
até um peer. Ausência de uma medida fica ausente na resposta; não há zero de
fallback.

**`GET/POST/DELETE /api/ui/chat/history`** - exige `ConnectionAuth` e guarda
sessões do Chat no `pom.db`, não no sistema de arquivos nem em um SQLite
separado. O POST recebe uma sessão completa em camelCase (`id`, `title`,
`model`, `modelAtStart`, `messages`, `updatedAt`) e a substitui atomicamente;
mensagens preservam conteúdo visível, conteúdo enviado, eventos, erros,
telemetria e trace. O GET devolve as sessões por atualização mais recente. O
DELETE sem id limpa todas as sessões e `DELETE /api/ui/chat/history/:id` remove
uma sessão. O conteúdo só é devolvido a um operador autenticado e nunca é
incluído em logs de erro.

## 3. Autenticação (`auth.rs`)

Há uma credencial de operador, uma credencial de inferência e um extractor
privado para sincronização entre nós:

- **`ConnectionAuth`** - lê só `authorization` (prefixo `Bearer` opcional) e
  compara com `connection_key`. É a credencial de operador: cobre `/admin/*`,
  `/api/ui/*` (mutáveis), `/api/transfer/*` e `/api/ui/deploy/*`. Sem header =
  401 `missing Authorization header`; chave errada = 403.
- **`ApiKeyAuth(pub Caller)`** - aceita `authorization` **ou** `x-api-key`
  (o cliente escolhe conforme a env var que tem: `OPENAI_API_KEY` manda
  `Bearer`, `ANTHROPIC_API_KEY` manda `x-api-key`). Só uma API key ativa
  mintada em `/admin/api-keys` autentica inferência, com `id`, `label` e
  `node_id`. O `Caller` construído aqui é o único lugar que vê a credencial e
  alimenta a auditoria com essa identidade; o segredo nunca é salvo nem
  devolvido pela rota de auditoria.
- **`InternalNodeAuth`** - exige a `connection_key` compartilhada mais o
  marcador `x-pom-internal-node: 1`; cobre a sincronização autenticada de API
  keys no principal e as chamadas entre peers na porta de inferência. Nunca é
  aceito nas rotas de inferência do cliente.

`POST /admin/regenerate-key` troca a `connection_key` (SHA256 de um UUID);
mintar chaves **acumula** em vez de substituir - um token por nó/harness, e o
log atribui cada geração a quem pagou por ela.

**Rotas sem auth**: `/peers`, `/health`, `/handshake` (a chave vai no corpo), os GETs de leitura do
`/api/ui` (`node`, `models`, `cluster`, `cluster/models`,
  `peer/:node_id/runtime`, `paths` GET, `jobs`, `jobs/:id`). `GET /api/ui/requests` e
`/api/ui/requests/stats` e `/api/ui/cluster-logs` são exceções porque podem
conter conteúdo sensível e
exigem `ConnectionAuth`. Os `/v1` OpenAI/Anthropic **exigem**
`ApiKeyAuth`. Uma chave desconhecida localmente consulta o principal pelo canal
interno autenticado; cada tentativa tem timeout de 2 s e há no máximo três
tentativas com backoff. Não há cache negativo: timeout, erro ou resposta
inválida recusam o pedido. Uma chave remota válida é armazenada no `pom.db` e
fica disponível nas próximas requisições até a revogação.

`POST /api/ui/chat/telemetry` fica fora da exceção de leitura: ele exige
`ConnectionAuth` porque repassa relatórios internos de RTT entre peers.

A porta interna de inferência (`inference_port`, normalmente 8181) tem outro
router. `/v1/prefill`, `/v1/step`, `/v1/step-stream`, `/v1/link`,
`/v1/session/release`, `/v1/session/rollback`, `/v1/session/slot` e
`/v1/tp/reduce` exigem `Authorization: Bearer <connection_key>` mais
`x-pom-internal-node: 1`; API keys de cliente são recusadas. `/health` continua
público para sondas de readiness.

O identificador `x-pom-inference-session` continua sendo apenas contexto opaco
de sessão. Ele não é credencial, não guarda API key e não substitui a
`connection_key` do canal interno.

`/v1/link` é a ligação persistente entre andares da pipeline: **um** POST cujo
corpo de ida é um fluxo de quadros `frame/v1-batch` e cujo corpo de volta é
outro fluxo, correlacionados pelo `batch_id`. A credencial é conferida na
abertura pelo mesmo middleware das demais rotas `/v1` **e reconferida a cada
quadro**, para que uma revogação feita por outro processo valha na hora e não
só quando o canal cair. O limite de corpo é desligado nessa rota porque o corpo
é a geração inteira, não um quadro - cada quadro dentro do fluxo continua
limitado pelo enquadramento do canal. Ver `inferencia.md` §2.1.

Entre peers, um lote de inferência desce esta escada de transporte, e só desce
um degrau depois de o anterior falhar por **transporte**: canal persistente
(`/v1/link`, quando o hop o anuncia), túnel libp2p/Noise, cliente direto
pinado. Uma recusa HTTP do peer é final em qualquer degrau e não é repetida por
outra via.

### `relay-result` e `remote-stream-result` usam Ticket

Essas rotas não usam `ConnectionAuth`, porque o hop que responde é um **peer de
inferência**. O remetente carrega no corpo o **ticket**: um UUID v4 sorteado por
quadro, conhecido só pela própria cadeia, removido do `ReplyInbox` no `Drop` em
qualquer caminho de saída. Não há API key HTTP nessas rotas. Uma resposta
atrasada leva `410` e nunca pode cair no token seguinte. O callback remoto mantém
essa mesma regra, mas entrega vários lotes ao `StreamTicket` limitado em vez de
resolver um canal oneshot.

## 4. `x_pom.source` - a prova de execução distribuída

A mesma string que o log de requisições guarda como `source`: `"pipeline:<id1>→<id2>"`
(com `" (partial)"` quando um peer falhou no meio mas sobrou texto),
`"local"` ou `"peer:<node_id>"`. Aparece como campo `source` no
`/api/generate`, como extensão `x_pom` no `/v1` não-streaming e no último
chunk do SSE, e como `source` no `reqlog`. `"none"` marca erro/abandono. É o
único jeito de um harness saber que a inferência foi conjunta: `source`
começando com `pipeline:` e com **dois** hops é a prova.

`x_pom.timings` é extensão própria, tanto no último chunk do SSE quanto nas
respostas não-streaming. O formato compatível com OpenAI continua intacto:
clientes que ignoram `x_pom` continuam lendo `usage`, `choices` e os demais
campos padrão. `x_pom.trace` carrega o mesmo `RequestTrace`, incluindo os
spans locais, `trace_id`, estado cold/warm, `outside_node_total_ms`,
`outside_node_accounted_ms`, `outside_node_unaccounted_ms`, contadores de
conexão, marcos do primeiro token, métricas de streaming e os quatro campos
de aceitação MTP por requisição. `x_pom.timings` separa TTFT legado,
`server_ttft_ms`, `engine_ttft_ms` e `client_ttft_ms`; campo ausente significa
que a fronteira não foi medida, não zero. A auditoria em
`GET /api/ui/requests` preserva esse mesmo objeto em `telemetry`, ao lado de
`timings` de geração.

`x_pom.finish_reason: "cycle"` e `x_pom.partial: true` identificam uma parada
por repetição nos adaptadores OpenAI e Anthropic, no chunk final e na resposta
não-streaming. O motivo padrão do provedor continua compatível (`stop` na
OpenAI e `end_turn` na Anthropic); clientes POM-aware devem ler a extensão para
distinguir ciclo de uma resposta normal. O gateway já transmitiu a ficha que
confirmou o ciclo, não pede outra ficha, libera a sessão e fecha o fluxo.

## 5. Testes de rota - o andaime é um só (`gateway::testkit`)

`gateway::testkit` (`#[cfg(test)]` em `gateway.rs`) monta um `GatewayState`
inteiro com a tabela de peers semeada à mão: `peer(node_id, hostname,
gateway_port)` constrói um `PeerInfo` de loopback, `state_with(node_id, peers)`
monta o estado (com um `Network` real em porta UDP efêmera, mas **nada fala
UDP** - um teste nunca anuncia na LAN nem cai no cluster de verdade), e
`state_with_secondary_admin(node_id, peers)` monta um administrador secundário
de verdade: outro nó é o principal e o nó local recebe uma concessão
`Secondary` assinada. `body_of(response)` extrai o JSON da resposta. É o único
jeito de testar rota de controle sem transmissão; `transfer`, `deploy` e
`webui` o usam. Para subir um nó de verdade sem sair anunciando, o equivalente
em produção é `--no-discovery`.

## 6. Defeitos anotados

- **DEFEITO ANOTADO: `/admin/transfer-offers` é um stub que sempre retorna
  vazio** (`admin.rs`, `transfer_offers` hardcoda `{ "offers": [] }`). As
  ofertas reais vivem em `GET /api/transfer/offers` (`transfer.rs::list_offers`);
  a rota `/admin` existe mas é morta, então qualquer consumidor dela nunca vê
  uma oferta.
- **DEFEITO ANOTADO (exposição): os GETs de leitura do `/api/ui` são abertos** -
  `node`, `models`, `cluster`, `cluster/models`, `peer/:node_id/runtime`,
  `paths` (GET), `jobs` e `jobs/:id` não têm `ConnectionAuth` (só os mutáveis exigem). `GET
  /api/ui/models` devolve os caminhos locais completos dos modelos, e
  `/api/ui/cluster` a tabela de peers, para qualquer cliente da rede sem
  credencial. Aceitável em LAN confiável, exposto fora dela não.
