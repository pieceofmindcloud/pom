# Caminho de inferência

O que o código faz hoje, verificado nos fontes (ver nota de versão no fim).

Um pedido de geração (OpenAI `/v1/chat/completions`, Anthropic `/v1/messages`,
`POST /api/generate`) desce por um único caminho: `gateway::generate_stream`.
Ele planeja a cadeia de peers com `routing/pipeline.rs`, manda o prompt e cada
token pelo pipeline distribuído e devolve tokens ao cliente. Este documento
descreve os três modos de execução, o formato binário do estado oculto, o
prefill em pedaços, a resposta direta, a decodificação especulativa e os
limites.

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
- **Modelo híbrido/recorrente não rascunha**: `should_draft` exige
  `speculate` pedido **e** `kv_can_shift`. Sem rollback parcial o llama.cpp não
  sabe desfazer (o estado recorrente é acumulado), e insistir não dá erro na
  hora - dá divergência entre `n_past` e o cache, que explode na rodada
  seguinte com "inconsistent sequence positions". A decisão é tomada antes de
  apostar. `/health` expõe `kv_can_shift` justamente para diagnosticar por que
  a especulação não acelera.
- **Uma rodada especulativa tem duas respostas legítimas.** A cabeça avisa que
  recusou rascunhar **omitindo `draft`** do quadro; sem `draft`, a cauda
  responde no formato antigo de um token (`{done, token_id, text}`) em vez de
  `{done, accepted, accept_count, batch_width}` - `speculated` fica `false` e
  `batch_width` é 1. `prefill::speculative_step_from_meta` aceita as duas
  formas; tratar a antiga como erro fatal derrubava a geração com um token e
  `partial: true` em toda requisição (2026-08-15, Qwen3.8-27B). Sem shell na
  máquina, `kv_can_shift` no `/health` é como se confere isso.
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

- **DEFEITO ANOTADO**: a documentação (AGENTS.md raiz e `backend/AGENTS.md`)
  descreve um fallback de três níveis "pipeline → Ollama local → Ollama de um
  peer". O código não implementa isso: `OllamaClient` só é usado para resolver
  modelos (`/api/tags`) e para o campo `ollama_available` do `/health`. Um
  prompt sem pipeline válido **não** cai no Ollama local nem de peer - a
  requisição falha. A descrição documentada é de um estado anterior do código.

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
  `should_draft`/`find_draft`/`longest_match` de produção, com `kv_can_shift`
  como botão. O motor é fingido (sem GGUF nesta máquina ela prova o contrato
  entre os processos, não o que o llama.cpp faz com o lote).

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

## 10. Defeitos anotados

- (não é defeito, é o comportamento de hoje) Porta ocupada: `runtime::free_port` acha quem segura a porta (`lsof -ti :PORT`
  no Unix, `netstat -ano`+`taskkill` no Windows) e mata (SIGKILL) antes de subir
  o servidor. Não conseguindo (ocupante não identificável, ou porta ainda presa
  depois de 5 s), o motivo vai para o `status.detail` como `Failed` já na
  primeira tentativa, e a linha "Mem loaded" da TUI passa a mostrar esse estado
  em vez do rótulo fixo `inference_server`. O histórico fica em
  `<project_root>/infer_server_<porta>.log` (`runtime::log_file_path`, apontado
  por `status.log_path`), porque a TUI limpa a tela a cada `--refresh`.
- O fallback "pipeline → Ollama local → peer Ollama" documentado antigamente
  não existe no código: `generate_stream` só tem o pipeline e falha sem ele
  (ver seção 7). A AGENTS.md raiz e `backend/AGENTS.md` foram corrigidas para
  dizer isso (2026-08-15).
- **Limitação conhecida**: `step_speculative` não foi estendida ao desvio
  direto (ver seção 5) - a especulação continua no anel/hub.
- **Sem cobertura de teste**: os quatro modos de falha do modo direto e a
  especulação no desvio direto (ver seção 8).

## Nota de versão

Documentado a partir dos fontes na branch `fm/rdma-p2p-linha-2` (2026-08-15):
`inference/infer_server.rs`, `inference/prefill.rs`, `inference/direct.rs`,
`inference/direct_lab.rs`, `inference/spec_lab.rs`, `inference/relay.rs`,
`inference/wire.rs`, `inference/runtime.rs`, `routing/pipeline.rs`,
`backend/gateway.rs`, `backend/openai.rs`, `backend/messages.rs`, e os AGENTS.md
de `inference/`, `routing/`, `backend/` e raiz. Onde o código e a documentação
divergem, o código vence e a divergência foi anotada como defeito.
