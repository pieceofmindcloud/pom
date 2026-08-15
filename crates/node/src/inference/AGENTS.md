# inference/ - servidor de inferência, wire format e supervisão

A porta de inferência (8181) e tudo que roda nela: o axum server
(`infer_server.rs`), o wrapper FFI do llama.cpp (`llm.rs`), a execução do
prefill/step nos três modos - hub, anel borbulhado (`relay/v1-chain`) e
resposta direta (`relay/v2-direct`, `direct.rs`) - a "lei" que mede o modo
direto em pernas de rede (`direct_lab.rs`), a bancada da especulação
(`spec_lab.rs`), o formato binário do tensor na rede (`wire.rs`) e a
supervisão do servidor Rust embutido (`runtime.rs`), que também mantém a
anotação da fatia servida (estado Pending/Serving em `models/lastslice.rs`).
O modelo é carregado **em-processo** por `llm.rs`; não há processo Python
(`inference_server.py` de `old/python-inference/` é legado). Nada aqui fala
com peers: `next_hops` chega pronto do `routing/pipeline.rs` e este diretório
executa a cadeia que o planejador montou.

## Arquivos

- `direct.rs` (474) - a resposta direta: a cauda entrega o resultado ao gateway
  por `/v1/relay-result` em vez de borbulhar pela pilha aninhada; inbox de
  tickets, `reply_to`, telemetria acumulada na ida.
- `direct_lab.rs` (580, `#![cfg(test)]`) - a lei: hops falsos falando o
  protocolo real, rede emulada por sítio, exige o mínimo teórico de pernas por
  token para N=2,3,4 com o gateway na cabeça/meio/cauda.
- `infer_server.rs` (2204) - axum server: `/v1/prefill`, `/v1/step`,
  `/v1/session/release`, `/v1/session/rollback`, `/health`; estado das sessões
  e o branch de relay.
- `llm.rs` (1374) - wrapper llama-cpp-sys-2: carga, decode, hidden state com
  residual capture, sampling.
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
  offload_kqv, max_sessions); uma `LlmContext` por sessão (`pool_tests` cobre a
  divisão de n_ctx por sessão).
- captura residual - `ggml_backend_sched_eval_callback` no `l_out-<N>` antes do
  `output_norm`, com `CaptureSlot` por contexto via `cb_eval_user_data` (já foi
  Mutex global e disputava com decodes paralelos).
- `profile_backend_sync` - perfil do backend síncrono, `RDMA_P2P_PROFILE_SYNC`
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
- `chunk_count`/`DEFAULT_CHUNK_TOKENS=512` - ~8 MiB f16 por hop por pedaço
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
  speculative_k/lookup_window.
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
  (SIGKILL). É a resposta ao "porta ocupada" que antes fazia o supervisor
  desistir em silêncio.
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

## Ligações

- **Chamado por**: `backend/gateway.rs` (monta `DirectReply` com `ReplyInbox` +
  `self_reply_urls`, registra `/v1/relay-result` sem auth de chave, passa
  `direct.as_ref()` a `prefill::run`/`step`; `step_speculative` vai sem
  direct), `backend/webui.rs` (rollback/release fan-out), `main.rs`
  (`NodeRuntime::apply` no arranque, inclusive com `lastslice::take`),
  `routing/pipeline.rs` (as capabilities `CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
  `CAP_SPECULATE_LOOKUP` decidem anel vs direto vs hub e o wire format por hop).
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

- **DEFEITO ANOTADO: `set_log` nunca é chamado** (`runtime.rs:704`):
  a função existe para redirecionar os logs do servidor embutido para um
  arquivo (`infer_server_<porta>.log`), mas nenhum caminho a invoca, então
  `log_path` fica sempre `None` e todo log vai para o stdout do processo - a
  TUI limpa a tela a cada `--refresh`, então o único jeito de ver é redirecionar
  o stdout do binário. O arquivo `inference_server_<porta>.log` (Python, antigo)
  não existe mais.
- **DEFEITO ANOTADO: desistência residual da porta ocupada** (`runtime.rs`):
  `free_port` mata o ocupante antes de subir, mas quando o ocupante não é
  identificável (`lsof`/`netstat` falham) ou a porta segue presa, o supervisor
  esgota `MAX_RESTARTS` em silêncio e a TUI continua mostrando "Mem loaded:
  inference_server" como se tudo estivesse bem - o peer só quebra na
  inferência, com `encerrou com erro: Address already in use` no log do nó.
