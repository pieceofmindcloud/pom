# inference/ - servidor de inferência, wire format e supervisão

A porta de inferência (8181) e tudo que roda nela: o axum server
(`infer_server.rs`), o wrapper FFI do llama.cpp (`llm.rs`), a execução do
prefill/step nos três modos - hub, anel borbulhado (`relay/v1-chain`) e
resposta direta (`relay/v2-direct`, `direct.rs`) - a "lei" que mede o modo
direto em pernas de rede (`direct_lab.rs`), a bancada da especulação
(`spec_lab.rs`), a predição multi-token pelo bloco `nextn` do modelo
(`mtp.rs` + a ponte C++ `llama_mtp_shim.cpp`), o formato binário do tensor na
rede (`wire.rs`) e a
supervisão do servidor Rust embutido (`runtime.rs`), que também mantém a
anotação da fatia servida (estado Pending/Serving em `models/lastslice.rs`).
O runtime consulta a política de `utils/thermal.rs` a cada 5 segundos, pausa
o modelo acima do limite com estado `thermal`, preserva a configuração para
retomada e só reimplanta depois de todas as GPUs conhecidas permanecerem
abaixo do limite de religar durante o cooldown. `stop` limpa essa recuperação
para respeitar uma desimplantação manual.
O modelo é carregado **em-processo** por `llm.rs`; não há processo Python
(`inference_server.py` de `old/python-inference/` é legado). Os caminhos
normais recebem `next_hops` pronto do `routing/pipeline.rs`; o TP é a exceção
controlada, pois coordena seus ranks e seu túnel de transporte neste diretório.

## Arquivos

- `collective.rs` (novo) - núcleo das coletivas de tensor parallelism (caminho
  C): `reduce_sum` (a soma matemática) e `ReduceBarrier` (barreira que
  acumula os parciais de cada rank e devolve a soma a todos via `oneshot`,
  modelada no `ReplyInbox` de `direct.rs`). O transporte HTTP
  (`POST /v1/tp/reduce`) é uma camada fina, ligada junto com o fork do
  llama.cpp.
- `direct.rs` (474) - a resposta direta: a cauda entrega o resultado ao gateway
  por `/v1/relay-result` em vez de borbulhar pela pilha aninhada; inbox de
  tickets, `reply_to`, telemetria acumulada na ida.
- `direct_lab.rs` (580, `#![cfg(test)]`) - a lei: hops falsos falando o
  protocolo real, rede emulada por sítio, exige o mínimo teórico de pernas por
  token para N=2,3,4 com o gateway na cabeça/meio/cauda. O fixture anuncia
  somente capabilities que o endpoint falso implementa.
- `infer_server.rs` (3400) - axum server: `/v1/prefill`, `/v1/step`,
  `/v1/session/release`, `/v1/session/rewind`, `/v1/session/rollback`,
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
  apenas o caminho selecionado, pois não representa uma requisição. O
  estado é acessado por `lock_inference_state`: um mutex envenenado vira erro
  HTTP ou falha da requisição em lote, nunca um novo panic em `unwrap`. EOS
  fecha apenas o acumulador da resposta; a sessão permanece disponível para
  rewind até o release explícito ou a coleta por ociosidade.
  `PhaseTrace` transporta também `node_moe_forward_ms`, que é o custo do
  forward externo fora de `llama_decode` e fecha a conta do backend. `cuda`
  só é publicado depois do forward NVFP4 real; `cpu-reference` inclui os
  dtypes GGML e o motivo da ausência de ponte CUDA. O `PhaseTrace` do gateway
  preserva esses campos por hop e por fase; `not-run` expõe a ausência de
  execução e não há fallback implícito para CPU.
  Os handlers decodificam o frame e entregam
  comandos a uma fila local limitada; o worker seleciona uma entrada por
  sessão e pode chamar o decode multi-sequência. Cada `Session`
  mantém seu próprio
  `Utf8Assembler`: `token_to_piece_bytes` preserva os bytes crus, só as
  posições aceitas alimentam o acumulador e o `flush` ocorre no EOS ou na
  limpeza de erro/cancelamento.
- `llm.rs` (1500) - wrapper llama-cpp-sys-2: carga, decode, hidden state com
  residual capture, sampling, parada por EOS ou fim de turno do template GGUF,
  renderer Gemma4 derivado de `tokenizer.chat_template`, cache KV tipado (`type_k`/`type_v`) e o dono da
  cabeça de rascunho `nextn` (`Option<MtpDrafter>`). `token_to_piece_bytes`
  devolve a peça sem conversão lossy para o acumulador da sessão.
  `decode_tokens_batch` e `decode_hidden_batch`
  constroem um `llama_batch` com sequências independentes e devolvem o layout
  para mapear logits e residuais por item.
- `scheduler.rs` - limites de admissão, janela de microbatch e seleção FIFO
  round robin. A janela tem default de 0 ms para não atrasar uma chamada
  isolada, mas continua configurável para workloads que priorizam coalescer
  chegadas. Cada processo cria sua própria fila; réplicas da mesma faixa não
  compartilham scheduler, KV ou capacidade de fila.
- `mtp.rs` (445) - predição multi-token (MTP): contexto de rascunho
  `ctx_type = LLAMA_CONTEXT_TYPE_MTP` sobre **os mesmos pesos**, e o algoritmo
  `observe`/`accept`/`draft` portado de `common_speculative_impl_draft_mtp` do
  llama.cpp.
- `llama_mtp_shim.cpp` (48) - ponte `extern "C"` para
  `llama_{set,get}_embeddings_nextn` e `llama_set_nextn_layer_offset`, que
  moram em `llama.cpp/src/llama-ext.h` e por isso **não** estão nos bindings do
  `llama-cpp-sys-2` (o `wrapper.h` dele só inclui `llama.h`). Compilada pelo
  `crates/node/build.rs`.
- `prefill.rs` (1688) - `run` (hub) / `run_ring` (anel) / `run_direct` (direto),
  `step_speculative`, as duas formas de resposta da especulação, traço de fases
  por requisição. `Cancellation` cancela a operação do gateway quando o
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
  direta sobrepõem cálculo e transmissão com `PrefillOverlapWindow`, limitada
  a 2 pedaços em voo por padrão e configurável por
  `prefill_max_inflight_chunks`; a janela cancela o par em erro, preserva a
  ordem e interrompe envios em voo quando o pedido é cancelado. O modo anel
  continua serializado pelo contrato aninhado. A telemetria também registra
  `prefix_match_tokens`, `prefix_reuse_status`, `prompt_tokens_transmitted` e
  transporte por travessia.
- `prefix_cache.rs` - cache global de sessões KV por modelo e rota, com leases
  ocupados, maior prefixo textual e LRU apenas entre entradas ociosas. Ele não
  contém peso, tensor ou transporte de arquivo. A chave textual vem do array de
  mensagens (`openai::conversation_cache_key`), não do prompt renderizado.
- `prefix_cache_lab.rs` - autoteste diferencial da política do cache, ou seja da
  **escolha**. A referência acompanha lookup, insert, busy, release e eviction;
  o proxy propositalmente quebra `cached_tokens` e a bateria precisa registrar a
  divergência. Nenhum defeito de execução é alcançável por ele.
- `prefix_reuse_lab.rs` - a bancada da **execução**: uma conversa de quatro
  turnos pelo caminho de `/v1/chat/completions`, nos três transportes (hub,
  anel, direto). Afirma que do turno 2 em diante
  `cached_tokens > 0`, que o trabalho por turno para de crescer com a conversa,
  registra o tempo até a primeira palavra e afirma que ele cai depois do turno
  frio, além de verificar que nenhum hop recebeu pedido de remoção parcial de
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
  e a bateria verifica que uma segunda perda termina com erro limitado e útil.
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
  `format_nvfp4.rs` implementa o decoder NVFP4 em CPU; o rastro completo
  `Nvfp4Trace`/`run_trace` fica restrito à prova diferencial de testes, enquanto
  o runtime consome somente a saída do expert; `cuda.rs` mais
  `cuda_kernels.cu` fornecem executores CUDA dirigidos por dtype quando a
  feature `cuda` está ligada: NVFP4, F32 (0), F16 (1), BF16 (30), Q4_K (12),
  Q6_K (14) e IQ3_S (21), IQ2_S (22), IQ4_XS (23). A ponte heterogênea usa
  descritores por papel e dtype para Q4_K/Q6_K e esses três IQ, sem converter a
  combinação para um dtype global. O hook comum recebe a rota do llama.cpp em
  `[expert_slot, token]`; `llm.rs` a transpõe para `[token, expert_slot]` antes
  do runtime. A ponte escalar reutiliza o payload de
  `format_plain.rs` e valida o cabeçalho antes do kernel.
  K-quants e IQ-quants validam a convenção GGUF
  `[columns, rows, experts]` e aceitam bancos gate/up fundidos ou separados
  antes de entregar o payload ao cache. A expansão IQ usa as primitivas GGML
  linkadas, enquanto cache, router e mat-vec permanecem no executor próprio.
  `format_heterogeneous.rs` é a fronteira para GGUF UD e outros arquivos que
  misturam dtypes: o inventário registra cada tensor por camada e papel, e o
  decoder é escolhido pelo dtype daquele tensor. O callback recebe a metadata
  do fork do llama.cpp e valida nome, geometria e dtype contra esse inventário;
  uma combinação ausente ou não suportada recusa nomeando tensor e dtype.
  O helper `llama_moe_reshape_route_2d` do fork materializa sempre a rota com
  `ggml_cont` antes do reshape: `ggml_is_contiguous` sozinho não prova que a
  área do produtor já está pronta para a cópia entre backends. Isso protege o
  primeiro token/slot contra leitura não inicializada. `qwen35moe` mantém
  `ffn_*_shexp` fora do inventário: o callback recebe apenas `ffn_*_exps`, e o
  expert compartilhado continua no `build_ffn` nativo.
  A prova E2E isolada é:
  `POM_MOE_NUMERIC_MODEL=/caminho/qwen35moe.gguf POM_MOE_NUMERIC_GPU_LAYERS=99`
  `cargo test -p node --features cuda --bin pom`
  `inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer`
  `-- --ignored --nocapture`. Ela compara quatro tokens na camada 1 entre o
  bloco nativo e o callback externo, exige executor `cuda`, ausência de NaN e
  `abs <= 5e-2 + 2e-2 * max(abs(native), 1)` por elemento; `MOE_NUMERIC_PROOF`
  publica `max_abs` e `max_relative`.
  `bandwidth.rs` transforma medições concorrentes CPU/transferência em uma
  fração determinística dos misses; `coexecution.rs` divide por expert,
  executa as duas lanes em paralelo e soma saídas com largura validada.
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
  camada: somente camadas com ponte device têm seus experts omitidos do
  llama.cpp; dtypes sem cache device e combinações heterogêneas fora do
  conjunto Q4_K/Q6_K/IQ3_S/IQ2_S/IQ4_XS ficam nativas e registram o dtype que
  barrou na telemetria. O
  fork mantém a lista como metadata
  copiada no modelo e o grafo escolhe external/native pela camada, nunca por
  uma flag global que possa deixar experts sem executor.
  A telemetria `moe_executor_diagnostics` expõe, quando o perfil CUDA está
  ligado, chamadas, hits/misses do LRU, bytes H2D/D2H e tempos de alocação,
  kernel, sincronização, liberação, busca/admissão e carregamento de experts no
  host, preparação do lote e cópia dos resultados para vetores Rust. O perfil é opt-in por
  `POM_MOE_CUDA_PROFILE=1` e serve para diagnóstico, não para alterar a
  política do cache.
  `cuda_kernels.cu` reproduz a convenção ggml de Q8_0 para as ativações,
  acumulação inteira por subbloco NVFP4 e decodificação direta dos superblocos
  Q4_K/Q6_K com SiLU no caminho K. O
  caminho CUDA agrupa os experts ativos de uma rota em índices `(expert, linha)`;
  depois do cache residente, o payload de peso é enviado uma vez por miss e o
  lote recebe ponteiros de device nos hits. O lote de rotas para múltiplos
  tokens envia ids, pesos e offsets uma vez por camada e agrega a saída no
  device; o caminho unitário continua disponível para o executor residente.
  O lote de rota usa seis lançamentos de kernel e duas sincronizações. O perfil publica `cuda_call_wall_ms`
  como parede externa não sobreposta da chamada FFI, separada de H2D, kernel,
  lançamentos, sincronização, D2H e liberação, que são submedições internas. O runtime
  também publica `runtime_forward_ms`, `host_batch_prepare_ms`,
  `host_result_copy_ms` e `host_aggregation_ms` para fechar a conta do forward
  MoE sem somar relógios sobrepostos.
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
  `mede_banda_h2d_d2h_do_lote_nvfp4`, executado explicitamente com
  `POM_MOE_CUDA_PROFILE=1`, mede H2D, D2H, kernel e parede externa em
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
  Quando a ponte CUDA existe, `MoeRuntime::start_device_warmup` percorre os
  payloads canônicos por camada, expert, papel e dtype em uma thread separada,
  usando a mesma admissão do cache residente. O orçamento continua derivado da
  memória livre e da reserva do compute buffer medido; `warmup_status`, bytes,
  candidatos e erros chegam em `moe_executor_diagnostics`. O prazo é
  `POM_MOE_WARMUP_MAX_MS` (30 s por padrão) e `cancel_device_warmup` faz
  cancelamento cooperativo entre payloads. Fontes sem ponte device não aquecem
  nem removem experts do llama.cpp.
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
  envia um frame único e distribui respostas por `item_id`. Não há mais um
  timer fixo no relay; `bubble_hop_timings` mantém a telemetria por salto.
- `runtime.rs` - `NodeConfig`/`NodeRuntime`: aplica a fatia, injeta os tipos de
  cache globais do `pom.json` e guarda o
  `deployment_name` operacional separado de `model_label`, sobe e
  supervisa o servidor Rust embutido (`infer_server::run`), mantém um cache de
  pesos entre gerações compatíveis e mantém a anotação da fatia servida
  (Pending → Serving via `/health`). `stop` limpa o cache; uma troca de fatia
  ou de parâmetros de carregamento também o invalida.
  `NodeConfig.inference_mode` preserva a preferência `auto`/`dense`/`moe` que
  veio do deploy; `infer_server::run` a entrega ao seletor antes de carregar o
  modelo. O campo escolhido ainda não substitui a observação: a telemetria
  continua sendo a fonte de verdade sobre o motor executado e deixa divergência
  visível.
- `spec_lab.rs` (613, `#![cfg(test)]`) - a bancada da especulação:
  `generate_stream_with_stops` de verdade contra hops falsos que decidem com
  `should_draft`/`find_draft`/`longest_match` de produção, com `kv_can_shift`
  como botão. O fixture não anuncia batching, pois o endpoint falso atende
  frames unitários.
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
  pinados. Se nenhum endereço responde por falha de transporte, tenta o
  `infer_tunnel` quando o membro anunciou `libp2p_peer_id` e
  `transport/libp2p-tunnel-v1`. Essa ordem é importante no reduce TP: a chamada
  fica dentro da barreira e um túnel indisponível não deve atrasar um endereço
  HTTPS alcançável. `broadcast_to` mantém a URL base e a rota separadas, porque
  o túnel encaminha a rota para o loopback do worker. O
  plano também leva os candidatos UDP do peer para o `TunnelOutbound`, então o
  swarm pode reconstruir o endereço TCP mesmo que o anúncio não exista na
  tabela local. O `TunnelRequest` leva os cabeçalhos da barreira, do rank e da autorização. A
  autorização interna usa a `connection_key` compartilhada e cai para a
  primeira API key ativa quando a chave de conexão não existe.

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

  **DEFEITO ANOTADO: `/v1/session/rollback` não é repassado às réplicas.**
  Ele só é usado pela decodificação especulativa, que está desligada nas configs
  de TP hoje; ligar especulação num grupo TP deixaria o KV do worker à frente do
  rank 0 depois de um rascunho recusado.

  Falha da coletiva **não pode ser silenciosa**: ela não degrada a resposta, ela
  a torna falsa. `note_failure`/`take_failure` levam o motivo até o decode, que
  derruba a requisição; e `decode_tokens` ainda compara `reduce_count()` antes e
  depois, porque "o callback nunca foi chamado" não tinha sintoma nenhum além do
  texto sem sentido (18/08).
- `wire.rs` - o único lugar que conhece o dtype/codificação do tensor em
  trânsito e o contrato inicial `frame/v1-batch` com offsets validados.
  `WireDtype` mantém f16 como default e implementa f8 FP8 E4M3FN na borda,
  sempre devolvendo f32 ao motor; `prefill.rs`/`relay.rs` propagam o dtype
  escolhido e registram bytes enviados, recebidos e formato efetivo por hop.

## Funções-chave

`infer_server.rs`:
- `InferenceServer::new` + rotas - monta o router axum sobre o `LlmEngine` e o
  estado de sessões (sessions, seq_pool, max_sessions, sampling).
- `Session` - **duas sequências de KV por sessão**: `prompt_seq`, onde o
  prefill escreve até a fronteira estável medida, e `gen_seq`, aberta por
  `open_generation` na primeira decodificação de token gerado, como cópia da
  primeira. A cauda volátil do prompt e a geração ficam na cópia. `seq_id()`
  devolve a que vale para o passo atual. É o que permite reter o prompt entre
  turnos sem nunca remover posições do meio do KV; ver `llm::kv_seq_budget` para
  o par de sequências e o que ele custa.
- `retain_prompt` - fim de turno: apaga a sequência da geração inteira (remoção
  total, que todo motor aceita) e devolve a sessão a `prompt_len`. O EOS chama
  isto, **não** `finish`; destruir a sessão no EOS era o que fazia o rewind do
  gateway chegar a uma "sessão desconhecida". `finish` continua sendo a
  liberação de vez (release explícito, cancelamento, erro).
- `plan_prefix_reuse` / `expected_prefill_pos` - a regra do reuso e a posição do
  pedaço, puras e testadas em `prefix_rule_tests`. A regra é só-acrescentar:
  fora dela, a sessão recomeça, o que custa o reuso do turno e nunca a
  requisição.
- `evict_retained` - uma sessão retida é cache reclamável: quando falta slot de
  sequência, ela cede antes de a requisição falhar com "seq pool esgotado".
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
  O `/health` expõe os limites e os últimos contadores de lote.
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
- `reap_idle`/`SESSION_IDLE_TIMEOUT_SECS=900` - sessão ociosa é liberada;
  reinício do processo também remove o estado em memória. Se um desses eventos
  ocorrer no meio de um prefill, o peer devolve sessão desconhecida e o gateway
  recomeça o prefill completo uma vez, nunca apenas o chunk que perdeu o KV;
  `INFERENCE_FRAME_BODY_LIMIT=64 MiB` corta frame impossível no body.
- `tcp_nodelay(true)` no `axum::serve` - ver nota gêmea em
  `gateway::start_gateway`: sem isto, o ack de resposta direta (escrita pequena)
  fica preso no Nagle.
- `/health` anuncia `relay/v2-direct`, expõe `kv_can_shift`,
  `profile_sync_enabled` (valor efetivo de `POM_PROFILE_SYNC`), o caminho
  `selected_path` e também o snapshot do scheduler (`queue_depth`, limites,
  tamanho e tokens do último lote). O motor observado sai por
  `node_timings.executed_engine` e `executed_engines` em cada hop do
  prefill/decode e é agregado no `RequestTrace` sem inferência da tela.
- `should_draft` (e `find_draft`/`longest_match`) - a cabeça decide se
  rascunha: `speculate` pedido **e** `kv_can_shift`. Recusar é resposta
  legítima, não erro: a cabeça omite `draft` no quadro e a cauda responde no
  formato antigo de um token (ver `step_blocking::incoming_draft` e a seção de
  duas formas de resposta em `prefill.rs`).

`llm.rs`:
- `LlmEngine::load_with_cache` - carga com `EngineConfig` (n_ctx total, n_batch,
  n_gpu_layers, offload_kqv, fit_memory, max_sessions e
  `moe_external_executor`). O runtime transforma
  o contexto por sessão em `n_ctx * max_sessions` antes de criar o engine. Com
  `fit_memory=true` e o padrão de
  camadas GPU (`99`), chama a ponte `llama_rs_fit_params` do llama.cpp antes de
  carregar os pesos. O plano preserva `tensor_split` e `tensor_buft_overrides`
  quando há mais de uma GPU local. Se a criação do contexto falha com o KV na
  GPU, tenta novamente com `offload_kqv=false`, mantendo as mesmas camadas de
  peso na GPU e deixando o KV cache na RAM. `/health` mostra se o fit foi
  aplicado, as camadas efetivas e `n_ubatch_effective` - e, em `placement` e
  `context_memory`, onde as camadas e o KV realmente foram parar. O `n_batch` público
  continua sendo o limite do pedaço de prompt, mas `fit_memory=true` limita o
  microbatch interno a 128 tokens para reduzir os temporários CUDA que só são
  reservados no primeiro grafo real; sem fit, o microbatch acompanha `n_batch`.
  `LlmEngine::pool` (N contextos sobre um modelo só, `pool_tests` cobre como o
  total é repartido entre contextos de uma sessão) ainda **não tem chamador**:
  o servidor carrega um contexto com `load`, e o `#[allow(dead_code)]` no
  `pool` é a marca disso. `LlmEngine::load_with_cache` usa o
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
- captura residual - `ggml_backend_sched_eval_callback` no `l_out-<N>` antes do
  `output_norm`, com `CaptureSlot` por contexto via `cb_eval_user_data` (já foi
  Mutex global e disputava com decodes paralelos). `accumulate_capture` junta um
  disparo por **microlote**: o llama.cpp parte o lote em pedaços de `n_ubatch` e
  chama a captura uma vez por pedaço, então substituir em vez de acumular
  guardava só o último - com `fit_memory=true` (padrão, `n_ubatch` limitado a
  128), um chunk de 512 tokens perdia 3/4 e o peer seguinte recusava com "got
  655360 floats, expected 2621440". Todo caminho de decode chama
  `reset_capture_slot` antes, senão a sobra de uma requisição entraria
  concatenada na próxima. A ordem depende do `split_simple` - ver Defeitos
  anotados.
- `profile_backend_sync_enabled` - perfil do backend síncrono, `POM_PROFILE_SYNC`
  (default ligado; "0"/"false" desliga), publicado explicitamente por peer em
  `/health`. `NodeTimings` mantém os contadores em microssegundos e serializa os
  campos `*_ms` de diagnóstico como milissegundos fracionários, então uma
  medição sub-milisegundo não vira zero no wire. `backend_completion_ms` é uma
  duração medida, não uma fonte de verdade sobre o estado da flag; a telemetria
  do Chat não deduz esse estado de um timing zero.
- `unsafe impl Send/Sync` atrás de Mutex do std (o llama.cpp não é thread-safe);
  `Drop` libera só o ctx - os pesos são do `LoadedModel`.
- `SamplingConfig` - temperature/top_p/top_k/repeat_penalty, aplicados por passo.
- `BatchLayout` - transforma a ordem física das linhas do `llama_batch` em
  ranges por `item_id`; a posição global de logits nunca é inferida pelo índice
  da sessão. Mistura de token e hidden state fica em lanes distintas.

`prefill.rs`:
- `run`/`step` - dispatch em três níveis: `run_direct`/`step_direct` quando
  `plan.supports_direct_reply()`, senão `run_ring`/`step_ring` quando
  `supports_relay_chain()`, senão hub.
- `run_direct` - mesmo loop chunk a chunk do ring, mas cada chunk é despachado
  **uma vez** para a cabeça com `reply_to`; a cauda entrega o resultado direto
  ao gateway (`await_result` no ticket). `is_last` é sempre `false` na cabeça
  (o modo direto exige >= 2 hops).
- `step_direct` - o gêmeo de decode: uma chamada de saída (para a cabeça), uma
  espera no ticket. É a diferença entre `2N` travessias (anel) e `N+1` (direto),
  e é por isso que o custo por token deixa de depender de onde mora a camada 0.
- `direct_failure` - resultado que chega com `error` no lugar do token nomeia o
  hop que morreu e marca **esse** (não a cabeça) com `routes.mark_failure`.
- `record_direct_hops` - dobra no `RequestTrace` o `hop_timings` e as evidências
  por travessia recebidas no modo direto; não inventa atribuição para o primeiro
  hop quando a medição não existe.
- `step_speculative` - **NÃO** estendida ao desvio direto: continua no anel/hub
  (não recebe o parâmetro `direct`).
- `speculative_step_from_meta` - interpreta a resposta da cauda, e aqui está o
  ponto que matava a geração em silêncio: a recusa de rascunhar é uma resposta
  **válida**. Uma rodada especulativa tem duas formas legítimas -
  `{done, accepted, accept_count, batch_width}` (a cabeça rascunhou) e o
  formato antigo de um token `{done, token_id, text}` (a cabeça recusou:
  `should_draft` falso). Quem lê a resposta tem que aceitar as duas; tratar a
  antiga como erro fatal derrubava a geração com um token e `partial: true` em
  toda requisição (2026-08-15, Qwen3.8-27B). Sem `draft`, `speculated` é
  `false` e `batch_width` é 1.
- `Speculate`/`DraftSource` (`infer_server.rs`) - o convite a rascunhar que
  viaja no campo `speculate` do quadro: `{k, m, source, draft?}` (`draft` é o
  rascunho pré-fabricado que o gateway injeta no modo fatiado - a cabeça só
  decodifica). `source` é o que separa os **dois** mecanismos, e eles não são o
  mesmo com números diferentes. Quadro sem `source` é gateway antigo e vale
  `Lookup`. `Speculate::to_meta`/`from_meta` e `DraftSource::wire` são o par
  de serialização.
- `should_draft(speculate, kv_can_shift, mtp_ready)` - `Mtp` exige as duas
  condições; `Lookup` só a primeira. Pedir MTP a um motor sem a cabeça `nextn`
  é recusa, nunca troca silenciosa por lookup. Na cauda de um pipeline fatiado
  (`mtp_ready` falso) a cabeça ainda aceita rascunho pronto: `kv_can_shift` e
  `speculate.draft` presente bastam.
- `DEFAULT_CHUNK_TOKENS=512` - ~8 MiB f16 por hop por pedaço
  (medido: 565 KB para 69 tokens, exatamente `hidden_dim 4096 × 2`).
- `RequestTrace`/`PhaseTrace`/`HopTrace` - timings por fase por hop; `round_trip_ms`
  conserva a ida e volta observada de cada chamada para a soma do RTT da execução,
  enquanto `node_lock_wait_ms` separa "nó lento" de "nó ocupado". Os campos
  derivados de `NodeTimings` preservam frações de milissegundo. Isso é telemetria
  do gateway/SSE e não muda o frame entre peers. Cada `PhaseTrace` lista
  `transport_calls` com transporte, fallback, RTT e bytes; `transport_trace_complete`
  denuncia recibos ausentes.

`relay.rs`:
- `send_frame` - HTTP para o próximo hop com o corpo wire-codificado;
  `SentFrame{round_trip_ms, bytes_sent/received, encode/headers/body/decode ms}`
  também identifica `transport` (`local`, `libp2p-tunnel` ou `https`) e se o
  HTTPS foi fallback depois de falha do túnel.
- O transporte normal tenta o túnel libp2p/Noise antes do cliente HTTP pinado;
  `TunnelRequest::get("/health")` é reservado para a medição de RTT de
  topologia. O `round_trip_ms` de `SentFrame` inclui processamento remoto e
  não alimenta a tabela direcional de rotas.
- `bubble_hop_timings`/`as_hop_timing_entry` - insere a entrada do próprio hop
  na frente (ordem head→tail) e dobra `processing_ms`+`node_timings` do callee
  com o round-trip do caller.
- `extract_relay_failure` - usado por `forward_direct` para nomear o hop que
  falhou na entrega do erro direto.
- `RelayFailure{failed_node_id, kind, message}`/`HttpRefusal`/`SendIssue` - o
  gateway responde 502 atribuindo o hop que falhou; `CHUNK_ATTEMPTS=3`.

`direct.rs`:
- `ReplyInbox`/`Ticket` - mapa ticket→canal oneshot que vive no `GatewayState`
  (é o gateway que abre a requisição); `register` sorteia UUID v4 por quadro;
  `Drop` remove o ticket do inbox em qualquer caminho de saída (erro, prazo) -
  é o que impede um ticket órfão de segurar memória e um resultado atrasado de
  cair no token seguinte. `deliver` devolve `false` para ticket já desistido.
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
  a autenticação é o próprio ticket (UUID v4 por quadro, só a própria cadeia
  conhece); resultado sem ticket vivo é `410 Gone`. A rota é registrada **sem**
  auth de chave por desenho: o hop que responde nunca teve a chave do operador.
- `await_result` - `select!` biased entre três coisas: o resultado do inbox
  (caso normal), o ack da cabeça (só interessa quando falha, ou quando um peer
  antigo borbulhou a resposta inteira - `token_id` presente vale como
  resultado), e o prazo `DIRECT_REPLY_DEADLINE=600 s` (hop morreu depois de
  aceitar).
- `deliver_result` - a cauda (ou o hop que falhou) tenta cada URL de `reply_to`
  em ordem, `DELIVERY_TIMEOUT=30 s` por tentativa; `410` para de tentar (o
  inbox já não existe); falhou em todas, o gateway fecha no prazo - não há
  terceira via.
- `deliver_failure` - entrega um `RelayFailure` no lugar do resultado para o
  gateway nomear o nó morto em vez de esperar o prazo inteiro.
- `hop_entry`/`accumulate` - cada hop descreve a si mesmo na ida: a lista viaja
  cabeça→cauda e a cauda a entrega junto do resultado.
- `record_transport`/`deliver_transport` - o chamador entrega uma evidência
  separada por travessia, com transporte, RTT e bytes. O inbox aguarda essas
  evidências por uma janela curta depois do resultado e marca a trace como
  incompleta quando alguma não chega.

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
  onde ela roda.
- `the_cost_per_token_does_not_depend_on_where_the_entry_point_is` - para
  N=2,3,4 com o gateway na cabeça, no meio e na cauda, pernas por token
  ≈ `minimum_legs` (tolerância 0,45 perna); as duas pontas empatam; a pior
  entrada (meio) custa no máximo uma perna a mais que a cabeça; e o anel
  borbulhado com a cabeça remota paga 4 pernas (o caso que doía, a queda de 25
  para 14 tok/s do captain), provando que é correção e não o comportamento de
  sempre. Assere ainda que cada quadro é entregue direto exatamente uma vez
  (prefill e decode) e que o prompt de mais de um pedaço atravessa inteiro (um
  `prompt_progress` perdido trunca no primeiro).
- Herda da investigação de 2026-08-14 (`labsse.rs`), que provou a assimetria e
  refutou a hipótese do SSE por contrafactual.
- **Sem cobertura de teste**: os quatro modos de falha (hop do meio morto,
  cauda morta, resposta direta perdida, prazo estourado) têm desenho e código
  em `direct.rs`, não teste de ponta a ponta.

`runtime.rs`:
- `NodeConfig` - tudo que define a fatia servida: model_path/label,
  start/end/total_layers, peer_count/index, primary, inference_port, n_gpu_layers,
  n_ctx, offload_kqv, n_batch, max_sessions, batching_enabled,
  max_queue_requests, batch_window_ms, max_batch_sequences,
  max_batch_tokens, prefill_chunk_tokens, max_new_tokens,
  sampling, no_thinking, single_peer, no_embeddings, speculative_decoding_enabled,
  speculative_k/lookup_window, mtp_enabled, mtp_draft_tokens,
  mtp_min_probability.
- `NodeConfig::mtp_effective` - o MTP liga em quem serve a última camada do
  modelo (onde mora o bloco `nextn`): o modelo inteiro (start_layer 0,
  peer_count 1) **ou** a cauda de um pipeline fatiado (start_layer > 0,
  peer_count > 1). Ver a seção "Predição multi-token" abaixo.
- `NodeRuntime::apply` - aplica a fatia e sobe o servidor Rust embutido
  (`infer_server::run`); muda a config → reinicia o servidor. Anota a fatia em
  `models/lastslice.rs` como **Pending** - mas a anotação só diz o que o `apply`
  decidiu, não o que o servidor conseguiu carregar.
- `spawn_health_watch` - o **único** ponto que prova a carga: quando `/health`
  do servidor responde `model_loaded`, promove a anotação de Pending para
  **Serving**. Só a anotação `Serving` autoriza retomar a fatia num reinício.
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
- `MAX_RESTARTS=10`, `MAX_CONTEXT_TOKENS=250_000`. O contador é por processo:
  se a fatia não cabe na memória, o processo morre antes de carregar e o
  `MAX_RESTARTS` morre junto - quem segura esse caso é a anotação (um processo
  que desiste em `MAX_RESTARTS` apaga a anotação, e a `NeverLoaded` não
  retoma).

`wire.rs`:
- `BatchFrame`/`BatchItem` - `frame_version=1`, vários itens nomeados no mesmo
  payload concatenado; `encode_batch`/`decode_batch` rejeitam dtype desconhecido,
  shape divergente, offsets fora do payload, sobreposição, item duplicado e
  payload acima de 64 MiB.
- `encode_batch_items`/`decode_batch_items` são o caminho distribuído; um
  pedido de uma sessão usa o mesmo protocolo por meio de
  `encode_batch_single`/`decode_batch_single`. Não existe fallback para frames
  v1/v2 anteriores, pois o produto ainda não foi lançado.
- `FrameEncoding::F16` permanece somente como marcador de chamadas internas;
  o fio oficial é sempre `inference/frame-v1-batch`. `WireDtype{F32, F16}`
  continua com `as_str`/`parse`/`bytes_per_value`.
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
frente **sem custo extra de travessia**: a cauda anexa `next_draft` à resposta
da `relay/v2-direct` que já volta ao gateway, o gateway o injeta no
`speculate.draft` da rodada seguinte, e a cabeça só decodifica (`should_draft`
aceita draft pronto com `kv_can_shift`, mesmo sem `mtp_ready`). A cabeça ecoa a
composição do lote (`tokens`) e a largura pedida (`mtp_k`) no reply, os hops do
meio repassam, e a cauda alimenta o `observe` com isso
(`decode_hidden(..., tokens)`) - sem o eco, o `observe` desliga o MTP com aviso,
e a resposta continua correta, só não especulativa. `PipelinePlan::supports_mtp`
exige `speculate/v1-mtp-sliced` em **todos** os hops (o protocolo `tokens`/
`mtp_k`/`next_draft` só existe em build novo) e `speculate/v1-mtp` na cauda.
Cauda híbrida/recorrente (`kv_can_shift` falso) não devolve `next_draft`; o
gateway para de pedir pelo resto da requisição, como no prompt-lookup.

**Ainda sem medição de tok/s** (cortado por cota em 2026-08-16): o ganho
prometido pelo bloco é real no LM Studio (21 → 35 tok/s no Qwen3.8-27B-Q3_K_S,
medido pelo captain), aqui a implementação está completa e verde nos testes de
unidade mas **não** tem número medido de ponta a ponta.

## Ligações

- **Chamado por**: `backend/gateway.rs` (monta `DirectReply` com `ReplyInbox` +
  `self_reply_urls`, registra `/v1/relay-result` sem auth de chave, passa
  `direct.as_ref()` a `prefill::run`/`step`; `step_speculative` vai sem
  direct), `backend/webui.rs` (rollback/release fan-out), `main.rs`
  (`NodeRuntime::apply` no arranque, inclusive com `lastslice::take`),
  `routing/pipeline.rs` (as capabilities `CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
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

## Defeitos anotados

Nenhum no momento.

O que estava anotado aqui - "a captura do residual assume a ordem do
`split_simple`, que só vale com `max_sessions = 1`" - deixou de valer quando o
cache de prefixo passou a exigir duas sequências de KV por sessão: os dois
pontos que criam contexto agora ligam `kv_unified` incondicionalmente
(`llm::kv_seq_budget`), e com `kv_unified` o cache tem um stream só
(`n_stream = kv_unified ? 1 : n_seq_max`) qualquer que seja `max_sessions`. O
llama.cpp continua, portanto, usando `split_simple`, que é a ordem de linhas que
`BatchLayout` espera. Desligar `kv_unified` reintroduz o defeito em silêncio -
está dito no doc de `accumulate_capture`.

## Manutenção deste arquivo

Este arquivo é um mapa de responsabilidades da pasta. Ao mudar uma interface
de inferência ou uma invariável de sessão, atualize aqui e o README do
subsistema no mesmo commit. Mantenha detalhes de implementação no código ou
em `docs/subsystems/inferencia.md`, para que este índice continue curto.
