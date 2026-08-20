# Caminho de inferência

O que o código faz hoje, verificado nos fontes (ver nota de versão no fim).

Um pedido de geração (OpenAI `/v1/chat/completions`, Anthropic `/v1/messages`,
`POST /api/generate`) desce por um único caminho: `gateway::generate_stream`.
Ele planeja a cadeia de peers com `routing/pipeline.rs`, manda o prompt e cada
token pelo pipeline distribuído e devolve tokens ao cliente. Este documento
descreve os três modos de execução, o formato binário do estado oculto, o
prefill em pedaços, a resposta direta, a decodificação especulativa e os
limites.

## 0. Coletiva de tensor parallelism (`collective.rs`)

Para o caminho C (tensor parallelism sem transferir peso), o núcleo de troca é a
coletiva all-reduce: cada rank produz uma soma parcial da projeção por linhas e
o resultado é a soma das parciais. `inference/collective.rs` tem `reduce_sum` (a
soma matemática) e `ReduceBarrier` (a barreira que acumula os parciais de cada
rank e devolve a soma a todos via `oneshot`, modelada no `ReplyInbox` de
`direct.rs`). O transporte HTTP (`POST /v1/tp/reduce`) é uma camada fina por
cima, ligada junto com o fork do llama.cpp que chama a coletiva no meio do
forward pass. `broadcast_to` e o reduce tentam primeiro o túnel libp2p quando o
membro anunciou `PeerId` e capability. Se o transporte Noise falhar, usam o
cliente direto pinado, com HTTPS quando a política TLS está ativa.
`TunnelRequest` preserva os cabeçalhos da barreira, do rank e da autorização.
As rotas de inferência aceitam a `connection_key` compartilhada ou uma API key
ativa. Ver
`docs/superpowers/specs/2026-08-17-tensor-parallelism-design.md`.

A medição de RTT do mapa é separada do tempo de um frame: `measure_rtt` envia
um GET vazio para `/health` pelo mesmo túnel libp2p, sem prefill, decode ou
estado oculto. Portanto `round_trip_ms` de um frame não é usado como RTT de
topologia.

Cada execução também pode carregar `node_timings.tp`. Esse bloco registra
reduces esperados e observados, espera total, p50/p95 da coletiva, bytes de
request/response, broadcasts e o caminho direto ou túnel. A espera inclui a
barreira síncrona e não é latência pura de pacote. Os bytes e waits TP ficam
separados dos frames de hidden state do pipeline. Com 32 camadas e dois pontos
row-parallel por camada, um decode de um token pode fazer 64 esperas coletivas;
isso é inerentemente caro entre máquinas na internet. Para WAN, pipeline com
camadas completas por nó troca um frame de hidden state entre blocos e evita
essas coletivas TP.

### Cache KV e fallback de VRAM

`n_gpu_layers` continua controlando onde ficam os pesos. O contexto tenta
primeiro respeitar `offload_kqv=true`, colocando o KV cache na GPU. Se o
`cudaMalloc` do KV cache falhar, `LlmEngine::load` repete a criação do contexto
com `offload_kqv=false`: os pesos permanecem nas mesmas camadas da GPU e o KV
cache passa para a memória RAM. A aplicação registra o fallback e `/health`
mostra o valor efetivo. Isso resolve falta de VRAM para o cache; se os próprios
pesos não couberem, ainda é necessário reduzir `n_gpu_layers` ou dividir o
modelo entre máquinas.

Nos deployments de pipeline ou de modelo inteiro, `RuntimeParams.fit_memory=true`
fica ligado por padrão. Antes da carga real, a aplicação chama o
`common_fit_params` do llama.cpp, que pode reduzir `n_gpu_layers` para caber na
memória disponível e preserva o `n_ctx` solicitado quando ele foi definido
explicitamente. O `/health` expõe `fit_memory_applied` e
`n_gpu_layers_effective`. Um `n_gpu_layers` diferente do padrão `99` é uma
escolha explícita e desliga o ajuste automático de camadas. O fit nativo não
suporta tensor parallelism; o planner e o runtime normalizam essa opção para
desligada no modo TP. O fallback do KV continua ativo depois do fit.
Para evitar que a reserva tardia de temporários CUDA derrube o processo quando a
GPU já está quase cheia, `fit_memory=true` mantém `n_batch` como limite do
chunk, mas usa `n_ubatch=min(n_batch, 128)` no contexto. O prompt continua
aceitando chunks de até `n_batch`; o custo é mais etapas internas de cálculo em
troca de uma reserva menor. `/health` expõe esse valor como
`n_ubatch_effective`. Com `fit_memory=false`, `n_ubatch` acompanha `n_batch`.
Quando o MTP está ligado, o carregamento também passa
`llama_model_params.load_mtp=true`, para que os pesos `nextn.*` sejam carregados
antes de criar o contexto de rascunho; deixar essa flag falsa mantém os
metadados MTP, mas deixa `nextn.eh_proj` nulo e faz o Qwen3.5 abortar na
construção do grafo.

## 1. Os três modos de execução

O dispatch acontece em `inference/prefill.rs` (`run` para o prefill, `step`
para cada token), nesta ordem:

1. **Resposta direta** (`relay/v2-direct`, `run_direct`/`step_direct`) - quando
   `PipelinePlan::supports_direct_reply()`: todos os passos anunciam
   `relay/v2-direct` **e** a cadeia tem pelo menos 2 hops. O gateway despacha
   cada quadro uma vez para a cabeça, carregando um `reply_to`; a cauda entrega
   o resultado direto ao gateway por `/v1/relay-result`.
2. **Anel borbulhado** (`relay/v1-chain`, `run_ring`/`step_ring`) - quando
   `supports_relay_chain()`: o gateway faz **uma** chamada (para a cabeça) e
   cada hop repassa ao seu próximo via `next_hops`; o resultado volta borbulhando
   pela pilha de chamadas aninhada.
3. **Hub** - o resto: o gateway chama cada hop em sequência, recebe o tensor de
   volta e reenvia ao seguinte.

O log do gateway marca qual modo entrou (`resposta=direta|anel|hub`). A escolha
é all-or-nothing por requisição: um único passo sem a capability derruba a
feature do plano inteiro (`supports_relay_chain`/`supports_direct_reply`/
`supports_speculation` em `routing/pipeline.rs`), para não misturar modos no
meio da cadeia. Nada a configurar numa atualização progressiva: um cluster com
nós velhos cai no anel de sempre.

## 2. Protocolo de estado oculto (`wire.rs`)

O tensor que atravessa a rede é o **único** lugar que conhece o dtype e a
codificação em trânsito. Formato:

```
[4 bytes big-endian: tamanho do header][JSON header][tensor cru]
```

- `dtype`/`shape` vão no JSON; o corpo é **binário** f16 (nunca base64, nunca
  f32 na rede). Precisão e codificação são parâmetro de `encode`/`decode`, não
  suposição.
- `FrameEncoding::F16` é o contrato v1; `ZstdF16` (`frame_version=2`,
  `ZSTD_LEVEL=6`) é opt-in: só é usado quando o receptor anuncia
  `frame/v2-zstd-f16`. `PipelineStep::frame_encoding` escolhe por passo, o que
  mantém rolling upgrade com nós velhos.
- `MAX_FRAME_PAYLOAD_BYTES = 16 MiB` limita payload no decode; o handler axum
  aplica o mesmo teto no body (`INFERENCE_FRAME_BODY_LIMIT`).
- O decode sempre devolve o tensor como f32: precisão reduzida é propriedade do
  fio, nunca da matemática (f16 é a precisão da GPU mesmo, então não custa
  qualidade - diferente do int8, considerado e recusado).

Medido com `tcpdump` no peer 2 (requisição de 35 tokens de prompt + 34
gerados): 565.336 bytes de estado oculto da cabeça para a cauda, 6.715 bytes de
tokens amostrados na volta. 565.336 / 69 tokens = **8.192 bytes por token por
hop**, exatamente `hidden_dim 4096 × 2` (f16). O formato antigo (f32 em
base64) custava 4096 × 4 × 4/3 = 21.846 bytes por token por hop - **62,5%
menos bytes** na rede hoje. Um pedaço de prefill de 512 tokens é ~8 MiB f16 por
hop por pedaço.

## 3. Prefill e step

Antes, o prompt cruzava num bloco só: memória escalava com o prompt, tudo tinha
que caber em `n_batch`, e um corpo de multi-gigabyte morria num enlace de
internet. Hoje o prompt avança em **pedaços** (`prefill.rs`): cada pedaço é
decodificado na própria posição e o estado oculto viaja sozinho.

- **Unidade de retry e de memória**: `chunk_size` (padrão
  `DEFAULT_CHUNK_TOKENS = 512`). Na prática o chunk é
  `min(prefill_chunk_tokens, n_batch)` (`prompt_chunk_of`), porque um pedaço é
  decodificado como um batch no peer e maior que `n_batch` abortaria o llama.cpp
  igual o prompt inteiro abortava.
- **Retentativa idempotente**: um pedaço perdido custa um pedaço, não a
  requisição. `relay::CHUNK_ATTEMPTS = 3` com backoff de 400 ms × tentativa. O
  peer receptor rebobina o KV cache para a posição do pedaço antes de refazer
  (`kv_remove` + `n_past = chunk_index * chunk_size`), então um reenvio nunca é
  contado duas vezes. Se a sessão já passou da posição, rebobina; se ainda não
  chegou lá, recusa com "chunk fora de ordem".
- **Step não retenta**: decode não tem proteção de rollback de KV, então o
  `step` sempre vai com `attempts = 1`. Prefill e decode não compartilham a
  constante para ela não derivar.
- Só a cabeça tokeniza (tem o prompt), e é a única autoridade em
  `more_chunks`/`n_prompt_tokens`. O gateway sempre manda `is_final_chunk:
  false` e quem tokenizou corrige (ver `prompt_progress` na seção 4).

## 4. Modo anel vs resposta direta

**Anel** (`relay/v1-chain`): pilha de chamadas HTTP aninhadas. Cada hop segura
a própria resposta aberta enquanto chama o seguinte, e o resultado volta
refazendo a ida. São **2N travessias de rede por token** numa cadeia de N hops,
e a conta piora quando a cabeça não mora na máquina que recebeu o pedido:
medido em 2026-08-14 entre os dois sites do captain (RTT 30 ms), 25 tok/s com a
cabeça local contra 14 tok/s com ela remota - exatamente um RTT a mais por
token.

**Resposta direta** (`relay/v2-direct`, `inference/direct.rs`): a cadeia não
volta. O gateway manda junto do quadro um `reply_to` - os endereços pelos quais
ele mesmo é alcançável, mais um **ticket** (UUID v4 sorteado por quadro). Cada
hop repassa `reply_to` intacto; quem termina (a cauda, ou o hop que falhou ao
repassar) faz **um** POST em `/v1/relay-result` direto ao gateway. São **N+1
travessias**, para qualquer N e qualquer ponto de entrada. O que cada hop
devolve ao chamador é só um aceite imediato (`direct_ack`, `{"accepted": true}`
wire-codificado), fora do caminho crítico - quem espera é o gateway, no ticket.

- `ReplyInbox`/`Ticket`: mapa ticket → canal oneshot que vive no `GatewayState`.
  O `Drop` do `Ticket` remove o ticket do inbox em **qualquer** caminho de saída
  (erro, prazo estourado) - impede ticket órfão de segurar memória e um
  resultado atrasado de cair no token seguinte. Resultado para ticket já
  desistido é **410 Gone**, e como o ticket é único por quadro nunca pode ser
  confundido com o do quadro seguinte.
- `DirectReply::new` devolve `None` quando o nó não conhece nenhum endereço de
  si mesmo - aí o anel assume em vez de despachar um quadro que nunca volta.
- `await_result`: `select!` biased entre o resultado do inbox (caso normal), o
  ack da cabeça (só interessa quando falha, ou quando um peer antigo borbulhou
  a resposta inteira - `token_id` presente vale como resultado), e o prazo
  `DIRECT_REPLY_DEADLINE = 600 s` (hop morreu depois de aceitar).
- `deliver_result`: a cauda tenta cada URL de `reply_to` em ordem,
  `DELIVERY_TIMEOUT = 30 s` por tentativa; `410` para de tentar (o inbox já não
  existe); falhou em todas, o gateway fecha no prazo - não há terceira via.
  `deliver_failure` entrega um `RelayFailure` no lugar do resultado, para o
  gateway nomear o nó que morreu em vez de esperar o prazo inteiro.
- `forward_direct`: o repasse corre em task própria (quem chamou já foi aceito).
  Falhou o repasse, o próprio hop denuncia direto ao gateway nomeando o nó que
  realmente morreu.
- Falha que chega com `error` no lugar do token (`direct_failure`) nomeia o hop
  morto e marca **esse** (não a cabeça) com `routes.mark_failure`.

**`prompt_progress`**: sem volta, a cabeça não pode mais sobrepor
`more_chunks`/`n_prompt_tokens` na resposta que passa por ela (no anel isso era
`overlay_head_authoritative_fields`). Ela manda os dois **adiante**
(`prompt_progress_for_forward`) e a cauda copia para o resultado
(`apply_prompt_progress`). Errar isso trunca todo prompt de mais de um pedaço
no primeiro, em silêncio.

**Telemetria** (`direct::hop_entry`/`accumulate`/`credit_transport`): cada hop
descreve a si mesmo na ida (`bytes_sent` = quadro que entrou; `round_trip_ms` =
`processing_ms`, porque a perna de rede não é observável por ninguém no modo
direto). A lista viaja cabeça→cauda e a cauda a entrega junto do resultado. O
transporte não atribuído (total menos a soma dos `processing_ms` declarados) vai
para o `round_trip_ms` da **cabeça**, e os `bytes_received` do resultado para a
cauda. O número somado continua certo; dividido por hop, ele não existe.

## 5. Decodificação especulativa

São **dois** mecanismos, com chaves de operador separadas, e nunca os dois na
mesma rodada (o lote é um só, e misturar as fontes tornaria a taxa de aceitação
impossível de atribuir). O gateway escolhe em `generate_stream`: com as duas
ligadas e o plano capaz, vence o MTP.

| | `speculative_decoding_enabled` | `mtp_enabled` |
|---|---|---|
| o que rascunha | `find_draft`: busca uma repetição do próprio final no texto já gerado | o bloco `nextn` treinado dentro do GGUF |
| usa peso do modelo | não - é busca em array | sim, uma cabeça treinada |
| quando acerta | saída repetitiva (código, listas, JSON) | texto qualquer, se o modelo trouxer o bloco |
| capability | `speculate/v1-lookup` (todos os hops) | `speculate/v1-mtp` (**a cauda**) + `speculate/v1-mtp-sliced` em todos |
| exige | nada além do pedido | a cauda com o bloco `nextn` (ou um `draft` já pronto) |

O campo que separa as duas no fio é `speculate.source` (`"lookup"` / `"mtp"`),
lido por `infer_server::Speculate::from_meta`. Ausente é gateway antigo e vale
`lookup`, que era a única especulação que existia quando ele foi escrito.

### 5.1 Prompt-lookup

`prefill::step_speculative` (prompt-lookup): a cabeça rascunha até `k` tokens a
partir da própria história recente (procurando na janela de `m` tokens; o
rascunho mais longo vence, e `m` degrada para `m-1, ..., 1`). O lote inteiro
(`1 + draft`) é decodificado pela pipeline num único passo e a cauda verifica
(`longest_match`): devolve `accepted` (prefixo confirmado + uma amostra real
extra), `accept_count` e `batch_width`.

- Depois de cada rodada o gateway faz fan-out de `rollback_sessions` para
  **todos** os hops com `accept_count`/`batch_width` (`handle_rollback` +
  `rollback_from` descartam do KV o que não foi confirmado; sessão desconhecida
  é no-op, não erro). Todo hop decodificou o lote inteiro, então todo hop
  precisa do rollback - não só a cauda.
- **Modelo híbrido/recorrente aceita rascunhar - `n_rs_seq` é quem decide, não
  `kv_can_shift`.** Achava-se (até 2026-08-16) que `should_draft` tinha que
  recusar sempre que `llama_memory_can_shift` fosse falso em modelo híbrido -
  engano duplo: essa flag é sobre o K-shift do `--ctx-shift` de referência do
  llama.cpp (reindexar RoPE quando o contexto estoura), que este projeto nunca
  liga; e o Qwen3.5/3.8 (`LLM_ARCH_QWEN35`) tem suporte dedicado a rollback
  recorrente parcial no llama.cpp vendorizado
  (`llm_arch_supports_rs_rollback`/`cparams.n_rs_seq`, "\[EXPERIMENTAL\]" no
  `llama.h`), só que **desligado por padrão** (`n_rs_seq = 0` = "sem
  rollback"). `EngineConfig::rs_seq_window` (`llm.rs`) pede
  `max(speculative_k, mtp_draft_tokens + 1)` - a maior largura de lote que este
  nó pode receber -, e arquitetura sem esse suporte simplesmente zera o pedido
  de volta (`llama-context.cpp`), então pedir aqui não machuca modelo comum.
  `should_draft` não recebe mais `kv_can_shift`: hoje toda rodada de lookup
  rascunha, e MTP só recusa sem cabeça `nextn` local e sem `draft` pronto.
- **A prova de verdade não é mais estática, é o `seq_rm` de cada rodada.**
  `handle_rollback` (`/v1/session/rollback`) chama `kv_remove`
  (`llama_memory_seq_rm`) e devolve `{rolled_back, reason}` no corpo -
  `rollback_sessions` (`gateway.rs`) lê esse corpo (200 OK não é sucesso por
  si, `handle_rollback` sempre responde 200); se algum hop devolver
  `rolled_back: false`, o gateway **encerra a geração** com o que já foi
  confirmado (`partial: true`) em vez de continuar sobre um cache que pode ter
  ficado com o rascunho recusado sem remover - é mais seguro que arriscar o
  modelo atender a tokens que o cliente nunca confirmou.
- **Uma rodada especulativa tem duas respostas legítimas.** A cabeça avisa que
  recusou rascunhar **omitindo `draft`** do quadro; sem `draft`, a cauda
  responde no formato antigo de um token (`{done, token_id, text}`) em vez de
  `{done, accepted, accept_count, batch_width}` - `speculated` fica `false` e
  `batch_width` é 1. `prefill::speculative_step_from_meta` aceita as duas
  formas; tratar a antiga como erro fatal derrubava a geração com um token e
  `partial: true` em toda requisição (2026-08-15, Qwen3.8-27B) - esse defeito
  era real e independente do engano do `kv_can_shift` acima.
- **Recusada a especulação, o gateway para de insistir** pelo resto da
  requisição (`especulando`, em `generate_stream`): passa a chamar
  `prefill::step`, que já usa a resposta direta (N+1 travessias). Não é só
  economia do campo `speculate` - insistir prende a cadeia no anel borbulhado
  (2N travessias por token).
- O gateway clampa `speculative_k` a `n_batch` (`speculative_k_of`), porque o
  lote especulativo também é um batch único na cabeça.
- **DEFEITO/limitação anotado**: `step_speculative` **não** foi estendida ao
  desvio direto - ela não recebe o parâmetro `direct` e continua no anel/hub
  mesmo quando o plano suportaria resposta direta. A decodificação especulativa
  nunca usa o caminho `relay/v2-direct`.

### 5.2 Predição multi-token (MTP) - `inference/mtp.rs`

O bloco `nextn` recebe o estado oculto do tronco na posição `p` com o embedding
do token de `p+1` e prevê o token de `p+2`. É a cabeça de rascunho que o LM
Studio liga quando oferece "MTP".

- **O motor embarcado já sabe executá-lo.** `llama-cpp-sys-2` 0.1.154 traz
  `LLAMA_CONTEXT_TYPE_MTP` e `llama_model_n_layer_nextn` nos bindings. As
  outras quatro entradas (`llama_{set,get}_embeddings_nextn`,
  `llama_set_nextn_layer_offset`) existem em `libllama.a` mas **não** nos
  bindings, porque moram em `llama.cpp/src/llama-ext.h`, fora do `extern "C"`
  de `llama.h` - o único cabeçalho que o `wrapper.h` da crate inclui. Ponte:
  `inference/llama_mtp_shim.cpp`, compilada pelo `crates/node/build.rs`.
- **Custa um contexto, não um modelo.** O contexto de rascunho é criado sobre
  **os mesmos pesos** (`llama_init_from_model` com `ctx_type = MTP`); o que ele
  acrescenta é um cache KV de uma camada.
- **Liga no modelo inteiro e no pipeline fatiado.**
  `NodeConfig::mtp_effective` decide por nó (modelo inteiro, ou a cauda de um
  pipeline fatiado que serve a última camada); `PipelinePlan::supports_mtp`
  confere no plano. Quem rascunha é a máquina que serve a última camada - no
  caso fatiado, a cauda, que tem o bloco `nextn`; quem monta o lote é a cabeça.
- **A cauda rascunha na própria resposta.** O rascunho viaja de trás para a
  frente **sem custo extra de travessia**: a cauda anexa `next_draft` ao reply
  da `relay/v2-direct` que já volta ao gateway, o gateway o injeta no
  `speculate.draft` da rodada seguinte, e a cabeça decodifica o lote sem
  rascunhar (`should_draft` aceita todo `draft` pronto incondicionalmente,
  mesmo sem `mtp_ready`). O custo real a medir no caso fatiado é a taxa de
  aceitação (o rascunho nasce uma rodada antes de ser usado, então prevê `p+2`
  a partir de `p`, exatamente como no caso local) - e não a rede.
- **A composição do lote viaja até a cauda.** A cabeça ecoa no reply o lote
  decodificado (campo `tokens`) e a largura do rascunho pedida (`mtp_k`); os
  hops do meio repassam; a cauda alimenta o `observe` do MTP com essa
  composição (`decode_hidden(..., tokens)`). Sem ela - peer velho que não
  ecoa - o `observe` desliga o MTP com aviso: a resposta continua correta, só
  não especulativa.
- **Exige binário novo em todos os hops.** `supports_mtp` pede
  `speculate/v1-mtp-sliced` em cada step (o protocolo `tokens`/`mtp_k`/
  `next_draft` só existe em build novo - um peer velho no anel receberia um
  `speculate` que não entende) **e** `speculate/v1-mtp` na cauda.
- **Modelo híbrido/recorrente rascunha no caso fatiado, desde que `n_rs_seq`
  esteja pedido** (ver a entrada de `should_draft` na seção 5.1 - mesmo
  mecanismo, mesma correção de 2026-08-16). Se mesmo assim o rollback de uma
  rodada falhar (`rolled_back: false`), o gateway encerra a geração em vez de
  seguir sem especular - mesmo flip de resultado que o prompt-lookup, causa
  diferente.
- O MTP fatiado roda pelo caminho direto (`prefill::step` + `relay/v2-direct`,
  N+1 travessias); `step_speculative` (prompt-lookup) continua sem desvio
  direto - ver seção 5.1.
- A fatia final é a única que carrega o bloco porque `splitter::split` zera
  `<arch>.nextn_predict_layers` fora dela - ver `docs/subsystems/fatiamento.md`.

## 6. Planejamento do pipeline (`routing/pipeline.rs`)

`plan_pipeline` varre de `0` até `total_layers`; em cada posição escolhe o peer
vivo que anuncia `inference_port` e o mesmo `model` cobrindo a posição, com
score **`layers_count * 100 - current_load * 10`** (mais camadas e menos
requisições em voo vencem). Devolve `None` no primeiro buraco de cobertura -
aí o gateway falha (ver seção 7). `LoadTracker::acquire` devolve um `LoadGuard`
RAII que decrementa no drop: o balanço de carga nunca fica preso num erro.

- `ready_for_pipeline` (gateway) sonda `/health` de cada peer com timeout de
  10 s **antes** de planejar - um anúncio UDP velho não pode envenenar o plano
  nem deixar requisição esperando por um hop que não serve mais HTTP.
- `resolve_step_url` escolhe o endereço de cada hop por ordem: rota medida na
  `RouteTable` (se ainda for candidato anunciado) → `inference_url` (endereço
  observado, o único comprovado antes da primeira sonda) → primeiro candidato.
  Fixa a escolha por sessão - o KV cache do outro lado é por sessão, e trocar
  de endereço no meio é pior que a rota lenta.
- O plano é montado por requisição; `PipelinePlan::hop_chain` dá
  "abc123→def456", a prova de execução distribuída que o gateway põe em
  `source`.

## 7. Fallbacks no gateway

O gateway **só tem o caminho do pipeline**. `generate_stream` falha com
"Nenhum pipeline válido para o modelo X (verifique a cobertura de camadas
0..N)" quando `plan_pipeline` devolve `None`; o handler de `/api/generate`
responde 503 com "Start peers with --inference-port." e `source: "none"`.

Não existe fallback para Ollama: `OllamaClient` só resolve modelos
(`/api/tags`) e alimenta o campo `ollama_available` do `/health`. Um prompt sem
pipeline válido **não** cai no Ollama local nem de peer - a requisição falha.
(Documentação antiga descrevia um fallback de três níveis; era estado anterior
do código, corrigido em 2026-08-15.)

## 8. direct_lab.rs - a "lei"

`direct_lab.rs` (cfg(test)) guarda a correção inteira, medida: **o custo por
token não depende de onde está o ponto de entrada da pipeline.** Hops falsos
falam o protocolo real (`wire`, `relay::send_frame`,
`infer_server::forward_direct`, `direct::deliver_result`; o gateway falso serve
o handler de produção de `/v1/relay-result`), dirigidos pelo `prefill::run`/
`step` reais. O único fingimento é o motor (dorme em vez de decodificar).

A rede é emulada por sítio: cada hop e o gateway moram num sítio, e toda
mensagem entre sítios diferentes custa `LINK_MS = 50` de mão única, cobrada na
chegada e de novo na volta. Assim a bancada mede travessias, não a máquina onde
roda. O teste `the_cost_per_token_does_not_depend_on_where_the_entry_point_is`
exige, para N=2,3,4 com o gateway na cabeça, no meio e na cauda, pernas por
token ≈ o mínimo teórico (tolerância 0,45 perna); que as duas pontas empatem; e
que a pior entrada (o meio) custe no máximo uma perna a mais que a cabeça.
A linha de base prova que é correção, não comportamento de sempre: o anel
borbulhado com a cabeça remota paga 4 travessias (o caso que doía, a queda de
25 para 14 tok/s). Assere ainda que cada quadro é entregue direto exatamente
uma vez (prefill e decode) e que o prompt de mais de um pedaço atravessa
inteiro.

- **Sem cobertura de teste**: os quatro modos de falha (hop do meio morto, cauda
  morta, resposta direta perdida, prazo estourado) têm desenho e código em
  `direct.rs`, mas **não** teste de ponta a ponta - decisão do captain em
  2026-08-14 (cota). A decodificação especulativa também segue sem cobertura no
  desvio direto (ver seção 5).
- A especulação tem a sua bancada própria, `spec_lab.rs` (cfg(test)):
  `generate_stream` de verdade contra hops falsos que decidem com
  `should_draft`/`find_draft`/`longest_match` de produção. O motor é fingido
  (sem GGUF nesta máquina ela prova o contrato entre os processos, não o que o
  llama.cpp faz com o lote); por isso ela não exercita `n_rs_seq`/`seq_rm` de
  verdade - só o llama.cpp faz isso, ver o doc do módulo para o que mudou em
  2026-08-16.

## 9. Limite de prompt

`gateway::prompt_budget_of(n_ctx, max_sessions, max_new_tokens)`:

```
n_ctx / max_sessions - max_new_tokens
```

O KV cache é dividido entre `max_sessions` slots (padrão **1**, não mais o
`/8` fixo), então a janela de uma requisição é `n_ctx / max_sessions`, menos a
sobra para os tokens que serão gerados. O gateway mede o prompt
(`estimate_tokens` ≈ chars/3, pessimista) **antes** de qualquer peer ver o
prompt e recusa com uma mensagem citando as flags.

- `n_batch` **não** entra na conta: o prompt vai em pedaços, então `n_batch`
  limita um pedaço (`min(prefill_chunk_tokens, n_batch)`), não o prompt. Antes
  de o prefill ser fatiado, `n_batch` na fórmula capava todo prompt em 512
  tokens por maior que fosse `--n-ctx`.
- Por que o limite existe: passá-lo faz o llama.cpp abortar o **processo
  inteiro** com `GGML_ASSERT(n_tokens_all <= cparams.n_batch)` - um prompt
  grande derruba o peer e todas as requisições em voo. Recusar cedo mantém o nó
  de pé. Números medidos: o OpenCode manda ~2200 caracteres com um agente mínimo
  e ~8000 no agente padrão; config que funcionou: `--n-ctx 32768 --n-batch 3072
  --max-new-tokens 256` → orçamento de 3072 tokens.

## 10. O canal de cada salto na telemetria

`SentFrame` e `PhaseTrace` carregam dois campos sobre o transporte, e eles
aparecem numa linha própria da tabela de telemetria do Chat.

- **`tls` / `tls_calls`** - se o salto foi cifrado. Na `PhaseTrace` é uma
  **contagem**, não um booleano: uma fase soma várias chamadas, e se só parte
  delas foi por TLS a tela mostra a fração (`TLS em 3/4 chamadas`) em vez de
  arredondar para "cifrado". É assim que uma virada de política pela metade
  aparece, e também como um ponto de chamada que ficou de fora da conversão para
  o cliente pinado se denuncia.
- **`tls_handshake_ms`** - medido **no servidor**, cercando o
  `TlsAcceptor::accept` do laço de accept (`backend/serve.rs`), que é onde o
  tempo é exato. Volta no `meta` da resposta e é agregado como os `node_timings`
  já são.

**O que esse número é, e por que a tela diz isso com todas as letras.** O
handshake acontece **uma vez por conexão**, e as conexões ficam no pool do
`reqwest`: quase todo salto reporta zero. Isso é a verdade, não falta de
medição. Sem o rótulo, quem olhasse um salto zerado concluiria que TLS é grátis
e quem olhasse o salto que abriu conexão concluiria que é caro por token - duas
leituras erradas do mesmo número. O custo por requisição depois do handshake é a
cifra do record layer, diluída dentro de `response_body_ms` e não separável sem
instrumentar o socket; a tela nomeia essa limitação em vez de inventar uma
coluna.

**Um salto pelo túnel libp2p reporta `tls: false`.** Ele é cifrado - por Noise,
ponta a ponta -, mas não por TLS, e dizer `true` ali alegaria uma garantia que
aquele salto não deu.

## 11. Defeitos anotados

- (não é defeito, é o comportamento de hoje) Porta ocupada: `runtime::free_port` acha quem segura a porta (`lsof -ti :PORT`
  no Unix, `netstat -ano`+`taskkill` no Windows) e mata (SIGKILL) antes de subir
  o servidor. Não conseguindo (ocupante não identificável, ou porta ainda presa
  depois de 5 s), o motivo vai para o `status.detail` como `Failed` já na
  primeira tentativa, e a linha "Mem loaded" da TUI passa a mostrar esse estado
  em vez do rótulo fixo `inference_server`. O histórico fica em
  `<project_root>/infer_server_<porta>.log` (`runtime::log_file_path`, apontado
  por `status.log_path`), porque a TUI limpa a tela a cada `--refresh`.
- **Limitação conhecida**: `step_speculative` não foi estendida ao desvio
  direto (ver seção 5) - a especulação continua no anel/hub.
- **Sem cobertura de teste**: os quatro modos de falha do modo direto e a
  especulação no desvio direto (ver seção 8).
- **MTP sem medição de tok/s** (2026-08-16, cortado por cota): a implementação
  está completa e verde nos testes de unidade, mas o ganho ponta a ponta não
  foi medido neste projeto - nem no caso fatiado, onde o que falta medir é a
  taxa de aceitação do `next_draft` (ver seção 5.2). O que existe é a medição
  do captain no LM Studio, no mesmo modelo e hardware: **21 → 35 tok/s** no
  Qwen3.8-27B-Q3_K_S. Enquanto não houver número medido aqui, o ganho no
  pom é expectativa, não fato.

## Nota de versão

Documentado a partir dos fontes na branch `fm/pom-linha-2` (2026-08-15):
`inference/infer_server.rs`, `inference/prefill.rs`, `inference/direct.rs`,
`inference/direct_lab.rs`, `inference/spec_lab.rs`, `inference/relay.rs`,
`inference/wire.rs`, `inference/runtime.rs`, `routing/pipeline.rs`,
`backend/gateway.rs`, `backend/openai.rs`, `backend/messages.rs`, e os AGENTS.md
de `inference/`, `routing/`, `backend/` e raiz. Onde o código e a documentação
divergem, o código vence e a divergência foi anotada como defeito.
