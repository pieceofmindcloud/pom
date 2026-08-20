# routing/ - planejador do pipeline de inferência

Único arquivo por ora. Decide a cadeia ordenada de peers que cobre
`0..total_layers` de um modelo, mede a carga em voo de cada peer e escolhe o
endereço por hop. É a entrada de nível mais alto consumida pelo `gateway.rs`
(`generate_stream`).

## Arquivos

- `pipeline.rs` - `LoadTracker`/`LoadGuard` (contagem RAII de requisições em
  voo por peer), `PipelineStep`/`PipelinePlan`/`PipelineEdge`, `plan_pipeline`
  (greedy) e `resolve_step_url` (endereço por hop, por sessão).

## Funções-chave

- `plan_pipeline` - varre de 0 até `total_layers`; em cada posição escolhe o
  peer vivo com `inference_port` e `model` iguais que cubra a posição, com
  score `layers_count * 100 - current_load * 10`. **Exclui workers de tensor
  parallelism** (`PeerInfo::is_tp_worker`): só o primary de um grupo entra na
  cadeia. Devolve `None` no primeiro buraco de cobertura - aí o gateway cai
  para Ollama local / peer.
- `LoadTracker::acquire` - incrementa o contador e devolve `LoadGuard`, que
  decrementa no drop (RAII): o balanço de carga nunca fica preso num erro.
- `PipelineStep::frame_encoding` - retorna o marcador `F16`; todos os bytes
  passam pelo frame inicial `inference/frame-v1-batch`.
- `CAP_FRAME_BATCH` - capability `inference/frame-v1-batch`; o relay só envia
  depois que o peer anuncia o servidor carregado. Não há rolling upgrade para
  formatos anteriores nesta primeira versão do produto.
- `PipelinePlan::supports_relay_chain` / `supports_direct_reply` /
  `supports_speculation` - all-or-nothing: um passo sem a capability derruba a
  feature do plano inteiro (`CAP_RELAY_CHAIN`, `CAP_RELAY_DIRECT`,
  `CAP_SPECULATE_LOOKUP`), em vez de misturar modos no meio da cadeia.
- `PipelinePlan::supports_mtp` - a exceção à regra all-or-nothing, e de
  propósito: a **cauda** precisa de `CAP_SPECULATE_MTP` (é ela quem tem o bloco
  `nextn`, na última camada do modelo, e quem rascunha - o draft sai na própria
  resposta direta, `next_draft`), **todos** precisam de `CAP_SPECULATE_LOOKUP`
  e de `CAP_SPECULATE_MTP_SLICED` (o protocolo novo - `tokens`/`mtp_k`/
  `next_draft` na wire - só existe em build novo; um peer velho no anel
  receberia um `speculate` que não entende). Ver `inference/mtp.rs`.
- `supports_direct_reply` - exige **todos** os passos com `CAP_RELAY_DIRECT`
  (`relay/v2-direct`, estritamente mais forte que `relay/v1-chain`: quem anuncia
  um também anuncia o outro) **e** pelo menos 2 hops - um plano de hop único já
  é o mínimo, e desviar a resposta só somaria um callback às mesmas duas pernas.
  É o que deixa o gateway despachar cada quadro **uma vez** (o modo direto de
  `inference/direct.rs`) em vez de borbulhar no anel.
- `resolve_step_url` - ordem: 1) rota medida na `RouteTable` (se ainda for
  candidato), 2) `inference_url` (endereço observado, o único comprovado antes
  da primeira sonda), 3) primeiro candidato. Fixa a escolha por sessão - o KV
  cache do outro lado é por sessão, e trocar de endereço no meio é pior que a
  rota lenta.
- `routing_request_from_peers` - traduz `PeerInfo` para o `RoutingRequest` do
  `routing-contract` (rota futura de planejador; produção ainda usa
  `plan_pipeline` até cobertura de equivalência).
- `PipelinePlan::hop_chain` - "abc123→def456", a prova de execução distribuída
  que o gateway põe em `source`.

## Ligações

- **Chamado por**: `gateway.rs::generate_stream` (plano por requisição) e
  `backend/webui.rs`.
- **Chama**: `network::PeerInfo` e `network::route::RouteTable`
  (`resolve_step_url`).
- **Consumido por**: `inference/relay.rs` e `inference/infer_server.rs` (as
  capabilities `CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/`CAP_SPECULATE_LOOKUP`/
  `CAP_SPECULATE_MTP`/`CAP_SPECULATE_MTP_SLICED` viajam em `next_hops` e no
  wire format); `inference/direct.rs` (o modo direto
  depende de `supports_direct_reply`); `inference/wire.rs` (dtype/codificação
  do tensor).
- `network.rs::self_announce` pede sincronia com as constants de capability aqui.

## Defeitos anotados

Nenhum.
