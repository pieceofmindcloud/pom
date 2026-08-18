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
  token para N=2,3,4 com o gateway na cabeça/meio/cauda.
- `infer_server.rs` (2204) - axum server: `/v1/prefill`, `/v1/step`,
  `/v1/session/release`, `/v1/session/rollback`, `/health`; estado das sessões
  e o branch de relay.
- `llm.rs` (1466) - wrapper llama-cpp-sys-2: carga, decode, hidden state com
  residual capture, sampling, e o dono da cabeça de rascunho `nextn`
  (`Option<MtpDrafter>`).
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
  por requisição.
- `relay.rs` (247) - `send_frame` para o próximo hop + `bubble_hop_timings`.
- `runtime.rs` (977) - `NodeConfig`/`NodeRuntime`: aplica a fatia, sobe e
  supervisa o servidor Rust embutido (`infer_server::run`), e mantém a
  anotação da fatia servida (Pending → Serving via `/health`).
- `spec_lab.rs` (613, `#![cfg(test)]`) - a bancada da especulação:
  `generate_stream` de verdade contra hops falsos que decidem com
  `should_draft`/`find_draft`/`longest_match` de produção, com `kv_can_shift`
  como botão.
- `tp.rs` (novo) - registro do callback de tensor parallelism no llama.cpp
  (caminho C): `register_tp_reduce`/`unregister_tp_reduce` e o `extern "C"` que
  lê a parcial (f32), POSTa para o `/v1/tp/reduce` do primary (que soma na
  `ReduceBarrier` do `infer_server`) e devolve a soma no buffer. O cliente é
  **pinado no `node_id` do primary** (`TpIdentity.rank0_node_id`): um
  `reqwest::Client` padrão recusa o certificado autoassinado do cluster e todo
  POST morria em 13 ms. O fork insere o op de reduce (`GGML_OP_CUSTOM`) depois
  de wo/ffn_down e chama esse callback durante o decode - confirmado em campo:
  `graph splits = 66` (2 por camada) e o log do primeiro reduce.

  Uma **réplica não tem opinião** (`tp::is_replica`): ela contribui com a metade
  dela em cada reduce e nada mais. Quem amostra o token, decide o fim da geração
  e encerra a sessão é o rank 0. A amostragem é estocástica e o RNG de cada
  processo é independente, então o worker amostrava EOS enquanto o rank 0
  amostrava um token normal, apagava a sessão, e o step replicado seguinte
  batia em "sessão desconhecida" - HTTP 400 (18/08).

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

  `relay::send_bytes_or_tunnel` tenta primeiro o HTTPS direto e, em falha de
  transporte ou timeout, repete o POST pelo `infer_tunnel` quando o membro
  anunciou `libp2p_peer_id` e `transport/libp2p-tunnel-v1`. `TpMember` carrega
  essa identidade e a capability para o `broadcast_to` do decode e para o
  reduce no primary. O `TunnelRequest` leva também os cabeçalhos, para que a
  barreira e o rank sejam preservados no salto pela internet.

  O snapshot `TpDecodeTelemetry`, anexado a `node_timings.tp`, separa
  reduces esperados e observados, espera total, p50/p95, bytes de
  request/response, broadcasts e chamadas diretas, pelo túnel ou por fallback.
  Ele mede a espera da barreira síncrona, não uma latência pura de pacote. Em
  um modelo de 32 camadas, a arquitetura atual tem dois pontos de coletiva por
  camada, ou 64 esperas síncronas por `llama_decode` de um token; isso é TP e
  não deve ser confundido com os frames de estado oculto do pipeline.

  O ciclo de vida da sessão da réplica é comandado pelo rank 0:
  `/v1/session/release` é repassado aos membros (`replicate_session_call`),
  porque o gateway só fala com o rank 0 e a réplica não encerra sessão sozinha.
  Sem o repasse a sessão do worker segurava o único slot e a **segunda**
  requisição morria com "seq pool esgotado" (18/08).

  **DEFEITO ANOTADO: `/v1/session/rollback` não é repassado às réplicas.**
  Ele só é usado pela decodificação especulativa, que está desligada nas configs
  de TP hoje; ligar especulação num grupo TP deixaria o KV do worker à frente do
  rank 0 depois de um rascunho recusado.

  Falha da coletiva **não pode ser silenciosa**: ela não degrada a resposta, ela
  a torna falsa. `note_failure`/`take_failure` levam o motivo até o decode, que
  derruba a requisição; e `decode_tokens` ainda compara `reduce_count()` antes e
  depois, porque "o callback nunca foi chamado" não tinha sintoma nenhum além do
  texto sem sentido (18/08).
- `wire.rs` (574) - o único lugar que conhece o dtype/codificação do tensor
  em trânsito.

## Funções-chave

`infer_server.rs`:
- `InferenceServer::new` + rotas - monta o router axum sobre o `LlmEngine` e o
  estado de sessões (sessions, seq_pool, max_sessions, sampling).
- `handle_prefill`/`handle_step` - dispatch para `prefill::run`/`run_ring`/
  `run_direct` e `step_speculative`; `spawn_blocking` para os decodes não
  travarem o runtime.
- bloco de resposta direta (nos dois handlers, quando `direct_reply_to(&meta)`
  existe) - acumula `hop_timings` na ida (`direct::accumulate` +
  `direct::hop_entry`); cauda (`next_hops` vazio): spawna `direct::deliver_result`
  ao `reply_to` e devolve `direct_ack()`; meio: spawna `forward_direct` para o
  próximo hop; meio sem residual local: `deliver_failure` com "hop intermediário
  não produziu estado oculto".
- `forward_direct` - repasse assíncrono em task própria (quem chamou já foi
  aceito e não espera mais nada); repassa `reply_to` intacto; remonta `scale=1.0`,
  `is_last` e `next_hops` remanescente. Falhou o repasse, entrega
  `direct::deliver_failure` nomeando o nó que realmente morreu.
- `direct_ack` - `{"accepted": true}` wire-codificado; é tudo que volta pelo
  caminho de quem chamou.
- `direct_reply_to` - o campo `reply_to` do meta; ausente no hub e no anel, é
  só ele que liga o caminho novo.
- `prompt_progress_for_forward`/`apply_prompt_progress` - a contraparte direta
  de `overlay_head_authoritative_fields` (agora `pub(crate)`): sem volta, a
  cabeça manda `more_chunks`/`n_prompt_tokens` **adiante** e quem entrega copia
  para o resultado - senão todo prompt de mais de um pedaço trunca no primeiro.
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
  `INFERENCE_FRAME_BODY_LIMIT=16 MiB` corta frame impossível no body.
- `tcp_nodelay(true)` no `axum::serve` - ver nota gêmea em
  `gateway::start_gateway`: sem isto, o ack de resposta direta (escrita pequena)
  fica preso no Nagle.
- `/health` anuncia `relay/v2-direct` junto das outras capabilities e expõe
  `kv_can_shift` (se o modelo permite deslocar o KV, condição de a cabeça
  rascunhar).
- `should_draft` (e `find_draft`/`longest_match`) - a cabeça decide se
  rascunha: `speculate` pedido **e** `kv_can_shift`. Recusar é resposta
  legítima, não erro: a cabeça omite `draft` no quadro e a cauda responde no
  formato antigo de um token (ver `step_blocking::incoming_draft` e a seção de
  duas formas de resposta em `prefill.rs`).

`llm.rs`:
- `LlmEngine::load` - carga com `EngineConfig` (n_ctx, n_batch, n_gpu_layers,
  offload_kqv, fit_memory, max_sessions). Com `fit_memory=true` e o padrão de
  camadas GPU (`99`), chama a ponte `llama_rs_fit_params` do llama.cpp antes de
  carregar os pesos. O plano preserva `tensor_split` e `tensor_buft_overrides`
  quando há mais de uma GPU local. Se a criação do contexto falha com o KV na
  GPU, tenta novamente com `offload_kqv=false`, mantendo as mesmas camadas de
  peso na GPU e deixando o KV cache na RAM. `/health` mostra se o fit foi
  aplicado e as camadas efetivas.
  `LlmEngine::pool` (N contextos sobre um modelo só, `pool_tests` cobre a
  divisão de n_ctx) ainda **não tem chamador**: o servidor carrega um contexto
  com `load`, e o `#[allow(dead_code)]` no `pool` é a marca disso.
- captura residual - `ggml_backend_sched_eval_callback` no `l_out-<N>` antes do
  `output_norm`, com `CaptureSlot` por contexto via `cb_eval_user_data` (já foi
  Mutex global e disputava com decodes paralelos).
- `profile_backend_sync` - perfil do backend síncrono, `POM_PROFILE_SYNC`
  (default ligado; "0"/"false" desliga).
- `unsafe impl Send/Sync` atrás de Mutex do std (o llama.cpp não é thread-safe);
  `Drop` libera só o ctx - os pesos são do `LoadedModel`.
- `SamplingConfig` - temperature/top_p/top_k/repeat_penalty, aplicados por passo.

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
- `record_direct_hops` + `direct::credit_transport` - telemetria da ida dobrada
  no `RequestTrace`; o transporte não atribuído vai para a entrada da cabeça
  (ver o doc de `direct::hop_entry`).
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
- `RequestTrace`/`PhaseTrace`/`HopTrace` - timings por fase por hop; o campo
  `node_lock_wait_ms` separa "nó lento" de "nó ocupado".

`relay.rs`:
- `send_frame` - HTTP para o próximo hop com o corpo wire-codificado;
  `SentFrame{round_trip_ms, bytes_sent/received, encode/headers/body/decode ms}`.
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
- `hop_entry`/`accumulate` - cada hop descreve a si mesmo na ida: `bytes_sent`
  é o quadro que entrou, `round_trip_ms` = `processing_ms` porque a perna de
  rede não é observável por ninguém no modo direto; a lista viaja cabeça→cauda
  e a cauda a entrega junto do resultado.
- `credit_transport` - fecha a telemetria com o que só o gateway sabe: o
  transporte não atribuído (total menos a soma dos `processing_ms` declarados)
  vai para o `round_trip_ms` da **cabeça**, e os `bytes_received` do resultado
  para a cauda. O número somado continua certo; dividido por hop, ele não
  existe.

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
  n_ctx, offload_kqv, n_batch, max_sessions, prefill_chunk_tokens, max_new_tokens,
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
- `encode`/`encode_with_encoding`/`decode` - `[4B BE len][JSON header][tensor cru]`;
  dtype/shape no JSON, corpo binário f16 (nunca base64, nunca f32 na rede).
- `FrameEncoding{F16, ZstdF16}` - zstd é opt-in (`frame_version=2`,
  `ZSTD_LEVEL=6`); `WireDtype{F32, F16}` com `as_str`/`parse`/`bytes_per_value`.
- `MAX_FRAME_PAYLOAD_BYTES=16 MiB`.

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

Nenhum em aberto nesta pasta.
