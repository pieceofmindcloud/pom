# inference/ - servidor de inferência, wire format e supervisão

> **Mudança de crate:** `llm.rs`, `mtp.rs` (+ `llama_mtp_shim.cpp`),
> `moe/`, `collective.rs`, `device_placement.rs` e `wire.rs` saíram para
> `crates/llama-engine` (ver o `AGENTS.md` de lá). `node` os re-exporta em
> `main.rs`, então `crate::llm::…`, `crate::wire::…` etc. seguem valendo.
> `tp.rs` **ficou** aqui (a coletiva fala com o primary por HTTP/túnel); o
> `llm` alcança o estado dela por `llama_engine::tp_probe`, instalado no
> arranque por `tp::install_probe`. As descrições abaixo dos módulos movidos
> continuam válidas como referência do que cada um faz.

A porta de inferência (8181) e tudo que roda nela: o axum server
(`infer_server/`), o wrapper FFI do llama.cpp (`llm.rs`), a execução do
prefill/step nos modos - hub, anel borbulhado (`relay/v1-chain`) e
resposta direta (`relay/v2-direct`, `direct.rs`) - mais o transporte remoto
por lotes oportunistas (`/v1/step-stream`) - a "lei" que mede o modo
direto em pernas de rede (`direct_lab.rs`), a bancada da especulação
(`spec_lab.rs`), a predição multi-token pelo bloco `nextn` do modelo
(`mtp.rs` + a ponte C++ `llama_mtp_shim.cpp`), o formato binário do tensor na
rede (`wire.rs`) e a
supervisão do servidor Rust embutido (`runtime.rs`), que também mantém a
anotação da fatia servida (estado Pending/Serving/Idle em
`models/lastslice.rs`). Quando a política por deployment pede descarregamento
ocioso, `RequestActivity` é avaliado pelo monitor existente fora do caminho de
inferência; a restauração recebe `reload_model`, espera o `/health` do runtime
e volta a anunciar `serving`. O supervisor só inicia a próxima geração depois
de o supervisor anterior terminar o encerramento, incluindo a liberação nativa
do modelo, e essa liberação roda em `spawn_blocking` para não parar o executor
HTTP. O gateway dispara esse evento em paralelo para os peers ociosos
planejados e limita a barreira a 15 minutos.
O runtime consulta a política de `utils/thermal.rs` a cada 5 segundos, pausa
o modelo acima do limite com estado `thermal`, preserva a configuração para
retomada e só reimplanta depois de todas as GPUs conhecidas permanecerem
abaixo do limite de religar durante o cooldown. `stop` limpa essa recuperação
para respeitar uma desimplantação manual.
As rotas de inferência entre peers usam a `connection_key` compartilhada com o
marcador `x-pom-internal-node: 1` em redes privadas e também no fallback HTTP
público; HTTPS e QUIC podem validar pelo transporte seguro e pelo marcador,
sem depender do segredo. API keys de cliente não atravessam os hops. Cada quadro
reconfere localmente o segredo em tempo
constante, sem chamada extra à rede. A chave pública e a assinatura Ed25519
pertencem ao handshake e à identidade do peer, não substituem a credencial
simétrica do canal privado.
O gateway valida API keys na superfície do cliente e pode consultar o principal
para uma chave desconhecida. Esse fluxo remoto não é alcançado pela porta de
inferência quando falta a credencial interna. Os callbacks de resultado no
gateway são outra fronteira: o `Ticket` por quadro é a credencial deles.
O listener HTTP de inferência é dual-stack, ou cai para IPv4 quando a máquina
não oferece IPv6, independentemente da exposição da interface administrativa:
ele é a reserva quando o túnel libp2p falha.
O listener HTTP/3 usa uma porta UDP dedicada, anunciada em
`inference_quic_port`, derivada do `p2p_port` **deste nó**
(`quic::inference_quic_port` = `p2p_port + 1000`); nunca de
`inference_port`/`gateway_port`, que são consecutivos entre nós co-locados e
fariam a porta QUIC de um nó cair sobre a porta de gateway do vizinho. O
cliente usa os endereços públicos `host:porta` do anúncio e cai para
HTTP/HTTPS se o endpoint não responder.
No handoff entre peers, a preferência QUIC escolhe o endpoint HTTP/3; sem ela,
o link persistente publica `https-link` ou `http-link` conforme o endereço
efetivamente usado. O prefill e o decode usam essa mesma escada, e uma queda
para libp2p ou para HTTP direto permanece registrada com a razão de fallback.
O modelo é carregado **em-processo** por `llm.rs`; não há processo Python
(`inference_server.py` de `old/python-inference/` é legado). Os caminhos
normais recebem `next_hops` pronto do `routing/pipeline.rs`; o TP é a exceção
controlada, pois coordena seus ranks e seu túnel de transporte neste diretório.
O campo `NodeConfig.inference_mode` seleciona `dense/llama.cpp`, `moe/q4_0` ou
`auto` quando essa escolha foi pedida explicitamente; se o campo não existe, o
padrão é `dense`. `POM_DESENV` não participa dessa seleção, pois só controla a
visibilidade dos controles de implantação na UI local.

## Arquivos

- `collective.rs` (novo) - núcleo das coletivas de tensor parallelism (caminho
  C): `reduce_sum` (a soma matemática) e `ReduceBarrier` (barreira que
  acumula os parciais de cada rank e devolve a soma a todos via `oneshot`,
  modelada no `ReplyInbox` de `direct.rs`). O transporte HTTP
  (`POST /v1/tp/reduce`) é uma camada fina, ligada junto com o fork do
  llama.cpp.
- `device_placement.rs` - ordena handles de dispositivos do mais lento ao
  mais rápido somente quando as medições sintéticas têm confiança suficiente e
  todos os handles pertencem ao mesmo backend; sem isso, o fit conserva a
  ordem padrão do llama.cpp.
- `direct.rs` (474) - a resposta direta: a cauda entrega o resultado ao gateway
  por `/v1/relay-result` em vez de borbulhar pela pilha aninhada; também mantém
  o inbox limitado do transporte remoto por lotes. O retorno normal do
  `remote-stream` usa `/v1/link` full-duplex; falha de abertura ou uso cai para
  `/v1/remote-stream-result` e leva motivo explícito de fallback HTTPS. O
  resultado direto pode ser liberado antes de recibos de transporte tardios; o
  `ReplyInbox` retém um snapshot limitado, deduplica por ticket e índice de hop
  e o reconcilia até a expiração.
  As URLs de `reply_to` saem das interfaces locais do gateway, então atrás de
  NAT/CGNAT nenhuma delas vale na outra ponta: `deliver_via_tunnel` é a última
  via, pelo túnel libp2p até o `libp2p_peer_id`/`libp2p_addrs` que o `reply_to`
  passou a carregar. Sem ela o quadro morria calado até o
  `DIRECT_REPLY_DEADLINE`, dez minutos por token. O transporte anotado na
  metadata é `libp2p-tunnel`, para o trace do gateway não publicar como HTTP um
  caminho que não foi. Gateway em binário anterior não manda os campos e a
  entrega segue sem a última via.
- `direct_lab.rs` (580, `#![cfg(test)]`) - a lei: hops falsos falando o
  protocolo real, rede emulada por sítio, exige o mínimo teórico de pernas por
  token para N=2,3,4 com o gateway na cabeça/meio/cauda. O fixture anuncia
  somente capabilities que o endpoint falso implementa.
- `link.rs` (860) - a ligação persistente entre andares
  (`transport/link-v1`, `POST /v1/link`): um corpo de ida em fluxo e um corpo
  de volta em fluxo carregando quadros `frame/v1-batch` sucessivos, em vez de
  uma requisição por ativação. A abertura dá até 400 ms por endereço e no
  máximo 800 ms para testar duas rotas; timeout e recusa entram na memória de
  falha, e a quarentena de 5 min acompanha a memória
  das rotas e evita reabrir o mesmo
  destino depois de uma falha. Os quadros vão num envelope
  `[tamanho][status][batch_id][corpo]`; o `batch_id` - o mesmo do
  `BatchCoordinator` - correlaciona ida e volta e é o que permite várias
  sessões no mesmo canal. Um canal por aresta (`node_id` + URL), aberto sob
  guarda por aresta para duas travessias da mesma aresta compartilharem a
  abertura. A abertura ocorre fora da trava global do mapa. Falha de transporte
  degrada para a escada de sempre; o destino que não abriu fica 5 min em
  quarentena. Recusa HTTP do peer é final e não é repetida por outra via.
  O receptor reconfere a `connection_key` e o marcador interno **a cada
  quadro**, porque uma conexão longa autenticada só na abertura manteria
  válido por horas um segredo que foi trocado por outro processo.
  `send_body_without_response_with_session` é o caminho de produção do
  `remote-stream`: cada frame é enfileirado no
  canal full-duplex sem barreira de ACK por token; a fila limitada mantém
  contrapressão e uma tarefa leitora observa ACKs tardios e falhas do canal.
  `send_body_with_session` fica apenas nos testes que validam o ACK HTTP no
  mesmo canal persistente.
  O controle inicial de `/v1/step-stream` continua fora do lote, pois o
  receptor espera o wire de controle dessa rota. Já o handoff normal de
  `/v1/step` com MTP, que carrega `draft` e `mtp_k`, entra como lote de um
  item: assim ele usa o mesmo canal persistente e aparece como `https-link`.
  A chave do `BatchCoordinator` marca esses lotes como especulativos para não
  misturá-los com frames densos.
  Ver `docs/subsystems/inferencia.md` §2.1.
- `link_lab.rs` (520, `#![cfg(test)]`) - a bancada do custo de rede por ficha
  no salto interno: hops falsos servidos por cima de um `TcpStream` com
  contador, para o número sair dos bytes que passaram no soquete e não de
  estimativa. Compara a mesma cadeia com e sem `transport/link-v1` e guarda a
  comparação (não o tempo absoluto, que depende da máquina).
- `infer_server/` - módulo axum dividido por base/scheduler,
  prefixo/sessões, servidor HTTP, prefill, step/stream, encaminhamento direto
  e step blocking: `/v1/prefill`, `/v1/step`,
  `/v1/step-stream`, `/v1/link`,
  `/v1/session/release`, `/v1/session/rewind`, `/v1/session/rollback`,
  `/v1/session/slot` (save/restore/erase de um snapshot de KV em disco, como o
  `--slot-save-path` do llama-server - ver `inferencia.md` §3.1.1),
  `/health`; estado das sessões, seleção do caminho `dense/llama.cpp` ou
  `moe/q4_0` e o branch de relay. O runtime MoE recebe hidden states antes do
  decode local ou transforma o residual que vai para o próximo hop somente
  quando o caminho selecionado é MoE; no hop terminal a captura do residual
  também chama o bloco MoE antes de amostrar a resposta. O forward é marcado
  depois de retornar. Atenção, logits e demais operações continuam no
  `LlmEngine`, que fornece a fronteira de cálculo restante. Os
  handlers publicam `selected_path`, `executed_engine`, `executed_engines`,
  `moe_executor`, `moe_executors`, `moe_executor_dtypes`,
  `moe_executor_reason` e `moe_forward_ms`, além das flags
  `engine_mismatch`/`mixed_execution` em `node_timings`; `/health` publica
  o caminho selecionado e o estado efetivo do executor MoE
  (`moe_requested`, `moe_executor`, dtypes e motivo), pois não representa uma
  requisição. O
  caminho MoE com executor externo não aplica o runtime aos hidden states uma
  segunda vez: se o hook não estiver instalado, a requisição falha fechado em
  vez de executar experts nativos e externos. O contexto MTP criado sobre o
  modelo metadata-only recebe o mesmo hook no `LlmEngine`; sem isso
  `llama_decode` do contexto `nextn` falha e a sessão perde MTP.
  Para o executor MoE CUDA, o `infer_server` passa os compute buffers medidos
  por ordinal ao runtime, que cria uma shard residente por `CUDA<n>` e publica
  os dispositivos no diagnóstico. A primeira rota real cancela o aquecimento
  oportunista para que ele não expulse o working set enquanto uma requisição
  está em voo.
  O metadata do primeiro prefill também carrega `thinking_enabled`, a escolha
  por inferência do Chat. A cabeça combina esse valor com `no_thinking` antes
  do template e os caminhos hub, anel e direto o preservam nos relays.
  estado é acessado por `lock_inference_state`: um mutex envenenado vira erro
  HTTP ou falha da requisição em lote, nunca um novo panic em `unwrap`. EOS
  fecha apenas o acumulador da resposta; a sessão permanece disponível para
  rewind até o release explícito ou a coleta por ociosidade.
  Os handlers de prefill, step e step-stream seguram um `LoadGuard` no tracker
  compartilhado com o gateway enquanto há computação em voo; por isso o
  `/api/ui/node` da própria máquina pode publicar a carga real, inclusive
  quando a requisição chegou diretamente à porta de inferência.
  `PhaseTrace` transporta também `node_moe_forward_ms`, que é o custo do
  forward externo fora de `llama_decode` e fecha a conta do backend. `cuda`
  só é publicado depois do forward NVFP4 real; `cpu-reference` inclui os
  dtypes GGML e o motivo da ausência de ponte CUDA. O `PhaseTrace` do gateway
  preserva esses campos por hop e por fase; `not-run` expõe a ausência de
  execução. Quando o runtime CPU suporta o formato, ele é instalado como
  executor externo, sem fallback nativo implícito.
  Os handlers decodificam o frame e entregam
  comandos a uma fila local limitada; o worker seleciona uma entrada por
  sessão e pode chamar o decode multi-sequência. Cada `Session`
  mantém seu próprio
  `Utf8Assembler`: `token_to_piece_bytes` preserva os bytes crus, só as
  posições aceitas alimentam o acumulador e o `flush` ocorre no EOS ou na
  limpeza de erro/cancelamento. `/v1/step-stream` inicia um produtor no peer
  remoto e um emissor separado: cada um usa a mesma decodificação existente,
  mas os tokens retornam ao gateway em lotes oportunistas limitados, com
  timeout de entrega de 500 ms e contrapressão.
  `node_timings` publica a captura em limites explícitos: ask-to-ready do
  scheduler, wall da chamada `ggml_backend_tensor_get`, pós-processamento e
  encode do payload e do frame residual. Ask-to-ready inclui compute e sync do
  scheduler; `tensor_get` em CUDA pode incluir D2H e sync da stream, sem expor
  esses custos internos separadamente. O wire mede o loop de conversão f8
  separado da serialização do cabeçalho/frame.
- `llm.rs` (1500) - wrapper llama-cpp-sys-2: carga, decode, hidden state com
  inicializa o backend para que o sysmon possa consultar descricoes de
  dispositivos, decode, hidden state com
  residual capture, sampling, parada por EOS ou fim de turno do template GGUF,
  renderer Gemma4 derivado de `tokenizer.chat_template`, cache KV tipado (`type_k`/`type_v`) e o dono da
  cabeça de rascunho `nextn` (`Option<MtpDrafter>`). `token_to_piece_bytes`
  devolve a peça sem conversão lossy para o acumulador da sessão.
  Quando `fit_memory` está ativo e há mais de um dispositivo elegível, mede
  uma vez por handle um grafo GGML genérico e passa ao fit uma lista explícita
  do mais lento ao mais rápido somente se todas as diferenças forem confiáveis
  e os handles forem do mesmo backend. Uma combinação heterogênea ou um nome de
  backend ausente conserva a ordem padrão do llama.cpp, pois a ordem explícita
  pode alterar a topologia de cópia e divisão do grafo entre backends.
  A lista fica no `FitPlan` até a carga real, fazendo o último dispositivo
  receber a cabeça de saída e a faixa final. Resultado ausente, equivalente ou
  inválido mantém a seleção padrão do llama.cpp, inclusive sem microteste para
  um único dispositivo.
  `fit_plan` registra o snapshot de memória retornado pelo backend, a margem
  efetiva, o orçamento por dispositivo e o uso de VRAM do próprio PID quando
  `nvidia-smi` o fornece.
  `fit_plan` é exclusivo do caminho de carregamento real: ele chama a FFI para
  ajustar camadas, contexto e divisão entre dispositivos e mantém os ponteiros
  vivos até `LoadedModel::load`. A pré-visualização de capacidade do deploy não
  chama mais esse caminho e não tem `FitPreview`: usa a conta analítica de
  layout GGUF, peso, KV e VRAM livre no
  `webui::capacity_preview_endpoint`. `runtime::engine_config_from_node`
  continua sendo a conversão `NodeConfig -> EngineConfig` usada pela carga
  real e pela montagem dos parâmetros da estimativa; `need_embeddings` continua
  parâmetro porque a carga real também soma `inference_path.is_moe()`, e o
  executor externo de MoE fica de fora da pré-visualização. A reserva MTP não abre um modelo nem constrói um
  grafo especulativo: para arquiteturas conhecidas, `fit_plan` calcula somente
  o KV determinístico dos blocos `nextn` a partir da metadata GGUF, e o coloca
  no último dispositivo da ordem slow-to-fast. O custo de compute fica para a
  criação real do contexto, que pode recuar para a RAM sem abortar o processo.
  A captura identifica o eixo de tokens pela quantidade conhecida de tokens do
  microlote físico e pela forma do tensor. `n_embd_inp` valida a forma 2D ou
  com eixos extras singleton; quando há um eixo extra não singleton,
  `n_embd_out` valida a largura física expandida reportada pelo llama.cpp. Essas
  larguras não participam da identificação do eixo: todos os eixos anteriores
  ao eixo de tokens são features e são preservados na ordem física, publicando
  o shape canônico `[tokens, features...]`. Eixos posteriores maiores que 1
  são recusados e eixos posteriores singleton são ignorados. Se mais de um
  eixo ainda for candidato, o menor eixo que fecha os strides contíguos é
  escolhido deterministicamente. A acumulação continua anexando microlotes na
  ordem produzida com `kv_unified=true`. Shape ou stride que não fecha essa
  geometria é recusado com diagnóstico explícito. `decode_hidden` e
  `decode_hidden_batch` usam a largura validada para dimensionar o batch Rust.
  `decode_tokens_batch` e `decode_hidden_batch`
  constroem um `llama_batch` com sequências independentes e devolvem o layout
  para mapear logits e residuais por item.
- `scheduler.rs` - limites de admissão, janela de microbatch e seleção FIFO
  round robin. A janela tem default de 0 ms para não atrasar uma chamada
  isolada, mas continua configurável para workloads que priorizam coalescer
  chegadas. Cada processo cria sua própria fila; réplicas da mesma faixa não
  compartilham scheduler, KV ou capacidade de fila.
- `mtp.rs` (445) - predição multi-token (MTP): contexto de rascunho
  `ctx_type = LLAMA_CONTEXT_TYPE_MTP` sobre os pesos do alvo na VRAM; um
  modelo irmão CPU-only no recuo para RAM; e o algoritmo
  `observe`/`accept`/`draft` portado de `common_speculative_impl_draft_mtp` do
  llama.cpp. O buffer `h` acompanha `n_embd_out`, porque o contexto MTP expõe a
  largura física de saída quando a arquitetura tem eixos de fluxo. `observe`
  passa posições, tokens e estados ocultos explícitos em um batch que pode ser
  dividido internamente em microlotes sem perder o pareamento; uma falha ao
  anexar ou observar guarda o motivo e aparece em `mtp_ready`/
  `mtp_unavailable_reason` no `/health`.
  Quando pronto, `mtp_allocation` diz se o contexto ficou na VRAM ou na RAM.
  O `fit_plan` encaixa primeiro o modelo principal e seu contexto; só aceita a
  reserva do segundo contexto se o resultado conjunto preservar todas as
  camadas principais na GPU. Quando não cabe, o alvo fica sem pesos `nextn` na
  VRAM e, se permitido, carrega o modelo MTP CPU-only com
  `offload_kqv=false` e `op_offload=false`. Um override só nos pesos `nextn`
  não basta porque o scheduler também pode escolher o dispositivo da camada e
  do output head. Antes do fit MTP, `llm.rs` lê `general.architecture` e aplica
  uma lista gerada pelo próprio fork para as arquiteturas cujo grafo
  `LLM_GRAPH_TYPE_DECODER_MTP` existe. `build/mtp_arch.rs` encontra a
  verificação efetiva nos arquivos de modelo, liga a verificação ao escopo de
  `build_arch_graph`, liga a classe ao dispatch de
  `llama-model.cpp` e resolve o nome GGUF em `llama-arch.cpp`. Comentários
  que apenas mencionam o símbolo não contam. A API pública do llama.cpp não
  expõe uma consulta de suporte; arquitetura desconhecida fica indisponível,
  com razão no `mtp_unavailable_reason`, antes de qualquer contexto MTP. Fork
  ausente, extração vazia, menor que treze entradas ou inconsistente faz o
  build falhar ruidosamente. Dispatch duplicado também interrompe o build. O contexto real MTP desliga explicitamente `embeddings`, pois
  sua saída é logits e `h_nextn`, não embeddings genéricos. MTP desligado não
  lê essa metadata, não estima orçamento e não tenta anexar contexto.
- `llama_mtp_shim.cpp` (48) - ponte `extern "C"` para
  `llama_{set,get}_embeddings_nextn` e `llama_set_nextn_layer_offset`, que
  moram em `llama.cpp/src/llama-ext.h` e por isso **não** estão nos bindings do
  `llama-cpp-sys-2` (o `wrapper.h` dele só inclui `llama.h`). Compilada pelo
  `crates/node/build.rs`.
- `prefill/` - suporte e telemetria, pipelines de prefill, steps/especulação e
  testes: `run_with_options` (hub) / `run_ring` (anel) / `run_direct`
  (direto), `step_speculative`, as duas formas de resposta da especulação, traço
  de fases por requisição. `Cancellation` cancela a operação do gateway quando o
  consumidor desaparece, antes do próximo chunk ou hop e também durante a
  espera HTTP; um frame já aceito pelo peer não recebe um novo header de fio.
  O prefill de continuação carrega `reuse_prefix` no primeiro chunk e
  `start_pos`/`next_pos` absolutos nos chunks seguintes, para que os peers que
  recebem hidden state mantenham a mesma posição KV da cabeça. O chunk 0 pode
  omitir `start_pos` porque a sessão fornece `n_past`; qualquer chunk posterior
  sem posição é erro, nunca uma posição inventada pelo índice do chunk. O
  `reuse_prefix` que os hops seguintes recebem é o que a **cabeça executou**,
  lido da resposta dela (`cached_tokens_of`, `head_reused_prefix`), não o que o
  gateway pediu. Intermediários em uma cadeia N-hop ecoam essa decisão em vez
  de recalculá-la. A resposta parcial da cabeça, tanto no caminho individual
  quanto no lote, também carrega `n_prompt_tokens`, `start_pos` e `next_pos`
  absolutos, inclusive no primeiro chunk de um modelo inteiro.
  `PrefillOutcome::cached_tokens` devolve ao gateway quantos tokens de prompt
  não foram reprocessados; `retained_tokens` devolve a fronteira estável que
  ficou na `prompt_seq` e que o gateway confirma no rewind. Hub e resposta
  direta rodam o prefill como um pipeline em ordem: uma bomba da cabeça
  despacha os pedaços em série e uma bomba de downstream (`FuturesOrdered`)
  os leva pelos hops restantes, até `W` em voo. `PrefillOverlapWindow` limita
  por contagem (`prefill_max_inflight_chunks`, efetivo 1..8, padrão 8) e por
  bytes de estado oculto em RAM no gateway (`INFLIGHT_BYTE_BUDGET`, 512 MiB);
  o permit de cada limite viaja com o pedaço. A janela cancela as futures em
  voo no erro ou no cancelamento e preserva a ordem do resultado. Um pedaço
  `k>0` que falha depois de a cadeia aceitar os anteriores anexa um
  `PrefillResumable` ao erro para diagnóstico, mas o gateway não repete esse
  ponto depois de uma falha de transporte: a resposta perdida não prova se o
  peer já gravou o chunk, e a reentrada poderia pedir remoção parcial de KV.
  A recuperação segura recria a sessão inteira a frio, uma vez; se ela falhar,
  o cleanup libera os hops alcançados. Mesmo sem ponto de retomada, o erro
  carrega o `RequestTrace` parcial observado até a falha para a auditoria; isso
  não muda a retentativa nem preenche tempos ausentes. O modo anel continua
  serializado pelo contrato aninhado e ignora a janela. A telemetria
  também registra `prefix_match_tokens`, `prefix_reuse_status`,
  `prompt_tokens_transmitted`, `prefill_pipeline_depth_max` e transporte por
  travessia. `RequestTrace` acrescenta o `trace_id`, spans locais do gateway,
  campos explícitos de `outside_node` somente quando há residual medido,
  estado cold/warm, conexões novas/reutilizadas,
  marcos do primeiro token e contadores de decode. `RequestTrace::telemetry`
  separa `response_complete` de `trace complete`: publica peers e spans
  esperados/observados, duplicatas, atrasos e o estado
  `collecting`/`response_complete`/`complete`/`incomplete`. A soma de compute e
  transporte é acumulada, nunca subtraída do wall-clock para fabricar
  `outside-node`; overlap e residual verdadeiro só aparecem quando há
  intervalos explícitos. O `remote-stream` carrega os `hop_timings` do head e
  dos peers downstream, além de contadores e histogramas limitados de frames,
  espera e processamento remoto. O produtor usa uma fila limitada de 64 itens
  e o emitter separa `remote_stream_wait` em produção, reserva do canal, fila
  e callback anterior, sem timer artificial. `remote_stream_downstream_wait_ms`
  identifica o relay aninhado dentro da produção; `node_timings.mtp_draft_ms`
  identifica o custo síncrono do nextn. Em plano remoto fatiado, MTP é omitido
  quando imporia esse draft a cada callback, enquanto o caminho legado continua
  elegível. `handle_step_stream` copia o
  `session_id` do frame de controle para o `reply_to`, e cada lote de retorno
  o recoloca no envelope antes de chegar ao ticket. O retorno normal usa o
  canal persistente e só o fallback usa HTTPS por callback, com contagem e
  razão visíveis. O modo normal não
  serializa spans por token;
  `POM_PROFILE_SYNC=1` acrescenta spans de frame e eventos de token com
  IDs de trace, request, pipeline e peer. Eventos de token sem intervalo
  individual permanecem com duração zero, sem repetir artificialmente o custo
  do lote. O perfil `off|basic|trace` controla apenas o detalhamento novo e
  não substitui `POM_PROFILE_SYNC`. O agregado `pipeline_cycle` guarda as
  travessias em sua direção lógica (`gateway -> cabeça`, avanços e retorno),
  por aresta e fase, sem dividir RTT por dois ou somar uma volta inexistente.
  No `remote-stream`, os avanços são reconstruídos dos `hop_timings` e o
  callback registra o peer emissor e o transporte efetivo; como o callback
  persistente não espera ACK, seu RTT permanece ausente até existir uma
  observação correspondente.
  No modo direto, `expected_decode_spans=0` significa que o total esperado não
  foi medido; `decode_span_expectation_available=false` impede a UI de
  apresentar uma fração `N/0` como cobertura completa.
- `prefix_cache.rs` - cache global de sessões KV por modelo e rota, com leases
  ocupados, maior prefixo textual e LRU apenas entre entradas ociosas. Ele não
  contém peso, tensor ou transporte de arquivo. A chave textual é o prompt
  renderizado canônico (`openai::conversation_cache_key`), na mesma ordem que a
  cabeça tokeniza. `PrefixLease` identifica miss por cache vazio, sessão
  correspondente ocupada, prefixo divergente ou rota alterada. No
  caminho Anthropic, `conversation_cache_key_with_budget` também inclui a
  identidade de `task_budget` para impedir que o KV de outro marcador seja
  reutilizado. `PrefixCache::adopt` registra uma sessão restaurada do disco
  como hit ocupado. `PrefixDiskIndex` (Phase 3) é a camada de disco opcional:
  índice `(rota, prompt, tokens, bytes, updated_at)` por `key_hash`, persistido
  em `prefix-auto-index.json` no `slot_save_path` do gateway. `record` supersede
  os ancestrais de prefixo estrito da mesma rota (uma linhagem, um `.bin`),
  aplica TTL configurável (`slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS`,
  padrão 7 dias) e orçamento LRU (`slot_auto_max_bytes`, padrão 2 GiB),
  devolvendo os `key_hash` a apagar. O gateway ainda faz debounce por
  `SLOT_AUTO_MIN_GROWTH_TOKENS` antes de regravar. Ligado por
  `slot_persist_prefix_cache`; ver `inferencia.md` §3.1.1.
- `prefix_cache_lab.rs` - autoteste diferencial da política do cache, ou seja da
  **escolha**. A referência acompanha lookup, insert, busy, release e eviction;
  o proxy propositalmente quebra `cached_tokens` e a bateria precisa registrar a
  divergência. Nenhum defeito de execução é alcançável por ele.
- `prefix_reuse_lab.rs` - a bancada da **execução**: uma conversa de quatro
  turnos pelo caminho de `/v1/chat/completions`, nos três transportes (hub,
  anel, direto). Afirma que do turno 2 em diante
  `cached_tokens > 0`, que o trabalho por turno para de crescer com a conversa,
  registra o tempo até a primeira palavra e verifica a queda pelo `prefill_ms`
  declarado pelo hop, não pelo relógio de parede sujeito ao escalonamento do
  processo. Também verifica que nenhum hop recebeu pedido de remoção parcial de
  KV. O contrato de posição repete anel e resposta direta com N=3 e N=5 hops;
  o teste SSE de `openai.rs` reproduz a sessão em 42 sem `next_pos` e impede o
  envio de chunk 1 em 512. Os hops falsos imitam o motor mais restrito que o
  produto atende - o recorrente, que recusa remoção parcial - e mantêm a
  fronteira estável do prompt separada da sequência de geração. A bancada usa
  as funções de produção nos pontos de decisão
  (`plan_prefix_reuse`, `reuse_prefix_for_forward`,
  `overlay_head_authoritative_fields`, `prompt_progress_for_forward`).
  Também simula um hop que perde a sessão no meio do prefill: o gateway repete
  o prefill completo a frio uma única vez, para reconstruir o KV desde o prompt,
  e a bateria verifica que uma segunda perda termina com erro limitado e útil,
  contando no máximo duas tentativas frias no chunk 0, sem usar o total de
  frames como proxy da limpeza durante a janela sobreposta.
  O cenário de corte no meio de um prefill confirma também que nenhum hop
  recebe remoção parcial e que as sessões remanescentes são liberadas.
- `moe/` - executor próprio de experts. `cache.rs` mantém um LRU global por
  chave de modelo/camada/expert/formato, com leases ativos e eviction apenas
  ociosa; `admission.rs` calcula o orçamento a partir de memória livre medida;
  no caminho CUDA, a admissão também preserva uma reserva derivada do compute
  buffer CUDA medido pelo `llama.cpp`, com margem relativa e recusa nominal se
  o relatório não existir ou o expert não couber;
  `format_q4_0.rs` compartilha o parser GGUF, o inventário de dtypes e o
  decoder Q4_0, `format_plain.rs` implementa os bancos escalares F32/F16/BF16,
  `format_legacy_quants.rs` implementa Q4_1/Q5_0/Q5_1/Q8_0,
  `format_low_bits.rs` implementa MXFP4/TQ1_0/TQ2_0/Q1_0/Q2_0,
  `format_k_quants.rs` implementa Q2_K/Q3_K/Q4_K/Q5_K/Q6_K/Q8_K, `format_iq_quants.rs`
  implementa IQ2_XXS/IQ2_XS/IQ3_XXS/IQ1_S/IQ4_NL/IQ3_S/IQ2_S/IQ4_XS/IQ1_M,
  com IQ4_NL usando blocos de 32 elementos e os demais IQ blocos de 256,
  `format_nvfp4.rs` implementa o decoder NVFP4 em CPU; o rastro completo
  `Nvfp4Trace`/`run_trace` fica restrito à prova diferencial de testes, enquanto
  o runtime consome somente a saída do expert; `cuda.rs` mais
  `cuda_kernels.cu` fornecem executores CUDA dirigidos por dtype quando a
  feature `cuda` está ligada: NVFP4, F32 (0), F16 (1), BF16 (30), Q4_0 (2),
  Q2_K (10), Q3_K (11), Q4_K (12), Q5_K (13), Q6_K (14), IQ3_XXS (18),
  IQ3_S (21), IQ2_S (22), IQ4_XS (23) e MXFP4 (39). Q8_K (15) fica de fora
  por ser formato de ativação, nunca de peso de expert.
  Todas as entradas FFI do executor externo recebem o ordinal da shard e fixam
  alocações, streams e eventos nessa GPU, restaurando o dispositivo corrente do
  chamador ao retornar. O cache mede cada `CUDA<n>` separadamente e as rotas
  agregam em paralelo os parciais das placas envolvidas. Isso é obrigatório
  porque o callback roda dentro do scheduler e pode ser seguido por uma cópia
  ou espera do llama.cpp em outra GPU.
  A ponte heterogênea usa descritores por papel e dtype para esses formatos, sem
  converter a combinação para um dtype global. O hook comum recebe a rota do llama.cpp já
  em ordem token-major, a ordem física produzida por `ggml_get_rows`; `llm.rs`
  valida e preserva essa ordem antes do runtime. A ponte escalar reutiliza o
  payload de `format_plain.rs` e valida o cabeçalho antes do kernel.
  K-quants e IQ-quants validam a convenção GGUF
  `[columns, rows, experts]` e aceitam bancos gate/up fundidos ou separados
  antes de entregar o payload ao cache. A expansão IQ usa as primitivas GGML
  linkadas, enquanto cache, router e mat-vec permanecem no executor próprio.
  `format_heterogeneous.rs` é a fronteira para GGUF UD e outros arquivos que
  misturam dtypes: o inventário registra cada tensor por camada e papel, e o
  decoder é escolhido pelo dtype daquele tensor. O callback recebe a metadata
  do fork do llama.cpp e valida nome, geometria e dtype contra esse inventário;
  uma divergência expõe as duas geometrias concretas. A execução CPU em lote
  agrupa as rotas por expert, descodifica cada linha uma vez e preserva a ordem
  token-major na saída, inclusive para combinações como IQ3_XXS + MXFP4 do
  `deepseek4`. Uma combinação sem decoder externo é recusada antes da carga,
  nomeando os dtypes, sem fallback nativo silencioso.
  Quando `ffn_gate_up_exps.weight` e os tensores split coexistem, o inventário
  segue a prioridade do llama.cpp e usa somente o fundido, sem contar os
  duplicados como uma segunda rota. Os loaders K e heterogêneo resolvem os
  sidecars canônicos `.scale` e `.input_scale` por expert, carregam os valores
  no payload/descriptor e aplicam as escalas nos estágios gate, up e down.
  O helper `llama_moe_reshape_route_2d` do fork materializa sempre a rota com
  `ggml_cont` antes do reshape: `ggml_is_contiguous` sozinho não prova que a
  área do produtor já está pronta para a cópia entre backends. Além disso,
  `llm.rs` recusa hidden de entrada e saída do executor com NaN ou infinito,
  identificando camada e índice antes de deixar o valor contaminar o próximo
  hop. Isso torna observável uma falha numérica, mas não substitui a prova do
  kernel CUDA. `qwen35moe` mantém
  `ffn_*_shexp` fora do inventário: o callback recebe apenas `ffn_*_exps`, e o
  expert compartilhado continua no `build_ffn` nativo.
  Na carga do primeiro `LoadedModel` do processo, o servidor registra uma vez,
  no target padrão, o valor bruto de `POM_MOE_ROUTE_DIAGNOSTICS` e o booleano
  efetivo do gate, antes de qualquer callback MoE. O teste ignorado
  `callback_global_emite_diagnostico_em_thread_externa_e_registra_gate` instala
  o mesmo dispatcher global, cria uma thread fora do Tokio e exige as linhas
  default e `moe.route_diagnostic` no mesmo sink.
  Os testes `expert_shared_nao_entra_no_inventario_de_rota` e
  `inventario_de_rota_separa_exps_de_shexp_e_dos_sidecars` verificam que somente
  os quatro nomes exps terminados em `.weight` entram na classificação, sem
  contar `shexp` nem sidecars. O teste
  `sidecars_fundidos_usam_componente_e_fallback_separado_corretos` verifica os
  nomes canônicos e a associação componente 0/1 do fundido, além do fallback
  para gate/up separados. A prova `q6_k_decodifica_bloco_ggml_real_com_referencia`
  fixa a convenção `block_q6_K` do GGML (`ql[128]`, `qh[64]`, `scales[16]`,
  `d[2]` no fim do bloco), e as provas de oito experts com hidden 2048 cobrem
  o `gate_up` K-quant e o caminho heterogêneo sem aceitar saída não finita.
  A prova diferencial CUDA do MoE exige GGUF completo e GPU CUDA reais, portanto
  fica fora da validação local; ver `TODO.md`, item 17.
  `bandwidth.rs` e `coexecution.rs` são compilados no caminho CUDA ou nos
  testes; transformam medições concorrentes CPU/transferência em uma fração
  determinística dos misses, dividem por expert, executam as duas lanes em
  paralelo e somam saídas com largura validada.
  A ponte IQ CUDA tem diagnóstico diferencial por dtype e estágio nos testes
  `executor_cuda_iq_preserva_iq3s_iq2s_e_iq4xs_do_decoder_cpu` e
  `diagnostico_cuda_iq_divergencia_por_estagio`. A implementação da ponte não
  é prova de correção por si só: a prova CUDA na `ksia` comparou IQ3_S, IQ2_S
  e IQ4_XS contra o decoder CPU em `gate_up`, ativação e `down`, sem primeira
  divergência. Os três dtypes podem ser publicados pelo dispatch somente após
  essa prova; uma divergência futura deve reativar a recusa nominal, não cair
  silenciosamente em outro formato.
  Não usar um dtype global nem adicionar ramo por arquitetura ou modelo.
  Antes da carga, `MoeRuntime::external_executor_plan` publica ownership por
  camada: camadas com ponte device ou decoder CPU próprio têm seus experts
  omitidos do llama.cpp; dtypes sem cache device e combinações heterogêneas
  fora do conjunto CUDA validado usam `CPUReference` e registram o dtype na
  telemetria. Somente NVFP4 sem CUDA e sem pedido explícito de CPU permanece
  indisponível com motivo visível. Q4_0 e Q5_K usam a ponte heterogênea CUDA
  quando a feature está disponível; sem ela, o decoder CPU continua funcional,
  mas o log de inicialização registra o
  `CPUReference` efetivo, os dtypes e a razão, além dos campos equivalentes em
  `/health`. **Basta um único dtype sem ponte para derrubar a rota inteira
  para `CPUReference`**, e quants dinâmicos (UD) misturam dtypes por papel de
  expert: confira a lista `dtypes` da telemetria, nunca o nome do ficheiro.
  O fork mantém a lista como metadata
  copiada no modelo e o grafo escolhe external/native pela camada, nunca por
  uma flag global que possa deixar experts sem executor.
  A telemetria `moe_executor_diagnostics` expõe automaticamente, quando o
  executor CUDA MoE está ativo, chamadas, hits/misses do LRU, bytes H2D/D2H e
  tempos de alocação, kernel, sincronização, liberação, busca/admissão e
  carregamento de experts no host, preparação do lote e cópia dos resultados
  para vetores Rust. `POM_MOE_CUDA_PROFILE` é legado e não é necessário nem
  altera a política do cache.
  Para investigar a origem de uma rota inválida, `POM_MOE_ROUTE_DIAGNOSTICS=1`
  habilita duas linhas adjacentes, `callback_entry_default` no target padrão e
  `callback_entry` no target `moe.route_diagnostic`, como primeira ação do
  callback, antes de qualquer validação e sem filtro de camada. A dupla existe
  para provar no mesmo ponto se o evento chega ao sink e se um filtro por target
  o remove. Ambas registram os ponteiros e contagens crus recebidos. Todo
  retorno `false` também emite
  `callback_exit` com o motivo estável do caminho que abortou, inclusive
  argumentos nulos, geometria, normalização, metadata e forward. A primeira
  chamada de cada requisição recebe a amostra detalhada;
  qualquer chamada com rota não finita também recebe essa amostra, mesmo que
  outra já tenha sido registrada. O log registra os valores crus de token 0 e
  seus deslocamentos na ordem token-major produzida por `ggml_get_rows`,
  contagens declaradas pelo llama.cpp e esperadas pelo POM, endereços e bytes
  lidos, dtypes da metadata de cada tensor e os dtypes assumidos pelos
  argumentos do callback. Também registra a
  mesma amostra depois da normalização Rust. Sem a variável, o instrumento fica
  desligado e não altera o caminho de execução. A normalização valida e
  preserva essa ordem; não deve transpor a rota novamente. O teste ignorado
  `llm::moe_route_tests::callback_entry_emite_warn_padrao_e_target_moe_no_mesmo_sink`
  executa o callback real com o mesmo `EnvFilter` do processo e deve ser rodado
  com `POM_MOE_ROUTE_DIAGNOSTICS=1 RUST_LOG=info`; a cobertura opt-in está em
  `TODO.md`, item 18.
  O runtime registra sempre a entrada da camada 0 em
  `moe.layer_input_diagnostic`, com contagem/primeiro índice não finito, faixa
  finita, amostras de hidden e rota, antes de escolher CPU ou CUDA. Isso separa
  contaminação a montante de falha em um executor.
  `cuda_kernels.cu` reproduz a convenção ggml de Q8_0 para as ativações,
  acumulação inteira por subbloco NVFP4 e decodificação direta dos superblocos
  Q4_K/Q6_K com SiLU no caminho K. Em Q6_K, ambos os decoders leem o layout
  `ql[128]`, `qh[64]`, `scales[16]`, `d[2]`, com `d` nos dois últimos bytes. O
  caminho CUDA agrupa os experts ativos de uma rota em índices `(expert, linha)`;
  depois do cache residente, o payload de peso é enviado uma vez por miss e o
  lote recebe ponteiros de device nos hits; cada lease também conserva o
  tamanho da alocação para validar a geometria antes do kernel K/Q6_K. O
  descritor heterogêneo carrega rows/columns da fatia de um expert, sem contar
  o banco inteiro, e a rota deriva sua geometria do down dessa fatia. O lote de
  rotas para múltiplos tokens envia ids, pesos e offsets uma vez por camada e
  agrega a saída no device; o caminho unitário continua disponível para o
  executor residente.
  O lote de rota usa seis lançamentos de kernel e duas sincronizações. O perfil publica `cuda_call_wall_ms`
  como parede externa não sobreposta da chamada FFI, separada de H2D, kernel,
  lançamentos, sincronização, D2H e liberação, que são submedições internas. O runtime
  também publica `runtime_forward_ms`, `host_batch_prepare_ms`,
  `host_result_copy_ms` e `host_aggregation_ms` para fechar a conta do forward
  MoE sem somar relógios sobrepostos.
  Alterações em `cuda_kernels.cu` só são validadas com compilação CUDA na
  máquina que tem `nvcc`; compilar o crate Rust no macOS não compila esse
  arquivo. O descriptor `CudaHeterogeneousTensor` mantém offsets ABI
  explícitos para `data`, `bytes`, `dtype`, `scale`, `input_scale`,
  `secondary_scale` e `secondary_input_scale`, além de asserções de offset e
  tamanho nas duas pontas.
  A descompactação NVFP4 e K-quant ainda ocorre no kernel a cada uso, mas os bytes quantizados
  do expert ficam residentes no device entre usos quando a ponte CUDA e o
  orçamento medido permitem. O cache frio de RAM e o cache quente de device têm
  estatísticas separadas. Depois de qualquer mudança
  CUDA, execute a build com
  `--features vulkan,cuda` na ksia e repita tanto os testes CUDA diferenciais
  quanto as provas reais `executor_cuda_nvfp4_lote_preserva_geometria_do_gguf_real`
  e `nvfp4_matches_llama_cpp_from_first_layer_and_stage`.
  Os loaders aceitam tanto `ffn_gate_up_exps.weight` fundido quanto
  `ffn_gate_exps.weight` e `ffn_up_exps.weight` separados; `router.rs` e
  `block.rs` fazem top-k e soma ponderada no bloco MoE. `runtime.rs` detecta o
  dtype dos experts, seleciona o decoder validado e liga o bloco ao hidden state
  nos caminhos unitário e batch. O teste ignorado
  `mede_banda_h2d_d2h_do_lote_nvfp4` mede H2D, D2H, kernel e parede externa em
  geometrias pequena e Gemma4 após o aquecimento do cache de device. O perfil
  agora usa dois slots CUDA persistentes, streams separados e buffers host
  pinned para alternar grupos de tokens; `buffer_mask`, `buffer_switches`,
  `kernel_span_ms` e `transfer_span_ms` distinguem sobreposição de trabalho
  apenas somado. `CudaTransferProfile` lê no runtime o sidecar opcional com as
  quatro bandas medidas; a criação e persistência do sidecar pertencem às
  bancadas de teste, e `runtime.rs` passa esse perfil ao NVFP4. Em uma rota
  NVFP4 CUDA,
  experts residentes ficam na GPU, misses são divididos pela fração medida e
  as lanes CPU e GPU executam em paralelo; a telemetria publica chamadas de
  cada lane e a fração escolhida. A validação CUDA e os números observados
  vivem em `docs/subsystems/freetoken-moe.md`.
  O wire continua em f16 e nenhum peso trafega; a costura não substitui ainda
  atenção, demais camadas ou a amostragem do `LlmEngine`.
  Quando a ponte CUDA existe, `MoeRuntime::start_device_warmup` pode percorrer os
  payloads canônicos por camada, expert, papel e dtype em uma thread separada,
  usando a mesma admissão do cache residente. O aquecimento é padrão e pode ser
  desligado explicitamente por `POM_MOE_WARMUP=0`, `false`, `off` ou `no`. O
  orçamento por placa continua derivado da memória livre e da reserva do
  compute buffer medido; quando ele se esgota, o warmup termina com
  `budget-exhausted`, pois não é possível manter todos os experts de um modelo
  grande na VRAM. Se o modelo tiver um sidecar
  `<modelo>.cuda-profile.json` com `bandwidth`, o bloco heterogêneo reaproveita
  a mesma fração medida do FreeToken para executar misses em lanes CPU e GPU
  sobrepostas; sem sidecar, mantém a política exclusivamente CUDA e não
  fabrica uma constante de hardware.
  `warmup_status`, bytes, candidatos e erros chegam em
  `moe_executor_diagnostics`. O prazo é `POM_MOE_WARMUP_MAX_MS` (30 s por
  padrão). O callback de inferência apenas sinaliza o cancelamento cooperativo
  entre payloads; ele não aguarda a thread de warmup. O `join` fica reservado
  à troca explícita de warmup, fora do caminho quente.
  Fontes sem ponte device não aquecem nem removem experts do llama.cpp.
  A prova numérica `nvfp4_matches_llama_cpp_from_first_layer_and_stage` é
  ignorada por depender de GGUF real: com `POM_MOE_NUMERIC_MODEL` e
  `POM_MOE_NUMERIC_ACTIVATION`, ela compara camada a camada `gate`, `up`,
  ativação e `down` do decoder CPU com o oráculo ggml do llama.cpp antes de
  qualquer comparação CUDA. O primeiro estágio divergente deve ser reportado,
  não corrigido silenciosamente.
  No NVFP4, o parser respeita a ordem GGUF `[input, output, experts]`: gate/up
  são `[hidden, intermediate, experts]` e down é
  `[intermediate, hidden, experts]`. A inversão desses eixos faz o runtime
  recusar o residual com largura incompatível.
- `executor.rs` - classifica a arquitetura GGUF e resolve o modo
  `auto/dense/moe`. No `infer_server::run`, uma escolha explícita persistida no
  deploy vence `POM_INFERENCE_MODE`; `auto` ainda aceita esse override e, sem
  ele, usa a metadata GGUF. O caminho denso fica em `LlmEngine`, e o MoE
  seleciona o runtime próprio por dtype. `ExecutionEngine` é a fonte única do
  rótulo que sai na telemetria; ausente em peer antigo vira desconhecido na
  interface.
- `relay.rs` - `BatchCoordinator` agenda o flush imediatamente e ainda agrupa
  chamadas concorrentes para o mesmo próximo hop no mesmo ciclo do executor;
  envia um frame único e distribui respostas por `item_id`. QUIC também
  publica em microssegundos o tempo aguardado em `send_data` e `finish`; num
  lote, o tempo compartilhado é agregado uma vez. Não há mais um
  timer fixo no relay; `bubble_hop_timings` mantém a telemetria por salto.
- `runtime.rs` - `NodeConfig`/`NodeRuntime`: aplica a fatia, injeta os tipos de
  cache globais do `pom.json` e guarda o
  `deployment_name` operacional separado de `model_label`, sobe e
  supervisa o servidor Rust embutido (`infer_server::run`), mantém um cache de
  pesos entre gerações compatíveis e mantém a anotação da fatia servida
  (Pending → Serving após confirmação estável de `/health`). `stop` limpa o
  cache e o journal; uma troca de fatia ou de parâmetros de carregamento também
  o invalida.
  O runtime mantém o `pom.db` para as decisões do gateway; o servidor de
  inferência autentica saltos internos pela `connection_key` compartilhada em
  redes privadas e por TLS em redes públicas. Não consulta API keys de cliente.
  `NodeConfig.inference_mode` preserva a preferência `auto`/`dense`/`moe` que
  veio do deploy; `infer_server::run` a entrega ao seletor antes de carregar o
  modelo. O campo escolhido ainda não substitui a observação: a telemetria
  continua sendo a fonte de verdade sobre o motor executado e deixa divergência
  visível. Depois de duas respostas consecutivas de `/health` com
  `model_loaded`,
  `spawn_health_watch` também publica no `Network` o `RuntimeCapacity` efetivo
  (`n_ctx`, `max_new_tokens`); apply, erro, stop e unload térmico removem esse
  valor até o próximo runtime pronto.
  O endpoint `/health` também devolve `model_label`, `deploy_id`,
  `start_layer`, `end_layer` e `total_layers`. O gateway usa esses campos como
  a fotografia executável mais recente durante a admissão, corrigindo uma
  cobertura P2P atrasada antes de chamar o planejador.
  Os testes de `runtime.rs` usam `Network::testkit`, com socket e destino UDP
  presos à loopback, para que anúncios de readiness nunca alcancem a LAN.
- `spec_lab.rs` (614, `#![cfg(test)]`) - a bancada da especulação:
  `generate_stream_with_stops` de verdade contra hops falsos que decidem com
  `should_draft`/`find_draft`/`longest_match` de produção. O fixture não
  anuncia batching, pois o endpoint falso atende frames unitários. Os cenários
  também conferem que a resposta real do gateway traz `GenerationTimings`
  agregados, inclusive no limite de um token; a política de
  `kv_remove` efetivo e o estado do stream remoto ficam cobertos nos testes
  de `infer_server`/`prefill`. A fixture do orçamento desliga o detector de
  ciclos para que sua sequência periódica não encerre a geração antes do teto
  e a comparação continue isolando `n_batch` do orçamento total.
- `tp.rs` (novo) - registro do callback de tensor parallelism no llama.cpp
  (caminho C): `register_tp_reduce`/`unregister_tp_reduce` e o `extern "C"` que
  lê a parcial (f32), contribui localmente quando é o primary ou POSTa para o
  `/v1/tp/reduce` do primary quando é worker. O endpoint soma na
  `ReduceBarrier` do `infer_server` e devolve a soma no buffer. O callback roda
  em uma thread `spawn_blocking` do llama.cpp e usa o `Handle` Tokio capturado
  no registro do grupo, nunca `Handle::current()` na thread nativa. O cliente é
  **pinado no `node_id` do primary** (`TpIdentity.rank0_node_id`): um
  `reqwest::Client` padrão recusa o certificado autoassinado do cluster e todo
  POST morria em 13 ms. O fork insere o op de reduce (`GGML_OP_CUSTOM`) depois
  de wo/ffn_down e chama esse callback durante o decode - confirmado em campo:
  `graph splits = 66` (2 por camada) e o log do primeiro reduce.

  Uma **réplica não tem opinião** (`tp::is_replica`): ela contribui com a metade
  dela em cada reduce e nada mais. Quem amostra o token e decide o fim da
  geração é o rank 0. EOS apenas fecha o `Utf8Assembler` para a resposta; a
  sessão física continua viva até o gateway decidir entre `/v1/session/rewind`
  e `/v1/session/release`. Isso evita que uma réplica veja uma sessão removida
  pelo rank 0 antes do rewind ou de um step já aceito.

  `register_tp_reduce`/`unregister_tp_reduce` **ligam e desligam** o nó do grupo,
  e desligar limpa tudo: estado, decode aberto, marca de réplica e falha
  pendente. `TP_STATE` era um `OnceLock`, que não tem como ser limpo, então um
  processo que um dia serviu TP se declarava TP para sempre - implantar um
  modelo inteiro por cima de um grupo herdava o estado, avisava "reduziu 1 vezes,
  esperado 64", e a checagem de zero reduces chegava a derrubar a requisição
  (18/08). Trocar o grupo também deixou de exigir reinício.

  A parcial viaja como **f32 cru em little-endian**, com `barrier_id` e `rank`
  em cabeçalho (`x-pom-tp-barrier`, `x-pom-tp-rank`), e a rota tem o mesmo
  limite de corpo das de quadro. Era JSON: ~12 bytes por float de 4, o que dava
  ~5 MB por reduce num prefill de 100 tokens - 64 vezes por decode - e estourava
  o limite default de 2 MiB do axum com `413 Payload Too Large` na segunda
  requisição (18/08). Cru é 4x menor e não passa por parse.

  `relay::send_bytes_or_tunnel` tenta primeiro os candidatos HTTPS diretos
  pinados. HTTP/3 vem antes de HTTP/HTTPS quando QUIC está ligado; o túnel só
  entra depois que todos os endereços diretos falham por transporte. Com QUIC
  desligado, o membro ainda precisa anunciar `libp2p_peer_id` e
  `transport/libp2p-tunnel-v1` para habilitar a última reserva. Essa ordem é
  importante no reduce TP: a chamada fica dentro da barreira e um túnel
  indisponível não deve atrasar um endereço HTTPS alcançável. `broadcast_to` mantém a URL base e a rota separadas, porque
  o túnel encaminha a rota para o loopback do worker. O
  plano também leva os candidatos UDP do peer para o `TunnelOutbound`, então o
  swarm pode reconstruir o endereço TCP mesmo que o anúncio não exista na
  tabela local. O `TunnelRequest` leva os cabeçalhos da barreira, do rank e da autorização. A
  autorização interna usa a `connection_key` compartilhada e o marcador de nó
  interno. Em redes públicas, HTTPS e QUIC podem validar pelo transporte seguro
  com o marcador, mas o fallback HTTP também leva o bearer compartilhado; sem a
  prova correspondente, a chamada é recusada em vez de cair para uma API key de
  cliente. A tentativa HTTP direta inteira, incluindo autorização, headers e
  corpo, também respeita o deadline de transporte compartilhado de 30 s.

  O snapshot `TpDecodeTelemetry`, anexado a `node_timings.tp`, separa
  reduces esperados e observados, espera total, p50/p95, bytes de
  request/response, chamadas locais, broadcasts e chamadas diretas, pelo
  túnel ou por fallback.
  Ele mede a espera da barreira síncrona, não uma latência pura de pacote. Em
  um modelo de 32 camadas, a arquitetura atual tem dois pontos de coletiva por
  camada, ou 64 esperas síncronas por `llama_decode` de um token; isso é TP e
  não deve ser confundido com os frames de estado oculto do pipeline.

  O ciclo de vida da sessão da réplica é comandado pelo rank 0:
  `/v1/session/release` é repassado aos membros (`replicate_session_call`),
  porque o gateway só fala com o rank 0 e a réplica não encerra sessão sozinha.
  Sem o repasse a sessão do worker segurava o único slot e a **segunda**
  requisição morria com "seq pool esgotado" (18/08).

  `/v1/session/rewind` segue a mesma regra de réplica, mas preserva a sessão:
  remove somente o sufixo gerado e atualiza `n_past` em todos os ranks. O
  gateway só publica o prefixo no cache depois de confirmar a resposta de cada
  hop. O fim normal da geração não chama `InferState::finish`; essa remoção é
  reservada ao release explícito e à coleta de sessões ociosas.

  Os handlers `handle_prefill` e `handle_step` também seguram
  `tp_batch_gate` no caminho de um frame com um único item. O gateway envia
  esse caso pelo `BatchCoordinator`, então o receptor precisa manter
  `item_id`/`batch_id` no meta da resposta e impedir que outra requisição
  altere o decode global enquanto a coletiva ainda espera todos os ranks. Os
  filhos internos de um batch pulam o lock por meio de `BATCH_CHILD_HEADER`.

  `/v1/session/rollback` ainda não é repassado às réplicas TP; a pendência está
  em `TODO.md`, item 13. Ele só é usado pela decodificação especulativa, hoje
  desligada nas configurações de TP.

  Falha da coletiva **não pode ser silenciosa**: ela não degrada a resposta, ela
  a torna falsa. `note_failure`/`take_failure` levam o motivo até o decode, que
  derruba a requisição; e `decode_tokens` ainda compara `reduce_count()` antes e
  depois, porque "o callback nunca foi chamado" não tinha sintoma nenhum além do
  texto sem sentido (18/08).
- `wire.rs` - o único lugar que conhece o dtype/codificação do tensor em
  trânsito e o contrato inicial `frame/v1-batch` com offsets validados.
  `WireDtype` mantém f16 como default e implementa f8 FP8 E4M3FN e i4g64/i4g128
  assinado com escala f16 por grupo na borda, sempre devolvendo f32 ao motor;
  `encode_profiled` mede o loop f8 e a serialização do cabeçalho/frame em
  limites separados. `prefill/`
  e `relay.rs` propagam o dtype escolhido e registram bytes enviados, recebidos
  e formato efetivo por hop.

## Funções-chave

`infer_server/`:
- `InferenceServer::new` + rotas - monta o router axum sobre o `LlmEngine` e o
  estado de sessões (sessions, seq_pool, max_sessions, sampling).
- `Session` - **duas sequências de KV por sessão**: `prompt_seq`, onde o
  prefill escreve até a fronteira estável medida, e `gen_seq`, aberta por
  `open_generation` antes do primeiro chunk a partir dessa fronteira, como
  cópia da primeira. A cauda volátil do prompt e a geração ficam na cópia.
  `prompt_cells` conta somente células escritas em `prompt_seq`; chunks na
  cópia não alteram esse contador. `seq_id()` devolve a sequência do passo
  atual. É o que permite reter o prompt entre turnos sem nunca remover posições
  do meio do KV; ver `llm::kv_seq_budget` para o par de sequências e o que ele
  custa.
- `retain_prompt` - fim de turno: apaga a sequência da geração inteira (remoção
  total, que todo motor aceita) e devolve a sessão a `prompt_len`. O EOS chama
  isto, **não** `finish`; destruir a sessão no EOS era o que fazia o rewind do
  gateway chegar a uma "sessão desconhecida". `finish` continua sendo a
  liberação de vez (release explícito, cancelamento, erro).
  `retain_prompt` apaga a sequência de geração e retém `prompt_len` na sequência
  do prompt. Se há células extras realmente em `prompt_seq`, também tenta uma
  remoção de **sufixo** (não de meio). `prompt_cells` não inclui o prefill
  enviado a `gen_seq`: somá-lo como se fosse escrito em `prompt_seq` deixava a
  posição lógica incompatível com as células físicas na retenção e bloqueava
  o reuso seguinte. `Session::apply_retention` só ajusta
  `prompt_cells`/`prompt_tokens` junto com `n_past` se o prompt já estava na
  fronteira estável ou se a poda foi aceita; um motor recorrente/híbrido que
  recusa uma poda necessária mantém a sessão retida com o prompt inteiro e
  perde o reuso daquele turno, sem deixar contagens incoerentes. Logs
  `prefix peer eligibility`, `prefix peer comparison` e `prefix peer retention`
  registram o motivo da decisão sem incluir texto do prompt.
- `plan_prefix_reuse` / `expected_prefill_pos` - a regra do reuso e a posição do
  pedaço, puras e testadas em `prefix_rule_tests`. A regra é só-acrescentar:
  fora dela, a sessão recomeça, o que custa o reuso do turno e nunca a
  requisição.
- `evict_retained` - despeja a sessão retida menos usada. **Não é mais chamado
  pelo `alloc_seq`**: trocar na hora o KV quente de uma conversa viva pelo
  prefill que acabou de chegar era o defeito de produção de 2026-09-08, em que
  um pedido de 35 caracteres despejava 62 mil tokens e a primeira palavra da
  conversa passava de 2,7 s para 41,1 s. Hoje só o escalonador o chama, e
  somente depois de `RETAINED_RECLAIM_GRACE` com um prefill de sessão nova
  esperando - a conversa quente tem a sua chance de voltar, e nenhuma
  requisição aceita fica pendurada até o `reap_idle`.
- `new_session_slots(free_slots, retained)` (`infer_server/core.rs`) - a
  admissão de sessão nova conta **só sequência livre**. Sessão retida é cache
  quente, não capacidade: contá-la como slot admitia um prefill que, no worker,
  só entrava destruindo o prefixo de outra conversa.
  `SchedulerQueue::oldest_new_session_wait` é o relógio dessa carência.
- `handle_prefill`/`handle_step` - autenticação, decode do frame e envio para o
  scheduler local. O worker usa `prefill_batch_blocking` ou
  `step_batch_blocking` quando as entradas são compatíveis e mantém o caminho
  individual para especulação e compatibilidade.
- `BatchRoute::group` - agrupa descritores somente quando modelo, deployment,
  fase, configuração e próximo hop coincidem; `RouteAffinity` em
  `pipeline.rs` preserva os node IDs das réplicas escolhidas durante a sessão.
- `spawn_scheduler`/`run_scheduler` - worker único por contexto llama.cpp,
  com backpressure antes de alocar sessão e resposta `429 Retry-After` quando
  a fila atingiu o limite. Com `batch_window_ms=0`, o primeiro item segue ao
  worker sem espera temporizada; um valor positivo mantém a janela de coleta.
  O `/health` expõe os limites e os últimos contadores de lote. `moe_health_summary`
  mantém o isolamento observável: um runtime denso publica
  `moe_executor=not-requested`, sem dtypes nem motivo MoE; o teste no
  `infer_server` trava esse contrato mesmo quando MTP está ativo.
- bloco de resposta direta (nos dois handlers, quando `direct_reply_to(&meta)`
  existe) - acumula `hop_timings` na ida (`direct::accumulate` +
  `direct::hop_entry`); cauda (`next_hops` vazio): spawna `direct::deliver_result`
  ao `reply_to` e devolve `direct_ack()`; a cabeça inclui a progressão de
  prefill em `prompt_progress` no ACK, para o gateway despachar o próximo
  chunk com `next_pos` absoluto durante a sobreposição; meio: spawna
  `forward_direct` para o próximo hop; meio sem residual local:
  `deliver_failure` com "hop intermediário não produziu estado oculto".
- `forward_direct` - repasse assíncrono em task própria (quem chamou já foi
  aceito e não espera mais nada); repassa `reply_to` intacto; remonta `scale=1.0`,
  `is_last` e `next_hops` remanescente. Falhou o repasse, entrega
  `direct::deliver_failure` nomeando o nó que realmente morreu.
- `direct_ack`/`direct_ack_with_progress` - ACK wire-codificado com
  `{"accepted": true}`; no prefill, a cabeça anexa `prompt_progress` com
  `more_chunks`/`start_pos`/`next_pos` autoritativos. O gateway lê esse campo
  aninhado antes de lançar o próximo chunk; não deriva posição pela ordem de
  chegada.
- `direct_reply_to` - o campo `reply_to` do meta; ausente no hub e no anel, é
  só ele que liga o caminho novo.
- `prompt_progress_for_forward`/`apply_prompt_progress` - a contraparte direta
  de `overlay_head_authoritative_fields` (agora `pub(crate)`): sem volta, a
  cabeça manda `more_chunks`/`n_prompt_tokens` **adiante** e quem entrega copia
  para o resultado - senão todo prompt de mais de um pedaço trunca no primeiro.
- `direct_lab::direct_prefill_preserva_posicao_absoluta_com_e_sem_reuso` -
  cenário ponta a ponta com dois chunks, ACK de progresso aninhado e cabeça
  remota; os hops falsos recusam continuação sem posição e conferem os valores
  absolutos tanto numa sessão fria quanto com prefixo reaproveitado.
- branch de relay (`should_relay`) - no anel, quando `next_hops` não é vazio e
  há hidden state local, reencaminha para a cauda em vez de devolver ao
  chamador; patcha `processing_ms`/`node_timings` próprios antes de codificar a
  volta.
- `owns_tokens`/`is_final_chunk_for_forward` - só a cabeça tem
  `more_chunks`/`n_prompt_tokens` autoritativos; a resposta bubbled do ring
  sempre traz falso/ausente, senão prompt de múltiplos chunks trunca em
  silêncio.
- `handle_rollback` - `RollbackRequest{accept_count, batch_width}`: descarta o
  KV não confirmado da especulação; sessão desconhecida é no-op (não erro);
  o gateway fan-out para todos os hops.
- `handle_slot` + `InferState::{save_slot,restore_slot,check_slot_compatible}` /
  `erase_slot`/`count_slot_entries` - snapshot de KV em disco
  (`--slot-save-path`). O health conta sidecars pela estrutura do diretório,
  fora do lock do motor, e só abre o JSON quando precisa salvar ou restaurar.
  `save_slot` grava **`prompt_seq` truncado em `prompt_len`**, nunca `gen_seq`,
  mais o sidecar `<name>.json` (identidade do modelo e da fatia).
  `restore_slot` confere o sidecar antes de tocar no motor, `alloc_seq` uma
  sessão nova `retained` na fronteira salva. `slot_dir` vem de
  `NodeConfig.slot_save_path`; sem ele a rota responde 400. `replicate_slot_call`
  repassa a ação aos workers de TP. Capability `inference/slot-persist-v1`.
  Ver `inferencia.md` §3.1.1.
- `reap_idle`/`SESSION_IDLE_TIMEOUT_SECS=900` - sessão ociosa é liberada;
  reinício do processo também remove o estado em memória. Se um desses eventos
  ocorrer no meio de um prefill, o peer devolve sessão desconhecida e o gateway
  recomeça o prefill completo uma vez, nunca apenas o chunk que perdeu o KV;
  `INFERENCE_FRAME_BODY_LIMIT=64 MiB` corta frame impossível no body.
- `tcp_nodelay(true)` no `axum::serve` - ver nota gêmea em
  `gateway::start_gateway_with_ready`: sem isto, o ack de resposta direta (escrita pequena)
  fica preso no Nagle.
- `/health` anuncia `relay/v2-direct` e, quando `slot_dir` está configurado,
  `inference/slot-persist-v1` (mantenha em sincronia com `network.rs`
  `self_announce` e a entrada local do gateway); expõe `kv_can_shift`,
  `profile_sync_enabled` (valor efetivo de `POM_PROFILE_SYNC`), o caminho
  `selected_path`, `slot_persist_enabled`/`slot_count` e também o snapshot do
  scheduler (`queue_depth`, limites, tamanho e tokens do último lote). O motor observado sai por
  `node_timings.executed_engine` e `executed_engines` em cada hop do
  prefill/decode e é agregado no `RequestTrace` sem inferência da tela.
- `NodeTimings` também publica, por chamada de `step`, os contadores MTP
  `mtp_draft_tokens_proposed`, `mtp_draft_tokens_accepted` e
  `mtp_draft_tokens_rejected`. `proposed` conta o draft submetido à verificação
  do alvo, não um `next_draft` que ficou sem uso no corte do orçamento.
  `RequestTrace` soma essas amostras por requisição e serializa
  `mtp_acceptance_rate` como aceitos/propostos, com `null` quando não houve
  proposta. Os incrementos ficam fora do laço por token e não mudam a política
  de especulação.
- `should_draft` (e `find_draft`/`longest_match`) - a cabeça decide se
  rascunha com base no convite e em `mtp_ready` ou em um draft pré-fabricado.
  Recusar é resposta legítima, não erro: a cabeça omite `draft` no quadro e a
  cauda responde no formato antigo de um token (ver
  `step_blocking::incoming_draft` e a seção de duas formas de resposta em
  `prefill/`). A cauda exige `mtp_ready` para produzir o próximo draft;
  `kv_can_shift` é somente um diagnóstico do K-shift de `--ctx-shift`. O
  rollback real continua sendo conferido por `kv_remove` e falha fechado se
  o motor recusar a remoção.

`llm.rs`:
- `LlmEngine::load_with_cache` - carga com `EngineConfig` (n_ctx total, n_batch,
  n_gpu_layers, offload_kqv, fit_memory, sua margem configurável por GPU,
  max_sessions e
  `moe_external_executor`). O runtime transforma
  o contexto por sessão em `n_ctx * max_sessions` antes de criar o engine. Com
  `fit_memory=true` e o padrão de
  camadas GPU (`99`), chama a ponte `llama_rs_fit_params` do llama.cpp antes de
  carregar os pesos. Antes disso, consulta pela ponte o livre e o total dos
  dispositivos do backend; o livre já exclui processos existentes e a margem
  padrão de 256 MiB cobre apenas alocações tardias e uma ocupação que apareça
  depois. O plano preserva `tensor_split` e `tensor_buft_overrides`
  quando há mais de uma GPU local. Se a criação do contexto falha com o KV na
  GPU, tenta novamente com `offload_kqv=false`, mantendo as mesmas camadas de
  peso na GPU e deixando o KV cache na RAM. `/health` mostra se o fit foi
  aplicado, as camadas efetivas e `n_ubatch_effective` - e, em `placement` e
  `context_memory`, onde as camadas e o KV realmente foram parar. O `n_batch` público
  continua sendo o limite do pedaço de prompt, mas `fit_memory=true` limita o
  microbatch interno a 512 tokens para reduzir os temporários CUDA que só são
  reservados no primeiro grafo real; sem fit, o microbatch acompanha `n_batch`.
  Esse microlote efetivo governa o buffer de cálculo tanto do contexto principal
  quanto do contexto MTP, então 512 custa bastante mais VRAM que 128.
  `llm::apply_context_batch_params` é a fonte única dessa política e também é
  usada pelo contexto MTP, que mantém o `n_batch` público mas não herda um
  microlote de 4096 por engano.
  `LlmEngine::pool` (N contextos sobre um modelo só, `pool_tests` cobre como o
  total é repartido entre contextos de uma sessão) ainda **não tem chamador**:
  o servidor carrega um contexto com `load`, e o `#[allow(dead_code)]` no
  `pool` é a marca disso; ver `TODO.md`, item 14. `LlmEngine::load_with_cache` usa o
  `ModelCache` do runtime para criar apenas um contexto novo quando a
  identidade da fatia e do carregamento de pesos não mudou; parâmetros de KV,
  contexto, sessões, lote e sampling não forçam releitura do GGUF. O flag
  `moe_external_executor` faz parte dessa identidade: quando ligado, o fork
  mantém metadata dos experts roteados em contexto `no_alloc`, sem buffer de
  pesos; uma carga densa/nativa nunca compartilha esse modelo metadata-only.
- `LlmEngine::placement` / `LlmEngine::context_memory` - onde as camadas e o
  cache KV ficaram **de fato**, do llama.cpp, por
  `llama_rs_model_placement_json` e `llama_rs_context_memory_json`
  (`vendor/llama-cpp-sys-2/wrapper_common.cpp`). Devolvem `Option`: falha da
  ponte é `None`, e o `/health` publica `null` em vez de um mapa derivado de
  `n_gpu_layers` - que mentiria sempre que um override de buffer type mandasse
  parte de uma camada para a CPU. `json_from_wrapper` é quem chama e quem
  libera a string (`llama_rs_string_free`, nunca `free` nem
  `CString::from_raw`).
- `LlmEngine::flash_attn` / `LlmEngine::load_params` - com que parâmetros de
  llama.cpp este contexto está de pé, para o `/health` e a tela de Detalhes.
  Nada aqui é eco do `EngineConfig`: os valores saem do contexto vivo
  (`llama_n_ctx`, `llama_n_batch`, `llama_n_ubatch`, `llama_n_seq_max`,
  `llama_n_threads`), porque o fallback de KV em RAM sobe um contexto com outra
  configuração e o operador precisa ver o que roda. `kv_unified`, `embeddings`
  e `n_rs_seq` não têm getter, então vêm de um `ContextParamsSnapshot` tirado
  na linha anterior ao `llama_new_context_with_model`. O Flash Attention é o
  único campo `Option`: o padrão do llama.cpp é `AUTO` e a decisão só existe
  em `get_cparams()`, lida por `llama_rs_context_flash_attn`; falha da ponte é
  `None` e vira `null` no `/health`, nunca `false`.
- captura residual - `ggml_backend_sched_eval_callback` no `l_out-<N>` antes do
  `output_norm`, com `CaptureSlot` por contexto via `cb_eval_user_data`; o
  DeepSeek4 chama a mesma fronteira de `l_last-<N>`, e o callback aceita os dois
  nomes. A cabeça que vem depois pode executar, mas não altera o residual já
  copiado. Quando `llama_model_n_embd_out` diverge de `llama_model_n_embd`,
  `cparams.embeddings` fica desligado para impedir que a cópia genérica do
  llama.cpp peça mais bytes que o tensor real; o callback continua sendo a
  saída usada pelo POM. O eixo de tokens é localizado pela contagem conhecida
  do microlote atual e pela forma física, nunca pelo produto até
  `n_embd_inp`; assim, eixos de fluxo ou outras features anteriores são
  preservados. Depois do eixo, dimensões maiores que 1 são recusadas e
  dimensões 1 são ignoradas; em caso ambíguo, o menor eixo fisicamente contíguo
  vence de forma determinística. `accumulate_capture` junta um disparo por
  **microlote**: o llama.cpp parte o lote em pedaços de `n_ubatch` e chama a
  captura uma vez por pedaço, então substituir em vez de acumular guardava só o
  último. Todo caminho de decode chama `reset_capture_slot` antes, senão a sobra
  de uma requisição entraria concatenada na próxima. A ordem é a de
  `split_simple`, que `kv_unified=true` mantém para o transporte do residual.
  Em prefill multimodal, um callback pode trazer só parte das células físicas
  (por exemplo, o texto antes das células expandidas pela imagem); a captura
  infere o eixo físico desse callback e acumula até o total esperado, sem usar
  `n_past` como contagem de linhas.
- `profile_backend_sync_enabled` - perfil do backend síncrono, `POM_PROFILE_SYNC`
  (default ligado; "0"/"false" desliga), publicado explicitamente por peer em
  `/health`. `NodeTimings` mantém os contadores em microssegundos e serializa os
  campos `*_ms` de diagnóstico como milissegundos fracionários, então uma
  medição sub-milisegundo não vira zero no wire. `backend_call_ms` mede a
  chamada de avaliação do llama.cpp, separada de fila e espera pelo mutex;
  `backend_sync_ms` mede a barreira explícita e pode incluir trabalho de GPU já
  enfileirado. Nenhum dos dois é tempo puro de kernel ou fonte de verdade sobre
  `POM_PROFILE_SYNC`; o Chat lê a flag publicada por `/health`.
- `unsafe impl Send/Sync` atrás de Mutex do std (o llama.cpp não é thread-safe);
  `Drop` libera só o ctx - os pesos são do `LoadedModel`.
- `SamplingConfig` - a cada passo, o sampler aplica a penalidade de repetição,
  top-k por contagem, softmax com temperatura positiva, núcleo top-p sobre as
  probabilidades normalizadas e sorteio multinomial. Temperatura não positiva
  devolve argmax. `repeat_last_n=64` continua padrão: a penalidade é por token
  e não detecta ciclos de n-gramas, então ampliar a janela não garantiria
  bloquear o ciclo observado e mudaria a política.
- `cycle.rs` - detector de última linha no gateway, não do servidor de
  inferência: observa peças já amostradas, normaliza apenas sufixos numéricos
  embutidos em identificadores e confirma três janelas canônicas consecutivas.
  A janela padrão é 32..512 tokens; limites e o interruptor ficam em
  `RuntimeParams`. Ao confirmar, o gateway envia a peça corrente, não pede
  outro decode, libera a sessão e marca o resumo como `partial` com
  `finish_reason: "cycle"`. O detector é deliberadamente separado do sampler,
  do estado recorrente e do contrato de wire.
- `BatchLayout` - transforma a ordem física das linhas do `llama_batch` em
  ranges por `item_id`; a posição global de logits nunca é inferida pelo índice
  da sessão. Mistura de token e hidden state fica em lanes distintas.

`prefill/`:
- `run_with_options`/`step` - dispatch em três níveis: `run_direct`/`step_direct`
  quando `plan.supports_direct_reply()`, senão `run_ring`/`step_ring` quando
  `supports_relay_chain()`, senão o pipeline hub. `run_with_options` recebe
  `max_inflight_chunks` e um `resume: Option<PrefillResumable>` para uma
  retomada que o chamador tenha provado segura; somente entrega ambígua ou
  estado remoto divergente exige reconstrução fria.
- hub e `run_direct` - pipeline em ordem, não par travado: `head_pump`
  (despacho serial à cabeça, lê `next_pos` do ACK) e um `drain` com
  `FuturesOrdered` que leva cada estado oculto pelos hops restantes, até `W`
  em voo. `run_direct` despacha cada chunk **uma vez** para a cabeça com
  `reply_to`; a cauda entrega o resultado direto ao gateway
  (`await_result_with_ack` no ticket). `is_last` é sempre `false` na cabeça
  (o modo direto exige >= 2 hops).
- `step_direct` - o gêmeo de decode: uma chamada de saída (para a cabeça), uma
  espera no ticket. É a diferença entre `2N` travessias (anel) e `N+1` (direto),
  e é por isso que o custo por token deixa de depender de onde mora a camada 0.
- `step_stream`/`RemoteDecodeStream` - control frame único para o head remoto;
  o peer continua decodificando e entrega callbacks limitados com no máximo
  oito itens prontos por lote. Planos com mais de um hop deixam os demais
  peers dentro do relay do head; o gateway não paga uma chamada por token.
  Quando o plano anuncia MTP fatiado, o callback leva `next_draft` junto com
  `speculated`, `speculation_active` e `speculation_disabled_reason`, e o head
  instala o rascunho na próxima rodada local. Cada hop faz o rollback do seu
  próprio sufixo antes de borbulhar a resposta, sem uma chamada adicional.
- `direct_failure` - resultado que chega com `error` no lugar do token nomeia o
  hop que morreu e marca **esse** (não a cabeça) com `routes.mark_failure`.
- `record_direct_hops` - dobra no `RequestTrace` o `hop_timings` e as evidências
  por travessia recebidas no modo direto; não inventa atribuição para o primeiro
  hop quando a medição não existe.
- `step_speculative` - continua no anel/hub para o caminho antigo; o transporte
  remoto novo leva a especulação pelo mesmo `handle_step` do peer que possui a
  sessão, preservando sampling, conteúdo e ordem.
- `speculative_step_from_meta` - interpreta a resposta da cauda, e aqui está o
  ponto que matava a geração em silêncio: a recusa de rascunhar é uma resposta
  **válida**. Uma rodada especulativa tem duas formas legítimas -
  `{done, accepted, accept_count, batch_width}` (a cabeça rascunhou) e o
  formato antigo de um token `{done, token_id, text}` (a cabeça recusou:
  `should_draft` falso). Quem lê a resposta tem que aceitar as duas; tratar a
  antiga como erro fatal derrubava a geração com um token e `partial: true` em
  toda requisição (2026-08-15, Qwen3.8-27B). Sem `draft`, `speculated` é
  `false` e `batch_width` é 1. Callbacks do stream podem declarar
  `speculation_active=false` e uma razão nomeada; sem os campos novos, o
  parser preserva o fallback antigo.
- `Speculate`/`DraftSource` (`infer_server.rs`) - o convite a rascunhar que
  viaja no campo `speculate` do quadro: `{k, m, source, draft?}`. No caminho
  legado, `draft` é o rascunho pré-fabricado que o gateway injeta no modo
  fatiado; no stream remoto a cabeça instala o rascunho recebido da cauda no
  próprio `step_meta` antes da próxima rodada. `source` é o que separa os
  **dois** mecanismos, e eles não são o mesmo com números diferentes. Quadro
  sem `source` é gateway antigo e vale `Lookup`.
  `Speculate::to_meta`/`from_meta` e `DraftSource::wire` são o par de
  serialização.
- `should_draft(speculate, mtp_ready)` - `Mtp` exige a cabeça local pronta ou
  um draft pré-fabricado; `Lookup` só exige o convite. Pedir MTP a um motor sem
  a cabeça `nextn` é recusa, nunca troca silenciosa por lookup. A cauda de um
  pipeline só deixa de devolver `next_draft` quando `mtp_ready=false` ou o
  rascunho produzido está vazio. `kv_can_shift` é exposto como diagnóstico,
  não como requisito de rollback; a remoção efetiva via `kv_remove` continua
  falha fechada. As demais recusas usam
  `tail_without_nextn`, `model_without_mtp` ou
  `peer_without_sliced_mtp_wire` conforme a evidência disponível.
- `DEFAULT_CHUNK_TOKENS=512` - ~8 MiB f16 por hop por pedaço
  (medido: 565 KB para 69 tokens, exatamente `hidden_dim 4096 × 2`).
- `RequestTrace`/`PhaseTrace`/`HopTrace` - timings por fase por hop; `round_trip_ms`
  conserva a ida e volta observada de cada chamada para a soma do RTT da execução,
  enquanto `node_lock_wait_ms` separa "nó lento" de "nó ocupado". Os campos
  derivados de `NodeTimings` preservam frações de milissegundo. Isso é telemetria
  do gateway/SSE e não muda o frame entre peers. Cada `PhaseTrace` agrega as
  travessias em `transport_summary`, por transporte, com chamadas, bytes, RTT
  mínimo/mediana/p95/máximo, fallback por destino e trocas de transporte. As
  amostras internas de RTT e decode têm teto fixo; `transport_trace_complete`
  denuncia recibos ausentes. `hidden_state_dtype` só é observado quando bytes
  de estado oculto cruzaram o hop; em um plano de modelo inteiro de um único
  nó, a ausência de bytes torna a precisão do fio não aplicável e o campo fica
  ausente. O transporte remoto registra `remote-stream` uma vez por callback
  de lote, com bytes recebidos e processamento agregado; o callback não é uma
  nova medição de RTT, por isso `round_trip_ms` fica zero nessa entrada.
  `RequestTrace` também carrega os contadores MTP por requisição e a taxa
  derivada, enquanto os campos `node_timings` preservam os valores de cada
  hop/step para auditoria.

`relay.rs`:
- `send_frame` - HTTP para o próximo hop com o corpo wire-codificado;
  `SentFrame{round_trip_ms, bytes_sent/received, encode/headers/body/decode ms}`
  também identifica `transport` (`local`, `libp2p-tunnel`, `https`,
`http`, `https`, `quic` ou `remote-stream`). O detalhe interno
`https-link` é normalizado para `http`/`https` na telemetria. O campo
`transport_fallback` denuncia quando a chamada desceu a escada depois de
falha de transporte.
- `send_batch_body` é o único ponto por onde todo lote remoto passa, inclusive
  as ativações `remote_stream` que não podem ser agrupadas com uma sessão
  diferente. É ali que a seleção de transporte vive: com QUIC ligado, tenta
  QUIC e cai para HTTP/HTTPS; com QUIC desligado, tenta o canal persistente
  (`link::send_batch`, quando o hop anuncia `transport/link-v1`), depois
  HTTP/HTTPS por requisição e só então o túnel libp2p. Cada degrau só é
  usado depois de o anterior falhar por **transporte**; uma recusa HTTP do peer
  é final, exceto 404/405 no QUIC, que significam peer sem a rota HTTP/3. O
  degrau usado vira `transport`, e ter descido a escada vira
  `transport_fallback`.
- O transporte por requisição tenta QUIC antes do cliente TCP quando a
  preferência está ligada; uma falha QUIC cai no cliente HTTP pinado e, só se
  todos os endereços diretos falharem, no túnel libp2p/Noise. Os destinos QUIC
  vêm do `host:porta` anunciado pelo peer, não da `inference_url`, e usam a
  porta UDP dedicada publicada no `Announce`. Com QUIC desligado, o cliente
  HTTP/HTTPS continua sendo tentado antes do túnel;
  `TunnelRequest::get("/health")` é reservado para a medição de RTT de
  topologia. O `round_trip_ms` de `SentFrame` inclui processamento remoto e
  não alimenta a tabela direcional de rotas.
- `bubble_hop_timings`/`as_hop_timing_entry` - insere a entrada do próprio hop
  na frente (ordem head→tail) e dobra `processing_ms`+`node_timings` do callee
  com o round-trip do caller.
- `extract_relay_failure` - usado por `forward_direct` para nomear o hop que
  falhou na entrega do erro direto. `is_unknown_session_error` percorre toda a
  cadeia `anyhow`, porque `PartialPrefillTrace` envolve a recusa no caminho
  direto; olhar só a frase externa perdia o marcador e impedia a reconstrução fria.
- `RelayFailure{failed_node_id, kind, message}`/`HttpRefusal`/`SendIssue` - o
  gateway responde 502 atribuindo o hop que falhou. `CHUNK_ATTEMPTS=3` repete
  somente falhas comprovadamente anteriores à entrega, com recuo exponencial
  de 100 ms e 200 ms. Depois de chunks aceitos, o prefill retoma na posição
  absoluta quando é seguro; entrega ambígua ou sessão divergente exige uma
  reconstrução fria com motivo explícito.

`direct.rs`:
- `ReplyInbox`/`Ticket` - mapa ticket→canal oneshot que vive no `GatewayState`
  (é o gateway que abre a requisição); `register` sorteia UUID v4 por quadro;
  `Drop` remove o ticket ainda não entregue do inbox em qualquer caminho de
  saída (erro, prazo). Quando há travessias esperadas, `deliver` libera o
  resultado e retém só o snapshot por 30 s para recibos tardios; depois disso
  o ticket desconhecido vira `410` e nunca cai no token seguinte.
- `StreamTicket`/`deliver_stream` - canal mpsc limitado a 64 lotes; um emissor
  remoto aguarda o receptor quando a fila enche, e `Drop` remove o ticket para
  que cancelamento, stop, ciclo ou erro não deixem entregas pendentes. O par
  producer/emitter também é encerrado junto: se a entrega detecta que o
  gateway sumiu, `run_remote_stream_tasks` aborta o producer em vez de deixar
  a sessão de KV e a admissão do scheduler vivas até o próximo timeout. No
  retorno pelo `/v1/link`, o ACK de cada callback continua assíncrono, mas um
  `410 Gone` é publicado ao `RemoteStreamDelivery`, que devolve
  `GatewayGone` e encerra o producer sem cair em HTTPS.
- `DirectReply::new(inbox, urls)` - `None` quando o nó não conhece nenhum
  endereço de si mesmo; aí o anel assume em vez de despachar quadro que nunca
  volta.
- `self_reply_urls` - URLs de gateway na ordem de preferência de
  `gateway::self_dial_host` (operador > LAN > candidatos das interfaces);
  curinga de bind e loopback (v4, v6 e `localhost`) são descartados por
  `is_unroutable_host` - só-loopback devolve lista vazia, e aí o anel assume.
  A exceção é o endereço que o operador fixou (`advertise_addr`), que passa
  mesmo sendo loopback: numa máquina só é a configuração certa. O que se recusa
  é o loopback que aparece sozinho, do fallback de `self_dial_host` ou da
  enumeração de interfaces.
- `handle_relay_result` - handler de `RELAY_RESULT_PATH` (`/v1/relay-result`);
  não usa API key de cliente: o `Ticket` UUID v4 por quadro, conhecido apenas
  pela cadeia, é a credencial da entrega; resultado sem ticket vivo é `410
  Gone`.
- `handle_remote_stream_result` - recebe `/v1/remote-stream-result` e aguarda o
  mpsc limitado antes de responder. Também usa somente o `Ticket`; lote tardio
  ou ticket cancelado recebe `410 Gone`.
- `await_result_with_ack` - `select!` biased entre três coisas: o resultado do inbox
  (caso normal), o ack da cabeça (só interessa quando falha, ou quando um peer
  antigo borbulhou a resposta inteira - `token_id` presente vale como
  resultado), e o prazo `DIRECT_REPLY_DEADLINE=600 s` (hop morreu depois de
  aceitar). Se o resultado chega antes do ack, ele não espera telemetria
  atrasada: o futuro do ack continua em tarefa destacada para registrar a
  evidência do primeiro hop.
- `deliver_result` - a cauda (ou o hop que falhou) tenta cada URL de `reply_to`
  em ordem de preferência, com memória de três falhas seguidas e quarentena
  por 5 min. Candidatos intermediários têm `DELIVERY_TIMEOUT=500 ms`; o último
  recebe 30 s para separar lentidão de morte. `410` para de tentar, porque o
  inbox já não existe. Uma URL que engole SYN é paga uma vez, a alternativa
  assume e os lotes seguintes não repetem a espera.
- `deliver_failure` - entrega um `RelayFailure` no lugar do resultado para o
  gateway nomear o nó morto em vez de esperar o prazo inteiro.
- `hop_entry`/`accumulate` - cada hop descreve a si mesmo na ida: a lista viaja
  cabeça→cauda e a cauda a entrega junto do resultado.
- `record_transport`/`deliver_transport` - o chamador entrega uma evidência
  separada por travessia, com transporte, RTT e bytes. O inbox anexa as
  evidências já recebidas ao resultado e marca a trace como incompleta quando
  alguma chega depois ou não chega; telemetria não segura a resposta.

`direct_lab.rs`:
- A "lei": **o custo por token não depende de onde está a cabeça da pipeline.**
  Hops falsos falam o protocolo real (`wire`, `relay::send_frame`,
  `infer_server::forward_direct`, `direct::deliver_result`, e o gateway falso
  serve o handler de produção de `/v1/relay-result`), dirigidos pelo
  `prefill::run`/`step` reais; o único fingimento é o motor (dorme em vez de
  decodificar).
- Rede emulada por sítio: cada hop e o gateway moram num sítio e toda mensagem
  entre sítios diferentes custa `LINK_MS=50` de mão única, cobrada na chegada e
  de novo na volta - é o que faz a bancada medir travessias em vez da máquina
  onde ela roda. O teste de custo pausa o relógio Tokio para que escalonamento
  do restante da suíte não seja contado como uma perna de rede.
- `the_cost_per_token_does_not_depend_on_where_the_entry_point_is` - para
  N=2,3,4 com o gateway na cabeça, no meio e na cauda, pernas por token
  ≈ `minimum_legs` (tolerância 1 perna para acomodar overhead do frame); as duas pontas empatam; a pior
  entrada (meio) custa no máximo uma perna a mais que a cabeça; e o anel
  borbulhado com a cabeça remota paga 4 pernas (o caso que doía, a queda de 25
  para 14 tok/s do captain), provando que é correção e não o comportamento de
  sempre. Assere ainda que cada quadro de resultado é entregue direto
  exatamente uma vez e que os recibos de transporte não entram nessa contagem.
  O prompt de mais de um pedaço atravessa inteiro (um `prompt_progress`
  perdido trunca no primeiro), inclusive com reuso de prefixo.
- Herda da investigação de 2026-08-14 (`labsse.rs`), que provou a assimetria e
  refutou a hipótese do SSE por contrafactual.
- Os quatro modos de falha do relay ainda não têm cenário E2E; ver `TODO.md`,
  item 16.

`runtime.rs`:
- `NodeConfig` - tudo que define a fatia servida: model_path/label,
  start/end/total_layers, peer_count/index, primary, inference_port, n_gpu_layers,
  n_ctx, offload_kqv, n_batch, max_sessions, batching_enabled,
  max_queue_requests, batch_window_ms, max_batch_sequences,
  max_batch_tokens, prefill_chunk_tokens, max_new_tokens,
  sampling, no_thinking, single_peer, no_embeddings, speculative_decoding_enabled,
  speculative_k/lookup_window, mtp_enabled, mtp_draft_tokens,
  mtp_min_probability, mtp_cpu_fallback_enabled.
- `NodeConfig::mtp_effective` - o MTP liga em quem serve a última camada do
  modelo (onde mora o bloco `nextn`): o modelo inteiro (start_layer 0,
  peer_count 1) **ou** a cauda de um pipeline fatiado (start_layer > 0,
  peer_count > 1). Ver a seção "Predição multi-token" abaixo.
- `NodeRuntime::apply` - aplica a fatia e sobe o servidor Rust embutido
  (`infer_server::run`); muda a config → reinicia o servidor. Anota a fatia em
  `models/lastslice.rs` como **Pending** - mas a anotação só diz o que o `apply`
  decidiu, não o que o servidor conseguiu carregar.
- `NodeRuntime::supervisor` - mantém o `JoinHandle` da geração atual. Ao
  aplicar ou recarregar, a geração recebe o sinal de parada e o runtime aguarda
  o supervisor concluir `serve_with_quic`, a limpeza FFI e a liberação da porta
  antes de iniciar outro servidor. Essa espera tem teto de 5 s; se estourar,
  preserva o handle e recusa iniciar a geração nova, para não sobrepor um
  `llama_free` ainda em andamento.
- `infer_server::run_native_cleanup` - executa `LlmEngine::unload` em
  `spawn_blocking`; `llama_free` e os destrutores nativos não podem bloquear a
  thread que atende o gateway ou as sondagens de `/health`.
- `spawn_health_watch` - o único ponto que promove a carga: depois de duas
  sondagens consecutivas de `/health` com `model_loaded`, promove a anotação
  de Pending para **Serving**. Um primeiro retorno não limpa o journal, pois a
  queda do processo pode ocorrer exatamente ao construir essa resposta. O
  watcher captura o caminho do journal quando sobe; outra instância/teste que
  altere `POM_CONFIG` depois não pode redirecionar sua anotação. Só `Serving`
  autoriza retomar a fatia num reinício.
- `note_unloadable_slice` - marca a fatia como não retomável e põe a razão no
  `status.detail` (o que o painel mostra); é o que um nó vazio diz em vez de
  calar - um nó vazio e calado é indistinguível de uma máquina que nunca
  recebeu fatia.
- `free_port` - desocupa a porta antes de subir o servidor: acha quem segura a
  porta (`lsof -ti :PORT` no Unix, `netstat -ano`+`taskkill` no Windows) e mata
  (SIGKILL). Devolve `Err(motivo)` quando o ocupante não é identificável ou a
  porta segue presa depois de 5 s, e o supervisor põe esse motivo no
  `status.detail` como `Failed` **desde a primeira tentativa** - antes o
  operador via só "carregando" até `MAX_RESTARTS` esgotar, e a TUI anunciava
  saúde enquanto o servidor morria em laço.
- `log_file_path` / `append_log` - `<project_root>/infer_server_<porta>.log`,
  um por porta, apontado por `RuntimeStatus.log_path` (`set_log`, chamado no
  arranque do supervisor). Carrega o ciclo de vida do servidor (arranque,
  tentativa, motivo da queda, modelo carregado); o stdout não serve porque a
  TUI limpa a tela a cada `--refresh`.
- `needs_residual_export` - **única** condição para instalar o callback de
  captura: custa 7,8-8,2 ms/token contra 0,14-0,67 ms sem ele (medido
  2026-08-12); sem o callback, `hidden_state` recusa com "residual capture
  did not fire".
- `MAX_RESTARTS=10`, `MAX_CONTEXT_TOKENS=250_000`. O orçamento de reinícios
  é persistido no journal `<config>.last-slice.json` e compartilhado com o
  supervisor: se a fatia não cabe na memória ou cai durante a confirmação,
  cada novo processo incrementa `Pending.attempts`. Ao atingir
  `MAX_AUTOMATIC_ATTEMPTS`, o nó sobe sem modelo e deixa a razão em
  `status.detail`; `pom undeploy` também grava um bloqueio persistente.

`wire.rs`:
- `BatchFrame`/`BatchItem` - `frame_version=1`, vários itens nomeados no mesmo
  payload concatenado; `encode_batch`/`decode_batch` rejeitam dtype desconhecido,
  shape divergente, offsets fora do payload, sobreposição, item duplicado e
  payload acima de 64 MiB.
- `encode_batch_items`/`decode_batch_items` são o caminho distribuído; um
  pedido de uma sessão usa o mesmo protocolo por meio de
  `encode_batch_single`/`decode_batch_single`. Não existe fallback para frames
  v1/v2 anteriores, pois o produto ainda não foi lançado.
- **O caminho quente não copia o tensor à toa.** `encode_batch_items` escreve o
  tensor uma vez, direto no buffer que vai para o fio (as posições saem de
  `tensor_payload_len`, que as calcula sem alocar; `frame_prefix` já dimensiona
  o buffer). `split_frame` separa header e payload **emprestando** o payload, e
  `decode_batch_items`/`decode_batch_single` saem daí. `peek_batch_header` e
  `validate_batch_bytes` leem só o header, para quem precisa da rota, do
  `batch_id`, da contagem de itens ou de uma validação não copiar o estado
  oculto inteiro. `validate_items` expressa a validação sobre o **tamanho** do
  payload, que é o que torna as duas coisas possíveis. A conversão passa por um
  bloco de pilha: crescer o vetor valor a valor custa mais que a conversão em
  si. Números em `docs/subsystems/inferencia.md` §2.2.
- `FrameEncoding::F16` permanece somente como marcador de chamadas internas;
  o fio oficial é sempre `inference/frame-v1-batch`. O helper
  `encode_with_encoding` é exclusivo da prova de equivalência em testes;
  `WireDtype{F32, F16}` continua com `as_str`/`parse`/`bytes_per_value`.
- `MAX_FRAME_PAYLOAD_BYTES=64 MiB`; o relay separa lotes pelo orçamento de
  payload e não coalesce a cabeça quando a resposta pode carregar hidden state
  sem que o tamanho apareça no request.

## Predição multi-token (MTP) - `mtp.rs`

**Não é a "decodificação especulativa" que já existia.** `find_draft`
(`infer_server.rs`) procura no texto já gerado uma repetição do próprio final e
chuta a continuação: é *prompt-lookup decoding*, uma busca em array que nunca
toca peso nenhum, e por isso só acelera saída repetitiva. MTP usa o bloco
`nextn` treinado **dentro do GGUF**: ele recebe o estado oculto do tronco na
posição `p` com o embedding do token de `p+1` e prevê o token de `p+2`. Duas
chaves separadas (`speculative_decoding_enabled`, `mtp_enabled`) porque são
dois mecanismos; quando as duas estão ligadas o gateway escolhe o MTP.

Nada disso muda a resposta: a verificação em `step_blocking` amostra cada
posição de novo pelo mesmo `sample_token_profiled` e só aceita o prefixo que
bateu. Rascunho ruim custa aceitação, nunca correção.

O motor embarcado **já sabe** executar o bloco: `llama-cpp-sys-2` 0.1.154 traz
`LLAMA_CONTEXT_TYPE_MTP` e `llama_model_n_layer_nextn` nos bindings; as outras
quatro entradas (`llama_{set,get}_embeddings_nextn`,
`llama_set_nextn_layer_offset`) estão em `libllama.a` mas não nos bindings,
porque moram em `llama.cpp/src/llama-ext.h`, fora do `extern "C"` de `llama.h`
que o `wrapper.h` da crate inclui. Daí `llama_mtp_shim.cpp`.
Ao carregar um motor com MTP ligado, `llama_model_params.load_mtp` também precisa
ser verdadeiro: sem isso o llama.cpp mantém `n_layer_nextn` nos metadados, mas
deixa os pesos `nextn.*` nulos e o grafo Qwen3.5 aborta ao construir `eh_proj`.
`LoadedModel` registra essa decisão de carga junto com a evidência dos tensores
no GGUF. Antes de `MtpDrafter::attach`, o runtime escolhe o companheiro CPU-only
quando existe e recusa o contexto se o modelo selecionado não carrega esses
pesos, publicando `o modelo carregado não traz o bloco nextn`; assim a guarda
acontece antes de o llama.cpp reservar o grafo.

Ciclo, por rodada (uma sequência, uma cabeça, memória não compartilhada - o
caminho qwen35/qwen35moe do upstream):

1. `LlmEngine::decode_tokens` chama `MtpDrafter::observe` depois de **todo**
   decode do alvo, prompt incluído. Ele repete o lote no contexto de rascunho
   com o estado oculto deslocado uma posição (linha `k` recebe `h` de `k-1`,
   linha 0 recebe o `pending_h` da chamada anterior) e guarda as linhas de `h`
   do alvo. Ficar dentro do `decode_tokens`, e não em cada chamador, é o que
   impede um caminho novo de esquecer o passo.
2. `MtpDrafter::accept(accept_len)` move o `pending_h` para a linha que a
   verificação confirmou.
3. `MtpDrafter::draft` decodifica a cabeça uma vez por token rascunhado e
   devolve os ids; `p_min` corta no primeiro palpite duvidoso.

Duas coisas que parecem zelo e são a correção:

- **`seq_rm` antes de cada decode do rascunho** (em `observe` e em `draft`). A
  rodada anterior deixou no cache do rascunho as posições que ela apostou, e a
  verificação pode ter recusado parte delas; reescrever por cima sem apagar
  deixaria duas células para a mesma posição e a atenção leria as duas.
- **O ponteiro de tokens do lote é nosso.** `llama_batch_init` aloca `token`
  **ou** `embd`, nunca os dois, e o MTP precisa dos dois; o ponteiro volta ao
  original antes do `llama_batch_free`, que liberaria memória do Rust.

**Liga no modelo inteiro e no pipeline fatiado.** Quem rascunha é a máquina que
serve a última camada (o bloco `nextn` mora aí) e quem monta o lote é a cabeça;
no caso fatiado são máquinas diferentes, e o rascunho viaja de trás para a
frente **sem custo extra de travessia**: a cauda anexa `next_draft` ao frame
do stream que já seria enviado, e a cabeça remota o instala em seu próximo
`step`. O gateway recebe o mesmo campo e o estado explícito
(`speculated`, `speculation_active`, `speculation_disabled_reason`) para
telemetria, mas não faz reinjeção no stream. No caminho direto legado, o
gateway ainda injeta `speculate.draft` na rodada seguinte. A cabeça ecoa a
composição do lote (`tokens`) e a largura pedida (`mtp_k`) no reply, os hops
do meio repassam, e a cauda alimenta o `observe` com isso
(`decode_hidden(..., tokens)`). `PipelinePlan::supports_mtp` exige
`speculate/v1-mtp-sliced` em **todos** os hops e `speculate/v1-mtp` na cauda.
Cauda híbrida/recorrente pronta (`mtp_ready` verdadeiro) pode devolver
`next_draft` mesmo quando `kv_can_shift` é falso: K-shift é diagnóstico do
motor, não prova de rollback. Cada hop desfaz o excedente com `kv_remove` e
aborta a rodada se a remoção efetiva falhar.

**Ainda sem medição de tok/s** (cortado por cota em 2026-08-16): o ganho
prometido pelo bloco é real no LM Studio (21 → 35 tok/s no Qwen3.8-27B-Q3_K_S,
medido pelo captain), aqui a implementação está completa e verde nos testes de
unidade mas **não** tem número medido de ponta a ponta.

## Ligações

- **Chamado por**: `backend/gateway.rs` (monta `DirectReply` com `ReplyInbox` +
  `self_reply_urls`, registra `/v1/relay-result` e
  `/v1/remote-stream-result` sem API key, usando o `Ticket` por quadro, passa
  `direct.as_ref()` a `prefill::run`/`step`; `step_speculative` vai sem
  direct), `backend/webui.rs` (rollback/release fan-out), `main.rs`
  (`NodeRuntime::apply` no arranque, inclusive com `lastslice::take`),
  `routing/pipeline.rs` (as capabilities `CAP_REMOTE_TOKEN_STREAM`/
  `CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
  `CAP_SPECULATE_LOOKUP`/`CAP_SPECULATE_MTP`/`CAP_SPECULATE_MTP_SLICED` decidem
  anel vs direto vs hub, o wire format por hop e qual das duas especulações
  roda).
- **Chama**: `llm.rs` (decode/hidden_state), `wire.rs` (encode/decode do
  frame), `relay.rs` (send_frame). `runtime.rs` consome `models/model.rs`
  (`LayerAssignment`, `ModelProfile`) e `network.rs`.
- **Consumido por**: `models/lastslice.rs` serializa `runtime::NodeConfig`
  (a fatia anotada reaplicada no reinício) e é quem guarda o estado da
  anotação: Pending (escrito por `apply`), promovido a Serving só por
  `spawn_health_watch`; `routing/AGENTS.md` lista as capabilities que
  `infer_server.rs`/`relay.rs` executam.
- **Externo**: nenhum processo externo. O modelo carrega em-processo via
  `llm.rs` (llama-cpp-sys-2). `old/python-inference/inference_server.py` e
  `split_model.py` são legado, não são mais invocados.

### Prefill multimodal

`LlmEngine` carrega o contexto `mtmd` somente quando `NodeConfig.mmproj_path`
está configurado. O prefill da cabeça transporta as imagens em base64 apenas
no primeiro chunk; os peers as decodificam e o `mtmd` faz o prefill em ordem.
Imagens não usam reuso de prefixo nem batching concorrente.
Antes de aplicar uma configuração, `NodeConfig::validate` lê os dois headers
GGUF e recusa um caminho que não seja projetor ou cuja identidade/dimensão de
saída contradiga o modelo. Isso também impede que um `mmproj_path` incompatível
persistido anteriormente chegue ao servidor.

**Célula de KV não é posição lógica.** Uma imagem escreve `n_tokens` células
de embedding no KV, mas em M-RoPE ocupa apenas
`mtmd_input_chunk_get_n_pos` = `max(nx, ny)` posições lógicas. As duas
contagens divergem **de propósito** - é desenho documentado do upstream, não
defeito, e as linhas `find_slot: non-consecutive token position` que aparecem
no lote de imagem são a consequência normal disso. `Session` guarda as duas:
`n_past` é a posição lógica e `prompt_cells` a contagem física.
`prefill_has_more_chunks` decide a continuação do prompt pelo par certo - um
decode multimodal consome todos os chunks do `mtmd` numa chamada e é sempre
completo. Confundi-las fazia o head pedir um segundo pedaço e decodificar o
preenchimento `vec![0; n_tokens]` de `prompt_tokens` como se fossem tokens
reais; a resposta saía como uma fila de pontos de exclamação.
`Session::positions_match_cells` fecha reuso de prefixo e snapshot para
sessões com mídia, onde `prompt_tokens` não indexa o KV célula a célula.
No hand-off hub, `mrope_positions` segue no metadata junto do residual para
que cada fatia a jusante preserve as posições lógicas da cabeça.

## Manutenção deste arquivo

Este arquivo é um mapa de responsabilidades da pasta. Ao mudar uma interface
de inferência ou uma invariável de sessão, atualize aqui e o README do
subsistema no mesmo commit. Mantenha detalhes de implementação no código ou
em `docs/subsystems/inferencia.md`, para que este índice continue curto.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
