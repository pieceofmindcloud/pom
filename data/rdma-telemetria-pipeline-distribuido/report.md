# Relatório técnico - telemetria do pipeline distribuído

Data: 2026-09-10

Status: instrumentação e correção implementadas e validadas em nível unitário,
de laboratório e frontend. A captura E2E com Ornith e dois peers foi executada
no cluster em 2026-09-10 após o deploy de `65e88d10`; o executor MoE foi
ignorado, conforme a spec.

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

## 11. Atualização sobre o caminho QUIC, o card e o ciclo visual

Os traces do captain no `e19640c2`, sem QUIC, fecharam a hipótese de que o
ganho de MTP já acontece no caminho distribuído: `tokens/decode = 1,99`,
geração de `26,4 tok/s` e TTFT de `840 ms`, contra aproximadamente `22,5
tok/s` antes. O mesmo trace observou o hop A -> B por `http`, com amostras de
`51/66/68/130 ms`. Portanto, o card que ainda mostrava `86,0 ms` de libp2p
estava consultando uma sonda separada, não a execução do Chat.

Esta rodada mantém a seleção de transporte em um único ponto: QUIC é tentado
quando a preferência está ativa; quando está desativada, o canal persistente
é preferido; a reserva por URL publica `http` ou `https` conforme a URL real.
Em caso de fallback, a metadata é codificada novamente por tentativa para que
um sucesso TCP não apareça como QUIC. O MTP usa o mesmo `SentFrame` e a mesma
identidade de transporte, portanto não há rótulo hardcoded no handoff.

O trace agora também publica `pipeline_cycle.decode`: cada amostra é atribuída
à perna lógica que a observou, na ordem `Gateway -> cabeça`, avanços entre
peers e retorno. O painel consulta esse ciclo e mostra o transporte efetivo no
selo da aresta. O total do Gateway final soma cada perna uma vez e o compute
dos hops, mas só é fechado quando todas as pernas têm medição. A duração de
uma operação de transporte é direcional no sentido da atribuição lógica; não
é dividida por dois, porque o código ainda não tem relógios monotônicos
sincronizados entre hosts para provar uma via física. O callback persistente
do `remote-stream` não espera o ACK por token e, por isso, não recebe um RTT
inventado a partir do intervalo entre lotes.

O canvas deixou de desenhar arestas de retorno entre peers. Ele desenha
somente os avanços, coloca um Gateway virtual no extremo final, liga o último
peer a ele e apresenta a decomposição de rede + compute no card do Gateway.
Uma perna sem evidência permanece `RTT -` e o total permanece `não medido`.

Os novos dados de produção que motivaram esta rodada também ficam registrados:
o residual do tail GPU -> CPU foi de `1924-2376 ms` no total, cerca de `9,5
ms/token`; o transporte ainda representou aproximadamente `44%` do caminho
crítico e `remote-stream wait` teve p50 de `75 ms/token`. A mensagem
`Telemetria de transporte incompleta` era falso positivo quando havia `2/2`
peers e `100%` dos spans: o estado de spans distribuídos agora é calculado
separadamente da reconciliação tardia dos recibos de transporte.

Limitação mantida deliberadamente: no callback persistente e na entrega final
assíncrona não existe, no mesmo envelope, uma duração de ida e volta observada
antes de o corpo ser enviado. O relatório e a UI deixam essa lacuna explícita;
eles não substituem o valor pela sonda libp2p, por `frame latency`, por zero ou
por uma subtração do E2E.

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
2. O retorno de `remote-stream` para o gateway usa o canal persistente
   `/v1/link` quando a capability está presente. A travessia interna A -> B
   agora admite também `remote_stream=true` no `BatchCoordinator`, portanto
   reutiliza o canal persistente e mantém a correlação por `batch_id`. O trace
   longo de `65e88d10` foi capturado antes desta alteração e confirmou o
   comportamento antigo: uma chamada `libp2p-tunnel` por ativação. A resposta
   ainda é aguardada para respeitar a dependência autoregressiva.
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

Agora, com um pin como `100.71.127.127:42777`:

- o `Announce.addr_candidates` contém somente o pin, e as respostas próprias
  ao gateway também retornam somente ele;
- o Announce recebido não acrescenta a origem UDP quando já há lista explícita;
- o pipeline, o network-chat e o fallback de `ensure_tunnel_peer_address`
  usam um único endpoint canônico, e o warm-up gossipsub registra somente a
  multiaddr correspondente;
- listen addresses recebidos via Identify não entram em Kademlia; entradas
  antigas do mesmo evento são removidas. O boot seguinte também não constrói o
  comportamento mDNS quando há pin, porque o próprio mDNS emitia os logs
  `discovered/expired peer on address` antes de o handler poder ignorar o
  evento. Sem pin, a descoberta LAN continua disponível.

A política preserva múltiplos candidatos apenas para o caminho UDP de controle
quando não há pin e para compatibilidade com anúncios antigos. Não há mais
fan-out de libp2p a partir de interfaces locais quando o endpoint foi fixado.
Os testes `a_pinned_address_replaces_local_announce_candidates`,
`libp2p_dial_uses_addr_canonico_e_ignora_candidatos_legados` e
`tunnel_addresses_preserve_only_the_explicit_advertised_set` passam.

O primeiro redeploy desta rodada revelou uma causa operacional adicional antes
de qualquer medição de throughput: `pom-ksia.sh` passa o override completo
`100.122.132.97:42777` a `scripts/gen-node-configs.py`. O gerador antigo copiava
esse endpoint literalmente para os quatro processos, embora as configs
mantivessem `p2p_port` 42777, 42778, 42779 e 42780. Assim, todos anunciavam o
mesmo socket UDP/TCP derivado, colapsando os peers e impedindo uma cobertura
válida `0..41`; a chamada E2E chegou ao gateway e retornou 503 por
`Nenhum pipeline válido`, enquanto `/peers` ainda mostrava apenas a faixa
remota 16-40. A evidência foi obtida nas quatro configs e na resposta HTTP,
não inferida do timeout.

O gerador agora interpreta `HOST:PORT` como host comum e reescreve somente a
porta para o `p2p_port` próprio de cada processo. Os quatro testes de
`scripts/test_gen_node_configs.py` passam. O pin mDNS também foi validado no
boot seguinte: não surgiram novos logs internos `discovered/expired peer on
address`; os registros antigos permanecem apenas no arquivo histórico. Na
Salem, as portas por serviço já eram distintas, mas o bootstrap configurado
continua apontando para o Tailscale antigo `100.71.127.127:42777`, um alvo
obsoleto separado que ainda precisa ser removido ou substituído na operação.
O redeploy e a medição E2E após o conserto das portas ainda estão pendentes.

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

### 17.7 E2E pós-correção de `session_id`

Em 2026-09-10, depois de o captain colocar `e0c747e` nas duas máquinas, foi
executada uma única chamada fria pelo gateway KSIA, com
`POM_PROFILE_SYNC=1`, prompt de 29 tokens, `max_tokens=16` e resposta não
streaming. O retorno foi HTTP 200 e o trace completo foi:

- `trace_id=dea4ae159bbf45ef887fb79683caadd4`;
- `request_state=cold`, `telemetry.status=complete`, `response_complete=true`;
- 2/2 peers, 2/2 spans de decode, 100% de completude, zero duplicatas, zero
  spans tardios e zero órfãos;
- 1.520 ms E2E, TTFT de 1.314 ms, 2 tokens e `tokens_per_sec=1,3158` no
  wall-clock. O trecho de geração isolado foi 206 ms, ou `9,7087 tok/s`;
- prefill de 1.013 ms e `prefill_tokens_per_sec=28,6278`;
- `https_fallback_calls=0`.

A fila artificial do segundo peer desapareceu. No hop `02eb8c40` o trace
registrou `node_queue_ms=0,043`, `node_scheduler_wait_ms=0,019` e
`node_decode_ms=602,768`; não há `queue` próximo de 5.002 ms. A espera do
primeiro hop também ficou em `node_queue_ms=0,052`. Isto comprova o objetivo
primário da propagação: o caminho não entrou na carência fixa de reclaim.

O trace ainda mostra o gargalo que permanece para a próxima iteração: o
retorno remoto agregou um frame com lifetime de 204 ms, `remote_stream_wait`
de 203 ms e `remote_stream_producer_wait` de 203 ms, enquanto
`remote_stream_queue_wait` foi 0 ms. Portanto, nesta execução a espera
residual é produção/round-trip interno, não fila local do callback. O hop B
teve decode de 99 ms e round-trip de 182 ms no decode; a telemetria de A
registrou a abertura do stream por `libp2p-tunnel` e uma troca posterior para
`remote-stream`, sem fallback HTTPS. O trace contém também os spans detalhados
dos dois peers, correlacionados pelo mesmo `trace_id`, `request_id` e
`pipeline_id`.

Este resultado é uma linha de base válida, mas não é comparação de throughput
de 16 tokens: o modelo encerrou a resposta em 2 tokens. O próximo benchmark
deve forçar uma saída longa e medir separadamente TTFT, geração, p50/p95/p99
do ciclo por token e o retorno full-duplex. O commit `65e88d10` implementa
esse último corte: callbacks persistentes são enfileirados sem esperar o ACK
individual, mantendo contrapressão na fila limitada e monitoramento
assíncrono de falha.

A próxima rodada deve ser implantada nas duas máquinas, com a Salem na mesma
versão e a KSIA anunciando o host `100.71.127.127` nas portas próprias
42777-42780. O critério objetivo é:
fila de prefill do segundo peer sem os 5 s, nenhum dial para LAN/bridge/IPv6
antigo, retorno normal pelo canal persistente, e comparação antes/depois de
TTFT, E2E tok/s, p50/p95/p99 por token, máximo de frame, split produtor/fila
e contagem de fallback HTTPS. Até essa execução, qualquer ganho de throughput
seria especulação.

### 17.8 Consolidação do controle de profiling e retorno assíncrono

O último resíduo do controle antigo foi removido do backend de prefill. A
função agora se chama `profile_sync_enabled` e consulta somente
`POM_PROFILE_SYNC`; uma busca no código, documentação, configuração e contrato
do frontend não encontra a variável legada nem o antigo helper. O
`POM_PROFILE=off|basic|trace` continua sendo o
nível do trace, enquanto `POM_PROFILE_SYNC=1` habilita a coleta detalhada de
spans sincronizados e eventos de token. São políticas distintas e não há duas
variáveis para o mesmo controle.

Também foi removida a barreira de ACK por callback no caminho normal. O
`RemoteStreamDelivery` agora enquadra e enfileira cada retorno no canal
persistente full-duplex; o ACK continua registrado para detectar erro e
descartar o canal de forma assíncrona. A fila limitada de 64 frames preserva
contrapressão sem reintroduzir um round-trip por token. O teste
`callbacks_persistentes_nao_esperam_ack_de_cada_frame` comprovou oito frames
com ACK artificial de 200 ms sem esperar os ACKs individualmente. O fallback
HTTPS continua sendo usado somente quando a entrega persistente falha, com
razão explícita na telemetria.

Na validação completa que antecedeu esse ajuste, a bancada Node terminou com
1.263 testes aprovados, 1 falha e 5 ignorados: a fixture
`spec_lab::quem_rascunha_no_anel_gera_o_texto_inteiro` completou o prefill
falso em menos de 1 ms, tornando corretamente nula a taxa pública que usa
milissegundos inteiros, mas o teste exigia que ela existisse. A reprodução
isolada passou; o teste agora verifica `prefill_tokens > 0`, sem fabricar uma
taxa a partir de duração zero. Os dois cenários da bancada passaram novamente
depois do ajuste.

A execução completa seguinte, serializada após a correção, terminou com 1.272
testes aprovados e 5 ignorados.

### 17.9 E2E após o corte do ACK por callback

Em 2026-09-10, com `65e88d10` nas duas máquinas, `POM_PROFILE=trace` e
`POM_PROFILE_SYNC=1`, foi repetido o mesmo pedido frio pelo gateway KSIA:
modelo Ornith, prompt `Responda apenas OK.`, 29 tokens de entrada,
`max_tokens=16` e resposta não streaming. O trace completo retornado pelo
gateway foi `27485236485841f99d02ff8e4dd1d08d`, com pipeline
`0bda9121 -> 02eb8c40`, e registrou ambos os peers e os spans detalhados dos
dois nós.

| Métrica | `e0c747e` / `dea4ae15` | `65e88d10` | Diferença observada |
|---|---:|---:|---:|
| TTFT | 1.314 ms | 1.316 ms | +2 ms |
| E2E wall | 1.520 ms | 1.515 ms | -5 ms |
| Geração | 206 ms | 199 ms | -7 ms |
| Geração | 9,7087 tok/s | 10,0503 tok/s | +3,5% |
| Remote-stream lifetime | 204 ms | 197 ms | -7 ms |
| Remote-stream wait | 203 ms | 197 ms | -6 ms |
| Espera de fila do remote-stream | 0 ms | 0 ms | estável |
| Decode B | 99 ms, RTT 182 ms | 99 ms, RTT 174 ms | compute estável, RTT -8 ms |
| Fallback HTTPS | 0 | 0 | estável |

O segundo peer continuou sem fila artificial: `node_queue_ms=0,043`,
`node_scheduler_wait_ms=0,021` e `node_decode_ms=600,699` no prefill, contra
nenhum valor próximo de 5.002 ms. A correção de identidade de sessão segue
confirmada.

O corte do ACK não produziu ganho material nesta corrida curta. Os dois
tokens foram entregues em um único frame (`frames_sent=1`), então não houve
uma sequência longa de ACKs para multiplicar o custo removido. O frame teve
197 ms de lifetime, espera de produção de 197 ms e espera de fila de 0 ms;
isso mostra que a espera residual está no ciclo remoto/compute, não em ACK ou
fila local. A pequena melhora de 3,5% na taxa e de 7 ms no frame é compatível
com a variação de RTT observada, pois o RTT de B também caiu 8 ms e o TTFT
subiu 2 ms.

Há uma mudança semântica importante visível no trace: `trace.complete=true`,
mas `transport_trace_complete=false` e `telemetry.status=response_complete`.
Isto é esperado no novo caminho: a resposta ao cliente não espera o ACK do
callback; o ACK fica sendo observado em background. A entrega foi aceita pelo
canal persistente e não houve fallback, mas a reconciliação do transporte
continua independente do fechamento da resposta.

Conclusão desta medição: o corte de ACK removeu a barreira por frame sem
regredir o caminho e manteve o retorno normal em `/v1/link`, porém o gargalo
dominante ainda é o request/reply síncrono de cada ativação entre A e B. O
trace mostra `libp2p-tunnel`, uma chamada de decode em B com RTT de 174 ms,
enquanto o retorno `remote-stream` já é um frame persistente. Uma saída longa
é necessária para quantificar o ganho do ACK; o próximo ajuste deve tornar a
travessia A -> B persistente também, preservando a dependência autoregressiva.

### 17.10 E2E longo para medir a geração

Ainda em 2026-09-10, foi executado no mesmo gateway KSIA um pedido longo com
`max_tokens=200`, usando o prompt `Escreva hello world 30 vezes. Não escreva
explicações.`. O modelo encerrou naturalmente em 90 tokens. O trace completo
foi `3d5f2efe26264d4581680e1169ffe91f`, com 2/2 peers, 178/178 spans de
decode, `complete=true` e `response_complete=true`.

| Métrica | Resultado `65e88d10` |
|---|---:|
| TTFT | 636 ms |
| Geração | 6.280 ms |
| Geração tok/s | 14,33 |
| Wall tok/s | 13,01 |
| Remote-stream frames | 89 |
| Frame/delivery p50 | 70 ms |
| Frame/delivery p95 | 74 ms |
| Frame/delivery máximo | 96 ms |
| Producer wait total / p50 / p95 | 6.209 ms / 69 ms / 74 ms |
| Queue wait total | 0 ms |
| Compute remoto total / p50 | 357 ms / 4 ms |

O resultado fica acima da referência de geração de 13,5 tok/s em cerca de
6,2%, mas abaixo da referência de 18,8 tok/s em cerca de 23,8%. Não é possível
atribuir os 6,2% ao corte do ACK: a amostra anterior de dois tokens variou o
RTT de B em 8 ms, e não há antes/depois longo no mesmo commit. A evidência
causal mais forte é outra: 89 frames tiveram `queue_wait=0`, enquanto quase
todo o `remote_stream_wait` foi `producer_wait`; portanto a remoção do ACK
eliminou a fila do retorno, mas não a espera do produtor.

O produtor esperou p50 de 69 ms por token. A tentativa com 100 repetições
produziu 99 tokens antes de ser interrompida pelo detector de repetição, por
isso não é usada como throughput final, embora tenha confirmado
`producer_wait` p50 de 70 ms, p95 de 74 ms, fila total de 0 ms e 98 chamadas
`libp2p-tunnel` no peer B, com RTT p50 de 65 ms, p95 de 69 ms e máximo de
96 ms. Assim, o caminho crítico ainda expõe a cauda de cada request/reply
interno, sem amortecimento por um canal persistente A -> B.

Conclusão operacional: o retorno persistente está funcionando e o corte de
ACK não era o gargalo restante. A alteração local agora faz o mesmo no hop
interno: `send_frame_or_tunnel` admite `remote_stream=true` no caminho
persistente correlacionado, mantendo a espera causal de um token antes do
seguinte. O E2E longo precisa ser repetido após o deploy dessa alteração, com
um prompt que evite o detector de repetição e com TTFT, geração, wall,
p50/p95/p99 e fallback HTTPS registrados separadamente.

### 17.11 E2E após o canal persistente A -> B

Depois do deploy de `15caf764` nas duas máquinas, com `POM_PROFILE=trace` e
`POM_PROFILE_SYNC=1`, foi repetido o pedido longo de `max_tokens=200` pelo
gateway KSIA. O modelo encerrou em 90 tokens. O trace completo foi
`chatcmpl-f553641ffcc849fba1fb666f1582d4c7`, com 2/2 peers, 178/178 spans,
`trace.complete=true`, `telemetry.status=complete` e zero fallback HTTPS.

| Métrica | `65e88d10` | `15caf764` | Variação |
|---|---:|---:|---:|
| TTFT | 636 ms | 1.367 ms | +731 ms |
| Geração | 6.280 ms | 4.146 ms | -2.134 ms |
| Geração tok/s | 14,33 | 21,71 | +51,5% |
| Wall tok/s | 13,01 | 16,33 | +25,5% |
| Remote-stream wait p50 | 70 ms | 44 ms | -26 ms |
| Remote-stream wait p95 | 74 ms | 48 ms | -26 ms |
| Producer wait p50 | 69 ms | 44 ms | -25 ms |
| Queue wait total | 0 ms | 0 ms | estável |
| Frames | 89 | 89 | estável |

O ganho de geração supera tanto a referência anterior de 13,5 tok/s (+60,8%)
quanto a referência de 18,8 tok/s (+15,5%). A amostra, porém, não deve ser
usada para declarar melhoria de TTFT: o TTFT subiu 731 ms nesta corrida
isolada, apesar de o prefill ter durado 1.065 ms e não haver fila de reclaim;
é necessária uma série fria/quente controlada para separar inicialização e
jitter do efeito do hop. O alvo de geração foi atingido: o tempo de decode
caiu 34,0% e o wall-clock total caiu 20,3%.

Uma segunda leitura longa, feita somente para extrair os campos por hop, teve
96 tokens e terminou com `telemetry.status=collecting`; por isso não entra na
taxa final. Mesmo assim, ela fechou a prova de transporte: no peer B, o decode
teve 95 chamadas `https-link`, RTT mínimo 35 ms, mediana 38 ms, p95 43 ms e
máximo 43 ms, com zero fallback. Não houve as 95 chamadas
`libp2p-tunnel` por token observadas no diagnóstico anterior. Os 95 itens de
retorno aparecem no A como `remote-stream`; a única chamada `libp2p-tunnel`
restante é o frame de controle/abertura do stream. O frame interno agora
reutiliza uma única ligação persistente, exatamente como o teste de bancada
publicado em `link_lab` exige.

O novo trace também confirma que a fila local não voltou a ser o gargalo:
`remote_stream_queue_wait_total_ms=0`, `remote_stream_producer_wait_p50_ms=44`
e `remote_stream_producer_wait_p95_ms=47`. O compute remoto somou 373 ms, com
p50 de 4 ms. A redução de aproximadamente 70 para 44 ms por rodada é,
portanto, coerente com a troca comprovada de request/reply libp2p por frame
para o canal `/v1/link`; a dependência autoregressiva continua serial, mas o
overhead de transporte e enquadramento por ativação deixou de ser pago como
uma nova chamada.

O traço longo anterior de 65e88d10 permanece a comparação de geração, e o
trace diagnóstico posterior serve somente para validar o transporte. O
próximo benchmark deve repetir pelo menos cinco corridas com o mesmo prompt,
separar primeira corrida de canal aquecido e publicar mediana/p95 de TTFT,
geração, wall e RTT A -> B.

### 17.12 Fechamento do caminho crítico por token

O trace diagnóstico `chatcmpl-d5d8e71f686d4962abb4775e1865e6b5`, embora tenha
terminado com `telemetry.status=collecting` por causa do detector de repetição,
tem 95 ciclos completos e permite fechar a aritmética do caminho crítico:

| Componente | Medição | Leitura correta |
|---|---:|---|
| A decode | 381 ms / 95 = 4,0 ms por token | Compute do head antes de enviar o próximo estado |
| B decode | 1.046 ms / 95 = 11,0 ms por token | Está dentro do RTT da chamada A -> B, não deve ser somado outra vez |
| A -> B `https-link` | 95 chamadas, p50 38 ms, p95 43 ms, máximo 43 ms | Um frame por token no canal persistente, ainda com resposta necessária |
| Ciclo do produtor | p50 43 ms, p95 48 ms, máximo 50 ms | A compute + RTT A -> B + enquadramento/scheduling residual |
| Fila callback | p50/p95 0 ms, total 0 ms | Não há bloqueio do emitter/gateway acrescentando espera |

Assim, os 43-44 ms chamados de `remote_stream_wait` são a espera causal pelo
próximo token, não um timer, ACK ou fila ociosa escondida. O RTT físico informado
de 28-33 ms é o piso de rede; o RTT de aplicação de 38-43 ms inclui o trabalho
de B e o protocolo. A diferença entre as duas medições não pode ser atribuída
inteiramente à rede, mas também não aparece como uma espera separada no trace.

Não existe sobreposição arbitrária adicional para uma única sequência: B não
pode calcular N+1 antes de receber de A o estado derivado do token N, e A não
pode produzir esse estado antes de receber/samplar a resposta de B. O trace
também exclui a hipótese de serialização desnecessária no retorno: `queue_wait`
é zero, o callback não aguarda o ACK para liberar o produtor e o canal
persistente foi reutilizado sem reabertura por token.

O campo `wall tok/s` não revela overhead de stream nesta amostra. Ele é
exatamente menor que `generation tok/s` pelo TTFT: 5.513 ms totais - 4.146 ms
de geração = 1.367 ms de TTFT. O restante do ganho precisa vir de reduzir o
número de ciclos causais, não de subtrair novamente spans sobrepostos.

Conclusão de implementação: a alteração de `15caf764` eliminou o custo evitável
de abrir/requestar o transporte interno por ativação e elevou a geração para
21,71 tok/s. Para avançar em direção ao throughput de uma máquina única, o
próximo mecanismo tecnicamente válido é uma rodada multi-token (MTP ou
especulação) quando o runtime anunciar a capability e o modelo estiver pronto.
Forçar múltiplos tokens para um modelo sem draft seria incorreto; não foi feita
essa alteração nem tocado o fork vendorizado do llama.cpp.

### 17.13 Correção de stream órfão

Além do ciclo normal, foi encontrado um caso de serialização de recursos que
podia contaminar as medições seguintes: `handle_step_stream` devolvia o ACK de
aceite e depois aguardava `producer` e `emitter` com `tokio::join!`. Se o
gateway fechasse o ticket, o emitter terminava ao observar `410 Gone`, mas o
producer continuava executando o laço autoregressivo, mantendo a sessão de KV,
o permit de carga e novas entradas no scheduler até a próxima falha de
callback ou timeout.

`run_remote_stream_tasks` agora executa os dois lados como tarefas irmãs. Se o
emitter termina primeiro, o producer é abortado e seu `JoinHandle` é aguardado;
se o producer termina normalmente, o emitter ainda drena a fila limitada para
entregar o último token. O teste
`emissor_para_quando_a_entrega_termina_antes_do_produtor` reproduz um producer
pendurado e confirma que ele é descartado quando a entrega termina. A mudança
foi publicada em `82c17b11`. Ela corrige vazamento de trabalho e evita que
requisições abandonadas falseiem o throughput posterior, mas não é declarada
como ganho de tok/s até ser redeployada e medida no cluster.

### 17.14 Série de cinco E2E após `82c17b11`

Com `POM_PROFILE=trace` e os dois peers em serviço, foram executadas cinco
chamadas sequenciais pelo gateway KSIA. O pedido foi o mesmo usado na série
anterior (`Escreva hello world 30 vezes. Não escreva explicações.`,
`max_tokens=200`, resposta não streaming). Todas retornaram HTTP 200, com
`trace_complete=true`, `response_complete` e zero fallback HTTPS.

| corrida | tokens | TTFT (ms) | geração (ms) | geração tok/s | wall tok/s |
|---:|---:|---:|---:|---:|---:|
| 1 | 94 | 1.361 | 4.276 | 21,98 | 16,68 |
| 2 | 90 | 591 | 3.969 | 22,68 | 19,74 |
| 3 | 90 | 310 | 3.996 | 22,52 | 20,90 |
| 4 | 60 | 602 | 2.661 | 22,55 | 18,39 |
| 5 | 90 | 1.414 | 3.932 | 22,89 | 16,84 |

O conjunto produziu 424 tokens. A média aritmética foi geração **22,52
tok/s**, wall **18,51 tok/s** e TTFT **856 ms**; as medianas foram 22,55
tok/s, 18,39 tok/s e 602 ms. Contra a referência de `15caf764` (21,71
tok/s de geração e 16,33 tok/s wall), isso representa +3,7% e +13,3%,
respectivamente. A comparação tem comprimentos naturais diferentes e não
fixa seed, portanto a média não isola causalmente o cancelamento de stream.

Foi feita também uma interrupção controlada de um stream após aproximadamente
1 s. Antes do abandono, os dois infer servers tinham `queue_depth=0` e 429
batches/items. Depois do abandono, a fila continuou reportando zero, mas os
contadores avançaram para 441/442 imediatamente, 468/469 em 1 s, 517/519 em
2 s, 635/636 em 5 s e 1.185/1.186 em 15 s. Após 35 s, o health da KSIA
expirou, enquanto Salem ainda mostrava 1.344 batches/items. A conclusão é
que `queue_depth` mede espera de admissão, não trabalho ativo: o callback
abandonado continuou consumindo a sessão e o scheduler.

### 17.15 Causa raiz do abandono persistente e correção preparada

Há duas falhas complementares:

1. O pump entre `generate_stream_inner` e o adaptador HTTP só esperava um
   evento em `inner_rx`. Como o gerador mantém outro clone do sender para o
   fechamento da sessão, o `Sender::closed()` observado dentro da geração não
   detectava prontamente o cliente que abandonou o stream, sobretudo quando
   ainda não havia evento disponível. Assim, a geração seguia usando KV e
   recursos remotos.
2. No caminho persistente `/v1/link`, o envio do callback retorna antes do ACK.
   Quando o gateway responde `410 Gone`, a tarefa de ACK marcava apenas o
   canal como morto e removia o registro. `RemoteStreamDelivery` continuava
   vendo `Delivery::Accepted`, logo `run_remote_stream_tasks` não recebia
   `GatewayGone` e o produtor permanecia ativo.

A correção local torna o pump sensível a `client_tx.closed()` em paralelo ao
recebimento de eventos e cancela a geração imediatamente. O ACK assíncrono
agora publica um estado explícito: `410 Gone` vira `GatewayGone`, enquanto
erro de transporte continua acionando o fallback HTTPS com motivo visível.
A entrega persistente propaga esse estado ao produtor, que encerra o stream
sem cair para HTTPS quando o cliente apenas abandonou o ticket.

Os testes de regressão passaram isoladamente: o pump cancela sem evento,
`callback_assincrono_sinaliza_gateway_desaparecido`,
`ack_410_do_callback_para_o_stream_sem_fallback` e as suítes de `link` (13)
e `direct` (19). A suíte completa do binário passou com **1.268 testes
aprovados e 5 ignorados** em execução serial. A correção foi publicada em
`146c2deb` e validada nas duas máquinas na seção 17.16. Não houve alteração
no llama.cpp nem início de MTP/especulação.

### 17.16 Validação em cluster após `146c2deb`

O commit `146c2deb` foi implantado nas duas máquinas com `POM_PROFILE=trace`.
A KSIA estava com 4/4 nós saudáveis, incluindo o node-3 com Ornith e slice
0-15; Salem estava com 3/3 containers saudáveis e o modelo carregado. O
gateway usado foi `100.71.127.127:8080`, com o mesmo prompt da série anterior
e `max_tokens=200`. A corrida 1 foi executada antes das corridas 2-5 e os
cinco traces retornaram resposta completa, 90 tokens, sem erro e
`trace.complete=true`:

O campo `transport_trace_complete` veio `false` na resposta da corrida 1,
como já observado no caminho de ACK assíncrono; isso não impediu o fechamento
do trace principal (`trace.complete=true`) nem a entrega da resposta. O
campo de fallback HTTPS foi zero nas cinco respostas.

| corrida | trace_id | tokens | TTFT (ms) | geração (ms) | geração tok/s | wall tok/s |
|---:|---|---:|---:|---:|---:|---:|
| 1 | `chatcmpl-08b9bc6b6b8647b2bacce77adca73e52` | 90 | 1.344 | 4.181 | 21,53 | 16,29 |
| 2 | `chatcmpl-b4d9a9ed902b42578e37511cce42f73c` | 90 | 599 | 3.937 | 22,86 | 19,84 |
| 3 | `chatcmpl-f86db09b7c33444eae0d07eead2a72a1` | 90 | 317 | 3.971 | 22,66 | 20,99 |
| 4 | `chatcmpl-aed8d19eca9d47809d7d746826742158` | 90 | 1.877 | 3.941 | 22,84 | 15,47 |
| 5 | `chatcmpl-8b799f5fa3444f2792f61ca5ec0de2c7` | 90 | 582 | 3.919 | 22,97 | 20,00 |

A média foi **22,57 tok/s de geração**, **18,52 tok/s wall** e **944 ms de
TTFT**. As medianas foram 22,84 tok/s, 19,84 tok/s e 599 ms. Contra a
referência de geração de `15caf764` (21,71 tok/s), o ganho médio é **+4,0%**;
contra o wall de 16,33 tok/s da mesma referência, é **+13,4%**. A geração
ficou praticamente no mesmo nível da média de `82c17b11` (22,52 tok/s), o que
não atribui ganho mensurável de throughput ao cancelamento isoladamente. O
TTFT variou de 317 a 1.877 ms, portanto continua dependente de jitter e
estado quente/frio.

O teste de abandono abriu o mesmo pedido em streaming e encerrou o cliente
por `--max-time 1`; o curl terminou com código 28 após 1.007 ms e 2.308
bytes recebidos. Antes do abandono, os infer servers estavam em 455
batches/items cada. Na primeira leitura após o fechamento, os contadores
estavam em 469/469 na KSIA e 468/468 em Salem, com `queue_depth=0`; esse
pequeno avanço é trabalho já em voo. Em todas as leituras seguintes os
contadores permaneceram inalterados:

| momento | KSIA batches/items | Salem batches/items | queue_depth | `/health` |
|---|---:|---:|---:|---|
| imediato | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +1 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +2 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +5 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +15 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +35 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |

No node-3, o log `/tmp/pom-ksia-node-3.log` tinha 124.092 bytes antes da
rodada, 124.524 bytes após as cinco gerações e 124.821 bytes no abandono;
ficou estável em 124.821 bytes de imediato até +35 s. A contagem exata de
`Too many open files` foi zero em todas as leituras do abandono. O limite
continuou em `ulimit -n=1024`; os processos usaram no máximo 128 FDs (e
caíram gradualmente até 121-29 por processo na última leitura), sem sinal de
pileup. Portanto o `146c2deb` passou este cenário sem exigir aumento de
ulimit, embora o limite baixo continue sendo um risco operacional separado.

Esta rodada confirma o objetivo específico do commit: o fechamento do
cliente não deixou batches crescendo depois do primeiro intervalo de
observação, os dois health endpoints continuaram responsivos por 35 s e o
spam de FD não retornou. Ela não resolve o limite arquitetural ainda aberto:
o decode continua serial por dependência autoregressiva e o TTFT/wall ainda
carrega a latência entre hops. MTP/especulação ainda não estava engatado
nessa medição; a seção seguinte registra a causa encontrada e a correção
autorizada pelo captain.

### 17.17 Causa raiz do MTP não engatado no pipeline

Antes da correção, uma execução E2E no cluster com `146c2deb` produziu o trace
`chatcmpl-4a70496f44c24917aa68e55fddaa495f`. O pedido terminou completo, mas
teve 89 chamadas de decode e 89 frames, com
`mtp_draft_tokens_proposed=0`, `mtp_draft_tokens_accepted=0` e
`mtp_draft_tokens_rejected=0`. Isso confirma que não houve rodada com lote
MTP, e não apenas uma taxa de aceitação baixa.

A negociação do pipeline não era a causa. No mesmo cluster, o plano atendia
às três condições de `PipelinePlan::supports_mtp`: o head anunciava
`speculate/v1-mtp-sliced` e `speculate/v1-lookup`, e a cauda anunciava essas
duas capabilities mais `speculate/v1-mtp`. A configuração persistida da fatia
head também tinha `mtp_enabled=true` e `mtp_draft_tokens=3`. O health do peer
Salem `02eb8c40` confirmou `mtp_ready=true`, `mtp_allocation="vram"` e
`kv_can_shift=false`; o health do head `0bda91213fe5` tinha `mtp_ready=false`,
como esperado para a fatia não-final, que não carrega o bloco `nextn`.

O bloqueio estava em `infer_server::step_blocking`, nos dois caminhos de
resposta da cauda. `mtp_tail_speculation_reason` recebia
`s.engine.kv_can_shift()` e, apesar de MTP estar pronto, transformava
`kv_can_shift=false` em `hybrid_tail_no_rollback`. Com isso `next_draft` era
omitido antes de chamar `mtp_draft`, e o gateway permanecia no ciclo unitário.
O campo `kv_can_shift` é uma chamada a `llama_memory_can_shift`, relacionada ao
K-shift de `--ctx-shift`; ele não prova se o motor consegue desfazer um lote
especulativo. O rollback efetivo do pipeline usa
`LlmEngine::kv_remove`, que chama `llama_memory_seq_rm`.

A correção removeu o gate por `kv_can_shift` da decisão de produzir
`next_draft`, mantendo as recusas de `mtp_ready=false` e draft vazio. Os dois
caminhos continuam chamando `kv_remove` depois da rodada e falham fechado se a
remoção efetiva for recusada. Assim, um modelo recorrente pronto pode tentar
MTP, mas nunca avança com tokens especulativos que o motor não consiga
reverter. Não houve alteração no wire nem no fork vendorizado do llama.cpp.

O teste de regressão foi primeiro colocado em RED com o caso pronto e
K-shift falso, reproduzindo a razão antiga, e depois passou com a nova
assinatura. Ainda não há número pós-correção de tok/s ou aceitação MTP nesta
seção: ele depende do redeploy dos dois peers. A próxima medição deve registrar
`proposed`, `accepted`, `rejected`, largura dos lotes, TTFT, geração tok/s e
wall tok/s, comparando com a base de 22,57 tok/s de geração, 18,52 tok/s wall
e TTFT de 944 ms da seção 17.16.

### 17.18 Transporte real do Chat e RTT do Painel

A observação do captain foi reproduzida por inspeção do caminho chamado pelo
produto. O Chat envia `POST /v1/chat/completions` com `stream: true`; o handler
`openai::stream_response` chama `gateway::spawn_logged_generation_with_stops`,
que é a mesma geração usada pelo harness. Não existe uma rota alternativa de
inferência para o Chat. Dentro da geração, `inference::relay::send_batch_body`
tenta `CAP_LINK_STREAM` e `link::send_batch_with_session_profile` antes do
túnel libp2p ou HTTPS de fallback. Portanto, o canal persistente de
`15caf764` não era exclusivo do harness. A divergência estava na fonte da
telemetria apresentada ao usuário.

O endpoint `POST /api/ui/chat/telemetry` consultava `RttTable` local e fazia
fan-out para `GET /api/ui/network/rtt` nos peers. Essa tabela é preenchida pelo
monitor periódico da rede com probes `libp2p`; ela é correta para seleção de
rota, mas não identifica o transporte de um frame da inferência. Por isso o
Chat podia mostrar `libp2p` mesmo quando o trace da execução tinha
`https-link`, e o card do Painel repetia o mesmo valor.

A correção faz a rota receber `execution_id`, que é o mesmo id
`chatcmpl-*` usado como `trace_id` e chave da auditoria. O gateway lê o trace
persistido no SQLite, espera brevemente a conclusão da escrita quando a
consulta ocorre no mesmo instante do último SSE, e deriva cada aresta do
`transport_summary` do hop downstream. Para `head -> tail`, o probe sintético
do hop zero não é usado: a medida vem de `hops[1].decode`, que registra a
travessia efetiva entre os peers. O Painel, que não tem um id de requisição,
consulta a última execução concluída e filtra as arestas pedidas. Sem trace ou
aresta correspondente, o retorno fica vazio e a UI mostra desconhecido; não
há fallback que renomeie `RttTable` como execução.

O teste de regressão sem rede semeia simultaneamente uma probe de 900 ms com
transporte `libp2p` e um trace de execução com `https-link` de 6 ms. Tanto a
consulta com `execution_id` quanto a consulta sem id retornam somente:

```json
{
  "from_node_id": "head",
  "to_node_id": "tail",
  "rtt_ms": 6,
  "transport": "https-link"
}
```

Assim, a causa raiz comum com o MTP foi confirmada no nível de integração do
gateway: o caminho de execução real já tinha as capacidades e o transporte
persistente, mas as decisões e a tela consultavam estado local do gateway que
não existe em um gateway puro. O gate MTP agora usa `plan.supports_mtp()` e
trata a ausência de `NodeConfig` local como configuração desconhecida, sem
ignorar um `Some(false)` explícito. Os logs também registram a decisão de MTP,
`plan_supports_mtp`, `remote_stream_capable` e `sliced_mtp` para a próxima
captura de campo.

Arquivos acrescentados nesta rodada:

- `crates/node/src/backend/gateway/budget.rs` e `generation.rs`: gate MTP
  baseado no plano e logs de decisão.
- `crates/node/src/backend/reqlog.rs`: leitura assíncrona do trace exato e da
  última execução concluída.
- `crates/node/src/backend/webui/catalog.rs`: derivação de RTT/transporte por
  aresta e remoção do uso de probes no endpoint do Chat.
- `apps/frontend/src/lib/queries.ts`: envio do `execution_id` e semântica do
  card baseada em execução.
- `crates/node/src/backend/webui/tests/core_b.rs` e
  `crates/node/src/backend/gateway/tests.rs`: regressões de endpoint, hop,
  probe libp2p e gateway sem runtime local.

Validação desta rodada:

- `cargo test -p node --bin pom gateway_sem_runtime_local_usa_capacidade_mtp_remota`: passou.
- `cargo test -p node --bin pom execution_telemetry_uses_downstream_edge_not_gateway_probe`: passou.
- `cargo test -p node --bin pom creates_then_completes_the_same_audit_record`: passou.
- `cargo test -p node --bin pom chat_telemetry_ignores_libp2p_probe_when_execution_trace_exists`: passou.
- `npm run build`: TypeScript e Vite concluídos após a atualização de documentação.
- `cargo test -p node --bin pom`: 1271 testes passados, 5 ignorados.

Overhead: a decisão MTP faz somente leituras já disponíveis do plano e da
configuração local, sem sincronização ou I/O no caminho quente. A leitura do
trace do endpoint é feita em `spawn_blocking` e somente quando a UI pede o
retrato, depois da geração; a consulta do Painel lê no máximo 32 traces
agregados para encontrar uma topologia correspondente. O caminho normal de
inferência não ganhou spans por token nem chamadas de rede adicionais.

Limitação: ainda é necessária uma captura E2E pós-deploy em KSIA e Salem para
confirmar que `mtp_draft_tokens_proposed` deixou de ser zero e medir a nova
geração. A correção de telemetria torna essa captura confiável, mas não
substitui a medição de produção. A tabela `RttTable` permanece deliberadamente
libp2p para o planner; ela não deve ser usada como RTT de execução.

### 17.19 E2E pós-deploy 0c671617: MTP ainda desviava para libp2p

O Chat foi exercitado contra a pipeline implantada com o binário de
`0c671617`, `POM_PROFILE=trace` e uma resposta longa de 199 tokens. A captura
persistida foi a execução `chatcmpl-e4fc6ee32b2847f8b0d2d7287bc7b480`.

| métrica | 0c671617 | base registrada |
| --- | ---: | ---: |
| geração | 21,77 tok/s | 22,57 tok/s |
| wall throughput | 20,43 tok/s | 18,52 tok/s |
| TTFT do servidor | 601 ms | 944 ms |
| MTP proposed / accepted / rejected | 279 / 104 / 175 | 0 / 0 / 0 |
| aceitação | 37,28% | - |

O tempo total desta captura foi 9,74 s para os 199 tokens gerados.

O trace comprovou que MTP já está engatado no gateway real, mas a aresta
`0bda9121 -> 02eb8c40` registrou 94 chamadas downstream como
`libp2p-tunnel`, com RTT mediano de 88 ms (mínimo 74 ms, p95 91 ms). A mesma
consulta autenticada ao endpoint usado pelo Painel retornou
`transport=libp2p-tunnel` e `rtt_ms=88`; portanto o valor de 86 ms observado
antes não era apenas cache visual ou sonda renomeada. O Painel estava refletindo
corretamente o transporte efetivo daquela execução, que ainda estava errado.

A causa raiz comum com o MTP ficou localizada em `relay::send_frame_or_tunnel`.
`send_batch_body` já priorizava `CAP_LINK_STREAM`, mas a seleção do
`BatchCoordinator` recusava qualquer meta com `draft` ou `mtp_k`. Cada handoff
MTP para o peer seguinte caía então diretamente no túnel libp2p. O controle
inicial de `/v1/step-stream` não pode ser alterado dessa forma, pois o receptor
espera seu wire de controle e não `frame/v1-batch`.

A correção remove essa exclusão somente da elegibilidade de lote. O handoff
`/v1/step` MTP agora é encapsulado como lote de um item e passa pela ordem já
existente: canal persistente (`https-link`), túnel libp2p e HTTPS direto como
fallback. `reply_to` e `speculate` continuam impedindo o lote de controle. A
chave do coordenador já separava contratos especulativos, então não há mistura
entre frames MTP e densos.

O teste novo foi observado primeiro em RED com a exclusão antiga e passou após
a correção: `mtp_handoff_can_use_persistent_link_batch_transport`. A regressão
`remote_stream_control_stays_out_of_persistent_link_batch_transport` preserva
o limite do controle inicial. Ainda falta o redeploy desta correção para
confirmar no Chat `https-link`, RTT de execução próximo dos ~30 ms do ping e a
melhora de geração. Os números acima são a medição anterior à correção e não
devem ser apresentados como resultado final.
