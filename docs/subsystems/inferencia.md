# Caminho de inferência

O que o código faz hoje, verificado nos fontes (ver nota de versão no fim).

Um pedido de geração (OpenAI `/v1/chat/completions`, Anthropic `/v1/messages`,
`POST /api/generate`) desce por um único caminho: `gateway::generate_stream_with_stops`.
Ele planeja a cadeia de peers com `routing/pipeline.rs`, manda o prompt e cada
token pelo pipeline distribuído e devolve tokens ao cliente. Este documento
descreve os modos de execução, o formato binário do estado oculto, o
prefill em pedaços, a resposta direta, a decodificação especulativa e os
limites.

Os handlers de `/v1/prefill`, `/v1/step` e `/v1/step-stream` seguram um
`LoadGuard` durante a computação. O tracker é compartilhado pelo gateway e pelo
servidor de inferência local, então `GET /api/ui/node` mede também pedidos que
chegam diretamente à porta de inferência; a ausência de requisições reporta
zero, e falha de leitura permanece desconhecida na UI.

## Porta de inferência e exposição administrativa

O servidor de inferência (`--inference-port`) mantém a escuta em IPv4 e IPv6,
com fallback para IPv4 puro quando a máquina não oferece IPv6. Essa porta não é
fechada pela preferência da interface administrativa: o HTTP direto é a
reserva necessária quando o túnel libp2p falha. No gateway, `/api/generate` e
`/v1/**` seguem a mesma regra de disponibilidade; somente `/api/ui/**` e
`/admin/**` entram na fronteira localhost/token descrita em `api-http.md`.

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
As rotas entre peers aceitam exclusivamente `Authorization: Bearer
<connection_key>` junto de `x-pom-internal-node: 1`; uma API key de cliente não
é fallback e não atravessa o pipeline. O loopback do túnel mantém o
`Content-Type` JSON do release de sessão em vez de aplicar o padrão binário.
O gateway valida a API key do cliente na superfície de entrada e pode consultar
o principal para uma chave desconhecida. Essa consulta não participa da
autenticação dos hops internos.
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
`hidden_state_dtype` é evidência do fio, não a configuração local do gateway:
ele só aparece quando bytes de estado oculto cruzaram o hop. Por isso fica
ausente, em vez de afirmar f8 ou f16, no caso de um plano de modelo inteiro de
um único nó, que não transmite estado oculto.

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
`RuntimeParams.fit_memory_margin_mb` configura quanto o fit deixa livre em
cada dispositivo, com padrão de 256 MiB. Antes de chamar o
`common_fit_params`, a ponte consulta o total e o livre no backend na mesma
ordem das GPUs usadas pelo fit; o livre de `cudaMemGetInfo` já exclui o que
outros processos ocupam. Assim a margem não subtrai essa ocupação novamente:
ela cobre a diferença entre a projeção e as alocações tardias, além de uma
ocupação que apareça entre a consulta e a carga. Se o livre ficar abaixo da
margem configurada, a reserva acompanha o livre; abaixo do piso de 4 MiB,
vira zero para não transformar toda tentativa de carga em uma meta impossível.
O valor não é consultado quando `fit_memory=false`.
Esse encaixe nativo é exclusivo do carregamento real. A pré-visualização de
capacidade da tela Implantar usa metadata GGUF, `plan::range_cost` e a VRAM
livre atual para uma estimativa analítica; ela não chama `llama_backend_init`,
`llama_rs_fit_params` nem `fit_plan`, para não concorrer com a carga em
andamento.
Quando `mtp_enabled=true`, o fit primeiro encaixa o modelo principal e seu
contexto sem MTP. Antes de considerar a reserva opcional, lê
`general.architecture` e verifica uma lista gerada pelo próprio fork para as
arquiteturas que implementam `LLM_GRAPH_TYPE_DECODER_MTP`. O
`crates/llama-engine/build/mtp_arch.rs` encontra a verificação efetiva em cada
`src/models/*.cpp`, cruza a classe com o dispatch de `llama-model.cpp` e
resolve o nome oficial na tabela `LLM_ARCH_NAMES` de `llama-arch.cpp`.
Comentários que apenas mencionam o símbolo não contam, e a verificação precisa
estar no escopo inequívoco de `build_arch_graph`. O build falha se o fork
estiver ausente, se a extração for vazia, menor que treze entradas ou
inconsistente, para que uma mudança incompatível do upstream não desligue o
MTP em silêncio. A API
pública do llama.cpp não expõe uma consulta de suporte ao grafo; metadata
desconhecida ou arquitetura fora da lista gerada desliga o MTP antes de criar
qualquer contexto e publica a razão em `mtp_unavailable_reason`. O peso `nextn`
sozinho não é prova de que o grafo é válido.

Para uma arquitetura conhecida, a reserva conjunta calcula somente o KV dos
blocos `nextn` por geometria GGUF, usando `n_ctx`, `kv_seq_budget` e os tipos de
cache configurados, e coloca esse custo no último dispositivo da ordem
slow-to-fast. Ela não abre modelo nem constrói um contexto MTP seco para medir
`compute`: a medição exata exigiria construir o grafo, e um assert interno do
llama.cpp nessa etapa abortaria o nó antes que o POM pudesse recuar. O custo de
compute é, portanto, validado apenas na tentativa real de criar o contexto,
que pode recuar para a RAM. A tentativa conjunta só é aceita quando preserva
esse encaixe integral; se reduzir `n_gpu_layers` ou falhar, o plano principal
vence, o alvo fica sem pesos `nextn` na VRAM e o MTP recua para o modelo
companheiro em RAM, se permitido, ou fica indisponível. Assim a reserva
conservadora protege as camadas que cada token precisa executar sem tornar o
fit uma fonte de abortos. MTP desligado não lê metadata MTP, não estima
orçamento e não tenta anexar contexto.
Antes de criar o contexto de rascunho, o runtime confere a evidência dos
tensores `nextn` no GGUF e se eles foram pedidos em
`llama_model_params.load_mtp`; `llama_model_n_layer_nextn` sozinho não basta,
pois expõe apenas metadata e pode continuar positivo no alvo sem esses pesos.
No recuo para RAM, a seleção do contexto é feita explicitamente sobre o modelo
companheiro CPU-only. Se o modelo selecionado não carrega `nextn`, a guarda
recusa o contexto antes da reserva do grafo, publica a razão exata
`o modelo carregado não traz o bloco nextn` e mantém a inferência principal.
Se a criação do contexto MTP ainda falhar na VRAM, o runtime tenta o recuo
quando `RuntimeParams.mtp_cpu_fallback_enabled=true`, valor padrão. Esse
recuo carrega um modelo irmão CPU-only para o contexto `nextn`, com
`offload_kqv=false` e `op_offload=false`; assim o scheduler não arrasta o
grafo de volta para a GPU do modelo alvo. O modelo alvo continua na colocação
definida pelo fit. A tentativa só termina em MTP indisponível se o operador
desligar o recuo ou a carga do modelo/contexto CPU também falhar.
O `/health` publica `mtp_ready`, `mtp_allocation` (`vram` ou `ram`) e
`mtp_unavailable_reason`. A tela mostra a localização em todo sucesso e a
razão operacional quando o modelo não tem `nextn`, quando o recuo foi recusado
ou quando VRAM e RAM falharam.
O campo `context_memory` continua separando o cache KV (`context`) do buffer de
cálculo (`compute`) para os contextos vivos. O fit MTP não tenta projetar o
`compute` por uma sondagem seca: `sched_reserve` e os grafos variam por
arquitetura, e construir esse grafo como medição não é uma operação recuperável
quando o llama.cpp encontra uma entrada inválida. A decisão é deliberadamente
conservadora: reservar o KV calculável é menos exato, mas mantém a possibilidade
de recuo para a RAM e não pode derrubar o processo. O wrapper ainda pode
reportar os buffers reais depois que o contexto MTP foi criado; isso é
telemetria, não parte do orçamento especulativo do fit.
`fit_plan` também registra o PID, a memória GPU do próprio processo quando
`nvidia-smi` a fornece, o total, o livre e a margem por dispositivo.
Para evitar que a reserva tardia de temporários CUDA derrube o processo quando a
GPU já está quase cheia, `fit_memory=true` mantém `n_batch` como limite do
chunk, mas usa `n_ubatch=min(n_batch, 128)` no contexto. O prompt continua
aceitando chunks de até `n_batch`; o custo é mais etapas internas de cálculo em
troca de uma reserva menor. `/health` expõe esse valor como
`n_ubatch_effective`. Com `fit_memory=false`, `n_ubatch` acompanha `n_batch`.
Essa política fica em `llm::apply_context_batch_params` e é aplicada tanto ao
contexto principal quanto ao contexto MTP, sem reduzir o `n_batch` público.

#### Flash Attention e os parâmetros de carga

O projeto nunca escreve `flash_attn_type`: todo contexto sai de
`llama_context_default_params()`, cujo padrão é `LLAMA_FLASH_ATTN_TYPE_AUTO`.
O llama.cpp sonda o backend ao criar o contexto e decide - o pedido não é a
resposta. Como nada no `llama.h` público devolve a decisão,
`llama_rs_context_flash_attn` (em `vendor/llama-cpp-sys-2/wrapper_common.cpp`)
lê `get_cparams()` e devolve o par `(ligado, veio da sonda)`.

`LlmEngine::load_params` junta essa decisão aos demais parâmetros do contexto e
o `/health` a publica em `load_params`. Os valores vêm do contexto vivo
(`llama_n_ctx`, `llama_n_batch`, `llama_n_ubatch`, `llama_n_seq_max`,
`llama_n_threads`) sempre que existe getter; só `kv_unified`, `embeddings` e
`n_rs_seq` são guardados num `ContextParamsSnapshot` tirado imediatamente antes
de `llama_new_context_with_model`, porque não têm getter. **Ecoar o
`EngineConfig` seria errado**: o fallback de KV em RAM reconstrói o contexto com
outra configuração, e o operador precisa ver o que está rodando. A tela de
Detalhes mostra tudo isso em "Parâmetros de carga do llama.cpp"
(ver `web-ui.md`).

#### Ordem segura por velocidade

Quando o fit automático encontra mais de um dispositivo elegível, o POM coleta
os handles na mesma seleção padrão do llama.cpp e captura o livre e o total
antes de qualquer probe. Depois mede uma vez por handle uma multiplicação
matriz-vetor F32 pequena através da interface genérica de grafo do GGML. A
pontuação fica em cache durante o processo, portanto a carga seguinte do mesmo
modelo não repete a medição. Não há API de fabricante, tabela de placas ou
prioridade fixa por backend.

A ordenação só é aplicada se todos os resultados forem finitos e positivos,
cada fronteira entre velocidades vizinhas superar 25% e todos os handles
usarem o mesmo backend. Se um backend não informa memória, não inicializa, não
aloca ou não executa o grafo, se as velocidades forem equivalentes, ou se os
backends forem heterogêneos ou desconhecidos, o resultado é indeterminado e o
POM não faz palpite: deixa `llama_model_params.devices` nulo, mantendo a ordem
e o comportamento atuais. O mesmo vale para um único dispositivo, que não paga
o probe. A guarda de backend é necessária porque uma lista explícita pode
alterar a topologia de cópia e divisão de um grafo misto Vulkan/CUDA; o fit
automático não deve transformar uma medição sintética em um novo caminho
heterogêneo. Quando há confiança, a lista explícita é passada tanto ao
`common_fit_params` quanto à carga real, do mais lento ao mais rápido. Como o
fit preenche as camadas de trás para a frente, o dispositivo mais rápido fica
com a faixa final e a cabeça de saída, que é percorrida em cada token, enquanto
as faixas continuam contíguas e o número de fronteiras do grafo não aumenta.

O cache KV não é deslocado artificialmente para o dispositivo lento. Ele é
acessado em cada token, cresce com o contexto e segue o dispositivo da camada;
assim, o dispositivo rápido recebe também o KV das camadas que ele executa e
a cabeça, dentro do limite de memória. A memória livre ainda é a restrição
dura para pesos, contexto e temporários. Esta política evita trocar uma
travessia repetida por uma economia nominal de bytes e mantém o fallback
existente para `offload_kqv=false` quando o KV não cabe na GPU.

Esta política é somente no POM. O `common/fit.cpp` do llama.cpp não foi
modificado: os nomes de registro já expostos pelo backend são suficientes para
impedir uma ordenação explícita insegura sem alterar a distribuição de memória
depois da sondagem.

O executor externo de MoE é um callback de host, mas usa CUDA internamente.
Todas as entradas da ponte fixam alocações, streams e eventos no CUDA 0, o
dispositivo cujo orçamento o cache mede, e restauram o dispositivo corrente do
thread ao sair. Sem essa guarda, o callback pode deixar o thread no CUDA 0 e a
próxima espera do scheduler no backend CUDA 1 receber um stream ou evento no
contexto corrente errado. As 59 divisões observadas com o hook são esperadas:
cada camada externalizada atravessa a operação customizada de host; reduzir
esse número não corrige a afinidade nem substitui a sincronização correta.

A guarda também é a fronteira de degradação para o defeito observado em
01/09/2026: a combinação Vulkan/CUDA mantém a ordem padrão do llama.cpp e não
recebe a lista que fez a fatia mudar de `CUDA,Vulkan` para `Vulkan,CUDA`. Isso
protege a carga conhecida antes do primeiro decode. Ainda não existe uma
fronteira em processo capaz de capturar qualquer `ggml_cuda_error`: o fork
chama `ggml_abort` e termina o processo depois de um erro CUDA, e
`catch_unwind`/o código de erro de `llama_decode` não interceptam esse abort.
Continuar no mesmo contexto CUDA após acesso ilegal também não seria seguro;
isolamento em processo separado ou mudança da política fatal do fork exigiria
uma decisão arquitetural própria.

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

## 1. Os modos de execução

O prefill acontece em `inference/prefill.rs` (`run`); no decode, o gateway
escolhe o transporte remoto quando o head é outro nó e anuncia
`inference/remote-token-stream-v1`. Caso contrário, `step` usa os três modos
tradicionais:

1. **Transporte remoto por lotes** (`inference/remote-token-stream-v1`,
   `step_stream`/`/v1/step-stream`) - o gateway envia um control frame ao head
   remoto e recebe os tokens por callbacks em `/v1/remote-stream-result`. O
   head mantém o laço de geração; em um pipeline com mais hops, os demais peers
   continuam dentro do relay do head. O produtor põe cada token numa fila
   limitada a 16 itens e não espera o callback; o emissor drena, até 8 itens que
  já estejam prontos. Se a fila enche, o envio do callback espera e aplica
  contrapressão até a geração. Cada URL de callback que falha fica fora por 5
  min e a entrega tem teto de 500 ms por tentativa, então uma rota que engole
  SYN não é repetida a cada lote. EOS e erro enviam
   um lote terminal; stop, limite, ciclo e cancelamento removem o ticket e
   fazem uma entrega atrasada receber `410 Gone`. Em plano MTP fatiado, o
   control frame inicial leva `speculate.source="mtp"`; a cauda devolve
   `next_draft` no mesmo callback e o head instala o draft no próximo `step`
   interno. O callback também informa `speculated`, `speculation_active` e
   `speculation_disabled_reason`, sem travessia ou chamada adicional por token.
2. **Resposta direta** (`relay/v2-direct`, `run_direct`/`step_direct`) - quando
   `PipelinePlan::supports_direct_reply()`: todos os passos anunciam
   `relay/v2-direct` **e** a cadeia tem pelo menos 2 hops. O gateway despacha
   cada quadro uma vez para a cabeça, carregando um `reply_to`; a cauda entrega
   o resultado direto ao gateway por `/v1/relay-result`.
3. **Anel borbulhado** (`relay/v1-chain`, `run_ring`/`step_ring`) - quando
   `supports_relay_chain()`: o gateway faz **uma** chamada (para a cabeça) e
   cada hop repassa ao seu próximo via `next_hops`; o resultado volta borbulhando
   pela pilha de chamadas aninhada.
4. **Hub** - o resto: o gateway chama cada hop em sequência, recebe o tensor de
   volta e reenvia ao seguinte.

O log do gateway marca qual modo entrou (`resposta=remote-stream|direta|anel|hub`). A escolha
é all-or-nothing por requisição: um único passo sem a capability derruba a
feature do plano inteiro (`supports_relay_chain`/`supports_direct_reply`/
`supports_speculation` em `routing/pipeline.rs`), para não misturar modos no
meio da cadeia. O transporte remoto exige que o head anuncie a capability e
que o gateway tenha endereço de callback; sem isso a requisição mantém o
caminho anterior. Um head local nunca usa o transporte remoto.

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

### 2.1 Ligação persistente entre andares (`transport/link-v1`)

O quadro acima descreve **o que** atravessa. Esta seção descreve **por onde**,
dentro da pipeline: entre um hop e o hop seguinte.

O transporte remoto por lotes (seção 1, item 1) tirou a travessia por ficha do
trecho gateway → cabeça. Dentro do laço da cabeça, porém, cada ativação ainda
montava uma requisição nova para o hop seguinte: `POST /v1/step`, cabeçalhos,
roteamento e enquadramento reconstruídos por ficha, por salto. A conexão TCP/TLS
por baixo já vinha do pool (é por isso que `tls_handshake_ms` só aparece no
salto que abre conexão); o que se refazia por ativação era a abstração de
requisição por cima dela.

`transport/link-v1` mantém **um canal por aresta** (`node_id` + URL de
inferência): um único `POST /v1/link` cujo corpo de ida é um fluxo de quadros
binários e cujo corpo de volta é outro fluxo de quadros binários. A abertura
testa os candidatos de endereço, tem teto de 2 s para os cabeçalhos e deixa a
rota que falhou em memória por 5 min. Um guarda por aresta serializa somente
aberturas concorrentes do mesmo destino; a abertura acontece fora da trava
global do mapa de canais. Cada quadro, nas duas direções:

```
[4B BE tamanho do resto][2B BE status][2B BE tamanho do id][id][frame/v1-batch]
```

- `status` é `0` numa ida e o código HTTP numa volta. Uma recusa do peer chega
  com o mesmo significado que teria no caminho por requisição e vira
  `HttpRefusal` no chamador - trocar de transporte não muda uma recusa
  considerada, então ela **não** é repetida por outra via.
- `id` é o `batch_id` do quadro, a identidade que o `BatchCoordinator` já usa.
  É ele que correlaciona ida e volta e é ele que permite **várias sessões no
  mesmo canal**: os quadros não voltam em ordem, voltam identificados. Cada
  quadro é despachado numa tarefa própria no receptor, então uma sessão lenta
  não bloqueia por cabeça de fila a sessão que chegou depois.
- A rota vai no `kind` do próprio `frame/v1-batch`, então o envelope não duplica
  caminho. O receptor despacha pela mesma tabela de rotas em processo que o
  caminho por requisição usa, e canal e requisição executam exatamente o mesmo
  código de inferência.
- A fila de ida tem 64 quadros: um hop lento segura quem o alimenta em vez de
  deixar a memória crescer. O limite de corpo é desligado nessa rota, porque o
  corpo é a geração inteira, não um quadro.
- **A credencial é reconferida a cada quadro**, não só na abertura. O receptor
  confere a `connection_key` e o marcador interno; uma conexão de 12 h que
  autenticou uma vez manteria válido por 12 h um segredo trocado por outro
  processo. Isso autentica o peer no transporte, não adiciona assinatura
  administrativa Ed25519 a cada frame de inferência. Um quadro recusado é recusa
  **daquele quadro** - o canal continua servindo os seguintes.

**Degradação.** Falha de transporte no canal não mata a geração: a chamada volta
pela escada de sempre (túnel libp2p, depois HTTPS por requisição) com
`transport_fallback=true`, e o canal morto sai do pool. Um destino cuja abertura
falhou fica 5 min em quarentena, o mesmo prazo da memória de rotas, para a
tentativa de abrir não se pagar por ficha num cluster onde o endereço direto não
serve (peer atrás de NAT alcançável só pelo túnel). O canal tem teto de vida de
12 h, porque o timeout do cliente HTTP vale para a requisição inteira e o corpo
em fluxo conta para ele; vencido o teto, a chamada em voo degrada e a seguinte
abre canal novo.

**Limpeza.** Toda entrega pendente vive num registro por canal e sai dele no
`Drop` de um guarda, em qualquer caminho de saída: resposta, erro, prazo,
cancelamento do chamador. Quando o canal morre, o leitor drena o registro
inteiro com erro antes de sumir - nenhum chamador fica pendurado, e uma resposta
atrasada não encontra ticket para entregar.

**Medição** (`inference/link_lab.rs`, 2026-09-03). A bancada serve cada hop
falso por cima de um `TcpStream` embrulhado num contador, então o número são os
bytes que realmente passaram no soquete, não uma estimativa. Motor falso de
propósito: um hop que decodificasse mediria a GPU, não o transporte. Residual de
`[1, 5120]` em f16 (10.240 bytes), 40 fichas por configuração, macOS/arm64,
build de teste. A única diferença entre "antes" e "depois" é o hop anunciar ou
não `transport/link-v1`.

| Cadeia | Bytes/ficha/travessia (1º hop) | Bytes/ficha na cadeia | ms/ficha (média de 5) |
|---|---:|---:|---:|
| 1 hop, requisição por ficha | 11.465 | 11.465 | 0,935 |
| 1 hop, canal persistente | **11.302** | **11.302** | **0,815** |
| 2 hops, requisição por ficha | 11.768 | 23.233 | 2,421 |
| 2 hops, canal persistente | **11.625** | **22.927** | **1,804** |
| 3 hops, requisição por ficha | 12.043 | 35.276 | 2,928 |
| 3 hops, canal persistente | **11.920** | **34.847** | **2,816** |

As colunas de bytes são **determinísticas** - repetem valor a valor entre
execuções, porque são bytes contados no soquete. A coluna de tempo é ruidosa
(build de teste, máquina compartilhada): entre cinco execuções ela varia mais
do que a diferença que mede em algumas linhas, então a leitura honesta dela é
"o canal não ficou mais lento e tende a ficar entre 4% e 25% mais rápido",
não um número exato. Quem quiser o tempo em produção tem que medi-lo lá, onde
a travessia inclui RTT e GPU.

**O ganho em bytes é pequeno e é preciso dizer o tamanho dele: 163 bytes por
ficha por travessia, -1,4%.** O que o canal remove é exatamente o envelope
HTTP por ativação, e é só isso que ele tem para remover. O motivo está na própria tabela: dos 11.302 bytes por ficha,
**10.240 são o estado oculto** - 90,6%. O custo de rede por ficha no salto
interno é o tensor, e o tensor não muda de tamanho por causa do transporte;
quem o reduz é o `dtype` do fio (seção 2). Dos ~1.060 bytes restantes, a maior
parcela é o header JSON, e dentro dele o `next_hops` repetido a cada ficha
(cerca de 275 bytes por hop restante, visível na coluna do 1º hop: 11.302 com
um hop contra 11.920 com três). Essa é a próxima alavanca, e ela exige o peer
guardar a rota por sessão - não é transporte puro, e não foi feita aqui.

O canal também é o que garante que **uma** ligação atende muitas fichas e
muitas sessões: a bancada afirma zero conexões novas depois do aquecimento, e
seis sessões concorrentes com dez fichas cada cabendo numa ligação só.

### 2.1.1 Regressão de endereço anunciado e memória de falha (2026-09-04)

A regressão que derrubou a cabeça remota para **2,2 tok/s**, com **TTFT de
6,04 s** e **31 s ponta a ponta**, não era trabalho do llama.cpp: o nó mediu
**593 ms** de processamento, enquanto a execução aguardou cerca de 30 s fora
do nó. O contrafactual do captain confirmou a causa: trocar o endereço
anunciado de IPv6 para o IPv4 da LAN fez a execução voltar à ordem de **43
tok/s**.

A validação pós-correção fechou o contrafactual sem depender do contorno: o
captain restaurou o endereço IPv6 original e mediu novamente a inferência,
sem regressão e com o mesmo resultado rápido observado no IPv4. Pela API,
`/api/generate` com a cabeça remota passou de **2,2 tok/s** antes da correção
para **67,4 tok/s** depois, com **TTFT de 58 ms** e **949 ms** ponta a ponta.
Isso coloca o caminho remoto na mesma ordem do caminho local, medido em **95,4
tok/s**.

Há três fatos diferentes:

- **Gatilho:** um candidato de endereço anunciado, em especial um IPv6 que não
  aceitava a rota de tráfego real, era usado para abrir a conexão TCP. O sistema
  operacional aguardava o timeout de conexão antes de a escada de transporte
  prosseguir. No `remote-stream`, introduzido por **`7ace9d5`**, a cauda também
  tentava a lista de callbacks do `reply_to` a cada lote, sempre começando no
  mesmo endereço e sem memória de falha. O resultado era uma espera repetida
  por lote, observada como cerca de 553 ms por ficha com o motor parado.
- **Condição que mascarava:** a cabeça local não precisava entregar callbacks
  pela rota remota, e o IPv4 escolhido pelo operador era alcançável. Além disso,
  desde **`1738c88`** a task de rotas sondava o `/health` do gateway e gravava a
  URL de inferência correspondente; um gateway saudável podia, portanto,
  promover uma porta de inferência que engolia SYN. `RouteTable::mark_failure`
  removia a escolha, mas não guardava o endereço falho: a resolução seguinte
  podia escolhê-lo outra vez. A tela também olhava apenas para os campos medidos
  no nó e na rede, por isso dizia que o backend dominava apesar de o total
  ponta a ponta estar muito maior.
- **Sintoma visível:** 8 callbacks `remote-stream` para 56 fichas, uma troca de
  transporte no meio, TTFT alto e taxa de geração 43 vezes menor, embora
  `llama_decode` e o processamento do nó permanecessem próximos do caminho
  local.

O RTT cru mostrou que a rede explica parte da anomalia, mas não os 30 s: pelo
  tailnet, o ping entre as máquinas ficou em **3,9-4,4 ms** de média e o
  `/health` em **9-22 ms**; pelo IPv4 direto da LAN, a média foi **25,0 ms**,
  com máximo de **91,7 ms**, e o sentido inverso para o IPv4 privado não foi
  alcançável. Esse jitter é alto para duas máquinas na mesma LAN e justifica
  conferir Wi-Fi e economia de energia, mas não pode produzir centenas de
  milissegundos por ficha quando o RTT do callback aparece como zero: o atraso
  estava na tentativa de conexão e não era medido pelo trace de
  `remote-stream`.

A correção é estrutural e vale para IPv4 e IPv6:

1. `RouteTable::measure_peer` sonda `/health` na própria porta de inferência,
   nunca em uma porta de gateway paralela.
2. Falhas reais ficam memorizadas por **5 min** em `RouteTable` e em
   `PeerClients`; uma resolução ou callback não escolhe novamente o endereço
   que falhou enquanto há candidato alternativo.
3. O `PeerClients` limita a abertura TCP a **2 s**, sem reduzir o timeout de
   processamento de uma requisição longa. A entrega de resultado/callback tem
   teto de **500 ms** nos candidatos intermediários e **30 s** no último, e
   testa a próxima URL na mesma chamada. Três falhas seguidas movem uma URL
   para o fim da ordem por 5 min.
4. O `transport/link-v1`, introduzido por **`d7b4e6e`**, ajuda depois de uma
   abertura que falha, pois sua quarentena de 5 min evita reabrir a mesma URL.
   Antes desta correção ela só cobria a chave do canal, não os demais
   candidatos, e a abertura ainda podia ficar presa no timeout de conexão.
   Agora o canal testa as URLs anunciadas, limita a abertura e compartilha a
   memória de falha. A quarentena deixa de ser uma espera escondida por ficha,
   sem ser tratada como a causa do callback repetido.

O teste `a_blackholed_reply_address_is_tried_only_once_across_stream_batches`
reproduz um destino que aceita o SYN e não responde ao HTTP: a primeira entrega
registra a falha e usa a alternativa saudável; a segunda entrega não toca mais
no destino morto. Os testes equivalentes de `relay` e `link` verificam o mesmo
contrafactual no frame direto e na abertura persistente. Com a implementação
anterior, a asserção de uma única conexão falhava porque `blackhole_hits` ficava
em 2; com a memória de falha, fica em 1. A bancada ignorada
`the_tunnel_carries_a_real_frame_to_a_live_peer` também usa `192.0.2.1` (TEST-NET)
para exercitar um endereço que engole SYN e confirmar que o timeout de abertura
acontece antes do fallback. A API não tem um caminho especial:
`/api/generate`, `/v1/chat/completions` e a interface chamam
`generate_stream_with_stops`, então uma execução pela API sofre a mesma seleção
de endereço e recebe a mesma correção.

### 2.2 Cópias no caminho quente do `wire.rs`

O enquadramento de uma travessia (codificar + decodificar um item de
`[1, 5120]` f16) custava 21,97 µs em release; hoje custa 20,94 µs - **-4,7%**
(`custo_do_enquadramento_por_travessia`, mesma máquina, 2.000 rodadas). O que
mudou:

- `encode_batch_items` escreve o tensor **uma vez**, direto no buffer que vai
  para o fio. As posições saem de `tensor_payload_len`, que as calcula sem
  alocar, e o header JSON fica pronto antes de qualquer conversão. Antes os
  mesmos bytes eram tocados quatro vezes: converter, concatenar no payload,
  clonar a `BatchFrame`, copiar para a saída.
- A conversão passa por um bloco de pilha de 1 KiB. Crescer o vetor valor a
  valor custa mais que a conversão em si - medido em quase o dobro do
  enquadramento inteiro, e foi por isso que a primeira versão desta mudança
  ficou **mais lenta** que o código que substituía.
- A decodificação **empresta** o payload do buffer recebido em vez de copiá-lo:
  `split_frame` separa header e payload sem `to_vec`, e `decode_batch_items`,
  `decode_batch_single` e `decode_inference_request` saem daí. A única cópia do
  tensor que resta é a expansão para f32 que a inferência consome.
- `peek_batch_header` e `validate_batch_bytes` leem só o header. Quem só
  precisa saber a rota, o `batch_id`, quantos itens vêm no quadro, ou se os
  bytes são um quadro válido, deixou de copiar o estado oculto inteiro para
  descobrir. Eram quatro pontos no caminho quente: as duas validações do
  transporte no `relay.rs` e o teste de "isto é um lote?" na entrada de
  `handle_step` e `handle_prefill`.

O formato do fio não mudou.

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
- **Falha de transporte no prefill**: cada pedaço recebe uma única tentativa
  (`relay::CHUNK_ATTEMPTS = 1`). Quando a resposta se perde, não há como saber
  se o peer gravou o chunk antes da desconexão; repetir cegamente poderia pedir
  remoção parcial de KV no meio de um modelo recorrente. Se os chunks anteriores
  já foram aceitos, o gateway reconstrói a sessão inteira a frio, começando no
  chunk 0. Se essa reconstrução falhar, a geração termina limpa e libera as
  sessões alcançadas. `start_pos` continua sendo a fronteira absoluta usada no
  fluxo normal; ausência nos chunks posteriores é erro de protocolo, nunca
  `chunk_index * chunk_size`.
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

**Retomada.** Um erro depois de um pedaço `k > 0` carrega
`PrefillResumable { from_chunk, start_pos, ... }` como diagnóstico do ponto
onde uma retomada poderia ser segura se o estado remoto fosse conhecido. Uma
resposta de transporte perdida não dá essa certeza: o gateway não reentra em
`k`, porque isso poderia pedir `kv_remove` no meio do KV. Ele faz uma única
reconstrução fria desde o pedaço 0, que reseta a sessão inteira em todos os
hops. Sessão perdida no peer segue a mesma reconstrução; se ela falhar, a
sessão termina limpa e o gateway libera os hops alcançados.

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
reinício do processo, remoção explícita ou resposta de transporte perdida -, o
gateway não repete um chunk posterior. Ele reconhece que o estado remoto pode
estar em qualquer posição e repete a rodada inteira do zero uma única vez,
mesmo quando o pedido não usava prefixo. O custo é refazer também os chunks já
aceitos e o KV em todos os hops. Se a reconstrução falhar, o erro final informa
que o sistema tentou reconstruir o contexto e que o pedido não foi executado;
antes dele, `release_sessions` percorre todas as URLs de cada hop e faz a
liberação em modo best effort. Reusar é otimização, nunca motivo para a
requisição falhar.

O prompt que o cache reconhece vem do **array de mensagens**
(`openai::conversation_cache_key`), não do texto renderizado, e `render_prompt`
rotula todo turno inclusive o primeiro: as duas coisas juntas garantem que o
prompt de um turno seja prefixo exato do prompt do seguinte. Texto cru sem
rótulo continua disponível em `/v1/completions` e `POST /api/generate`.
No caminho Anthropic, quando há `output_config.task_budget`,
`conversation_cache_key_with_budget` também prefixa `type`, `total` e
`remaining`; trocar qualquer valor invalida o KV porque troca o marcador que o
modelo enxerga. Como o marcador é final e muda a cada requisição, ele não
coincide entre turnos, conforme a semântica documentada do provedor.

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

### 3.1.1 Snapshots de KV cache em disco (`--slot-save-path`)

O cache de prefixo acima vive **só em memória**: reinício do processo, coleta de
ociosidade (`reap_idle`) ou pressão de capacidade (`evict_retained`) apagam o KV
retido e o próximo turno paga um prefill frio (ver "Por que a sessão
desaparece"). O diretório de snapshots liga um mecanismo para gravar e reler
esse KV, equivalente ao `--slot-save-path` do llama-server. Ele vem, nesta
precedência, da flag `--slot-save-path`, da variável `POM_SLOT_SAVE_PATH` ou do
campo `slot_save_path` do `pom.json` (`Config::slot_save_path_effective`); a env
var é por processo e não é persistida. Numa implantação pela UI o valor é
injetado no `NodeConfig` em `apply_locked` a partir da instalação, como
`cache_type_*`, mas a flag da linha de comando, quando presente, vence.

- **Motor** (`llama-engine`): `LlmEngine::state_seq_save_file` /
  `state_seq_load_file` embrulham `llama_state_seq_save_file` /
  `llama_state_seq_load_file`. O `.bin` do llama.cpp serializa as células de uma
  sequência com suas posições e um cabeçalho de tokens **consultivo** (nunca
  conferido contra as células), e valida ao restaurar a geometria de KV
  (contagem de camadas, tipo e tamanho de linha por camada) - uma divergência é
  `0` bytes, virando erro, nunca abort.
- **Peer** (`infer_server`): `POST /v1/session/slot`
  `{action: save|restore|erase, session_id, slot_name, dest_session_id?}`.
  `save` grava **`prompt_seq` truncado em `prompt_len`** - nunca `gen_seq`, a
  cauda volátil não entra num snapshot - mais um sidecar `<name>.json` com a
  identidade do modelo (`model_label`, faixa de camadas, `n_embd`) que
  `restore` confere **antes** de tocar no motor. `restore` cria uma sessão nova
  `retained`, com `n_past`/`prompt_len` na fronteira salva; a cabeça grava os
  tokens reais no sidecar, os hops seguintes um vetor do tamanho certo. A rota
  também é replicada aos workers de TP (`replicate_slot_call`), que servem a
  mesma faixa e gravam a própria cópia. `/health` publica
  `slot_persist_enabled` e `slot_count`; a capability é
  `inference/slot-persist-v1`, anunciada só quando há diretório configurado.
- **Gateway** (`gateway::snapshot_deployment` / `restore_deployment` /
  `erase_snapshot`): um snapshot de implantação é tudo-ou-nada. O gateway
  resolve o pipeline (`resolve_plan_for_model`, mesmo planejador da geração),
  exige `inference/slot-persist-v1` em todos os hops e chama `/v1/session/slot`
  em cada um com o nome `"{snapshot}__{node_id}"` (um diretório guarda a fatia
  de cada peer sem colisão). Uma falha em qualquer hop apaga os que já
  gravaram; no `restore`, hops que voltem com fronteiras de tokens diferentes
  liberam a sessão nova em todos e devolvem erro. Operador: `GET /admin/slots`,
  `POST /admin/slots/{name}?action=save|restore|erase` (ver `api-http.md`).

Restaurar reproduz o mesmo estado físico de uma sessão viva retida (células
além de `prompt_len` podem sobrar em `prompt_seq`, exatamente como no caminho
normal), então o turno seguinte segue a regra só-acrescentar de
`plan_prefix_reuse` sem mudança. Motores recorrentes/híbridos: o `.bin`
restaura a sequência inteira de uma vez, sem remoção parcial, então o mecanismo
vale para todo modelo servido.

#### Cache de prefixo persistente (automático)

Com `slot_persist_prefix_cache` ligado (campo do `pom.json` ou
`POM_SLOT_PERSIST_PREFIX_CACHE=1`; desligado por padrão) e `slot_save_path`
configurado em **todos** os hops, o gateway torna o cache de prefixo durável ao
reinício, sem chamada manual:

- **Spill.** No fim de cada turno, depois de `rewind_sessions` reter o prompt,
  o gateway grava um snapshot automático numa task destacada (fora do caminho
  da resposta): `snapshot_deployment` com o nome `auto__<key_hash>`, onde
  `key_hash = hash(PrefixRouteKey + cache_key)`. Um índice pequeno
  (`prefix-auto-index.json`, ao lado dos `.bin` no `slot_save_path` deste
  gateway) guarda `(rota, prompt, tokens, bytes, updated_at)` por `key_hash`.
- **Debounce.** Se o prefixo retido cresceu menos que
  `SLOT_AUTO_MIN_GROWTH_TOKENS` (256) desde o último snapshot da mesma
  linhagem, o turno só renova o carimbo do ancestral e **não** regrava - cada
  spill é um serialize de KV mais um fan-out.
- **Supersessão.** Ao gravar, `PrefixDiskIndex::record` solta toda entrada da
  mesma rota cujo prompt é prefixo estrito do novo e apaga os `.bin` dela
  (`erase_snapshot`). Uma conversa que cresce mantém **um** `.bin`, não um por
  turno. Uma conversa que se ramifica perde o ponto intermediário e cai no
  ancestral que sobrar ou no prefill frio.
- **Reload no miss.** Quando o `PrefixCache` em memória não tem a conversa
  (típico logo após um reinício), o gateway procura no índice o maior prefixo
  **estrito** do prompt pedido; achando, `restore_deployment` recria o KV em
  todos os hops numa sessão nova, o `PrefixCache` adota essa sessão como hit
  (`PrefixCache::adopt`) e o turno segue como um acerto normal - a cabeça
  reaproveita pela mesma regra só-acrescentar. O trace publica
  `prefix_reuse_status = "disk"`.
- **Limites.** `slot_auto_max_bytes` (padrão 2 GiB, somado entre fatias) com
  despejo LRU por `updated_at`, e um TTL configurável por `slot_auto_ttl_secs`
  no `pom.json` ou `POM_SLOT_AUTO_TTL_SECS` (padrão 7 dias). `updated_at` é
  renovado a cada gravação e a cada restauração, então uma conversa ativa não
  expira. Um `.bin` despejado é apagado com `erase_snapshot`.
- **Fronteiras.** O índice é **local do gateway**: se o papel de gateway mudar
  de máquina, o novo gateway não enxerga as entradas e cai no prefill frio (os
  `.bin` órfãos saem por TTL/LRU). Um `restore` que falha (arquivo sumiu num
  hop, geometria incompatível) esquece a entrada e segue a frio - reusar é
  otimização, nunca motivo para a requisição falhar.

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
no runtime. O plano de ownership externaliza todos os formatos que o runtime
próprio consegue executar em CPU, mesmo sem ponte CUDA; só NVFP4 sem feature
CUDA e sem pedido explícito de CPU fica indisponível. O caminho CUDA continua
reservado aos dtypes com ponte validada e não há afirmação de sobreposição
CPU/GPU além da telemetria publicada.

`path/engine mismatch` na UI é um rótulo de telemetria real: o `selected_path`
registra o caminho escolhido (`moe/q4_0` ou `dense/llama.cpp`) e
`executed_engine` registra o executor que de fato rodou. A flag aparece quando
eles divergem, por exemplo, quando o caminho MoE selecionado recua para
`llama.cpp`; não é sintoma do transporte remoto nem da rede. Ausência total de
amostra mantém o motor como desconhecido e não é chamada de divergência.

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

### 3.4 Amostragem da completion

A cauda prepara a distribuição no `LlmEngine` em uma ordem única: copia os
logits, aplica a penalidade aos ids distintos da janela final de
`repeat_last_n`, devolve argmax quando a temperatura não é positiva e, para
temperatura positiva, seleciona no máximo `top_k` logits. Calcula softmax
estável apenas nesses sobreviventes, normaliza, ordena por probabilidade e
mantém o menor prefixo cuja massa alcance `top_p`; após renormalizar esse
núcleo, faz o sorteio multinomial com o LCG protegido por `Mutex`.

Empates no corte de `top_k` são resolvidos pelo id original e não aumentam o
conjunto além de K. O corte top-p também mantém somente o prefixo mínimo, mesmo
com empates, para que a massa escolhida seja observável e determinística antes
do sorteio. `repeat_last_n=64` permanece o padrão: a penalidade é unigramática,
não um detector de ciclos de n-gramas ou blocos inteiros, portanto aumentar a
janela não garantiria prevenir o ciclo observado e mudaria a política de
amostragem.

### 3.5 Detecção de ciclo na saída

O POM escolhe a família **C, detectar e parar**, como uma rede de segurança no
gateway. A família **D, repetir a inferência**, é incompatível com o fluxo: no
momento em que o padrão fica evidente, as fichas já chegaram ao cliente e uma
segunda execução teria de descartar minutos de saída já transmitida. A família
**A** pode ser acrescentada ao sampler no futuro, mas uma penalidade estrutural
precisa ser afinada contra tabelas, listas e código legítimos. A família **B**
também fica aberta para uma intervenção futura, mas não pode retirar a ficha que
já saiu nem promete recuperar um modelo que ficou confiante no padrão.

`inference/cycle.rs` observa o texto das peças depois da amostragem, mantém uma
história limitada e procura três janelas consecutivas iguais. Para o caso de
nomes incrementados, dígitos no sufixo de um identificador são normalizados para
uma marca comum (`SRS_FINAL_FULL_2`, `_3`, `_4`); números separados em dados de
tabela continuam distintos. A janela mínima padrão é 32 fichas, a máxima é
512 e o detector aceita limites configuráveis de 1 a 1024 fichas e de 2 a 8
repetições. A confirmação exige três ocorrências para reduzir falsos positivos:
uma tabela ou lista que repete a moldura com valores novos não casa, porque o
conteúdo canônico também muda.

O detector é ligado por padrão, não altera logits, temperatura, `top_p`, `top_k`,
`repeat_penalty`, `repeat_last_n`, o estado recorrente ou `kv_seq_budget`, e não
faz parte do protocolo entre peers. Quando confirma um ciclo, o gateway inclui
a ficha que acabou de receber, esvazia o `StopMatcher`, não pede outra rodada de
decode, libera a sessão e emite um resumo com `partial: true` e
`finish_reason: "cycle"`. `/api/generate` expõe esses campos diretamente. Nos
adaptadores OpenAI e Anthropic, o motivo específico fica em `x_pom.finish_reason`
no quadro final e `x_pom.partial` fica verdadeiro; os motivos padrão dos
provedores continuam compatíveis (`stop` e `end_turn`). Não há truncamento
silencioso nem nova inferência.

O operador pode desligar o comportamento com `--no-cycle-detection` no processo
ou desmarcando `cycle_detection_enabled` na configuração de implantação. Os
campos `cycle_detection_min_window_tokens`,
`cycle_detection_max_window_tokens` e `cycle_detection_repetitions` ajustam a
política pelos parâmetros de runtime da tela de implantação e são propagados a
todos os peers.

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
- `await_result_with_ack`: `select!` biased entre o resultado do inbox (caso normal), o
  ack da cabeça (só interessa quando falha, ou quando um peer antigo borbulhou
  a resposta inteira - `token_id` presente vale como resultado), e o prazo
  `DIRECT_REPLY_DEADLINE = 600 s` (hop morreu depois de aceitar).
- `deliver_result`: a cauda tenta cada URL de `reply_to` em ordem; candidatos
  intermediários têm `DELIVERY_TIMEOUT = 500 ms` e o último tem 30 s.
  Três falhas seguidas movem a URL para o fim por 5 min. `410` para de tentar
  (o inbox já não existe); falhou em todas, o gateway fecha no prazo - não há
  terceira via.
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

**Transporte remoto por lotes e a causa da travessia por ficha.** A investigação
confirmou a travessia por ficha no `step_ring`: `generate_stream_inner` aguardava
cada `prefill::step`, e o `step_ring` fazia um `/v1/step` para a cabeça antes de
pedir o próximo token. No túnel, `send_frame_via_tunnel_with_auth` mede a
resposta inteira e preenche `response_headers_ms` com o mesmo `round_trip_ms`.
Portanto, `gateway: to headers` é o tempo até o ACK/resposta do túnel, não o
tempo que o gateway passou escrevendo SSE para o frontend. O código do gateway
já tinha um pump separado para o cliente e um canal limitado, então a hipótese
de a escrita SSE ser a causa dos 774 ms não se confirmou.

O transporte novo envia um control frame `/v1/step-stream` uma vez ao head
remoto. O produtor do peer chama o mesmo `handle_step` e continua a inferência;
um emissor separado drena no máximo oito tokens que já estejam na fila e faz um
POST por lote em `/v1/remote-stream-result`, sem sleep nem temporizador. A fila
de callback tem capacidade 16: um cliente lento bloqueia o emissor quando ela
enche e, em seguida, o produtor, em vez de deixar memória crescer sem limite.
Em planos fatiados, o head mantém os demais hops dentro da rede. O trace conta
`remote-stream` por callback, registra bytes e soma os tempos do nó; a travessia
de controle aparece separadamente. O conteúdo continua vindo do sampler e do
UTF-8 assembler existentes, na mesma ordem.

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
fase correspondentes, sem lançar o residual no primeiro hop. O resultado não
espera recibos atrasados: se uma evidência ainda não chegou, o pedido segue com
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
- **Modelo híbrido/recorrente não produz draft quando
  `kv_can_shift=false`.** A cauda aplica essa guarda no momento de produzir o
  `next_draft`, informa `hybrid_tail_no_rollback` e o gateway desativa MTP pelo
  restante da requisição. A cabeça ainda pode aceitar um draft que já tenha
  chegado para concluir uma rodada; o que não pode acontecer é iniciar outra
  rodada com estado cuja cauda não consegue reverter. `should_draft` continua
  decidindo apenas se a cabeça monta o lote, com base em `mtp_ready` ou em
  draft pronto.
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
  formas e também lê `speculation_active` e
  `speculation_disabled_reason` dos callbacks novos. A ausência desses campos
  preserva a compatibilidade com peers antigos.
- **Recusada a especulação, o gateway para de insistir** pelo resto da
requisição (`especulando`, em `generate_stream_with_stops`): passa a chamar
  `prefill::step`, que já usa a resposta direta (N+1 travessias). Não é só
  economia do campo `speculate` - insistir prende a cadeia no anel borbulhado
  (2N travessias por token).
- O gateway clampa `speculative_k` a `n_batch` (`speculative_k_of`), porque o
  lote especulativo também é um batch único na cabeça.
- A rota legada `step_speculative` continua no anel/hub, sem usar
  `relay/v2-direct`. O transporte remoto usa a mesma especulação em planos de
  um ou mais hops quando todos anunciam `speculate/v1-mtp-sliced` e a cauda
  anuncia `speculate/v1-mtp`; cada hop faz rollback local no reply já em voo.
  Peer sem a capacidade nova degrada para o sampler plain com a razão
  `peer_without_sliced_mtp_wire`.

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
- **Custa um contexto no caminho VRAM, e um modelo irmão no recuo.** O contexto
  de rascunho é criado sobre os pesos do alvo (`llama_init_from_model` com
  `ctx_type = MTP`); quando precisa ir para a RAM, um segundo modelo CPU-only
  mantém todo o grafo `nextn` no host. O custo adicional de contexto continua
  sendo um cache KV de uma camada, e o GGUF pode compartilhar páginas mmap.
- **A VRAM é tentada com orçamento conjunto.** O `fit_plan` mede o contexto
  MTP seco por dispositivo e reserva seus bytes de KV e computação junto com o
  contexto principal. O custo acompanha `n_ctx` e o microlote efetivo, sem
  constante fixa; com mais de uma GPU cada reserva fica na GPU reportada pela
  sondagem, não numa soma global.
- **Há recuo explícito para a RAM.** Se a criação do contexto MTP falhar na
  VRAM e `mtp_cpu_fallback_enabled` estiver ligado, o carregamento é repetido
  com um modelo MTP CPU-only e com `offload_kqv=false` e `op_offload=false` no
  contexto MTP. O alvo é então recarregado sem os pesos `nextn` da tentativa
  VRAM, para que essa memória também seja devolvida à GPU. Um override apenas
  nos tensores `nextn` foi investigado, mas não basta porque o scheduler também
  pode escolher o dispositivo da camada e do output head. O padrão é ligado;
  o operador pode desligá-lo na implantação quando a medição local não
  compensar a sincronização CPU/GPU.
- **O microlote menor preserva o pareamento.** `observe` monta cada linha com
  posição, sequência, token e estado oculto explícitos: a linha 0 recebe o
  `pending_h`, e as linhas seguintes recebem `h` do alvo na posição anterior.
  O alvo emite `h` para todas as posições e o rascunho pede logits apenas nas
  posições necessárias. O `llama_decode` do rascunho pode dividir esse batch
  interno em vários `n_ubatch`, mas não recalcula nem infere a correspondência
  pelas fronteiras do microlote. A sonda
  `mtp::tests::the_nextn_head_drafts_from_a_real_model` cruza um microlote com
  `fit_memory=true` e valida o ciclo observe, draft e accept sobre um GGUF real
  quando executada com `--ignored`; os testes de política cobrem `n_batch=4096`
  com fit ligado e desligado.
- **Liga no modelo inteiro e no pipeline fatiado.**
  `NodeConfig::mtp_effective` decide por nó (modelo inteiro, ou a cauda de um
  pipeline fatiado que serve a última camada); `PipelinePlan::supports_mtp`
  confere no plano. Quem rascunha é a máquina que serve a última camada - no
  caso fatiado, a cauda, que tem o bloco `nextn`; quem monta o lote é a cabeça.
- **A cauda rascunha na própria resposta.** O rascunho viaja de trás para a
  frente **sem custo extra de travessia**: no stream remoto, a cauda anexa
  `next_draft` ao callback que já seria enviado e a cabeça o instala no
  `speculate.draft` do próximo `step` interno. O gateway recebe esse campo e
  os metadados de estado, mas não reinjeta o draft nesse modo. No caminho
  `relay/v2-direct` legado, o gateway continua fazendo a injeção na rodada
  seguinte. O custo real a medir no caso fatiado é a taxa de aceitação - e não
  a rede.
- **A composição do lote viaja até a cauda.** A cabeça ecoa no reply o lote
  decodificado (campo `tokens`) e a largura do rascunho pedida (`mtp_k`); os
  hops do meio repassam; a cauda alimenta o `observe` do MTP com essa
  composição (`decode_hidden(..., tokens)`). Sem ela - peer velho que não
  ecoa - o `observe` desliga o MTP com aviso: a resposta continua correta, só
  não especulativa.
- **A aceitação é telemetria por requisição.** Cada `step` publica em
  `node_timings` os contadores `mtp_draft_tokens_proposed`,
  `mtp_draft_tokens_accepted` e `mtp_draft_tokens_rejected`. `proposed` é o
  tamanho do draft efetivamente submetido à verificação, portanto cada token
  verificado termina em aceito ou rejeitado. O gateway soma os contadores de
  todos os hops e pernas no `RequestTrace` e calcula `mtp_acceptance_rate` como
  `accepted / proposed`; sem proposta MTP a taxa é `null`. A contagem ocorre uma
  vez por lote verificado, fora do laço por token, e não altera os parâmetros nem
  o critério da especulação.
- **Exige binário novo em todos os hops.** `supports_mtp` pede
  `speculate/v1-mtp-sliced` em cada step (o protocolo `tokens`/`mtp_k`/
  `next_draft` só existe em build novo - um peer velho no anel receberia um
  `speculate` que não entende) **e** `speculate/v1-mtp` na cauda.
- **Modelo híbrido/recorrente não rascunha quando `kv_can_shift=false`.** A
  cauda informa `hybrid_tail_no_rollback` e não devolve `next_draft`; o gateway
  encerra a especulação de forma limpa. Em stream remoto, cada hop faz
  rollback local antes de a resposta subir, sem endpoint extra. No caminho
  legado, permanece o fan-out de `/v1/session/rollback`.
- O MTP fatiado usa `step_stream` quando o head é remoto: um control frame
  inicia o ciclo e os callbacks carregam os tokens e o estado. Se o head é
  local, o gateway mantém o caminho direto `prefill::step` +
  `relay/v2-direct`.
- A fatia final é a única que carrega o bloco porque `splitter::split` zera
  `<arch>.nextn_predict_layers` fora dela - ver `docs/subsystems/fatiamento.md`.

Quando o operador pede MTP e o contexto de rascunho não pode ser criado, o
runtime mantém a inferência principal disponível, publica `mtp_ready=false` e
`mtp_unavailable_reason` em `/health`, e a aba de detalhes do Painel mostra o
estado e o motivo no card da máquina. A razão distingue modelo sem bloco
`nextn`, falta de VRAM com recuo recusado e falta de memória nos dois lados.
Quando pronto, `/health` publica `mtp_allocation` como `vram` ou `ram`, sempre
com o local medido nos buffers do contexto. Uma falha posterior do `observe`
também desliga a cabeça e atualiza esse motivo, para que a degradação não
dependa da leitura do log.

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
uma vez (prefill e decode), sem contar os recibos de transporte, e que o prompt
de mais de um pedaço atravessa inteiro, inclusive com reuso de prefixo.

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
quando presente, exige valor maior que zero. O limite efetivo é o menor entre o
pedido e o espaço restante de `n_ctx` depois do prompt. O primeiro token devolvido pelo prefill já
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
| **total** (`total`) | `min(pedido, n_ctx - prompt)` | a **requisição inteira** |

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

**O teto pedido pelo cliente é imposto e inviolável.** Nada o ultrapassa, por
motivo nenhum, nem uma chamada de ferramenta aberta no token do corte.
Atingi-lo é **parada terminal**, com o motivo de parada correspondente. É a
regra dos dois provedores, verificada na documentação viva em 2026-08-29:
`max_tokens` é "um teto por resposta imposto, do qual o modelo não tem
conhecimento", e ao ser atingido a saída "trunca no meio e exige nova
tentativa". Não existe continuação, folga ou transbordo em nenhuma das duas
APIs.

Houve aqui uma folga que deixava a geração passar do teto para fechar uma
chamada aberta. Ela foi **removida**: o
que os provedores oferecem no lugar do transbordo é o [orçamento de
tarefa](api-http.md#orçamento-de-tarefa), que avisa o modelo em vez de esticar
o teto.

**A capacidade do nó é derivada do runtime, não é um corte silencioso.**
O `n_ctx` efetivo confirmado pelo `/health` é anunciado em `/v1/models` como
`context_length` e `max_completion_tokens`. O gateway usa o menor contexto
confirmado entre os peers vivos do modelo e reduz o orçamento somente quando o
prompt ocupa parte dessa janela. Não há uma capacidade absoluta adicional no
arquivo de configuração.

O contexto que sobrou continua apertando o teto **sem** recusar, e isso é
deliberado: ele sai de uma estimativa de tokens do prompt (`estimate_tokens`),
e recusar por estimativa rejeitaria pedido válido. `x_pom.output_limits` conta
a verdade, que é o canal certo para um número aproximado.

**O corte dentro de uma chamada devolve a chamada estruturada e incompleta.**
Esta é a forma dos dois provedores, apurada na documentação viva em 2026-08-29.
A Anthropic devolve o último bloco como `tool_use` com o `input` incompleto e
`stop_reason: "max_tokens"`, e manda o cliente conferir exatamente esse par
para repetir o pedido com um `max_tokens` maior. A OpenAI devolve
`tool_calls[].function.arguments` - documentado como string que "nem sempre é
JSON válido", a validar antes de executar - junto de `finish_reason: "length"`.
Nenhum dos dois descarta a chamada, e nenhum dos dois a degrada em texto.

`TagScanner::finish_truncated` recupera o que dá: o nome da ferramenta e os
parâmetros que já saíram, incluindo o que estava sendo escrito no momento do
corte. Os argumentos saem numa string JSON **inacabada**, cortada onde o modelo
foi cortado - ela não parseia de propósito, e é isso que faz um cliente que
ignore o motivo de parada falhar em vez de executar meia chamada. O `input` da
Anthropic, que é tipado como objeto, leva o mesmo conteúdo já recuperado.

| situação | o cliente recebe |
|---|---|
| corte dentro da chamada, nome já emitido | a chamada **estruturada e incompleta**, `length`/`max_tokens`, `x_pom.truncated_tool_call: true` |
| corte antes de o nome fechar | só o texto anterior à abertura; um nome pela metade não é ferramenta |
| nenhuma chamada aberta | texto truncado, `length`/`max_tokens`, nada mais muda |

Marcação crua nunca é resposta: o scanner segura o corpo da chamada até saber
o que ele é, então nada de dentro da tag vaza como prosa. Em streaming os
argumentos saem em fragmentos conforme o modelo gera (ver
`api-http.md`, "Streaming incremental dos argumentos"), e o que já saiu conta:
o fechamento por corte emite **só o que falta**, de modo que um fragmento
parcial normal e um truncamento terminal nunca se confundem. `x_pom` conta a
verdade:
`truncated_tool_call` diz se o corte caiu dentro de uma chamada, e `partial` é
`true` sempre que isso aconteceu ou houve perda de transporte.

Um ciclo detectado também é uma parada terminal explícita, mas usa
`finish_reason: "cycle"` no `x_pom`, com `partial: true`; ver a seção 3.5.

**Por que não há reprefill.** O laço não reabre geração: ele continua a que já
está aberta. A alternativa - encerrar a perna, reabrir com o texto gerado como
prefixo e contar com o cache de prefixo - custaria reprefill do que já saiu, a
cada perna, e ocuparia exatamente o mesmo KV no fim. Não há vantagem nenhuma em
pagar por ela.

**Capacidade de todos os peers.** Depois que o `/health` prova o carregamento,
`runtime::spawn_health_watch` publica em `Announce.runtime_capacity` o
`n_ctx` efetivo por sessão retornado pelo `/health` (o total do engine dividido
por `max_sessions`), e o `max_new_tokens` da perna. O gateway usa o menor valor entre todos os
peers vivos do modelo/implantação tanto em `/v1/models` quanto na validação da
requisição. Apply, erro, stop e unload térmico limpam o anúncio antes do
restart, então uma capacidade antiga não fica no catálogo. Sem peer
correspondente, a configuração local continua sendo fallback para rotas sem
implantação; havendo peer vivo sem capacidade reportada, a requisição fica
indisponível até todos confirmarem o runtime, sem esconder um estágio antigo.

**Limite conhecido.** `context_available` ainda usa a janela por sessão; com
um pool (`max_sessions > 1`) cada contexto recebe `n_ctx / n`, então esse
número é otimista. A capacidade publicada deve ser lida novamente depois de
qualquer restart ou alteração da implantação.

A bancada é `crates/node/src/backend/continuation_lab.rs`.

## 10. O canal de cada salto na telemetria

`SentFrame` e `PhaseTrace` carregam a identidade do transporte e a medição de
cada travessia, e eles aparecem numa linha própria da tabela de telemetria do
Chat.

- **`transport_summary`** - resumo limitado das travessias deste hop, agrupado
  por transporte (`local`, `libp2p-tunnel`, `https`, `https-link`,
  `remote-stream` ou `unknown`). Cada entrada
  informa chamadas, bytes enviados e recebidos, RTT mínimo, mediana, p95 e
  máximo, além de quantas chamadas chegaram por fallback naquele transporte.
  O resumo também informa `fallback_calls` total e `transport_switches` agrupadas
  por par origem/destino, para uma troca durante a execução não desaparecer no
  agregado. O caminho direto recebe os recibos por mensagens autenticadas pelo
  ticket, mas a resposta não espera recibos atrasados;
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

**`https-link` é o canal persistente da seção 2.1.** Ele é o mesmo HTTPS
pinado - `tls` segue a política do cluster, como em `https` -, mas uma
travessia por ali não abriu requisição: o quadro entrou num corpo já aberto.
Uma travessia que aparece como `https` ou `libp2p-tunnel` com
`transport_fallback=true` num hop que anuncia `transport/link-v1` é o canal
tendo falhado e a geração tendo continuado - que é o comportamento pedido, não
um defeito. `tls_handshake_ms` fica zero nas travessias do canal pelo mesmo
motivo de sempre: o handshake aconteceu uma vez, na abertura.

**Um salto pelo túnel libp2p reporta `tls: false`.** Ele é cifrado - por Noise,
ponta a ponta -, mas não por TLS, e dizer `true` ali alegaria uma garantia que
aquele salto não deu.

No transporte remoto, o registro `remote-stream` representa um callback de
lote, não uma nova ida e volta de inferência. `bytes_received` é o corpo do
lote e `processing_ms`/`node_timings` são agregados dos tokens contidos nele;
`round_trip_ms` fica zero porque o callback é entregue ao inbox do gateway, que
já separa a espera do cliente da geração. O primeiro control frame mantém a
medição do transporte usado para iniciar o stream.

## 11. Defeitos anotados

- **SIGSEGV ao iniciar o contexto, independente dos logs (2026-09-04)**: na
  Apple M4, o binário release foi iniciado com o mesmo comando para
  `target/qwen2.5-0.5b-instruct-q4_k_m.gguf` e
  `target/gemma-3-270m-it-Q8_0.gguf`, usando `--memory 16000MB`,
  `--n-ctx 1024`, `--n-batch 512`, `--n-gpu-layers 0`, `--no-discovery` e
  `--single-peer`. Os dois carregaram pesos, reservaram o contexto e
  terminaram antes de aceitar requisições. O backtrace LLDB recuperado nos
  dois foi `EXC_BAD_ACCESS (code=1, address=0x0)` em
  `libsystem_platform.dylib` / `_platform_strlen + 4`; a pilha acima do
  frame libc não foi recuperável pelo unwinder desta execução.

  Comandos exatos da árvore desta tarefa:

  ```bash
  rtk lldb --batch -o 'settings set target.env-vars POM_HOME=/Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/target/bench-home POM_UI_DIST=/Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/apps/frontend/dist' -o run -o 'bt all' -o quit -- ./target/release/pom --model /Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/target/qwen2.5-0.5b-instruct-q4_k_m.gguf --memory 16000MB --port 52777 --inference-port 18181 --no-discovery --single-peer --start-layer 0 --end-layer 23 --n-ctx 1024 --n-batch 512 --max-new-tokens 64 --n-gpu-layers 0 --no-thinking
  rtk lldb --batch -o 'settings set target.env-vars POM_HOME=/Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/target/bench-home POM_UI_DIST=/Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/apps/frontend/dist' -o run -o 'bt all' -o quit -- ./target/release/pom --model /Users/jairpereira/.treehouse/rdma-p2p-242c70/4/rdma-p2p/target/gemma-3-270m-it-Q8_0.gguf --memory 16000MB --port 52778 --inference-port 18182 --no-discovery --single-peer --start-layer 0 --end-layer 23 --n-ctx 1024 --n-batch 512 --max-new-tokens 64 --n-gpu-layers 0 --no-thinking
  ```

  Para separar o achado desta mudança, `origin/feat/implantar3` em
  `4868f79764f906eeb5e71e49e028a0245b6376a4` foi exportado para uma árvore
  limpa, compilado com o mesmo checkout de `llama.cpp` e executado com o
  mesmo par de comandos, trocando apenas o binário por
  `target/clean-origin-pom` e as portas por `52779/18183` e `52780/18184`.
  Qwen e Gemma reproduziram o mesmo `EXC_BAD_ACCESS` em
  `_platform_strlen + 4`. O problema não foi investigado nesta tarefa.

- (não é defeito, é o comportamento de hoje) Porta ocupada: `runtime::free_port` acha quem segura a porta (`lsof -ti :PORT`
  no Unix, `netstat -ano`+`taskkill` no Windows) e mata (SIGKILL) antes de subir
  o servidor. Não conseguindo (ocupante não identificável, ou porta ainda presa
  depois de 5 s), o motivo vai para o `status.detail` como `Failed` já na
  primeira tentativa, e a linha "Mem loaded" da TUI passa a mostrar esse estado
  em vez do rótulo fixo `inference_server`. O histórico fica em
  `<project_root>/infer_server_<porta>.log` (`runtime::log_file_path`, apontado
  por `status.log_path`), porque a TUI limpa a tela a cada `--refresh`.
- **Sem cobertura E2E**: os quatro modos de falha do modo direto legado ainda
  não têm uma bancada ponta a ponta; o transporte remoto novo tem testes de
  unidade para lote, ordem, erro, terminação e contrapressão.
- **Resultado da revisão de 2026-09-04:** a medição de referência em
  `origin/feat/implantar3` tinha 1022 verdes, oito vermelhos e levou 609,55 s.
  Depois desta correção, a mesma medição levou 27,72 s, com 1049 verdes e
  cinco vermelhos. Ficaram verdes
  `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`,
  `direct_lab::direct_prefill_preserva_posicao_absoluta_com_e_sem_reuso` e
  `link_lab::canal_indisponivel_degrada_para_o_caminho_por_requisicao`.
  Nos dois testes de `direct_lab`, a causa era a memória de rotas de
  `84e7141`: a lista de candidatos ficava vazia e a entrega nunca era
  tentada. A contagem de entregas agora separa resultados dos recibos de
  transporte, e telemetria atrasada não segura o resultado.
  O teste de `link_lab` também ficou verde com a escada que não desativa a
  rota por requisição.
- Continuam vermelhos, sem alteração de expectativa nesta tarefa:
  `spec_lab::um_orcamento_acima_de_n_batch_nao_e_truncado` (espera teto 2048,
  recebe 96), três de `deploy::whole_route_tests` (estado de catálogo/fatias)
  e `webui::tests::merge_parts_joins_two_slices_into_a_whole` (metadado
  `head_count_kv_by_layer`). São expectativas de outros subsistemas e ficam
  anotados sem conserto de passagem.
- A contagem acima repete a medição da revisão com
  `--skip prefix_reuse_lab`. Sem esse filtro, a suíte também mantém vermelho
  `prefix_reuse_lab::unknown_session_recovery_is_bounded` (o hub repete os três
  chunks depois de perder a sessão); esse caso não fazia parte dos oito
  achados medidos e não foi alterado aqui.
- **Os testes de `webui` que repassam para uma máquina falsa são instáveis sob
  carga** (2026-09-03): cada rodada da suíte cheia derruba **um** deles, e nem
  sempre o mesmo - vistos `webui::leave_tests::leaving_for_a_peer_is_forwarded_to_that_peer`
  e `webui::peer_runtime_tests::the_peers_health_is_forwarded_whole`, ambos
  passando isolados. Sobem um servidor falso numa porta efêmera de loopback
  (`gateway::testkit`, cliente com 600 s de timeout, então não é prazo) e
  conferem que o repasse chegou. A causa não foi investigada aqui; o que está
  medido é que a instabilidade não depende do canal persistente - ela aparece
  igual na árvore sem ele.
## Nota de versão

Documentado a partir dos fontes na branch `fm/rdma-gateway-remoto-por-token`
(2026-09-01):
`inference/infer_server.rs`, `inference/prefill.rs`, `inference/direct.rs`,
`inference/direct_lab.rs`, `inference/spec_lab.rs`, `inference/relay.rs`,
`inference/wire.rs`, `inference/runtime.rs`, `routing/pipeline.rs`,
`backend/gateway.rs`, `backend/openai.rs`, `backend/messages.rs`, e os AGENTS.md
de `inference/`, `routing/`, `backend/` e raiz. Onde o código e a documentação
divergem, o código vence e a divergência foi anotada como defeito.
