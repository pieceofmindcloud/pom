# Relatório técnico - telemetria do pipeline distribuído

Data: 2026-09-09

Status: instrumentação e correção implementadas e validadas em nível unitário,
de laboratório e frontend. A captura E2E com Ornith e dois peers continua
bloqueada pelo ambiente local sem modelo servido; portanto não há ganho de
tok/s medido neste worktree. O executor MoE foi ignorado, conforme a spec.

## Resumo executivo

Foram comprovadas quatro causas relacionadas:

1. `outside-node` era calculado na UI como `total_ms - soma(processing_ms)`.
   Isso misturava wall-clock E2E com soma de spans de peers diferentes e tratava
   tempos sobrepostos como uma partição exclusiva.
2. O segundo peer não aparecia no decode porque `hop_timings` era produzido
   pelo relay, mas descartado pelo `RemoteStreamToken`, não serializado no
   callback e nunca lido por `RemoteDecodeStream::next`. A perda era
   determinística, não uma chegada tardia ou correlação errada.
3. O fechamento do `ReplyInbox` removia o ticket ao entregar a resposta.
   Recibos de transporte que chegavam depois eram recusados com `410`, sem
   fase de reconciliação.
4. Não existia instrumentação agregada de abertura, lifetime, frames, bytes,
   primeira entrega e espera do `remote-stream`; portanto não era possível
   separar custo de callback, espera e processamento remoto.
5. No decode direto, `await_result_with_ack` cancelava o ACK da cabeça quando a
   resposta da cauda vencia o `select`; isso também eliminava a evidência do
   primeiro hop justamente no caso de resposta distribuída mais rápida.

A implementação agora preserva os hops downstream, separa
`response_complete` de `telemetry.status == complete`, agrega histogramas
limitados, deduplica frames/recibos e somente apresenta overlap ou residual
verdadeiro quando há intervalos que os medem. Nenhuma decomposição nova é
obtida por subtração do E2E.

## Reprodução e limites da evidência

- `pwd -P` e `git rev-parse --show-toplevel` apontaram para o worktree
  isolado `/Users/jairpereira/.treehouse/rdma-p2p-242c70/7/rdma-p2p`.
- A UI em `http://localhost:8080/admin-ui/chat` mostrou `The network is not
  serving any model`. Não há, portanto, número local honesto para Ornith,
  dois peers ou comparação antes/depois de throughput.
- Os números de produção abaixo são os fornecidos pela spec do captain. Eles
  orientaram a investigação, mas não são uma captura produzida por este
  worktree.

## 1. Origem de `outside-node`, `unaccounted` e `node processing`

Antes da alteração, `apps/frontend/src/lib/telemetryAnalysis.ts` fazia:

```ts
const processing = total(rows, (phase) => phase.processing_ms);
const outsideNode = timings == null ? 0 : Math.max(0, timings.total_ms - processing);
```

Quando a diferença passava o limiar, o diagnóstico retornava
`kind: "outside-node"`. Portanto, sim, era E2E menos a soma acumulada de
`processing_ms` dos hops.

Não existe um cálculo Rust separado chamado `unaccounted`. O restante não
nomeado dentro de `processing_ms` aparecia na UI como `unattributed`; esse é
um residual local da decomposição do nó, não uma prova de tempo fora do nó. O
agregado de processamento fica em `PhaseTrace::record` e
`PhaseTrace::merge`, em `crates/node/src/inference/prefill/support.rs`.

O defeito é conceitual: `processing_ms`, RTT, callbacks e peers distintos são
somatórios de evidências, enquanto `total_ms` é wall-clock. Um pipeline pode
ter processamento e transporte sobrepostos. A diferença não identifica qual
intervalo ocorreu e não pode ser apresentada como decomposição exclusiva.

## 2. Por que o segundo peer aparecia com `decode = 0`

A cadeia antiga funcionava assim:

1. `handle_step` media o hop local e `relay_and_bubble` preservava
   `hop_timings` downstream.
2. `remote_stream_producer` recebia esse metadata ao chamar o step local.
3. `RemoteStreamToken` guardava apenas `processing_ms` e `node_timings`.
4. `remote_stream_batch` serializava apenas esses campos.
5. `RemoteDecodeStream::next` criava uma entrada somente para o hop 0 e não
   percorria `meta["hop_timings"]`.

Logo, os spans do segundo peer eram gerados no relay, chegavam ao produtor
remoto e eram descartados na fronteira do `remote-stream`. Não eram spans
correlacionados a outro trace, nem spans que simplesmente chegavam tarde. O
resultado observável era `decode = 0` para o peer downstream.

Agora cada callback preserva a lista ordenada downstream, cada entrada leva
`span_count` para distinguir tokens de callbacks, e o gateway registra o
head mais todos os hops recebidos. A identidade causal leva `trace_id`,
`request_id`, `pipeline_id`, `stream_id`, frame index e IDs de span no modo
de profiling.

## 3. `remote-stream` versus `libp2p-tunnel`

A diferença foi confirmada no código. O caminho anel/direto via
`libp2p-tunnel` já mantinha `SentFrame` e `hop_timings`, e o gateway
iterava a lista retornada. O `remote-stream` mantinha apenas o head por lote
e não registrava frame, lifetime, espera, primeira entrega ou custo remoto
agregado.

O caminho novo não transforma o callback em RTT de inferência. O primeiro
control frame registra o transporte que abriu o stream. Os callbacks registram
localmente lifetime, intervalo entre entregas, espera de fila, bytes e
processamento remoto, sem subtrair relógios de máquinas diferentes.

## 4. Recibos tardios, correlação e perda

Antes, `ReplyInbox::deliver` removia o `ReplyState` imediatamente e
`record_transport` só funcionava enquanto o ticket existisse. O teste antigo
`a_result_does_not_wait_but_late_transport_receipts_close_the_trace` documentava que recibos
posteriores eram rejeitados.

O `reply_to` usa um UUID por quadro. Um ticket desconhecido recebe `410`, e
não pode cair no token seguinte. Agora o estado entregue fica retido por 30 s,
recibos fora de ordem atualizam `trace_snapshot`, recibos duplicados por hop
são recusados e a resposta ao cliente continua sem esperar a reconciliação.
Após a expiração, o descarte continua explícito e não vira tempo desconhecido
retroativamente.

Além disso, quando o resultado vence o `select` de `await_result_with_ack`, o
ACK da cabeça continua em uma tarefa destacada e registra o transporte assim
que termina. O teste `a_result_first_keeps_the_head_ack_for_transport_reconciliation`
comprova essa ordem sem fazer o cliente esperar.

## 5. Regressão em função dos tokens

Os dois pontos do caso real são:

| Tokens | E2E | Node processing | `unaccounted` histórico |
|---:|---:|---:|---:|
| 11 | 1.232 s | 545 ms | 662 ms |
| 192 | 8.807 s | 1.124 ms | 7.653 ms |

O ajuste desses dois pontos é:

```text
per_token = (7653 - 662) / (192 - 11) = 38,624 ms/token
fixed = 662 - 38,624 * 11 = 237,1 ms
R2 = 1,000
```

O `R2 = 1` é degenerado: dois pontos sempre determinam uma reta. O resultado
sugere custo incremental por token, mas não separa callback, transporte,
espera, compute remoto ou overlap. Não foi usado para alterar o runtime.

Foi adicionada a função de análise `fitTokenLatencyRegression` e um teste com
os tamanhos obrigatórios 1, 10, 50, 100, 200 e 500. O teste usa uma série
sintética exata de `237 + 38,6 * tokens`, retorna seis amostras e `R2 = 1`.
Isso valida a ferramenta de cálculo, não substitui uma captura E2E real.

## 6. Modelo de telemetria antes e depois

| Aspecto | Antes | Depois |
|---|---|---|
| Gap E2E | `total - soma(processing)` classificado como `outside-node` | Não é calculado como decomposição; o gap fica sem classificação enquanto não houver evidência |
| Hops remotos | Head do callback somente | Head e todos os `hop_timings` downstream, com `span_count` |
| Fechamento | Resultado e trace terminavam juntos | `response_complete` separado de `collecting`, `response_complete` e `complete`; ACK tardio também reconcilia o head |
| Transporte | Recibo tardio era descartado | Retenção curta, ordem arbitrária, deduplicação e snapshot atualizado |
| Remote stream | Sem métricas de frame/lifetime/espera | Contadores e histogramas agregados: open, lifetime, frames, bytes, primeira entrega, latências min/p50/p95/p99/máximo, compute min/p50/p95/p99/máximo e espera total/p50/p95 |
| TTFT | Um campo ambíguo | `local_engine_first_event_ms`, `pipeline_first_token_ms`, `server_ttft_ms` e `client_ttft_ms` |
| Overlap | Implícito na diferença E2E | `overlap_ms` e `true_residual_ms` nulos até haver intervalos suficientes |
| Profiling | Sem correlação detalhada do stream | `POM_PROFILE_SYNC=1` habilita spans de frame e eventos por token limitados |

`critical_path_ms` no resumo é o wall-clock medido da requisição. Ele não é
uma reconstrução causal completa enquanto não houver timestamps sincronizáveis
entre peers. Os spans detalhados usam domínio monotônico do peer produtor.

## 7. Integridade e estados

`TelemetrySummary` publica `expected_peers`, `reported_peers`,
`missing_peers`, `expected_decode_spans`, `reported_decode_spans`,
`completeness_percent`, `duplicate_spans`, `late_spans`,
`orphan_spans`, `wall_clock_ms`, compute acumulado, transporte acumulado e
os agregados do stream.

O cálculo limita completude a 100%. Um peer sem decode não é considerado
coberto só porque fez prefill: quando há rastreamento de decode, a cobertura de
peer usa `decode.reported_spans`. Assim, o caso do segundo peer ausente fica
explicitamente pendente.

## 8. Arquivos alterados

### Node

- `crates/node/src/inference/prefill/support.rs`: contrato serializável,
  agregados, histogramas, deduplicação, estados de coleta e profiling.
- `crates/node/src/inference/prefill/step.rs`: abertura do stream, callback do
  head, `hop_timings` downstream e control transport.
- `crates/node/src/inference/infer_server/step.rs`: preservação e agregação
  remota, identidade causal e métricas de frame/lifetime/espera.
- `crates/node/src/inference/direct.rs`: recibos tardios, retenção,
  deduplicação de frames e snapshot do trace.
- `crates/node/src/backend/gateway/budget.rs`: TTFT semântico e atualização do
  resumo de telemetria antes da serialização de `x_pom`.

### Frontend

- `apps/frontend/src/lib/chatStream.ts` e `types.ts`: tipos de telemetria,
  spans, agregados e TTFT do cliente.
- `apps/frontend/src/routes/Chat.tsx`: medição de TTFT no browser.
- `apps/frontend/src/components/ExecutionTelemetry.tsx`: cobertura, estado,
  compute/transporte acumulados, métricas do stream e TTFTs distintos.
- `apps/frontend/src/lib/telemetryAnalysis.ts`: classificação sem
  `total - processing` e regressão de análise.
- `apps/frontend/src/i18n/messages.csv`: estados, métricas e rótulos novos.
- Testes correspondentes em `apps/frontend/src/lib` e
  `apps/frontend/src/routes/Chat.telemetry.test.tsx`.

### Documentação

Foram atualizados os mapas das pastas tocadas e os subsistemas
`docs/subsystems/inferencia.md` e `docs/subsystems/api-http.md`, incluindo a
semântica de `remote-stream`, ticket, cobertura e quantis.

## 9. Matriz dos dez itens da seção 20

| Item | Comprovação no código |
|---:|---|
| 1 | `outside-node` em `apps/frontend/src/lib/telemetryAnalysis.ts`, a partir de `total_ms - processing`, antes da alteração. |
| 2 | Não havia cálculo Rust independente de `unaccounted`; o conceito era o diagnóstico UI e o residual textual local. |
| 3 | `PhaseTrace::record` soma `processing_ms`; `PhaseTrace::merge` agrega chamadas, com campos separados de RTT e rede. |
| 4 | `summary_for` define `trace.complete` e agora chama `refresh_telemetry`; `ReplyInbox` mantém a coleta após a resposta por retenção curta. |
| 5 | `reply_to` correlaciona por ticket UUID; `trace_id`, `request_id`, `pipeline_id` e IDs de frame/span completam a correlação do stream. |
| 6 | `handle_step_stream`, `remote_stream_producer`, `remote_stream_batch` e `RemoteDecodeStream::next` são os pontos de registro e propagação. |
| 7 | `hop_timings` era descartado entre `RemoteStreamToken` e callback, deixando downstream sem decode. |
| 8 | `record_transport` agora aceita recibos depois de `deliver`, atualiza o snapshot e `await_result_with_ack` mantém o ACK tardio do head. |
| 9 | Havia perda determinística de `hop_timings` e perda tardia de recibos; hoje ambas são visíveis por cobertura, duplicatas, atrasos e retenção. |
| 10 | Não há partição E2E por soma de spans; callbacks duplicados são deduplicados e overlap/residual só aparecem quando medidos. |

## 10. Testes e validação

Passaram:

- `rtk cargo fmt --all`;
- `rtk git diff --check`;
- `rtk cargo test -p node remote_stream`: 27 testes;
- testes direcionados de cobertura, ACK de abertura, ACK tardio do head, spans
  profiling, recibos tardios, recibos duplicados e agregação sem sincronização
  extra;
- `rtk npm test -- --run`: 80 arquivos, 541 testes;
- `rtk npm run build`: TypeScript e Vite concluídos;
- testes frontend direcionados: 22 testes, incluindo a regressão nas seis
  cardinalidades e a paridade dos dois locales.

A execução completa `rtk cargo test -p node` terminou com 1157 testes
passados, 5 ignorados e 8 falhas fora da telemetria:

- `admin::tests::a_secondary_admin_cannot_promote_another_node`;
- três `deploy::whole_route_tests` sobre seleção/enriquecimento de catálogo;
- `prefix_reuse_lab::unknown_session_recovery_is_bounded`;
- `spec_lab::um_orcamento_acima_de_n_batch_nao_e_truncado`;
- `webui::dead_address_tests::a_dead_announced_address_is_forgotten_and_the_peer_is_read_at_the_new_one`;
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`.

Essas falhas se repetem no worktree sujo com alterações de outros trabalhos e
nenhuma é uma falha dos testes de telemetria. A suíte completa não pode ser
declarada totalmente verde neste estado compartilhado.

### Cenários A-L

- A/B/C/D: `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`
  percorre 2 a 4 hops, entradas diferentes e RTT artificial do laboratório.
- E: `spec_lab::resposta_de_um_token_mantem_as_taxas_finitas` passou.
- F: o cenário de resposta longa existe em `spec_lab`, mas
  `um_orcamento_acima_de_n_batch_nao_e_truncado` está entre as falhas
  preexistentes acima.
- G: `remote_stream` tem 27 testes unitários, incluindo callbacks, hops
  downstream, cobertura de três peers e métricas agregadas. Não houve E2E real
  sem um modelo servido.
- H: o laboratório direto cobre a resposta `relay/v2-direct` e o caminho sem
  troca para `remote-stream`; o túnel é representado nas evidências de
  transporte unitárias.
- I/J/K/L: recibo tardio, peer ausente, duplicata e ordem fora de sequência
  estão cobertos pelos testes de `direct` e `prefill`.

## 11. Overhead e limitações restantes

No modo normal, o trace usa contadores, soma incremental e amostras limitadas
a 1024 elementos. A deduplicação mantém no máximo 1024 IDs recentes de frame.
Não há vetor por token nem sincronização adicional por token no hot path. O
callback usa uma fila limitada a 64 lotes e a contrapressão continua
sendo parte do transporte.

`POM_PROFILE_SYNC=1` mantém no máximo 1024 spans de frame/evento. Eventos
por token sem intervalo individual são eventos de duração zero: representam
participação e ordem, não compute medido. Não foi possível medir percentual de
overhead contra uma inferência real porque o ambiente não serviu um modelo.

Limitações conhecidas:

- não há captura E2E Ornith de dois peers neste worktree;
- `overlap_ms` e `true_residual_ms` permanecem nulos sem intervalos
  sincronizáveis suficientes;
- `local_engine_first_event_ms` permanece nulo porque o backend ainda não
  publica esse marco local explícito; pipeline/server TTFT são medidos;
- o snapshot tardio do `ReplyInbox` é uma primitiva interna e não provoca
  atualização viva da UI depois do `x_pom` já enviado;
- após a expiração, spans órfãos não são persistidos em um contador global;
- quantis são exatos até a amostra limitada e aproximados depois do reservoir;
- o stream transporta agregados downstream por callback, não um profiler
  detalhado independente em cada peer.

Conclusão: a implementação corrige a perda comprovada do segundo peer e deixa
de chamar a diferença E2E de `outside-node` quando a execução distribuída não
está explicada. A decomposição agora distingue evidência conhecida,
telemetria pendente e medições acumuladas de uma partição exclusiva que o
sistema ainda não consegue provar.

## 12. Fase 2 - causa raiz da lentidao e correcao autorizada

Esta seção incorpora a telemetria da execução fornecida pelo captain e a
leitura do caminho de código. Os números do captain não são uma nova captura
local. O baseline observado foi 90 tokens em 4,78 s, cerca de 23 tok/s
distribuído contra cerca de 112 tok/s em um nó, e cerca de 11 tok/s quando a
medição da aresta foi 807 ms.

### 12.1 O RTT da aresta não é ICMP

`crates/node/src/network/network.rs` chama
`infer_tunnel::measure_rtt`. A medição começa antes de
`TunnelOutbound::send` e termina quando o `TunnelRequest::get("/health")`
recebe a resposta pelo request-response libp2p. Assim, os 51,1 ms da aresta
`5b03 -> 02eb8c40` são um round-trip de aplicação no túnel: fila do swarm,
eventual dial/estabelecimento, envio, processamento remoto de `/health` e
retorno. Não é o RTT físico ICMP e não abre nem mede o `https-link`.

O ping ICMP direto para `100.85.81.27` fornecido pelo captain tem mediana de
aproximadamente 28-33 ms, com picos de 67 e 114 ms. A diferença para 51,1 ms
não pode ser atribuída somente à rede física: a medição da aresta inclui o
protocolo e o endpoint `/health`. No caminho HTTPS direto, o relógio do frame
começa antes do `reqwest::send` e termina depois dos headers, corpo e
validação; no `https-link`, a abertura é uma operação separada
(`https-link-setup`) e o round-trip de cada frame começa depois que o canal
persistente existe. Portanto o setup não é multiplicado por token.

### 12.2 Onde foram os ~44 ms/token

O cálculo direto do caso real é `4.780 / 90 = 53,11 ms/token`, não 44 ms. A
telemetria fornecida também informa 1.670 ms de compute acumulado, 2.543 ms
de transporte observado e 1.296 ms de `remote-stream wait`:

| Evidência | Total | Média em 90 tokens | Interpretação |
|---|---:|---:|---|
| Compute acumulado | 1.670 ms | 18,56 ms/token | Soma de processamento nos peers, não wall-clock exclusivo |
| Transporte observado | 2.543 ms | 28,26 ms/token | Inclui travessias e custo de aplicação; não é ICMP puro |
| `remote-stream wait` | 1.296 ms | 14,40 ms/token | Intervalo real aguardando item na fila |
| E2E | 4.780 ms | 53,11 ms/token | Caminho crítico medido pelo gateway |

Essas linhas se sobrepõem e não formam uma decomposição: 18,56 + 28,26 +
14,40 = 61,22 ms/token. Isso confirma por que `E2E - soma(spans)` não é uma
correção válida. O piso evitável não é obtido subtraindo essas médias.

O retorno antigo do peer B fazia uma unidade request/reply por token. O
`HttpsLink` já reutilizava TCP/TLS, mas cada frame ainda esperava a resposta
antes de a próxima rodada avançar. A telemetria do captain registra cerca de
37 ms por frame, 88 chamadas, 234 KiB enviados e 134 KiB recebidos, com p50
de 36-38 ms e máximo de 61 ms. Isso é custo de aplicação exposto pelo
protocolo síncrono, não handshake repetido. O `remote-stream wait` de p50 13 ms
e p95 16 ms era espera real pela chegada do item à fila, não um timer artificial
encontrado no código. O emitter esperava `recv()`; o problema era que o
produtor e a entrega ficavam pouco desacoplados e a callback HTTP podia impor
a cadência do caminho.

O piso físico para uma cadeia autoregressiva contém pelo menos uma travessia de
estado por token e o compute necessário em A e B. O custo evitável comprovado
é a serialização de cada callback e o request/reply por frame no retorno. O
compute de token N+1 não pode consumir o estado oculto de N antes de N chegar,
então não existe sobreposição arbitrária entre os computes de A e B sem
especulação. O ganho permitido aqui é transportar a resposta N em paralelo
com a preparação do próximo ciclo que não depende dela, usando fila limitada e
canal persistente. MTP/especulação e o executor MoE não foram usados.

### 12.3 Jitter e propagação sem amortecimento

O p50 do frame de retorno era aproximadamente 42 ms, p95 43 ms e máximo
71 ms. Junto dos picos ICMP de 67-114 ms, isso é compatível com uma cadeia que
expõe a cauda de rede ao tempo de geração: sem buffer, um pico bloqueia o
próximo callback e, por dependência autoregressiva, a resposta inteira. A
telemetria anterior não permitia afirmar se cada pico era ICMP, callback ou
compute; a instrumentação nova separa `token_delivery_interval_*`,
`remote_stream_wait_*`, frame latency e compute remoto. O benchmark E2E ainda
precisa coletar quantis por token no mesmo cenário RTT~30 ms para fechar a
prova de propagação, em vez de inferi-la apenas do ping.

### 12.4 O que foi implementado

1. O produtor do `remote-stream` calcula uma rodada, coloca cada token numa
   fila limitada de 64 entradas e pode iniciar a rodada seguinte sem esperar a
   callback. O emitter entrega os itens em paralelo. A fila absorve rajadas
   curtas de jitter e aplica contrapressão apenas quando a entrega fica
   sustentadamente mais lenta que o compute.
2. A travessia normal entre peers usa o canal persistente `/v1/link` quando a
   capability está presente. O `BatchCoordinator` não intercepta o
   `remote-stream`, e cada frame permanece correlacionado por `batch_id` sem
   reconstruir uma chamada HTTP por token.
3. O retorno do head remoto para o gateway usa também `/v1/link`, com um frame
   de `REMOTE_STREAM_RESULT_PATH` dentro de um canal full-duplex. O gateway
   entrega o frame ao ticket e devolve apenas o ACK do envelope. Se a abertura
   ou o uso do canal persistente falhar, o emissor cai para o POST HTTPS por
   callback e marca cada callback desse stream com
   `remote_stream_transport_fallback=true` e
   `remote_stream_transport_fallback_reason`.
4. A UI mostra sempre `HTTPS fallback: N`, inclusive zero, e as razões
   limitadas. A camada Rust agrega `https_fallback_calls` e
   `https_fallback_reasons`; o indicador não depende de renomear o transporte.
5. A abertura do canal e o round-trip do frame têm spans distintos no modo
   `POM_PROFILE_SYNC=1`. O modo normal mantém contadores, histogramas e
   quantis de ciclo/entrega, sem vetor de evento por token.

O `remote_stream_wait` não foi falsamente zerado. Ele continua sendo a medida
`recv()` da fila do emitter. A correção reduz a espera causada por callback
serial e torna observável a espera residual; zerá-la por apresentação apagaria
a evidência. A fila não remove o piso físico e não converte a cadeia
autoregressiva em execução especulativa.

## 13. Antes/depois e medição de ganho

Não foi possível executar Ornith com dois peers e modelo servido neste
worktree: a UI local informa `The network is not serving any model`. Por isso
não há número honesto de tok/s antes/depois nem promessa de ganho publicado.
O baseline que deve ser repetido pelo captain é:

| Cenário | Baseline informado |
|---|---:|
| Um nó | ~112 tok/s |
| Distribuído, RTT da aresta ~51,1 ms | ~23 tok/s |
| Distribuído, medição da aresta ~807 ms | ~11 tok/s |
| ICMP direto para Salem | mediana ~28-33 ms, picos 67/114 ms |

Para a comparação, usar o mesmo prompt, modelo, número de tokens e rota, com
`POM_PROFILE_SYNC=1`, e calcular `output_tokens / (total_ms / 1000)`.
Registrar no antes e no depois: total E2E, TTFT, token cycle p50/p95/p99/máximo,
delivery interval p50/p95/p99/máximo, `remote_stream_wait` total/p50/p95,
frame latency p50/p95/máximo, compute remoto, transporte, fallback HTTPS e
bytes. O ganho de tok/s só pode ser declarado depois dessa execução. O
critério de sucesso da implementação é que o caminho normal mostre zero
fallback HTTPS, canal persistente reutilizado e que um pico de entrega não
congele o produtor até esgotar a fila.

## 14. Arquivos e modelo de telemetria da fase 2

- `crates/node/src/inference/link.rs`: canal enquadrado persistente aceita
  frames de callback sem exigir resposta `frame/v1-batch`; setup, envio,
  round-trip e recepção são separados em profiling.
- `crates/node/src/inference/direct.rs`: handler do callback dentro de
  `/v1/link`, `RemoteStreamDelivery` com fallback explícito e teste de entrega
  ao ticket.
- `crates/node/src/backend/gateway/http.rs`: `/v1/link` no gateway para o
  retorno full-duplex, com validação da credencial interna por frame.
- `crates/node/src/inference/infer_server/step.rs`: fila de 64 itens,
  producer/emitter concorrentes, índice de token e quantis de entrega; a
  espera continua medida, sem timer fixo.
- `crates/node/src/inference/relay.rs`: caminho persistente antes de túnel e
  HTTPS, com razão de fallback propagada no recibo.
- `crates/node/src/inference/prefill/support.rs` e `prefill/step.rs`:
  contadores de fallback, spans limitados, intervalos de callback e ciclos de
  token.
- `crates/node/src/backend/gateway/budget.rs`: o resumo não preenche mais
  `outside_node_*` por subtração de E2E e processamento acumulado.
- `apps/frontend/src/lib/chatStream.ts` e
  `apps/frontend/src/components/ExecutionTelemetry.tsx`: contrato e
  apresentação de fallback HTTPS e quantis de entrega.
- `apps/frontend/src/i18n/messages.csv`: rótulos nos dois locales.
- `docs/subsystems/inferencia.md`, `docs/subsystems/api-http.md` e mapas
  `AGENTS.md`: contrato operacional do callback persistente e fallback.

## 15. Validação, overhead e limitações da fase 2

Validações concluídas no worktree:

- `rtk cargo check -p node`;
- `rtk cargo test -p node --no-run`;
- `rtk cargo test -p node remote_stream -- --nocapture`;
- `rtk cargo test -p node persistent_remote_stream -- --nocapture`;
- `rtk cargo test -p node remote_stream_https_fallback -- --nocapture`;
- `rtk npm test -- --run`: 80 arquivos, 541 testes;
- `rtk npm run build`.

`rtk cargo fmt --all` e `rtk git diff --check` devem ser repetidos depois do
rebase final. O modo normal não cria spans por token. A fila limitada e os
contadores agregados adicionam apenas o estado necessário para contrapressão e
histogramas; o custo detalhado de profiling fica atrás de
`POM_PROFILE_SYNC=1`, com limite de amostras.

A principal limitação é a ausência de relógio sincronizado entre peers:
timestamps monotônicos de A e B não são subtraídos entre si. A telemetria mede
relações causais por `trace_id`, `stream_id`, frame e `token_index`, e apresenta
ausência quando a fronteira não foi medida. Outra limitação é que os números de
RTT físico e baseline vieram do captain; o ganho final depende da repetição E2E
no cluster.

## 16. Recomendação para o benchmark do captain

Executar o mesmo cenário com o código publicado, primeiro com o caminho
normal e depois forçando uma queda controlada de `/v1/link` apenas para validar
que o indicador mostra o contador e a razão. Não usar a queda controlada para
medir throughput normal. Comparar os relatórios JSON de `x_pom.trace` e o
throughput, e anexar os valores ao final deste relatório. Se o canal normal
continuar com `remote_stream_wait` alto apesar de a fila permanecer abaixo de
64, o próximo diagnóstico deve ser o ponto de entrega do gateway ou o compute
do peer, não uma nova subtração de `outside-node`.

## 17. Fase 3: prova dos 5 s, identidade de sessão e endereços de dial

### 17.1 O que o trace comprovou

O trace `trace-2sessoes-2026-09-09.md` é uma execução de duas pernas
`0bda9121 -> 02eb8c40`, ainda com o resultado ruim: 13,5 tok/s na geração,
TTFT de 5,85 s, E2E de 20,3 s e 9,6 tok/s E2E. A evidência decisiva é o
prefill do segundo peer, com `queue 5002.31 ms`, enquanto o primeiro peer tem
fila próxima de zero. Isto não é uma estimativa: `PrefillJob.queued_at` começa
antes do submit e `NodeTimings.queue_us` inclui a espera até a seleção pelo
scheduler.

O código confirma a origem. `run_scheduler` em
`crates/node/src/inference/infer_server/core.rs` calcula `new_session_slots`
somente com sequências livres, não conta sessões `retained` como capacidade e,
para um prefill novo, chama `evict_retained()` apenas depois de
`RETAINED_RECLAIM_GRACE = Duration::from_secs(5)`. O mesmo laço usa
`tokio::time::sleep(RETAINED_RECLAIM_GRACE)` para reavaliar a fila. Portanto os
5002 ms observados são exatamente o ramo de carência de reclaim, e não compute
do modelo, handshake ICMP ou uma diferença de relógios.

Há uma nuance importante para não transformar o diagnóstico em uma falsa
correção: uma sessão lógica pode ter o mesmo `session_id` em A e B, mas a
primeira chegada em B ainda não tem uma entrada `Session` local. Portanto
`is_existing(sid) == false` no primeiro prefill frio de B é esperado pelo
modelo local atual. A solução aplicada marca frames internos com
`pipeline_session=true` e faz reclaim imediato somente nesse caminho; a
carência de cinco segundos continua reservada para um pedido avulso de outra
sessão. Assim preserva a proteção contra despejar KV quente para uma chamada
curta, sem alegar que uma entrada local inexistente já existe.

### 17.2 Rastro de `session_id` e eliminação de identidades sintéticas

A origem única continua em `backend/gateway/generation.rs`, onde
`PrefixCache::acquire` recebe um UUID novo apenas no miss. O mesmo valor é
passado a `prefill::run_with_options`, `prefill::step_stream` e às
continuações. Os construtores de metadata em `inference/prefill/{pipeline,step}.rs`
e os forwards em `infer_server/{prefill,step}.rs` agora carregam tanto
`session_id` quanto `pipeline_session=true`. `/v1/session/rewind`,
`/v1/session/rollback`, `/v1/session/release` e `/v1/session/slot` continuam
recebendo o mesmo ID no JSON e no header
`x-pom-inference-session`.

Foi encontrado e corrigido um ponto de perda no wire: ao decodificar um batch
de um item, metadata antiga podia permanecer e esconder a identidade do
envelope. `llama-engine/src/wire.rs` agora sobrescreve explicitamente
`meta.session_id` com `BatchItem.session_id`; o teste
`decoded_session_identity_comes_from_the_batch_envelope` reproduz o caso
divergente e passa. `handle_prefill`, `handle_step`, o batch coordinator e o
retorno persistente não criam mais `__session__`: frame sem ID é rejeitado e
fica visível como erro de protocolo.

O resultado é uma propagação comprovada estaticamente de uma identidade por
frame, não uma prova de que B já tem uma sessão local antes do primeiro
prefill. O teste do scheduler
`prefill_de_pipeline_nao_entra_na_carencia_de_pedido_avulso` confirma o
comportamento novo de admissão imediata para o caso distribuído. Ainda falta
confirmar no cluster que o próximo trace não mostra a fila de 5 s e que o
`session_id` visto em A, B e no retorno é o mesmo.

### 17.3 O que foi comprovado sobre `outside-node` e `unaccounted`

Hoje esses campos não são `E2E - soma(spans)`. O caminho em
`inference/prefill/support.rs::refresh_outside_node_accounting` calcula
`outside_node_accounted_ms` como `critical_path_ms(&spans, "gateway")` e
`outside_node_unaccounted_ms` como `outside_node_total_ms.saturating_sub(accounted)`.
Porém `outside_node_total_ms` só é alimentado quando existe um residual
explicitamente medido; `backend/gateway/budget.rs` não o fabrica a partir de
E2E e compute acumulado. Logo, no trace atual, “total fora do nó: não medido”
é uma ausência honesta, enquanto `accumulated_compute_ms` e
`accumulated_transport_ms` são somas diagnósticas que podem se sobrepor e não
formam uma decomposição exclusiva do wall-clock.

### 17.4 Novo corte do `remote-stream`

O trace anterior mostrava `remote_stream_wait` total de 14311 ms, p50 65 ms,
p95 69 ms, frame p95 70 ms e máximo 1400 ms, com compute remoto de apenas
784 ms. A espera antiga misturava o tempo em que o produtor ainda não tinha
gerado o token com o tempo de um token pronto parado no canal. A nova
instrumentação carrega um timestamp monotônico interno não serializado em
cada `RemoteStreamToken` e publica, por lote, estes contadores e quantis:

- `remote_stream_producer_wait_*`: do início do ciclo até o produtor montar o
  token;
- `remote_stream_queue_wait_*`: do token pronto até o emitter recebê-lo,
  incluindo eventual backpressure do canal limitado de 64;
- `remote_stream_wait_*`: o intervalo total entre entregas, mantido para
  preservar comparabilidade e revelar sobreposição.

O teste `emitter_separa_tempo_do_produtor_e_fila_de_entrega` força um token já
pronto e demonstra que ele entra na segunda parcela. A agregação foi coberta
também por `remote_stream_aggregation_is_order_independent_and_deduplicates_frames`.
Isso permite separar, no próximo trace, um produtor lento de uma entrega
bloqueada sem zerar artificialmente o `remote-stream wait`.

### 17.5 Endereços não anunciados e churn de libp2p

Foi confirmado no código que `advertise_addr` não substituía uma lista de
external addresses do `Swarm`: não há chamada a `Swarm::add_external_address`
no repositório, e o Identify continua reportando os listen addresses locais
por contrato da libp2p. Antes da mudança, `announce_addresses` colocava o pin
na frente e acrescentava os endereços locais; o recebimento de Identify
copiava `info.listen_addrs` para Kademlia; mDNS e o fallback de túnel também
podiam alimentar o livro de endereços. Isso explica a exposição de
`192.168.100.x`, `172.17.0.1` e do Tailscale antigo nos logs de “expired peer on
address”.

Agora, com um pin como `100.122.132.97:42777`:

- o `Announce.addr_candidates` contém somente o pin, e as respostas próprias
  ao gateway também retornam somente ele;
- o Announce recebido não acrescenta a origem UDP quando já há lista explícita;
- o pipeline, o network-chat e o fallback de `ensure_tunnel_peer_address`
  usam um único endpoint canônico, e o warm-up gossipsub registra somente a
  multiaddr correspondente;
- listen addresses recebidos via Identify não entram em Kademlia; entradas
  antigas do mesmo evento são removidas, e mDNS é ignorado quando há pin.

A política preserva múltiplos candidatos apenas para o caminho UDP de controle
quando não há pin e para compatibilidade com anúncios antigos. Não há mais
fan-out de libp2p a partir de interfaces locais quando o endpoint foi fixado.
Os testes `a_pinned_address_replaces_local_announce_candidates`,
`libp2p_dial_uses_addr_canonico_e_ignora_candidatos_legados` e
`tunnel_addresses_preserve_only_the_explicit_advertised_set` passam.

### 17.6 Estado da rodada e próximos números

Validações locais desta rodada: `cargo test -p node --no-run`, a suíte Node
com 1270 testes e 5 ignorados em modo serial, testes da agregação remota, do
split produtor/fila, do wire da identidade, da admissão do scheduler e dos
endereços canônicos. A suíte frontend passou com 541 testes e o build
TypeScript/Vite passou. A execução paralela da suíte Node teve uma falha
flutuante isolada no laboratório de prefill, que passou isoladamente e na
execução serial. Não há novo número E2E pós-correção neste worktree: o último
valor medido pelo captain continua sendo 13,5 tok/s de geração, 9,6 tok/s E2E
e TTFT 5,85 s.

A próxima rodada deve ser implantada nas duas máquinas, com a Salem na mesma
versão e a KSIA anunciando `100.122.132.97:42777`. O critério objetivo é:
fila de prefill do segundo peer sem os 5 s, nenhum dial para LAN/bridge/IPv6
antigo, retorno normal pelo canal persistente, e comparação antes/depois de
TTFT, E2E tok/s, p50/p95/p99 por token, máximo de frame, split produtor/fila
e contagem de fallback HTTPS. Até essa execução, qualquer ganho de throughput
seria especulação.
