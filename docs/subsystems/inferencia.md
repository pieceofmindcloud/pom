# Caminho de inferência

O que o código faz hoje, verificado nos fontes (ver nota de versão no fim).

Um pedido de geração (OpenAI `/v1/chat/completions`, Anthropic `/v1/messages`,
`POST /api/generate`) desce por um único caminho: `gateway::generate_stream_with_stops`.
Ele planeja a cadeia de peers com `routing/pipeline.rs`, manda o prompt e cada
token pelo pipeline distribuído e devolve tokens ao cliente. Este documento
descreve os três modos de execução, o formato binário do estado oculto, o
prefill em pedaços, a resposta direta, a decodificação especulativa e os
limites.

### Montagem UTF-8 no streaming

O llama.cpp devolve a peça de cada token como bytes crus: um token BYTE pode
ser apenas um byte e um token normal pode terminar no meio de um codepoint.
`llm::LlmEngine::token_to_piece_bytes` preserva esses bytes até o limite de
emissão. Cada `Session` de `infer_server.rs` possui um `Utf8Assembler` próprio,
que só entrega no delta os caracteres completos. No caminho especulativo,
somente as posições aceitas avançam o acumulador; posições rejeitadas não
podem contaminar a rodada seguinte.

O acumulador é drenado com `flush` quando o modelo encerra por EOS. Limpeza
por release explícito, erro, cancelamento ou timeout também chama `flush` antes
de devolver o slot KV, descartando o resultado nesses caminhos e garantindo
que nenhum rabicho sobreviva para outra requisição. Uma sequência realmente
inválida mantém a semântica de `from_utf8_lossy`: produz um único U+FFFD.

## 0. Coletiva de tensor parallelism (`collective.rs`)

Para o caminho C (tensor parallelism sem transferir peso), o núcleo de troca é a
coletiva all-reduce: cada rank produz uma soma parcial da projeção por linhas e
o resultado é a soma das parciais. `inference/collective.rs` tem `reduce_sum` (a
soma matemática) e `ReduceBarrier` (a barreira que acumula os parciais de cada
rank e devolve a soma a todos via `oneshot`, modelada no `ReplyInbox` de
`direct.rs`). O transporte HTTP (`POST /v1/tp/reduce`) é uma camada fina por
cima, ligada junto com o fork do llama.cpp que chama a coletiva no meio do
forward pass. O rank 0 contribui diretamente na `ReduceBarrier` local; os
workers enviam a parcial ao primary. `broadcast_to` mantém a URL base e a rota
separadas para o túnel encaminhar a requisição ao path correto no loopback do
worker. O broadcast e o reduce remoto tentam primeiro os candidatos HTTPS
diretos pinados. Se nenhum endereço responde por falha de transporte, tentam o
túnel libp2p quando o membro anunciou `PeerId` e capability. Essa ordem é
necessária para a coletiva: a chamada fica aguardando dentro da barreira e um
túnel indisponível não deve atrasar um endereço HTTPS alcançável.
`TunnelRequest` preserva os cabeçalhos da barreira, do rank e da autorização.
As rotas de inferência aceitam a `connection_key` compartilhada ou uma API key
ativa. As chamadas internas usam a `connection_key` e caem para a primeira API
key ativa quando a chave compartilhada não existe. O loopback do túnel mantém o
`Content-Type` JSON do release de sessão em vez de aplicar o padrão binário.
Quando o destino anunciado de um passo é o próprio `node_id` deste processo, o
relay despacha o quadro diretamente para o handler de inferência em processo.
Essa chamada não abre socket nem passa por HTTP. O servidor HTTP de inferência
continua ativo para health checks e para peers remotos; nesses peers a ordem é
túnel libp2p e fallback HTTP direto.
Ver
`docs/superpowers/specs/2026-08-17-tensor-parallelism-design.md`.

O callback nativo do llama.cpp roda em uma thread de `spawn_blocking`, fora do
contexto local de uma task Tokio. Por isso o registro do grupo captura o
`tokio::runtime::Handle` do servidor e o callback usa esse handle para aguardar
o reduce; chamar `Handle::current()` dentro da thread nativa causa panic.

A medição de RTT do mapa é separada do tempo de um frame: `measure_rtt` envia
um GET vazio para `/health` pelo mesmo túnel libp2p, sem prefill, decode ou
estado oculto. Portanto `round_trip_ms` de um frame não é usado como RTT de
topologia.

Cada execução também pode carregar `node_timings.tp`. Esse bloco registra
reduces esperados e observados, espera total, p50/p95 da coletiva, bytes de
request/response, chamadas locais, broadcasts e o caminho direto ou túnel. A
espera inclui a barreira síncrona e não é latência pura de pacote. Os bytes e waits TP ficam
separados dos frames de hidden state do pipeline. Com 32 camadas e dois pontos
row-parallel por camada, um decode de um token pode fazer 64 esperas coletivas;
isso é inerentemente caro entre máquinas na internet. Para WAN, pipeline com
camadas completas por nó troca um frame de hidden state entre blocos e evita
essas coletivas TP.

O `PhaseTrace` também preserva `round_trip_ms`, a soma das idas e voltas
observadas das chamadas daquela fase. Esse campo segue no `x_pom` do stream
para a tela do Chat e não entra no frame entre peers nem no planejador. O Chat
usa `/api/ui/chat/telemetry` para buscar separadamente o RTT direcional das
arestas entre os peers do trace. O `/health` publica `profile_sync_enabled`
por máquina, derivado diretamente de `POM_PROFILE_SYNC`; um zero em
`node_backend_completion_ms` nunca é usado para deduzir essa flag.

### Cache KV e fallback de VRAM

`n_gpu_layers` continua controlando onde ficam os pesos. O contexto tenta
primeiro respeitar `offload_kqv=true`, colocando o KV cache na GPU. Se o
`cudaMalloc` do KV cache falhar, `LlmEngine::load_with_cache` repete a criação do contexto
com `offload_kqv=false`: os pesos permanecem nas mesmas camadas da GPU e o KV
cache passa para a memória RAM. A aplicação registra o fallback e `/health`
mostra o valor efetivo. Isso resolve falta de VRAM para o cache; se os próprios
pesos não couberem, ainda é necessário reduzir `n_gpu_layers` ou dividir o
modelo entre máquinas.

O tipo numérico do cache é uma configuração da instalação, no `pom.json`, e não
uma escolha por máquina dentro de uma implantação:

```json
{
  "cache_type_k": "q8_0",
  "cache_type_v": "bf16"
}
```

Os valores aceitos são `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `iq4_nl`,
`q5_0` e `q5_1`. Quando os campos faltam, ambos usam `f16`. O runtime aplica a
mesma configuração ao contexto principal, ao contexto MTP e ao
`llama_rs_fit_params`; `/health` expõe `cache_type_k` e `cache_type_v` depois
da carga para confirmar o valor efetivo. A implantação continua podendo
escolher `offload_kqv` por máquina, porque isso controla apenas a localização
do cache, não o tipo numérico.

Nos deployments de pipeline ou de modelo inteiro, `RuntimeParams.fit_memory=true`
fica ligado por padrão. Antes da carga real, a aplicação chama o
`common_fit_params` do llama.cpp, que pode reduzir `n_gpu_layers` para caber na
memória disponível e preserva a capacidade total correspondente a
`n_ctx * max_sessions` quando ela foi definida explicitamente. O `/health` expõe
`fit_memory_applied` e
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

### Reconfiguração sem reler pesos

`NodeRuntime` mantém um `ModelCache` local com o `Arc<LoadedModel>` da última
fatia. Em uma nova geração, `NodeConfig::weights_are_compatible` compara o
caminho, a faixa, a contagem total, a forma de carregamento GPU, o MTP efetivo
e a identidade TP. Se tudo coincide, `infer_server::run` chama
`LlmEngine::load_with_cache` e cria somente um `llama_context` novo, aplicando os
novos valores de contexto, sessões, KV, lote e sampling. A geração anterior
continua dona do contexto antigo até o servidor parar de aceitar conexões e
drenar as requisições em voo.

Uma troca de fatia, caminho ou parâmetro de carregamento limpa o cache antes de
publicar a nova geração. `NodeRuntime::stop` também limpa o cache, porque
unload explícito deve devolver os pesos. O cache nunca é compartilhado entre
processos ou máquinas e não altera nenhum contrato de fio.

### Onde as camadas ficaram

Nada acima diz onde o modelo **de fato** foi parar. `n_gpu_layers` é o pedido; o
`fit` o reduz numa GPU cheia, e um override de buffer type manda parte de uma
camada - tipicamente os experts de um MoE - para a CPU sem mudar o dispositivo
do bloco. Derivar a colocação desses números erra exatamente nos casos que
importam.

Por isso o `/health` do servidor de inferência traz dois campos medidos depois
da carga, os dois vindos do llama.cpp por `wrapper_common.cpp` (ver o
`AGENTS.md` de `vendor/llama-cpp-sys-2/`):

- **`placement`** - por camada, o dispositivo atribuído (`dev_layer`) e quantos
  bytes de peso ficaram em cada buffer, mais a cabeça de saída, os totais e
  `has_tensor_overrides`. Uma camada é "na GPU" pelos bytes dela, não pelo
  dispositivo do bloco: com override, os dois divergem.
- **`context_memory`** - a memória do contexto por buffer, separada em `model`,
  `context` e `compute`. O `context` é o cache KV, e é ele que responde onde o
  KV ficou; `offload_kqv` é só o que foi pedido, e o fallback de VRAM acima pode
  não atendê-lo. O tamanho que aparece já reflete o tipo configurado, que o
  mesmo `/health` publica em `cache_type_k` e `cache_type_v`.

Os dois são `null` quando a ponte não responde, e aí a interface diz que não
sabe em vez de inventar um mapa. Quem os mostra é o card de máquina do Painel,
que os busca por `/api/ui/peer/:node_id/runtime` só com o card aberto - ver
`api-http.md` e `web-ui.md`.

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

- `dtype`/`shape` vão no JSON; o corpo é **binário** f16 por padrão ou f8
  quando a implantação escolhe a alternativa (nunca base64). Precisão e
  codificação são parâmetro de `encode`/`decode`, não suposição.
- `frame_version=1` (`inference/frame-v1-batch`) é o primeiro e único contrato
  oficial. Um header JSON carrega `batch_id` e itens `{item_id, session_id,
  shape, dtype, offset, len, meta}`, seguido pelo payload concatenado. O
  decoder valida versão, dtype, tamanho derivado do shape, limites e
  sobreposição antes de expor qualquer tensor.
- Uma requisição individual usa o mesmo frame por meio de
  `encode_batch_single`. O relay agenda o flush imediatamente e agrupa, quando
  possível, chamadas concorrentes para o mesmo próximo hop, modelo,
  implantação, configuração, fase e rota no mesmo ciclo do executor. Assim
  uma chamada isolada não espera um timer fixo, enquanto chamadas que já
  chegaram juntas ainda compartilham o frame. O peer receptor distribui os
  itens concorrentemente para o scheduler local e devolve os resultados pelo
  `item_id` original.
- O produto ainda não foi lançado, portanto não há fallback, adapter ou
  código de compatibilidade para formatos v1/v2 anteriores.
- `MAX_FRAME_PAYLOAD_BYTES = 64 MiB` limita payload no decode; o handler axum
  aplica o mesmo teto no body (`INFERENCE_FRAME_BODY_LIMIT`). O relay separa
  lotes de tensores pelo orçamento restante do frame e não agrupa uma cabeça
  que pode devolver hidden state quando o tamanho da resposta não aparece no
  request. Uma recusa determinística de tamanho não repete o mesmo prefill a
  frio.
- O decode sempre devolve o tensor como f32: precisão reduzida é propriedade do
  fio, nunca da matemática. `f16` é o padrão preservado; `f8` usa FP8 E4M3FN,
  um byte por valor, e é uma escolha explícita de `RuntimeParams` para medir o
  trade-off. O conversor recusa NaN, infinito e valores fora do alcance finito
  em vez de saturá-los silenciosamente. O f32 continua representável em
  caminhos internos, mas não é uma opção de implantação.
- `RuntimeParams.hidden_state_dtype` é global para a implantação e segue o
  mesmo caminho persistido de contexto, batch, chunk prefill e cache KV. O
  padrão é `f16`; `f8` só é enviado quando o operador o escolhe
  explicitamente. Cada peer anuncia `inference/frame-v1-dtype-f8` em
  `/api/ui/node`. O planejador bloqueia uma implantação f8 se qualquer peer
  selecionado não anunciar a capability, e o servidor ainda recusa um frame
  incompatível com uma mensagem explícita. Não há interpretação silenciosa.
- O trace de cada hop registra `hidden_state_bytes_sent`,
  `hidden_state_bytes_received` e `hidden_state_dtype`. A UI mostra esses
  campos por travessia e o formato efetivo, inclusive quando uma configuração
  pedida não pode ser usada. O número observado, e não uma estimativa de
  transporte, é a medida para comparar f16 e f8.

### Forma do residual e custo por token

O residual capturado não é assumido como uma matriz de duas dimensões. A
captura encontra, pela quantidade conhecida de tokens do microlote físico e
pela forma do tensor, o eixo de tokens. `n_embd_inp` descreve a largura do
hidden state de entrada e não é usado para localizar esse eixo: em arquiteturas
com fluxos, as features anteriores podem fazer o produto ultrapassar essa
largura. Todos os eixos anteriores ao eixo de tokens são preservados na ordem
física. O wire normaliza isso para `[tokens, features...]`. Dimensões posteriores
maiores que 1 são recusadas; dimensões posteriores iguais a 1 são ignoradas.
Se mais de um eixo tiver a contagem de tokens, o menor eixo que também fecha os
strides contíguos é escolhido de forma determinística. Geometria ou stride que
não fecha essa regra é recusado com o shape no erro.

Na recepção, a validação usa `n_embd_inp` para a forma 2D e para dimensões
extras singleton. Quando há um eixo extra não singleton, a largura efetiva é
`n_embd_out`, que o llama.cpp calcula a partir da arquitetura e que já
representa a largura física produzida pelo callback. A contagem de fluxos da
metadata descreve a geometria, mas não é multiplicada novamente pelo receptor.
Assim, uma forma `[tokens, n_embd, fluxos...]` só passa quando o produto das
features coincide com a largura expandida reportada; uma largura incompatível
continua sendo recusada com a forma no erro. `decode_hidden` e
`decode_hidden_batch` usam essa mesma largura validada para reservar o batch
Rust, evitando aceitar uma forma que depois estoure a cópia para o buffer.

Quando existe uma dimensão de fluxo extra, o residual precisa carregar cada
fluxo. Não há uma fronteira genérica em que uma largura `n_embd` combinada seja
matematicamente equivalente: os ops de pré-combinação e pós-combinação mantêm
os fluxos, e o op de combinação só mistura fontes em destinos. A combinação
observada depois das camadas é uma saída auxiliar, não a entrada equivalente
para o próximo peer. Portanto, para essa arquitetura, o caminho correto é
transportar todos os fluxos; o planner não comprime o tensor por heurística de
nome de família.

O custo abaixo é o payload do tensor, medido pelo mesmo cálculo de
`tensor_payload_len`; não inclui o header JSON nem o transporte. Para `n_embd =
4096` e quatro fluxos, o custo por token é:

| Representação | Quatro fluxos | Uma largura combinada |
|---|---:|---:|
| f16 | 32.768 bytes | 8.192 bytes |
| f32 | 65.536 bytes | 16.384 bytes |
| f8 | 16.384 bytes | 4.096 bytes |

A coluna de uma largura é apenas o limite teórico de uma compressão equivalente;
ela não é usada quando a arquitetura não fornece essa fronteira. A ordem dos
microlotes não muda com a dimensão extra: `CaptureSlot` acumula cada bloco na
ordem entregue pelo `split_simple`, habilitado por `kv_unified=true`.

Medido com `tcpdump` no peer 2 (requisição de 35 tokens de prompt + 34
gerados): 565.336 bytes de estado oculto da cabeça para a cauda, 6.715 bytes de
tokens amostrados na volta. 565.336 / 69 tokens = **8.192 bytes por token por
hop**, exatamente `hidden_dim 4096 × 2` (f16). O formato antigo (f32 em
base64) custava 4096 × 4 × 4/3 = 21.846 bytes por token por hop - **62,5%
menos bytes** na rede hoje. Um pedaço de prefill de 512 tokens é ~8 MiB f16 por
hop por pedaço. Com f8, o payload do mesmo tensor cai para um byte por valor,
mas a redução efetiva deve ser lida em `hidden_state_bytes_*`, por travessia,
porque header, transporte e padrão de tráfego também entram na medição.

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
  peer receptor confere a posição absoluta do pedaço antes de refazer, então
  um reenvio nunca pode ser contado duas vezes. `start_pos` vem da fronteira
  que o peer anterior confirmou; para o pedaço 0 sem posição explícita, a
  própria sessão fornece `n_past`. Para qualquer pedaço posterior, a ausência
  de `start_pos` é erro de protocolo, nunca `chunk_index * chunk_size`.
  Se a sessão já passou da posição, o peer tenta a remoção necessária para a
  retentativa; se o motor recusa ou ainda não chegou lá, nomeia a divergência e
  o gateway pode repetir o prefill inteiro a frio.
- **Step não retenta**: decode não tem proteção de rollback de KV, então o
  `step` sempre vai com `attempts = 1`. Prefill e decode não compartilham a
  constante para ela não derivar.
- Só a cabeça tokeniza (tem o prompt), e é a única autoridade em
  `more_chunks`/`n_prompt_tokens`. O gateway sempre manda `is_final_chunk:
  false` e quem tokenizou corrige (ver `prompt_progress` na seção 4).
- Quando a cabeça ainda precisa de outro pedaço, a resposta parcial também
  publica `n_prompt_tokens`, `start_pos` e `next_pos` absolutos. Isso vale tanto
  para o caminho individual quanto para o lote, inclusive no primeiro chunk de
  um modelo inteiro, para o gateway e os hops seguintes não fabricarem a
  posição pelo índice do chunk.
- O fechamento do stream cria um cancelamento cooperativo por geração.
  `prefill::Cancellation` interrompe a espera HTTP em voo e impede o próximo
  retry, hop ou chunk. Um frame que o peer já aceitou pode terminar, porque o
  contrato de fio não ganhou um cancelamento remoto; depois disso o gateway
  libera as sessões dos peers alcançados.

**Pipeline de prefill (H1).** Nos caminhos hub e resposta direta, quando há
mais de um hop, o prefill roda como um pipeline em ordem, não como um par
travado. Duas rotinas concorrentes na mesma task do gateway (`prefill.rs`,
`run`/`run_direct`): a **bomba da cabeça** despacha os pedaços em série - cada
um é uma perna de rede + o decode da cabeça - e a **bomba de downstream** leva
cada estado oculto pelos hops restantes, mantendo até `W` pedaços em voo ao
mesmo tempo e colhendo os resultados na ordem de envio (`FuturesOrdered`). É
isso que mantém todos os enlaces e todas as GPUs ocupados em paralelo, em vez
de um estágio só.

`PrefillOverlapWindow` impõe dois limites: a **contagem** de pedaços em voo
(`prefill_max_inflight_chunks` em `pom.json` ou `--prefill-max-inflight-chunks`,
efetivo entre 1 e 8, padrão 8; 1 desliga a sobreposição) e um **orçamento de
bytes** de estado oculto em RAM no gateway (`INFLIGHT_BYTE_BUDGET`, 512 MiB
fixo, que só morde com `n_embd` grande ou janela alta). O permit de cada limite
viaja com o pedaço e só é solto quando o downstream dele termina. Uma falha ou
a desconexão do cliente cancela as futures em voo; no modo direto, uma
sequência UUID e uma fila por peer (`DirectForwardSequence`) mantêm o repasse
hop a hop em ordem. O ACK da cabeça carrega o `next_pos` absoluto dentro de
`prompt_progress`, e é dali que a bomba da cabeça despacha o pedaço seguinte
sem esperar o resultado do anterior. A posição nunca é inferida pela ordem de
conclusão ou chegada.

**Retomada.** Se um pedaço `k > 0` falha depois de a cadeia já ter aceitado os
anteriores, o erro carrega um `PrefillResumable { from_chunk, start_pos, ... }`
e o gateway repete o prefill **a partir de `k`** em vez de reenviar o prompt
inteiro: cada peer refaz o próprio KV pela reentrada de pedaço que
`prefill_blocking` já trata (`kv_remove` da cauda; motor recorrente que recusa
cai no retry frio total, que continua sendo a segunda recuperação). Sessão
perdida no peer não é retomável - vai direto para o retry frio.

O modo anel permanece serializado pelo contrato de resposta aninhada, para não
abrir uma segunda requisição na mesma sessão sem a confirmação da anterior; ele
ignora a janela.

H1 aproxima o prefill do regime limitado por transmissão: com `K` pedaços e o
pipeline cheio, o tempo tende a `K · max(perna, cálculo)` mais o enchimento, em
vez de `K · (pernas + cálculo)`. Ela **não reduz os bytes**: os 126 MB de um
prompt de 30.000 tokens continuam atravessando cada fronteira. O trace publica
`prefill_pipeline_depth_max`, `prefill_overlapped_chunks` e
`prefill_max_inflight_chunks`.

### 3.1 Cache de prefixo entre turnos

O gateway mantém um `PrefixCache` global com leases por sessão. A chave combina
modelo e sequência de hops, e o cache guarda apenas texto, identidade da sessão
e contagem de tokens, nunca pesos ou tensors. Uma entrada hit fica ocupada
durante a geração; entradas ociosas seguem LRU dentro da capacidade configurada.
Leases ocupados não são despejados, mesmo que o total passe temporariamente a
capacidade nominal.
Em um miss, uma entrada ociosa pode ser reciclada antes do prefill novo, o que
mantém o caminho compatível com `max_sessions=1` sem compartilhar entrada
ocupada.

**A geração não acontece na sequência do prompt.** Cada sessão de um peer tem
duas sequências de KV: `prompt_seq`, onde o prefill escreve apenas até a
fronteira estável medida para o próximo turno, e `gen_seq`, aberta na primeira
decodificação depois dessa fronteira como cópia da primeira
(`llama_memory_seq_cp`, metadado apenas - as duas dividem as mesmas células com
`kv_unified`). A cauda do template, os tokens que completam o prompt atual e a
geração ficam na cópia. No fim do turno some a sequência da geração **inteira**,
que é remoção total e todo motor aceita, enquanto `prompt_seq` mantém o prefixo
que o próximo prompt pode reutilizar. É por isso que o reuso não depende de o
modelo aceitar remover posições do meio do cache - uma operação que
`llama_memory_recurrent::seq_rm` recusa sempre que o recuo passa da janela de
snapshots (`n_rs_seq`), ou seja em qualquer geração de tamanho real num modelo
híbrido/recorrente como o Qwen3-Next/Ornith. O custo declarado é uma segunda
sequência por sessão (`llm::kv_seq_budget`), que o `fit_memory` já contabiliza.

Reter é ato local do peer: no EOS a sessão **não** é destruída, ela volta para
`prompt_len`. `/v1/session/rewind` confere esse estado e cobre as paradas que
não passam por EOS (limite de tokens, sequência de parada, cliente que
desligou); é idempotente e não recua nada. Liberar de vez continua sendo ato
explícito do gateway, por `/v1/session/release`.

O comprimento confirmado no rewind é a fronteira estável medida durante o
prefill (`retained_tokens`), não necessariamente o total tokenizado do prompt:
o rabicho do template e a entrada que fornece os logits do primeiro token ficam
na sequência de geração daquele turno.

No turno seguinte o primeiro frame leva `reuse_prefix`. A regra da cabeça é **só
acrescentar** (`plan_prefix_reuse`): o prefixo serve quando os tokens retidos
são prefixo exato dos novos e acabam antes deles; qualquer outro caso -
divergência no meio, prompt idêntico ao retido, sessão sem prompt - recomeça a
sessão do zero, o que custa o reuso daquele turno e nunca a requisição. A
resposta declara o que foi **executado** (`reuse_prefix`, `cached_tokens`) e é
essa decisão, não o pedido do gateway, que os hops seguintes obedecem: quem
tokeniza é quem sabe. Em uma cadeia com N hops, o intermediário apenas ecoa a
decisão autoritativa da cabeça; recalculá-la como `false` faria o hop seguinte
resetar a sessão e receber um hidden state numa posição positiva. `start_pos` e
`next_pos` são posições absolutas da sessão, confirmadas pelo peer que acabou
de executar. No pedaço 0 a posição sem campo é onde a sessão está - julgá-la
por `chunk_index * chunk_size` = 0 apagava o KV reaproveitado uma linha depois
de aproveitá-lo. Nos pedaços seguintes, campo ausente é erro e provoca o
fallback frio, em vez de fabricar 512 para uma sessão que está em 42.

O acerto parcial é observado, mas não aplicado como KV parcial. Se os primeiros
tokens coincidem e a divergência exigiria encolher a sequência retida, a cabeça
informa `prefix_match_tokens` e recomeça a sessão a frio; `cached_tokens` fica
zero e `prompt_tokens_transmitted` volta a ser o prompt inteiro. Isso evita
resposta errada em motores recorrentes ou híbridos que recusam remoção parcial.
`prefix_reuse_status` distingue `full`, `partial-cold`, `full-cold`, `miss` e
`cold`; quando uma perda de sessão exige a repetição fria, o gateway publica
`session-lost-cold`.

Se um peer perder a sessão durante o prefill - por coleta de ociosidade,
reinício do processo ou remoção explícita -, ele devolve "sessão desconhecida".
O gateway reconhece essa recusa e repete a rodada inteira do zero uma única vez,
mesmo quando o pedido não usava prefixo: o estado anterior daquele peer não
existe mais, então repetir apenas o chunk recusado poderia produzir uma resposta
silenciosamente errada. O custo é refazer também os chunks já aceitos e o KV em
todos os hops. Se a segunda rodada falhar, o erro final informa que o sistema
tentou reconstruir o contexto e que o pedido não foi executado; ele não expõe ao
cliente a instrução interna de enviar `reset=true`. Reusar é otimização, nunca
motivo para a requisição falhar.

O prompt que o cache reconhece vem do **array de mensagens**
(`openai::conversation_cache_key`), não do texto renderizado, e `render_prompt`
rotula todo turno inclusive o primeiro: as duas coisas juntas garantem que o
prompt de um turno seja prefixo exato do prompt do seguinte. Texto cru sem
rótulo continua disponível em `/v1/completions` e `POST /api/generate`.

### Por que a sessão desaparece

A investigação do caminho real não encontrou um único evento que possa ser
atribuído retrospectivamente ao peer da ocorrência: o erro 400 não leva o
motivo da remoção. O estado de uma sessão é um `HashMap` em memória dentro de
`InferState`; não há persistência nem restauração depois de um reinício. As
remoções possíveis hoje são:

- reinício do processo, que recria `sessions` vazio;
- coleta de ociosidade em `reap_idle`, depois de 900 segundos sem uso, chamada
  quando `alloc_seq` precisa de um slot;
- pressão de capacidade: `max_sessions` é 1 por padrão, e `alloc_seq` despeja
  primeiro a sessão retida menos usada em `evict_retained`;
- remoção ativa por `finish`, usada pelo `/v1/session/release`, pelo cleanup de
  cancelamento/erro e pelo próprio despejo de capacidade.

O limite de sessões não remove uma sessão ativa: o despejo seleciona apenas
sessões retidas, e falta de slot sem vítima devolve `seq pool esgotado`, não
`sessão desconhecida`. A coleta de ociosidade também não é um timer contínuo.
Durante um prefill normal, cada chunk aceito atualiza `last_used`, e uma sessão
em andamento ainda não está marcada como retida. Portanto, para o sintoma
observado em um chunk posterior, a evidência do código aponta para reinício do
processo ou remoção explícita/concurrente durante o prefill; TTL e capacidade
explicam principalmente o cache de prefixo ficar apontando para uma sessão
perdida entre turnos. O pedido novo funcionar logo depois no mesmo peer prova
que ele voltou a atender, mas não distingue reinício de remoção.

Conclusão: a perda da sessão é possível e, em parte, intencional no ciclo de
vida atual, mas não há telemetria suficiente para dizer qual dos eventos ocorreu
na máquina do relato. Recomenda-se uma tarefa separada para registrar o motivo
de cada `finish`, `reap_idle` e `evict_retained`, correlacionar o início do
processo com o `node_id` e alinhar a capacidade do `prefix_cache` do gateway
com `max_sessions` dos peers. A recuperação automática continua sendo a
resposta correta para a perda legítima; sem essa tarefa, ela permanece uma
proteção contra divergência de estado, não uma prova de que a causa deixou de
existir.

EOS ou fim de turno apenas fecha o acumulador UTF-8 da resposta. O servidor
mantém a sessão física até receber `/v1/session/rewind` ou `/v1/session/release`,
porque o gateway pode precisar preservar o KV para o próximo turno. A remoção
direta no caminho terminal faria o rewind responder `sessão desconhecida` e
deixaria o código de resposta indexar uma sessão ausente.

O cache é uma otimização interna. O envelope binário `frame/v1-batch` e as APIs
públicas não mudaram, mas a economia agora é **publicada**:
`usage.prompt_tokens_details.cached_tokens` (OpenAI),
`usage.cache_read_input_tokens` (Anthropic) e `x_pom.trace.cached_tokens`
reportam tokens de prompt que o pipeline não reprocessou - medida do executado,
não da escolha. O trace também publica `prefix_match_tokens`,
`prefix_reuse_status` e `prompt_tokens_transmitted`, para separar candidato
comum de tokens realmente poupados. `/health` publica `sessions_retained`,
`prefix_tokens_retained` e `kv_seq_max`.

`prefix_cache_lab.rs` compara cada operação do cache em memória com uma
referência e contém um proxy propositalmente quebrado que precisa disparar a
bateria - ele valida a **escolha**. `prefix_reuse_lab.rs` valida a **execução**:
uma conversa de quatro turnos pelo caminho de `/v1/chat/completions`, nos três
transportes (hub, anel, resposta direta), afirmando que do turno 2 em diante
`cached_tokens > 0`, que o trabalho por turno para de crescer com a conversa,
que o TTFT cai em relação ao turno frio e que nenhum hop recebeu pedido de
remoção parcial de KV. O contrato de sincronização repete anel e resposta
direta com N=3 e N=5 hops, verificando em cada peer `start_pos == n_past` antes
do decode e `next_pos == n_past` depois. O teste SSE de `openai.rs` reproduz o
peer que confirma a sessão em 42 sem `next_pos` e afirma que o fallback não
envia chunk 1 na posição 512 nem entrega `server_error`. Os hops falsos dessa
bancada imitam o motor mais restrito que o produto tem que atender - o
recorrente que recusa remoção parcial; o atraso determinístico da bancada torna
o custo executado observável, sem confundir a escolha do cache com a economia
real.

### 3.2 Seleção do executor

Antes de carregar os pesos, `executor::select` lê o cabeçalho GGUF. Só um
`expert_count > 1` classifica o modelo como MoE. Em `auto`, qualquer modelo
denso ou com metadados ausentes permanece em `dense/llama.cpp`. O campo
persistido `NodeConfig.inference_mode` chega ao seletor antes da carga: uma
escolha explícita `dense` ou `moe` vence o ambiente derivado da marca
`POM_INFERENCE_MODE`; em `auto`, esse ambiente ainda pode forçar um modo e, sem
override, a metadata escolhe. `moe` recusa um GGUF denso com erro explícito.

O caminho MoE atual aparece como `moe/q4_0` no `/health` quando o seletor
escolhe esse modo e abre o runtime próprio em `inference/moe/`: o dispatcher
detecta o dtype do GGUF, calcula o router top-k e usa o LRU global. Hidden states recebidos são transformados
antes do decode local; residuals produzidos a partir de tokens passam pela
mesma ponte antes de seguir ao próximo hop. O wire continua em f16 e nenhum
peso é transportado. Atenção, operações restantes e sampling final ainda
delegam ao `LlmEngine`, então esta é uma ligação parcial do bloco, não um
transformer MoE completo. NVFP4, K-quants e IQ-quants usam loaders próprios
com validação da ordem GGUF `[input, output, experts]` antes do forward. A Fase
5 ainda não está ativa: não há executor GPU próprio nem perfil medido carregado
no runtime. O caminho atual é CPU para router e experts; não afirma
sobreposição CPU/GPU.

Antes de os eventos chegarem aos adaptadores HTTP, o gateway também aplica
stop strings por requisição. `StopMatcher` mantém o sufixo que ainda pode ser
prefixo de uma parada, remove a própria parada da saída e encerra a geração com
`finish_reason=stop` e a sequência correspondente. Sem stop strings o caminho
continua emitindo os eventos como antes.

### 3.3 Scheduler local e batching contínuo

Cada processo de inferência possui um scheduler próprio para o contexto
`llama.cpp`. Ele recebe os frames depois da autenticação e do decode do wire,
aplica backpressure antes de criar uma sessão e responde `429` com
`Retry-After: 1` quando `max_queue_requests` foi atingido. O padrão é fila 128,
janela de coleta de 0 ms, `max_batch_sequences` limitado por `max_sessions` e
`max_batch_tokens` limitado por `n_batch`. Uma janela positiva continua
disponível na configuração para workloads que priorizam formar microbatches.

Na telemetria de cada hop, `queue_ms` começa no `queued_at` marcado pelo
handler imediatamente antes de `SchedulerHandle::submit_prefill` ou
`submit_step`, e termina na entrada do worker, antes de adquirir o mutex do
estado e do motor. O atraso fixo vinha do `tokio::time::sleep(cfg.batch_window)`
em `run_scheduler` quando a fila ainda não estava cheia. `lock_wait_ms` começa
depois e mede a espera pelo mutex separadamente, portanto não deve ser usado
como explicação para uma fila uniforme entre GPUs diferentes. Com o default de
0 ms não há espera temporizada; um valor positivo mantém exatamente a janela
opt-in. O relay tinha ainda um `sleep` fixo de 4 ms fora dessa métrica, agora
substituído por flush assíncrono imediato.

Quando as entradas são compatíveis, o worker monta um único `llama_batch` com
uma sequência física por sessão. Prefill e decode de tokens ficam em uma lane;
hidden states ficam em outra. Uma sessão aparece no máximo uma vez por
microbatch, e o `BatchLayout` usa `item_id` e o range global de linhas para
associar cada residual e posição de logits à resposta correta. Especulação,
rollback e Tensor Parallelism continuam no caminho individual até terem
contratos de batch próprios.

O scheduler é local ao peer. Duas réplicas que servem a mesma faixa possuem
filas e KV independentes; o roteamento deve escolher uma réplica e preservar
essa afinidade durante a sessão. Dois ranks do mesmo grupo TP são outra
unidade: eles precisam executar o mesmo lote coordenado pela barreira TP, por
isso a execução multi-sequência ainda fica desabilitada no TP atual.

### 3.2 Telemetria do nó

O servidor de inferência acumula os cronômetros internos em microssegundos em
`NodeTimings`. Ao montar o metadado `node_timings`, os campos públicos
`*_ms` usam milissegundos fracionários (`us / 1000.0`) para não perder uma
etapa menor que 1 ms. Isso vale para a chamada de decode, a conclusão medida
por `llama_synchronize`, captura D2H, logits, amostragem, fila e os demais
limites do bloco.

`POM_PROFILE_SYNC` fica ligado por padrão e controla se a barreira de
diagnóstico é executada. `backend_completion_ms` é uma duração: zero pode
representar uma duração nula ou uma medição pequena demais apenas se um
consumidor antigo ainda tiver truncado o número, e não deve ser usado para
deduzir sozinho o estado da configuração do peer.

### 3.3 Template de conversa e completions

O gateway distingue o formato do pedido antes de iniciar o prefill. Chat
OpenAI (`/v1/chat/completions`), Anthropic (`/v1/messages`) e
`POST /api/generate` enviam `chat_template: true` no metadado do primeiro
prefill. A cabeça, que é a única dona do tokenizer, aplica o
`tokenizer.chat_template` do GGUF uma vez e abre o turno do assistente; os
demais peers recebem somente o estado oculto e não reformatam o prompt.
Para Gemma4, o renderer lê o template do GGUF e aplica `enable_thinking` e
`add_generation_prompt` no fluxo suportado pelo modelo, sem inserir marcadores
de outro formato. A cauda encerra em EOS ou no fim de turno tokenizado pelo
vocabulário do mesmo GGUF, sem enviar esse marcador ao cliente.

`/v1/completions` continua sendo completion cru e envia `chat_template: false`.
Essa flag explícita tem precedência sobre o padrão legado do servidor, que
existia para chamadas internas sem metadado. `no_thinking` controla apenas a
abertura do bloco de raciocínio e não decide mais se o prompt é conversa. Assim
o modo de pensamento e o enquadramento de conversa não podem se confundir.

No TP, `handle_prefill` e `handle_step` serializam também o caminho de um
frame com um único item usando `tp_batch_gate`. O gateway pode enviar esse
frame pelo `BatchCoordinator`, e outra requisição não pode substituir o decode
global enquanto os ranks ainda aguardam a coletiva. Os filhos fan-out de um
batch já protegido usam `BATCH_CHILD_HEADER` para não tentar adquirir o mesmo
lock novamente. Ao reencodar a resposta de um item único, os handlers
preservam `item_id` e `batch_id`; sem isso o coordenador não consegue associar
a resposta ao pedido que está aguardando.

### 3.2 Sub-faixa executada por hop

`PipelineStep` separa a faixa que o peer carregou (`start_layer`/`end_layer`)
da faixa que aquele passo deve executar (`exec_start`/`exec_end`). O planejador
preenche a segunda com a cobertura que falta a partir da posição atual. Esses
dois campos viajam no `meta` de cada item do frame, tanto no prefill quanto no
decode de `/v1/step`; o envelope `frame/v1-batch` não muda. Sem os campos, o
receptor mantém o comportamento de executar a fatia inteira.

O handler confere a sub-faixa contra a faixa carregada antes de enfileirar o
trabalho. Quando `exec_start` coincide com `start_layer`, `exec_end` menor que
`end_layer` reposiciona a captura de residual para o bloco local correspondente.
O callback copia o residual antes das operações seguintes e deixa o grafo
continuar; portanto uma cabeça obrigatória pode ser executada depois da captura
sem alterar o estado oculto enviado ao próximo peer. No DeepSeek4, o grafo chama
essa fronteira `l_last-N`; o alias `l_out-N` continua aceito pelo POM. Quando o
pedido chega até `end_layer`, a captura normal da fatia inteira é usada. Uma
sub-faixa invertida ou fora da fatia recebe HTTP 400; começar acima de `start_layer`
também é recusado explicitamente, porque o motor atual sempre entra pelo
primeiro bloco da fatia e não consegue pular a cabeça já carregada.

Quando `llama_model_n_embd_out` é diferente de `llama_model_n_embd`, o contexto
não liga `cparams.embeddings`. A extração genérica do llama.cpp calcula a leitura
com a largura de saída da metadata, que pode ser maior que o tensor materializado
no grafo e provocar um `GGML_ASSERT` de leitura fora dos limites. O callback de
`l_out`/`l_last` continua sendo o único caminho de captura usado pelo POM.

O caminho de entrada mantém a forma do residual até `llama_decode`: a recepção
valida o shape, o relay preserva os eixos, e o `llama_batch` Rust é alocado com
a largura efetiva. O fork publicado em `pom-tensor-parallel` também carrega
essa largura explícita pelo allocator e pelo input do grafo. Para uma entrada
expandida, o grafo normal usa a largura física reportada por `n_embd_out` e a
reinterpreta diretamente como `[n_embd, fluxos, tokens]`, sem repetir a primeira
faixa. Para a entrada canônica, o allocator e o grafo continuam usando
`n_embd_inp`; uma largura que não seja canônica nem a expandida reportada é
recusada com a geometria no erro.

Por isso o planejador só escolhe uma faixa que começa exatamente na posição
executada. Faixas sobrepostas no inventário podem formar entradas alternativas,
mas cada caminho de uma requisição avança por sub-faixas contíguas. Quando há
réplicas equivalentes na entrada, o gateway escolhe a de menor RTT medido a
partir do nó que recebeu a requisição e mantém essa escolha na sessão.

O relay de anel e a resposta direta escolhem a faixa do próximo `PipelineStep`
ao montar o próximo quadro. Um microbatch com limites de captura diferentes é
recusado, pois um único grafo não pode parar em dois blocos. Essa execução
parcial é uma capacidade do binário atual, não um fallback para peers antigos.

`prefill::BatchRoute` é a chave de compatibilidade para o próximo passo do
transporte: modelo, deployment, fase, configuração relevante e próximo hop
precisam coincidir para dois itens compartilharem um frame. A escolha efetiva
das réplicas fica registrada por `pipeline::RouteAffinity`, que rejeita uma
rota diferente no meio da mesma sessão.

O endpoint `/health` expõe `scheduler.queue_depth`, os limites efetivos, o
tamanho e os tokens do último lote, além dos contadores acumulados. Isso
permite diferenciar fila local de tempo de rede e de tempo de decode.

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
- `forward_direct`: o repasse corre em task própria (quem chamou já foi aceito),
  mas a sequência de prefill espera o `chunk_index` predecessor antes de
  transmitir. Assim, a computação do próximo pedaço pode acontecer em paralelo
  sem permitir que um estado oculto posterior seja aplicado primeiro. Falhou o
  repasse, o próprio hop denuncia direto ao gateway nomeando o nó que realmente
  morreu.
- Falha que chega com `error` no lugar do token (`direct_failure`) nomeia o hop
  morto e marca **esse** (não a cabeça) com `routes.mark_failure`.

**`prompt_progress`**: sem volta, a cabeça não pode mais sobrepor
`more_chunks`/`n_prompt_tokens` na resposta que passa por ela (no anel isso era
`overlay_head_authoritative_fields`). Ela manda os dois **adiante**
(`prompt_progress_for_forward`) e a cauda copia para o resultado
(`apply_prompt_progress`). Errar isso trunca todo prompt de mais de um pedaço
no primeiro, em silêncio. O ACK imediato da cabeça também carrega essa
estrutura aninhada; `prefill::run_direct` a lê para enviar o chunk seguinte com
`start_pos = next_pos`, inclusive quando há sobreposição e quando o primeiro
chunk reaproveita um prefixo. `direct_lab.rs` recusa chunks posteriores sem
`start_pos` e cobre as duas situações.

**Telemetria** (`direct::hop_entry`/`accumulate`/`record_transport`): cada hop
descreve a si mesmo na ida e o chamador envia uma evidência separada da
travessia. `transport_traversals` contém uma entrada por chamada, em ordem,
com `transport` (`local`, `libp2p-tunnel` ou `https`), `transport_fallback`,
tempo e bytes enviados e recebidos. O trace copia cada entrada para o hop e
fase correspondentes, sem lançar o residual no primeiro hop. Se uma evidência
não chegar em até 2 segundos depois do resultado, o pedido segue com
`transport_trace_complete=false` e a UI deixa a ausência visível. Um hop local
na mesma máquina pode ter zero de rede corretamente; isso não é uma atribuição
ao primeiro hop.

## 5. Decodificação especulativa

São **dois** mecanismos, com chaves de operador separadas, e nunca os dois na
mesma rodada (o lote é um só, e misturar as fontes tornaria a taxa de aceitação
impossível de atribuir). O gateway escolhe em `generate_stream_with_stops`: com as duas
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
requisição (`especulando`, em `generate_stream_with_stops`): passa a chamar
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

O buffer `h` do contexto MTP usa `n_embd_out`, a largura física reportada pelo
llama.cpp. Em arquiteturas com eixos de fluxo, usar apenas `n_embd` perderia
parte do estado antes de chamar `llama_decode`; em modelos sem expansão os dois
valores são iguais.

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

O gateway **só tem o caminho do pipeline**. `generate_stream_with_stops` falha com
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
  `generate_stream_with_stops` de verdade contra hops falsos que decidem com
  `should_draft`/`find_draft`/`longest_match` de produção. O motor é fingido
  (sem GGUF nesta máquina ela prova o contrato entre os processos, não o que o
  llama.cpp faz com o lote); por isso ela não exercita `n_rs_seq`/`seq_rm` de
  verdade. Como os hops falsos implementam frames unitários, o fixture só
  anuncia capabilities compatíveis e não entra no `BatchCoordinator`; só o
  llama.cpp faz isso, ver o doc do módulo para o que mudou em 2026-08-16. Ela
  também confere o resumo agregado `GenerationTimings` no caminho de geração do
  gateway, incluindo uma resposta de um token.

## 9. Limite de prompt

`gateway::prompt_budget_of(n_ctx, max_sessions, max_new_tokens)`:

```
n_ctx - max_new_tokens
```

`n_ctx` é a janela completa de cada sessão. `max_sessions` (padrão **1**) só
multiplica a capacidade total reservada no `llama_context`, então o runtime usa
`n_ctx_total = n_ctx * max_sessions` e cada sessão continua com `n_ctx`. O
gateway mede o prompt
(`estimate_tokens` ≈ chars/3, pessimista) **antes** de qualquer peer ver o
prompt e recusa com uma mensagem citando as flags. O `/health` expõe os dois
valores como `n_ctx_total` e `n_ctx_per_session`.

O adaptador OpenAI de chat faz essa mesma verificação antes de iniciar o SSE e responde
HTTP 413 quando o orçamento foi excedido. Sem esse preflight, a geração emitia
um evento de erro dentro de um HTTP 200; clientes como OpenCode aguardavam uma
resposta válida e mostravam apenas uma execução sem resposta. Requisições que
cabem no orçamento mantêm o SSE e o caminho distribuído inalterados.

O `POST /api/generate` aceita `max_tokens` opcional. Na ausência, usa 512;
quando presente, exige valor maior que zero e no máximo o `max_output_tokens`
do `pom.json` (ver 9.1), recusando 400 com os dois valores quando excede. O
limite é o total de tokens de saída: o primeiro token devolvido pelo prefill já
conta, portanto um pedido de 64 nunca gera 65 por somar uma rodada extra de
decode.

- `n_batch` **não** entra na conta: o prompt vai em pedaços, então `n_batch`
  limita um pedaço (`min(prefill_chunk_tokens, n_batch)`), não o prompt. Antes
  de o prefill ser fatiado, `n_batch` na fórmula capava todo prompt em 512
  tokens por maior que fosse `--n-ctx`.
- Por que o limite existe: passá-lo faz o llama.cpp abortar o **processo
  inteiro** com `GGML_ASSERT(n_tokens_all <= cparams.n_batch)` - um prompt
  grande derruba o peer e todas as requisições em voo. Recusar cedo mantém o nó
  de pé. Números medidos: o OpenCode 1.18.21 manda ~2200 caracteres na chamada
  de título e ~54k no agente padrão, com `max_tokens: 32000`, ferramentas e
  `stream_options.include_usage`. Config que fez funcionar para o agente padrão:
  `--n-ctx 32768 --n-batch 3072 --max-new-tokens 256` → orçamento de 32512
  tokens; sem uma janela compatível o nó responde HTTP 413 antes do SSE.

### 9.1 Os dois orçamentos de saída e o laço de continuação

O `max_new_tokens` da implantação **nunca foi limite do backend**. O
`infer_server` não o conhece: ele decodifica enquanto o gateway pedir `/v1/step`.
Ele sempre foi um corte do gateway - e cortar ali, calado, era o defeito. Medido
no cluster em 2026-08-27, `max_tokens: 4000` contra uma implantação de 1000
devolvia 1000 tokens e `finish_reason: "length"`, sem erro nenhum; com uma
ferramenta declarada, o corte caía **dentro** do `<tool_call>` e a resposta
voltava sem `tool_calls`, com a chamada inacabada degradada em texto solto. Um
harness que trate `max_tokens` como condição terminal - o opencode trata - larga
o turno aí e o arquivo fica pela metade.

Hoje são **dois números**, e nunca um só (`gateway::OutputBudget`):

| | quem decide | o que limita |
|---|---|---|
| **perna** (`leg`) | `NodeConfig.max_new_tokens` da implantação | **uma** geração de backend |
| **total** (`total`) | `min(pedido, max_output_tokens, n_ctx - prompt)` | a **requisição inteira** |

Esgotar a perna não encerra a resposta. `gateway::note_leg_boundary` conta a
perna e o laço segue decodificando na **mesma** sessão de KV, na mesma conexão,
no mesmo fluxo SSE: nada é reprefilado, nada é reenviado, o cliente não tem como
distinguir a costura. Só o **total** encerra, e só ele produz
`finish_reason: "length"`. O `x_pom.continuation_legs` diz quantas pernas a
resposta usou - `1` é uma geração só.

Isso vale para `stream: true` e `stream: false` pelo mesmo caminho: o
`stream: false` costura internamente e devolve uma resposta única. A análise da
chamada de ferramenta acontece sobre a saída **completa** nos dois - o
`TagScanner` só é fechado (`finish()`) no fim de verdade -, porque um pedaço
intermediário é tag inacabada por construção.

**O teto absoluto é obrigatório.** Sem ele um modelo em laço ocuparia o nó
indefinidamente, já que a perna deixou de encerrar a resposta.
`max_output_tokens` no `pom.json` (padrão `DEFAULT_MAX_OUTPUT_TOKENS = 8192`) é
esse freio, e é ele que aparece como `absolute_max_tokens` no
`x_pom.output_limits`. Zero ali é recusa nomeada, não corte.

**Por que não há reprefill.** O laço não reabre geração: ele continua a que já
está aberta. A alternativa - encerrar a perna, reabrir com o texto gerado como
prefixo e contar com o cache de prefixo - custaria reprefill do que já saiu, a
cada perna, e ocuparia exatamente o mesmo KV no fim. Não há vantagem nenhuma em
pagar por ela.

**Limite conhecido.** `context_available` usa `cfg.n_ctx`, que é a capacidade
total do contexto; com um pool (`max_sessions > 1`) cada contexto recebe
`n_ctx / n`, então esse número é otimista. É imprecisão anterior a esta
funcionalidade, e o `max_output_tokens` conservador é o que a mantém inofensiva
na prática.

A bancada é `crates/node/src/backend/continuation_lab.rs`.

## 10. O canal de cada salto na telemetria

`SentFrame` e `PhaseTrace` carregam a identidade do transporte e a medição de
cada travessia, e eles aparecem numa linha própria da tabela de telemetria do
Chat.

- **`transport_summary`** - resumo limitado das travessias deste hop, agrupado
  por transporte (`local`, `libp2p-tunnel`, `https` ou `unknown`). Cada entrada
  informa chamadas, bytes enviados e recebidos, RTT mínimo, mediana, p95 e
  máximo, além de quantas chamadas chegaram por fallback naquele transporte.
  O resumo também informa `fallback_calls` total e `transport_switches` agrupadas
  por par origem/destino, para uma troca durante a execução não desaparecer no
  agregado. O caminho direto recebe os recibos por mensagens autenticadas pelo
  ticket e espera no máximo 2 segundos depois do resultado;
  `transport_trace_complete=false` deixa a perda explícita. As amostras usadas
  para quantis são limitadas internamente, portanto a resposta de produção não
  cresce com o número de tokens.

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
