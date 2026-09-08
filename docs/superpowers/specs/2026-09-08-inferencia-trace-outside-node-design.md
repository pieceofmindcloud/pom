# Trace de inferência e decomposição de outside-node - design

## Contexto e mapa verificado

O pedido parte de uma execução real: pipeline `5b035803`, um hop L0-L40,
E2E de 3134 ms, processamento no nó de 1179 ms e `outside-node` de 1955 ms.
O servidor é Rust com Tokio. O cliente é React/TypeScript. Não há Redis no
runtime deste repositório, e o relatório citado em
`data/rdma-vermelho-spec-orcamento/report.md` não está presente neste checkout.

O caminho atual é:

| Fronteira | Código atual | Telemetria existente |
|---|---|---|
| navegador -> gateway | `Chat.tsx`, `streamSse`, handlers OpenAI/Anthropic e `/api/generate` | `performance.now()` ainda não é usado para o cliente; SSE carrega `x_pom.trace` no evento final |
| ingresso e execução | `gateway/generation.rs`, `spawn_logged_generation_with_stops` | UUID da auditoria, `Instant`, auditoria SQLite e `GenerationTimings` |
| seleção e rota | `ready_for_pipeline`, `plan_pipeline_with_rtt`, `resolve_step_url` | plano, RTT snapshot e avisos da rota |
| conexão e despacho | `inference/relay.rs`, `network/peer_client.rs`, `inference/link.rs` | RTT, encode/decode HTTP, headers/body, TLS e transporte; `reqwest::Client` por peer mantém pool |
| handler do peer | `infer_server/prefill.rs` e `step.rs` | `NodeTimings`: fila, lock, setup, decode, sampling, encode e processamento |
| motor | `infer_server/step_blocking.rs` e `LlmEngine` | contadores de MTP e `decode_us`; MTP e lookup podem produzir mais de um token por chamada |
| retorno e streaming | `generation.rs`, `openai.rs`, `messages.rs` | `GenEvent`, `RemoteDecodeStream`, chunks SSE e `[DONE]`; não há marcos explícitos de primeiro token |
| apresentação | `ExecutionTelemetry.tsx`, `telemetryAnalysis.ts` | total, TTFT, prefill, processamento, `outside-node` heurístico e RTT agregado |

`RequestTrace` e `PhaseTrace` são a extensão correta da telemetria atual. A
auditoria UUID é o identificador de correlação já existente e será reutilizada
como `trace_id`; não será criado um ID paralelo.

## Objetivos

Uma execução deve permitir responder, sem inventar tempo, quanto do
`outside-node` foi atribuído a pipeline/rota, conexão, despacho, retorno do
primeiro token, streaming e finalização, e quanto continua desconhecido. A
mesma execução deve separar `engine_ttft_ms`, `server_ttft_ms` e
`client_ttft_ms`, além dos tempos de transporte.

O contrato preserva `outside-node`, `ttft_ms`, `processing_ms`, `network_ms`,
o formato de resposta e o frame binário de estado oculto. Os novos campos são
opcionais ou aditivos para que um peer sem a instrumentação continue sendo
representado como dado desconhecido.

## Modelo de trace

`RequestTrace` ganha:

- `trace_id`, `request_state` (`cold`, `warm` ou `unknown`) e host de medição;
- spans locais com nome, pai, início em wall clock apenas para correlação,
  duração medida por `Instant`, e grupo paralelo quando aplicável;
- marcos opcionais do primeiro token: motor produziu, nó serializou, nó
  enviou, coordinator recebeu, gateway emitiu e cliente recebeu;
- resumo de `outside_node_total_ms`,
  `outside_node_accounted_ms` e `outside_node_unaccounted_ms`;
- resumo TTFT com os componentes observados;
- `decode_calls`, `generated_tokens` e `tokens_per_decode_call`, além dos
  contadores MTP já existentes;
- métricas de streaming: primeiro chunk, primeiro token, duração, último
  token até o fim, quantidade/tamanho de chunks quando disponíveis;
- estado de conexão por peer em nível de pool (`new`, `reused` ou `unknown`) e
  tempos de resposta já medidos.

O `trace_id`, `span_id`, `parent_span_id` e wall timestamp acompanham os
metadados JSON de requisições e respostas. O conteúdo binário do tensor não
será alterado. Cada máquina calcula durações somente com o próprio
`Instant`. Wall timestamps servem para ordenar e correlacionar logs, nunca
para subtrair relógios de máquinas diferentes. RTT continua sendo medido no
host que envia e recebe.

## Spans e cálculo

Os spans refletem fronteiras que existem no código:

`ingress`, `validation`, `orchestration`, `pipeline_selection`,
`route_resolution`, `connection_acquire`, `dispatch`, `node_handler`,
`node_queue`, `engine`, `first_token_return`, `streaming` e `finalization`.

O span `node_handler` inclui o tempo local do peer e seus filhos de fila,
setup, prefill/decode e sampling. `connection_acquire` começa na primeira
tentativa de saída para um peer e registra o estado do pool. Em uma conexão
nova, tempos de DNS/TCP/TLS só aparecem quando o cliente HTTP os consegue
observar; o tempo atual de headers não será rebatizado como handshake. Quando
essa distinção não for possível, o campo fica desconhecido.

`outside_node_total_ms` mantém a semântica compatível: E2E do servidor menos
o caminho crítico de processamento local dos nós observados. Os componentes
contabilizados são a união dos intervalos do caminho crítico do gateway, não a
soma de spans inclusivos. Spans de hops paralelos permanecem individuais e
recebem grupo paralelo. Assim:

```text
outside_node_unaccounted_ms = max(
  0,
  outside_node_total_ms - outside_node_accounted_ms
)
```

O valor nunca é reduzido para fazer a soma fechar. Se faltarem marcos ou um
peer antigo não devolver trace detalhado, a UI mostra explicitamente que uma
quantidade não pôde ser atribuída.

## TTFT e primeiro token

`server_ttft_ms` começa no ingresso do servidor e termina no primeiro evento
de token que o gateway envia ao stream. `engine_ttft_ms` é local ao peer e
começa no início do trabalho do motor. `client_ttft_ms` é medido pelo
navegador com `performance.now()` desde o início do `fetch` até a chegada do
primeiro chunk/token. Nenhuma destas medidas usa wall clock para duração.

Os marcos no retorno são opcionais e monotônicos dentro de cada host:

```text
engine produced -> node serialized -> node sent -> coordinator received
-> gateway stream flush -> client received/rendered
```

No SSE, cada chunk continua sendo enviado com a mesma forma de dados. O
cliente somente acrescenta as medições locais ao snapshot da mensagem. O
primeiro token não é inferido a partir de `prefill_ms` ou de um zero de
`network_ms`.

## Espera assíncrona, conexão e estado cold/warm

O equivalente útil ao atraso de event loop é medido nas fronteiras de fila e
agendamento do Tokio: espera de `LoadTracker`, espera do scheduler local,
espera de lock e atrasos de retry/timer que já pertencem ao transporte. Não
será instalado um profiler de cada future nem uma dependência pesada.

`PeerClients` continua sendo o dono dos clientes reqwest e dos pools. A
instrumentação distingue reutilização do cliente/pool da reutilização de um
socket individual, que reqwest não expõe de forma confiável nesta camada.
Socket/DNS/TCP/TLS ficam `unknown` quando não houver observação direta. O
trace registra cold/warm para modelo carregado, sessão/prefixo, pipeline,
peer conhecido e pool de conexão quando o estado estiver disponível.

O nível `POM_PROFILE=off|basic|trace` controla o custo da instrumentação:

- `off`: mantém os campos compatíveis essenciais e não coleta spans
  detalhados;
- `basic`: coleta resumo, marcos do primeiro token, fila, conexão e contadores;
- `trace`: coleta spans hierárquicos e detalhes de chunks/transportes.

`POM_PROFILE_SYNC` continua sendo a configuração independente que governa a
sincronização do backend do motor. O estado dela continua vindo do health
real, sem dedução por tempos zerados.

## Relatório visual

`ExecutionTelemetry` passa a apresentar, fechado por padrão, E2E, TTFT,
processamento no nó, `outside-node`, accounted e unaccounted. A árvore mostra
os spans observados e marca o maior componente como principal gargalo somente
quando há dados; caso contrário diz que não foi possível atribuir o intervalo.
Os dados de uma mensagem histórica ficam presos ao snapshot daquela execução.

## Regressões e testes

Os testes cobrem trace ID estável, fechamento em sucesso/erro/cancelamento,
timeout, retry, fallback, desconexão, pipeline de um e vários nós, spans
paralelos, cálculo não negativo e compatibilidade com traces antigos. Também
cobrem a concatenação dos fragmentos de chamada de ferramenta, contadores
MTP/decode e a medição de `performance.now()` no cliente.

## Fora de escopo

- plataforma de observabilidade, exporter OpenTelemetry ou Redis;
- sincronização NTP obrigatória;
- alteração de comportamento da rota, do planner, do modelo, da quantização,
  do offload, do `FIT_MAX_UBATCH` ou do frame binário;
- otimização de chunking, keep-alive ou transporte antes de haver evidência;
- métricas falsas para preencher lacunas ou soma ingênua de spans paralelos;
- alteração de arquivos gerados ou do `llama.cpp` vendorizado.
