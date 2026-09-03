# routing/ - planejador do pipeline de inferência

Único arquivo por ora. Decide uma cadeia ordenada de peers que cobre
`0..total_layers` por sub-faixas executáveis, mede a carga em voo e o RTT de
entrada de cada peer e escolhe o endereço por hop. É a entrada de nível mais
alto consumida pelo `gateway.rs` (`generate_stream_with_stops`).

## Arquivos

- `pipeline.rs` - `LoadTracker`/`LoadGuard` (contagem RAII de requisições em
  voo por peer), `PipelineStep`/`PipelinePlan`/`PipelineEdge`, `plan_pipeline`
  (greedy) e `resolve_step_url` (endereço por hop, por sessão). Cada
  `PipelineStep` preserva também os candidatos UDP do hop para o túnel libp2p.

## Funções-chave

- `plan_pipeline_with_rtt` - varre de 0 até `total_layers`; em cada posição
  escolhe somente um peer cuja faixa carregada comece exatamente na posição.
  Isso garante que `exec_start` não peça entrada no meio de uma fatia. O score
  é `camadas_executadas * 100_000 - penalidade_de_rtt - carga * 10`, portanto o
  RTT escolhe entre ramos equivalentes e a cobertura continua dominante.
  Empates finais usam `node_id` para manter o plano determinístico. **Exclui
  workers de tensor parallelism** (`PeerInfo::is_tp_worker`): só o primary de
  um grupo entra na cadeia. Devolve `None` no primeiro buraco executável.
- `plan_pipeline` - chama a variante acima sem medições de RTT.
- `PipelineStep::exec_start`/`exec_end` - faixa que o hop executa, distinta da
  faixa que ele carregou; `PipelineEdge` só existe quando essas sub-faixas são
  contíguas.
- `LoadTracker::acquire` - incrementa o contador e devolve `LoadGuard`, que
  decrementa no drop (RAII): o balanço de carga nunca fica preso num erro.
- `PipelineStep::frame_encoding` - retorna o marcador `F16`; todos os bytes
  passam pelo frame inicial `inference/frame-v1-batch`.
- `CAP_FRAME_BATCH` - capability `inference/frame-v1-batch`; o relay só envia
  depois que o peer anuncia o servidor carregado. Não há rolling upgrade para
  formatos anteriores nesta primeira versão do produto.
- `CAP_LINK_STREAM` - capability `transport/link-v1`, verificada **por hop**
  como a do túnel, não all-or-nothing: o chamador abre o canal persistente
  quando o hop seguinte a anuncia, e qualquer falha de transporte desce para a
  escada de sempre sem encerrar a geração. Ver `inference/link.rs`.
- `CAP_FRAME_DTYPE_F8` e `PipelinePlan::validate_wire_dtype` - capability
  `inference/frame-v1-dtype-f8` e validação all-or-nothing da precisão f8 no
  plano inteiro; uma ausência identificada bloqueia o deploy ou a requisição
  com a razão do peer, sem deixar bytes serem interpretados como outro dtype.
- `CAP_REMOTE_TOKEN_STREAM` - capability anunciada pelo servidor de inferência
  para que um head remoto mantenha o laço de decode no peer e entregue tokens
  ao gateway em callbacks limitados por lotes oportunistas. O gateway exige
  também um endereço de callback; o atalho local nunca usa este transporte.
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
- `PeerInfo::runtime_capacity` não participa da escolha greedy de camadas: o
  gateway agrega essa capacidade antes de validar a requisição, mantendo o
  `max_new_tokens` por perna separado do orçamento total derivado do contexto.
- `PipelinePlan::hop_chain` - "abc123→def456", a prova de execução distribuída
  que o gateway põe em `source`.

## Ligações

- **Chamado por**: `gateway.rs::generate_stream_with_stops` (plano por requisição) e
  `backend/webui.rs`.
- **Chama**: `network::PeerInfo` e `network::route::RouteTable`
  (`resolve_step_url`).
- **Consumido por**: `inference/relay.rs` e `inference/infer_server.rs` (as
  capabilities `CAP_REMOTE_TOKEN_STREAM`/`CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
  `CAP_SPECULATE_LOOKUP`/
  `CAP_SPECULATE_MTP`/`CAP_SPECULATE_MTP_SLICED` viajam em `next_hops` e no
  wire format); `inference/direct.rs` (o modo direto
  depende de `supports_direct_reply`); `inference/wire.rs` (dtype/codificação
  do tensor).
- `network.rs::self_announce` pede sincronia com as constants de capability aqui.

## Defeitos anotados

Nenhum.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
