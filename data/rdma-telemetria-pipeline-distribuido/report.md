# Relatório técnico - telemetria do pipeline distribuído

Data: 2026-09-09

Status: instrumentação e correção implementadas e validadas em nível unitário,
de laboratório e frontend. A captura E2E com Ornith e dois peers continua
bloqueada pelo ambiente local sem modelo servido. O executor MoE foi ignorado,
conforme a spec.

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
| Profiling | Sem correlação detalhada do stream | `POM_TELEMETRY_PROFILE=1` habilita spans de frame e eventos por token limitados |

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

- `rtk cargo fmt --all -- --check`;
- `rtk git diff --check`;
- `rtk cargo test -p node remote_stream`: 24 testes;
- testes direcionados de cobertura, ACK de abertura, ACK tardio do head, spans
  profiling, recibos tardios, recibos duplicados e agregação sem sincronização
  extra;
- `rtk npm test -- --run`: 73 arquivos, 499 testes;
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
- G: `remote_stream` tem 24 testes unitários, incluindo callbacks, hops
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
callback usa uma fila existente limitada a 16 lotes e a contrapressão continua
sendo parte do transporte.

`POM_TELEMETRY_PROFILE=1` mantém no máximo 1024 spans de frame/evento. Eventos
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
