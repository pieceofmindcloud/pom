# Goal FreeToken e caminho MoE

Este documento acompanha o goal `feat/ftk`. Ele registra as medições, decisões
e limites do caminho MoE novo sem alterar o contrato do caminho denso, que
continua usando o `llama.cpp`.

## Fonte e escopo

A fonte primária do estudo é
`/Users/jairpereira/Sites/firstmate/data/rdma-freetoken-estudo/report.md`, lido
integralmente antes do trabalho. O FreeToken foi usado como fonte de desenho e
algoritmo, não de código. Nenhum peso trafega na rede por causa do fatiamento.

O goal será entregue em fases coesas:

1. medições locais e documentação do baseline;
2. orçamento de KV em bytes, rebuild de contexto sem reler pesos, cancelamento
   de prefill e stop strings;
3. detecção de MoE com bifurcação reversível;
4. motor MoE próprio com um formato de quantização por vez e cache LRU global;
5. co-execução CPU/GPU baseada em banda medida, ainda pendente de um executor
   GPU concreto.

### Fase 1: S9 entregue

O gateway agora aceita `stop` no formato OpenAI e `stop_sequences` no formato
Anthropic, além do campo equivalente em `/api/generate`. Todos passam por um
`StopMatcher` incremental comum. Ele só libera texto quando o próximo sufixo
não pode mais completar nenhuma parada; ao casar, descarta a parada e o texto
posterior, encerra a geração e propaga `finish_reason=stop` mais
`stop_sequence`. O protocolo entre peers não mudou.

Os testes unitários cobrem parada dividida entre eventos, prefixo pendente,
UTF-8, duplicatas e múltiplas paradas. A suíte nativa foi executada depois de
materializar localmente o `llama.cpp` exigido pelo build script; os números
consolidados ficam na seção de validação ao fim deste documento.

Modelos densos devem continuar no caminho existente. A seleção do caminho MoE
será uma decisão explícita baseada em metadados GGUF, com teste que proíbe um
modelo denso de entrar no motor novo.

### Fase 2: S1, cache de prefixo entre turnos

Mesmo sem o baseline de desempenho, conforme a decisão registrada pelo captain,
o cache de prefixo foi implementado por segurança de estado e reaproveitamento
de trabalho. `PrefixCache` é global ao gateway, com capacidade nominal de 32
entradas e LRU entre entradas ociosas. Leases ocupados não são despejados, então
o total pode exceder a capacidade até uma geração terminar. A política não
guarda pesos nem tensors: cada entrada guarda somente a chave do modelo e dos
hops, o texto do prompt, o
`session_id` e a quantidade de tokens confirmada pela cabeça. Um lease marca a
sessão como ocupada para impedir que duas gerações usem o mesmo KV.

Em um miss, uma entrada ociosa pode ser reciclada para que o cache não exija
um segundo slot de KV quando `max_sessions=1`; o prefill novo reseta essa
sequência antes de decodificar. Entradas ocupadas nunca são compartilhadas.

Ao terminar o prefill com sucesso, o gateway envia `/v1/session/rewind` a todos
os peers do plano. Cada peer remove do KV o sufixo gerado e confirma o
`n_past` resultante. Só depois de todas as confirmações o lease entra no cache;
qualquer falha descarta a entrada e libera a sessão. No turno seguinte, um hit
mantém a rota e a sessão, sinaliza `reuse_prefix` no primeiro chunk e a cabeça
tokeniza o novo prompt. Ela calcula o maior prefixo de tokens comum, recua o KV
para essa posição e propaga `start_pos`/`next_pos` nos chunks seguintes. Os
peers que não tokenizam recebem apenas o estado oculto, mas executam na posição
explícita, evitando buracos silenciosos entre estágios.

Os campos são metadados adicionais do frame de inferência existente; o envelope
binário e as APIs públicas não mudaram. O autoteste diferencial de
`prefix_cache_lab.rs` executa operações contra uma referência simples e exige
que um proxy que altera `cached_tokens` seja detectado. O teste puro do cache
também cobre maior prefixo, leases ocupados e eviction LRU. A suíte nativa foi
executada com o symlink local documentado em `build-execucao.md`. Nenhum ganho
de desempenho é afirmado; os números ficam para a medição posterior pelo
captain.

### Fase 3: detecção de MoE e bifurcação reversível

`gguf::read_info` agora lê `<arquitetura>.expert_count` e
`<arquitetura>.expert_used_count`. A classificação é positiva somente quando
`expert_count > 1`; nome de arquivo, falha de carga e nomes de arquitetura sem
essa evidência não selecionam MoE. Em `auto`, um cabeçalho denso ou ausente
segue explicitamente para `dense/llama.cpp`.

O override `POM_INFERENCE_MODE` aceita `auto`, `dense` ou `moe` quando o deploy
está em `auto`; uma escolha explícita persistida pelo deploy vence o ambiente.
`dense` força o caminho existente mesmo para um GGUF MoE. `moe` recusa um modelo sem
`expert_count > 1`, em vez de desviar silenciosamente. O caminho MoE desta
fase foi inicialmente `moe/llama.cpp-delegated`, uma delegação temporária ao
`LlmEngine`. A costura posterior da Fase 4 substituiu essa seleção operacional
por `moe/q4_0`; a referência à delegação permanece somente para explicar o
estado histórico e o teste de telemetria que garante que uma delegação nunca
seja rotulada como motor MoE. Os testes de classificação cobrem denso, MoE,
override e metadados ausentes.
Os testes isolados de GGUF e executor passaram em um harness `rustc` com 15
casos. A suíte `cargo test -p node` também passou com o symlink local do
`llama.cpp` materializado durante a validação.

### Fase 4: cache global, loader GGUF e primeiro bloco MoE

O desenho foi reimplementado em Rust a partir do FreeToken fixado em
`bd372b630a028e3faa51f4ab0ef6a98c2f2de501`. A instância `ExpertCache` é
compartilhada pelo servidor e indexa cada entrada por modelo, camada, expert e
formato. Um `ExpertLease` conta referências ativas; eviction só escolhe a
entrada ociosa com menor `last_used`, portanto uma requisição em andamento não
perde seu peso.

`admission::MemoryAccount` não usa uma constante de VRAM. Ele desconta do
baseline livre medido os pesos, buffers fixos e a reserva de ativações, e cruza
esse teto com uma nova leitura de memória livre antes de cada miss. A sonda
local usa `MemAvailable` no Linux, `vm_stat` no macOS e
`GlobalMemoryStatusEx` no Windows. Uma futura sonda de VRAM pode implementar a
mesma trait sem mudar a política.

O primeiro formato é Q4_0 packed em CPU. `format_q4_0.rs` valida a geometria
GGML, mmapa o GGUF, copia somente os bancos do expert solicitado para o cache,
decodifica os blocos de 32 elementos, executa gate-up, SiLU gated e down. O
loader aceita tanto `ffn_gate_up_exps.weight` fundido quanto
`ffn_gate_exps.weight` + `ffn_up_exps.weight` separados, recombinando estes
últimos no payload interno do executor. `router.rs` escolhe top-k de forma
determinística e renormaliza os pesos;
`block.rs` conecta essa seleção ao `Q4_0Executor`, usando uma chave global por
modelo, camada, expert e formato. Nenhuma linha do FreeToken foi copiada.

O harness isolado passou com 28 testes, incluindo parser GGUF sintético,
router F32, forward do bloco, cache hit, eviction global, memória medida,
leases ativos e rejeições de geometria. Isso prova as fronteiras puras do
motor, não um modelo completo. O parser também aceita arrays GGUF grandes,
limitando-os pelo espaço restante do arquivo em vez de um teto arbitrário.
O runtime HTTP agora detecta o dtype dos bancos de experts, abre o decoder
próprio correspondente e calcula os logits do router antes de aplicar o bloco
ao hidden state recebido ou ao residual que será encaminhado. Q4_0 e NVFP4
compartilham a mesma fronteira de cache e dispatch, e Q4_K (12), Q6_K (14),
IQ3_S (21), IQ2_S (22) e IQ4_XS (23) agora também passam por essa seleção. O
decoder K-quant valida a ordem GGUF
`[columns, rows, experts]`, os blocos de 256 elementos e a convenção fundida ou
separada dos bancos gate/up antes de copiar um expert. O frame continua em f16
e não leva pesos.
O decoder NVFP4 aplica a mesma regra de dimensões: no GGUF, uma matriz de peso
é `[input, output, experts]`. Portanto gate/up usam
`[hidden, intermediate, experts]` e down usa
`[intermediate, hidden, experts]`; inverter esses eixos produz um runtime com
hidden errado e recusa no primeiro forward, em vez de truncar o residual.
Ainda delegam ao `LlmEngine` a atenção, as operações que não pertencem ao
bloco costurado e a amostragem final do token; portanto esta entrega não
afirma ser um transformer MoE completo. O captain assumiu a prova ponta a
ponta com um modelo real e fará essa validação fora deste ambiente, sem
alterar o caminho denso.

O bloco K-quant foi validado localmente com fixtures Q4_K e Q6_K, nas duas
convenções de nomes, e com `MoeRuntime::open` seguido de `forward_batch`. A
falha inicial era uma inversão no validator: GGUF declara a dimensão de
colunas em `dims[0]`, enquanto o contrato do mat-vec recebe `rows, columns`.
O validator agora checa as duas dimensões e o eixo de experts antes do forward.
Não há ainda uma prova física de Qwen3.6 na ksia neste worktree.

Os três IQ-quants priorizados usam o mesmo loader e a mesma fronteira de cache.
O executor próprio mantém parser, mmap, router, top-k e mat-vec; a expansão do
bloco usa as primitivas de dequantização GGML linkadas pelo crate nativo, sem
duplicar os codebooks oficiais em Rust. Fixtures locais cobrem IQ3_S, IQ2_S e
IQ4_XS, os dois nomes de gate/up e a execução ponta a ponta do runtime. Um
dtype diferente de 21, 22 ou 23 continua recusado pelo dispatch com ID e nome.

### Fase 4A: F32, F16 e BF16

Os dtypes escalares `F32` (0), `F16` (1) e `BF16` (30) agora têm decoder CPU
próprio em `format_plain.rs`. O loader valida rank 3, ordem lógica
`[columns, rows, experts]`, todas as dimensões, dtype e limites do mmap antes
de copiar um expert para o cache. As convenções fundida
`ffn_gate_up_exps.weight` e separada `ffn_gate_exps.weight` mais
`ffn_up_exps.weight` são aceitas; o down continua exigindo
`ffn_down_exps.weight`. O router escalar pode ser F32, F16 ou BF16 e é
decodificado pelo dtype declarado no próprio tensor.

O bloco próprio aplica o mesmo top-k, cache LRU e contrato de hidden state dos
demais formatos. Um dtype quantizado entregue a esse decoder recusa com o ID e
nome GGML, e o dispatcher geral agora encaminha 0, 1 e 30 para essa família em
vez de tentar Q4_0.

Não há um GGUF MoE F32, F16 ou BF16 disponível neste worktree para prova física.
Os fixtures sintéticos cobrem os três dtypes, as duas convenções de nomes, o
forward com cache e a recusa nominal, portanto estes formatos estão
**implementados, mas não provados em GGUF real**. Nenhum número de desempenho é
afirmado.

### Fase 4B: Q4_1, Q5_0, Q5_1 e Q8_0

Os blocos clássicos `Q4_1` (3), `Q5_0` (6), `Q5_1` (7) e `Q8_0` (8) têm
decoder CPU próprio em `format_legacy_quants.rs`. Os tamanhos dos blocos e as
fórmulas de dequantização seguem a definição ativa em `ggml-common.h` e
`ggml-quants.c`: escalas e mínimos em F16, quinto bit separado para Q5 e
quantes assinados para Q8. O loader valida divisibilidade por blocos de 32,
dimensões `[columns, rows, experts]`, dtype uniforme, offsets e limites do
arquivo. Gate/up fundido e separado compartilham o mesmo caminho de cache e
router, mas nenhum dtype é aceito pelo decoder de outro.

Os testes locais cobrem vetor de referência para cada bloco, forward do expert,
as duas convenções de nomes e o forward do `MoeRuntime`. Não há GGUF MoE
Q4_1/Q5_0/Q5_1/Q8_0 disponível neste worktree, então estes formatos estão
**implementados, mas não provados em GGUF real**. Nenhum número de desempenho é
afirmado.

Não há número de desempenho nesta fase. A validação nativa foi executada com o
symlink local de `llama.cpp` documentado em `build-execucao.md`; esse artefato
não entra no commit e não substitui a prova ponta a ponta com um modelo real.

### Fase 4C: MXFP4, TQ1_0, TQ2_0, Q1_0 e Q2_0

Os formatos de baixa largura `MXFP4` (39), `TQ1_0` (34), `TQ2_0` (35),
`Q1_0` (41) e `Q2_0` (42) têm decoder próprio em `format_low_bits.rs`. Cada
formato declara seu bloco, tamanho packed e fórmula de dequantização. MXFP4
usa expoente E8M0 por bloco; TQ1/TQ2 usam seus codebooks ternários; Q1/Q2
usam os campos de bits GGML correspondentes. O loader valida dtype, rank,
geometria `[columns, rows, experts]`, offsets e limites do mmap antes de
admitir qualquer expert no cache.

O caminho aceita gate/up fundido e separado, calcula o slice por expert sem
confundir bytes de uma linha com bytes de um bloco e liga o executor ao
`MoeRuntime`. Há testes de vetor, fonte mmap, cache, router, convenções de
nomes e forward ponta a ponta para os cinco dtypes. Não há um GGUF MoE real
destes formatos disponível neste worktree; portanto estão **implementados,
mas não provados em GGUF real**. Nenhum número de desempenho é afirmado.

### Fase 4D: K-quants restantes

`format_k_quants.rs` agora cobre também `Q2_K` (10), `Q3_K` (11), `Q5_K`
(13) e `Q8_K` (15), além de `Q4_K` e `Q6_K`. Cada formato usa o tamanho de
bloco e a disposição de campos GGML próprios: escalas e mínimos quantizados
de Q2/Q4/Q5, high bits de Q3/Q5 e o bloco escalar de Q8. O dispatch por dtype,
o cálculo de bytes por linha e a validação de `[columns, rows, experts]` são
compartilhados sem aceitar um bloco com decoder de outro formato.

Os testes cobrem dequantização dos quatro formatos novos, slices por expert
com gate/up fundidos e separados, cache e forward do `MoeRuntime` para todos
os seis K-quants. Não há GGUF MoE real K-quant disponível neste worktree;
estes formatos estão **implementados, mas não provados em GGUF real**. Nenhum
número de desempenho é afirmado.

### Fase 4E: IQ-quants restantes

`format_iq_quants.rs` agora cobre `IQ2_XXS` (16), `IQ2_XS` (17), `IQ3_XXS`
(18), `IQ1_S` (19), `IQ4_NL` (20) e `IQ1_M` (29), além de `IQ3_S`, `IQ2_S` e
`IQ4_XS`. O executor seleciona a estrutura packed e a rotina de
dequantização C correspondente a cada dtype do GGML; nenhum codebook de um
IQ é usado para outro. Parser, mmap, validação de geometria, cache, router e
top-k permanecem no motor Rust próprio.

Os testes cobrem os nove IQ-quants ativos, payload truncado, recusa de dtype
fora da enumeração, gate/up fundidos e separados e forward do `MoeRuntime`.
Não há GGUF MoE real IQ disponível neste worktree; os seis formatos novos estão
**implementados, mas não provados em GGUF real**. Nenhum número de desempenho é
afirmado.

### Ligação e telemetria do hop

`infer_server` mantém o caminho denso em `llama.cpp`. Quando a seleção GGUF é
MoE, `MoeRuntime` é aberto somente para esse caminho e o router local escolhe
os experts do decoder selecionado. O ownership do bloco de fronteira é
explícito: um hidden recebido ou um residual destinado ao próximo hop só é
entregue ao runtime MoE quando o caminho selecionado é MoE; no hop terminal a
captura do residual também chama o runtime antes da amostragem. O executor é
registrado somente depois de o forward retornar com sucesso, portanto uma
seleção MoE sem chamada real continua aparecendo como divergência. O wire
permanece o estado oculto f16 já existente, sem peso na rede. Atenção, logits e
operações restantes continuam no `LlmEngine`, que fornece a fronteira de
cálculo restante ao redor do bloco próprio.

Cada resposta de inferência publica `selected_path`, `executed_engine`, a lista
`executed_engines` e as flags `engine_mismatch`/`mixed_execution`. O primeiro
vem da seleção GGUF; os demais só são gravados depois que o decode llama.cpp
ou o forward MoE retornam. `/health` publica somente `selected_path`, porque
não é uma medição de uma requisição. O gateway agrega os campos por fase de
cada hop, sem deduzi-los do nome do modelo, da arquitetura ou de tempos. A
interface mostra o caminho selecionado e o motor observado; mistura e
divergência ficam visíveis como diagnóstico, nunca são reduzidas a um rótulo
único.

### Diagnóstico do executor CUDA

O primeiro instrumento da Fase 5 publica também `moe_executor`,
`moe_executors` e `moe_forward_ms`. O backend só entra nesses campos depois de
`MoeRuntime::forward_batch` retornar com sucesso. `cuda` significa o executor
CUDA NVFP4; `cpu-reference` significa o decoder de referência; `not-run` é uma
seleção MoE sem forward registrado e `mixed` denuncia mais de um backend no
mesmo hop. Um binário sem a feature `cuda` não transforma NVFP4 em CPU por
acidente: ele recusa a carga com a mensagem nominal do executor, salvo quando
`POM_MOE_NVFP4_BACKEND=cpu` pede explicitamente o fallback de referência.

A investigação da medição Gemma4 encontrou uma divergência anterior a qualquer
otimização de kernel. O contexto captura `l_out-29`, mas em
`vendor/llama-cpp-sys-2/llama.cpp/src/models/gemma4.cpp` esse tensor é criado
depois de `build_moe_ffn`, `ffn_post_norm`, a soma residual e `build_cvec`. O
callback de captura copia o tensor e retorna `true`; portanto a chamada de
`llama_decode` continua pelo grafo. Em seguida, `infer_server::apply_moe_hidden`
chama o `MoeRuntime` depois de `take_residual`. Isso acrescenta uma passada dos
experts à passada completa do llama.cpp, em vez de substituir `build_moe_ffn`.

O contrafactual mínimo foi identificado no próprio callback: retornar `false`
ao encontrar `l_out-29` só abandonaria nós posteriores a esse tensor, mas não
impediria o `build_moe_ffn` que já o produziu. Logo, esse contrafactual não
elimina o trabalho duplicado; a correção estrutural precisa mover a fronteira
para antes do FFN MoE e injetar a saída no restante do bloco. A medição também
mostra uma segunda causa possível, ainda não quantificada isoladamente: o
executor CUDA atual aloca buffers e copia hidden, gate/up e down de host para a
GPU em cada chamada, sincroniza os kernels e copia o resultado de volta. Esse
perfil só deve ser otimizado depois de a telemetria confirmar `moe_executor=cuda`
e separar o custo da transferência do custo do kernel.

O renderer Gemma4 lê `tokenizer.chat_template` do próprio GGUF, valida os
marcadores `<bos>`, `<|turn>`, `<turn|>`, `<|think|>`, `<|channel>` e
`<channel|>`, e reproduz o controle `enable_thinking` e
`add_generation_prompt`. Não há fallback Qwen nem sistema inventado quando o
template não pede um bloco system. Um template Gemma4 que não contenha os
marcadores esperados é recusado nominalmente. A geração termina por EOS ou pela
sequência de fim de turno tokenizada pelo mesmo vocabulário; o marcador de
controle não é enviado ao cliente. O teste ignorado
`gemma4_hello_world_request_uses_moe_and_stops_on_its_turn` exercita a rota
`/api/generate` real quando `POM_FTK_E2E_URL` aponta para um gateway Gemma4.

O Gemma4 real inspecionado na ksia usa os nomes separados, com dimensões
`[2816, 704, 128]` para gate/up, `[704, 2816, 128]` para down,
`dtype=40` (NVFP4), 30 camadas, 128 experts e top-k 8. A mesma requisição
`Escreva hello world 10 vezes.` foi validada na rota real: HTTP 200, 55 tokens
gerados, `input_tokens=22`, `complete=true`, 6,42 tok/s, e prefill/decode com
`executed_engine=moe`, sem divergência ou mistura. Os testes puros do executor,
do parser de nomes separados e da telemetria continuam cobrindo as fronteiras
locais; a validação física confirma a costura NVFP4 no modelo real.

### Goal retomado: executor CUDA e co-execução CPU/GPU

Este é o novo goal formal da Fase 5, decidido pelo captain depois da medição
na ksia com Gemma4 NVFP4 na mesma GPU. O caminho CPU atual mede cerca de
`54 ms/token` no prefill e `80 ms/token` no decode; a referência FreeToken mede
`3,1 ms/token` e `18,8 ms/token`, respectivamente. A diferença observada é de
aproximadamente `17,4x` no prefill e `4,3x` no decode. Mais de 90% do tempo do
nosso caminho fica fora de `llama_decode`, no decodificador de referência em
CPU. O fato de o FreeToken manter experts em CPU/RAM elimina offload como
explicação suficiente: o gargalo é o cálculo do expert.

Há uma segunda referência, medida no CHAT real da ksia sem a amarra de batch 8:
o `llama.cpp` faz `292` tokens de entrada em `983 ms`, cerca de `3,4 ms/token`
de prefill, e estabiliza em cerca de `30 ms/token` no p50 de decode. Portanto,
o prefill de `3,1 ms/token` é simultaneamente o alvo FreeToken e a paridade
prática com `llama.cpp`; no decode, `30 ms/token` é um marco intermediário de
paridade com o motor denso antes do alvo FreeToken de `18,8 ms/token`.

O objetivo é reimplementar a abordagem de cálculo do FreeToken na plataforma
Rust, com kernels CUDA próprios e chamadas feitas pelo host Rust. Python,
Triton e o FreeToken como motor externo estão fora do escopo. O FreeToken é
somente fonte de desenho e algoritmo; nenhum código é copiado. O caminho denso
continua em `llama.cpp` sem alteração de seleção ou de comportamento.

O executor GPU reutilizará o `ExpertCache` LRU global existente. A entrada
canônica continua residente em CPU/RAM e com lease por requisição; os buffers
CUDA são staging temporário por camada e expert, com capacidade determinada
pelo orçamento medido, sem transformar a rede ou o cache em transporte de
pesos. O router e a política de admissão continuam no Rust. Cada forward
publicará `gpu_expert_calls`, `cpu_expert_calls`, bytes e banda H2D/D2H,
`buffer_index`, tempo de sobreposição e o motivo de qualquer fallback.

### Objetivo formal: substituir execução e carga dos experts

Este objetivo é uma extensão formal da Fase 5. O motor MoE deve substituir o
bloco de experts do `llama.cpp` sem manter uma segunda residência dos mesmos
pesos. A ordem é deliberada: primeiro provar e medir a supressão da execução
duplicada; somente depois suprimir a carga dos pesos. Assim, a variação de cada
medição atribui-se a uma mudança isolada.

#### F5-E1: substituição genérica da execução

O ponto de extensão vive no `build_moe_ffn` comum do fork. Quando o callback
opt-in está instalado, ele recebe os ids e pesos de roteamento já resolvidos,
quantidade total e ativa de experts, arquitetura, ativação, convenção de
gate/up fundida ou separada, nomes, geometrias, dtypes e escalas GGML. O
executor Rust escolhe o decoder pelo dtype recebido como dado. Não há ramo de
substituição por arquitetura ou `dtype == NVFP4`.

O callback retorna diretamente a saída do bloco e impede que os matmuls nativos
de experts entrem no grafo. Atenção, normalizações, cache KV, embeddings,
tokenização e amostragem continuam no `llama.cpp`. Callback ausente preserva o
grafo nativo, inclusive para modelos densos. Arquitetura, geometria, ativação
ou dtype que o executor não consiga atender devem produzir recusa nominal no
limite de decode, nunca executar o bloco nativo como fallback silencioso.

O ponto comum normaliza os tensores de ids e pesos da rota antes de remodelá-los:
se já são contíguos, o caminho não copia; se top-k ou gather produziu uma view
não contígua, o grafo cria uma cópia `ggml_cont` inevitável antes do reshape.
`llama.cpp/tests/test-moe-route-layout.cpp` cobre as duas disposições sem
assumir arquitetura ou dtype.

Pronto quando a telemetria de uma requisição MoE mostrar o callback executado,
`executed_engine=MoE`, sem divergência, e a medição isolada comparar prefill e
decode contra F5.0. O grafo denso deve continuar sem callback e sem regressão.

#### F5-E2: supressão opt-in da carga dos pesos

Quando e somente quando F5-E1 estiver selecionada, o carregador do fork recebe
uma opção explícita para não alocar os tensores de experts. Ele preserva no
objeto do modelo apenas a metadata necessária para construir o descriptor do
hook: nomes, rank e geometrias, dtype, quantidade total de experts e quantidade
ativa, além das escalas e da indicação de gate/up fundido ou separado. Os bytes
dos pesos continuam acessíveis somente à fonte GGUF do executor Rust e ao seu
cache LRU, não a buffers do `llama.cpp`.

A implementação local já propaga `EngineConfig::moe_external_executor` até
`llama_model_params`, separa essa opção na identidade do `ModelCache` e cria os
experts roteados do descriptor em um contexto GGML `no_alloc`. Eles não entram
em `ctxs_bufs`, não recebem buffer CUDA/CPU e não passam por
`load_all_data`. O contexto de metadata fica vivo em `llama_model` para que o
hook continue lendo nome, dtype e geometria. A classificação é pelos enums
semânticos genéricos `LLM_TENSOR_*EXP`, nunca por arquitetura, nome de modelo
ou dtype. Experts compartilhados (`LLM_TENSOR_*SHEXP`) permanecem no
`llama.cpp` porque são um cálculo nativo separado e ainda não fazem parte do
descriptor do callback; eles não são descartados silenciosamente nem contados
como propriedade do cache Rust.

O documento do fork em `llama.cpp/docs/rdma-p2p-moe-hook.md` registra o ponto de
carga, o contexto de metadata e o comportamento padrão. A ausência de callback
no modo opt-in recusa no limite de decode, portanto não reativa os experts
nativos por fallback silencioso. A compilação local sem CUDA passou; a
validação obrigatória com `--features vulkan,cuda` será feita no ramo
`fm/ftk-cuda-check` da ksia depois que a implantação corrente liberar a placa,
antes de promover esta fase a `feat/ftk`.

O opt-in deve ser genérico por arquitetura e dtype. Sem ele, ou sem o motor
MoE selecionado, o carregador mantém o comportamento atual byte a byte. Um
modelo denso nunca pode ativar a opção. O fork terá documento próprio com o
ponto de carga alterado, a metadata preservada e o comportamento padrão.

Pronto somente com medição na ksia usando o mesmo modelo e configuração: VRAM
e RAM antes e depois, com quebra por buffer (`CUDA0`, `CUDA_Host` e CPU), RSS e
PSS do processo, mais prefill e decode por token. A prova deve mostrar que os
buffers de experts do `llama.cpp` desaparecem e que não existe residência dupla
com o cache do executor. Nenhum ganho será atribuído à supressão de carga sem
comparar esses números isoladamente aos números obtidos após F5-E1.

#### F5-E3: regressão e publicação

Cada fase produz commit separado no `rdma-p2p` e no fork. O teste de execução
deve cobrir callback opt-in, callback ausente e recusa nominal. O teste de
carga deve cobrir metadata preservada, ausência de tensor de dados e caminho
denso intacto. A suíte densa continua obrigatória; a prova de desempenho e de
residência exige o modelo real na ksia e não pode ser substituída por fixture.

### Fases do goal CUDA

1. **F5.0, perfil de referência reproduzível.** Reexecutar na ksia o mesmo
   Gemma4, GPU, prompt e configuração que produziram os números do captain,
   separando router, espera do LRU, dequantização, H2D, kernel, D2H e o
   restante de `llama_decode`. O alvo desta fase é reproduzir `54 ms/token` de
   prefill e `80 ms/token` de decode dentro de uma faixa de `+/-10%`, com a
   fração fora de `llama_decode` ainda acima de 90%. Sem essa concordância o
   número novo não é comparável.

2. **F5.1, executor CUDA mínimo para prefill.** Implementar em CUDA o
   mat-vec dos experts e a soma gate/up/down, com dispatch, leases e cópia dos
   slices controlados pelo Rust. O primeiro caminho cobre o Gemma4 NVFP4 já
   provado e mantém o decoder CPU somente como fallback explícito. Pronto
   quando 100% dos forwards de expert do prefill da carga de referência
   publicarem `gpu_expert_calls`, `cpu_expert_calls=0`, a saída passar a
   comparação numérica com o decoder CPU e o prefill atingir `<= 5,4
   ms/token`, uma redução de pelo menos uma ordem de grandeza sobre os `54
   ms/token` atuais. O alvo final de `3,1 ms/token` continua sendo medido na
   F5.3; o marco de `5,4` não pode ser substituído por apenas não regredir.

3. **F5.2, perfil de banda e double buffer reais.** Medir H2D e D2H na mesma
   GPU, para os tamanhos de tokens e experts da carga, persistir o perfil
   efetivo e selecionar a política CPU/GPU pelo tempo medido. Substituir o
   andaime de testes por dois buffers CUDA realmente alternados, de modo que
   enquanto um buffer é calculado o outro possa ser transferido. Pronto quando
   o runtime carregar o perfil medido, registrar os dois índices em uso e, em
   100 iterações aquecidas por ponto de carga, o tempo combinado satisfizer
   `T_total <= 1,10 * max(T_compute, T_transfer)`. O alvo de segurança é não
   regredir o prefill medido na F5.1.

4. **F5.3, prefill no alvo.** Otimizar agrupamento de experts, fusão de
   operações e reutilização do LRU/staging guiado pelo perfil da F5.2. Pronto
   somente quando a média de prefill, com a mesma definição da referência,
   for `<= 3,1 ms/token` na ksia, com erro numérico dentro da tolerância da
   F5.1, `engine_mismatch=false`, sem fallback CPU e decode ainda `<= 80
   ms/token` durante a medição.

5. **F5.4, paridade de decode com llama.cpp.** Aplicar a mesma lane ao decode
   de um token, primeiro sem exigir o alvo FreeToken. Pronto quando o p50 de
   decode atingir `<= 30 ms/token` na mesma carga e GPU, empatando o
   `llama.cpp` medido no CHAT real, enquanto o prefill permanecer `<= 3,1
   ms/token`. A telemetria deve mostrar `executed_engine=MoE`, sem mistura ou
   divergência; um fallback deve falhar nominalmente ou aparecer com seu
   motivo, nunca ser contado como GPU.

6. **F5.5, alvo FreeToken e co-execução completa.** Sobrepor roteamento,
   transferência e cálculo entre requests, preservando leases do LRU. Pronto
   quando o decode atingir `<= 18,8 ms/token` na mesma carga e GPU, o prefill
   permanecer `<= 3,1 ms/token`, e a telemetria de cada hop mostrar
   `executed_engine=MoE`, sem mistura ou divergência.

7. **F5.6, prova final e regressão.** Repetir os pontos F5.0 a F5.5 em pelo
   menos três execuções independentes na ksia, anexar mediana e dispersão dos
   tempos, e rodar a suíte densa completa. O goal só fecha se os limites
   `3,1 ms/token` e `18,8 ms/token` forem atingidos na carga de referência,
   sem regressão funcional ou de desempenho observada no caminho denso. Caso
   uma fase não alcance seu número, registrar o valor real e a evidência, sem
   declarar verde por suite unitária.

Esta retomada substitui o estado anterior de “Fase 5 ainda não ativa”. Antes
de qualquer kernel, o desenho e esses alvos devem ser publicados no status do
goal. A F5.1 já tem a primeira entrega de código: `cuda.rs` faz o dispatch
Rust e `cuda_kernels.cu` executa gate/up/down NVFP4 em CUDA, compilados pelo
`build.rs` somente com a feature `cuda`. `POM_MOE_NVFP4_BACKEND=cpu` é o
fallback explícito para comparação; uma falha do kernel CUDA não cai em CPU
silenciosamente. O build CUDA foi validado na ksia, mas a comparação numérica
do kernel e a medição de prefill ainda aguardam uma janela sem disputa de GPU.
Até essa prova, nenhuma melhoria de desempenho é afirmada.

## Goal: executor MoE para todos os dtypes GGUF

Este é um goal formal adicional ao caminho `feat/ftk`: o runtime próprio deve
executar experts MoE em todos os dtypes GGUF ativos suportados pelo
`ggml_type` do `llama.cpp`, e não somente em Q4_0. A fonte da enumeração é
`llama.cpp/ggml/include/ggml.h`, especialmente `GGML_TYPE_COUNT`; os IDs
removidos de arquivos GGUF não são tratados como formatos válidos. O escopo
preserva a bifurcação: modelos densos continuam no `llama.cpp`, e nenhum peso
ou bloco de quantização atravessa a rede por causa do fatiamento.

O contrato de segurança é comum a todas as fases: o cabeçalho GGUF é validado
antes de qualquer decodificação, o dispatch usa o ID e a geometria do tensor,
e uma combinação não implementada recusa com `dtype=<id> (<nome>)`. Um dtype
desconhecido também é recusado com seu ID bruto. É proibido interpretar bytes
de um formato como se fossem outro. Ao concluir o goal, essa recusa será o
caminho raro para um dtype realmente novo ou um tensor inválido, não a saída
normal para modelos conhecidos.

### Fases e critérios de pronto

1. **Inventário e conformance.** Mapear todos os IDs ativos da enumeração,
   nomeá-los no loader, levantar os dtypes dos experts em um corpus real e
   adicionar fixtures de cabeçalho para formatos suportados e recusados. Pronto
   quando cada ID chega a uma decisão explícita, cada recusa nomeia o dtype e o
   parser não lê um payload sem validar limites, rank, geometria e offset.

2. **NVFP4, dtype 40.** Implementar primeiro o decoder CPU de referência para
   o layout NVFP4 usado pelo Gemma4, incluindo escalas E4M3, blocos, gate/up
   fundido ou separado e down. Pronto quando fixtures sintéticas cobrem cada
   escala e sinal, o resultado é comparado operação a operação com uma
   referência independente e um GGUF real NVFP4 completa a abertura e o
   forward do expert sem cair no loader Q4_0.

3. **Camada genérica e compatibilidade Q4_0.** Extrair `ExpertDecoder` e um
   dispatch por dtype, mantendo a chave do cache, admissão por memória e
   formatos de nomes fora do decoder. Migrar Q4_0 e NVFP4 para essa camada,
   com testes de contrato idênticos por formato. Pronto quando o caminho denso
   não importar o runtime MoE, Q4_0 continuar passando e nenhum branch puder
   escolher um decoder por nome de arquivo.

4. **Cobertura por impacto, sem recorte de famílias.** Depois dos blocos já
   publicados NVFP4, K-quants `Q4_K` (12) e `Q6_K` (14), e IQ-quants `IQ3_S`
   (21), `IQ2_S` (22) e `IQ4_XS` (23), a fila restante segue esta ordem:
   primeiro os escalares `F32` (0), `F16` (1) e `BF16` (30), depois os blocos
   clássicos `Q4_1` (3), `Q5_0` (6), `Q5_1` (7) e `Q8_0` (8), e então
   `MXFP4` (39), `TQ1_0` (34), `TQ2_0` (35), `Q1_0` (41) e `Q2_0` (42).
   A próxima fila cobre os demais tipos, sem abandonar nenhum: numéricos
   `F64`, `I8`, `I16`, `I32` e `I64`; K-quants `Q2_K`, `Q3_K`, `Q5_K` e
   `Q8_K`; e IQ-quants `IQ1_M`, `IQ1_S`, `IQ2_XS`, `IQ2_XXS`, `IQ3_XXS` e
   `IQ4_NL`. A ordem dos dois primeiros blocos foi definida pelos modelos
   reais disponíveis na ksia. Nenhum MoE Q4_0 existe na ksia, então Q4_0
   permanece coberto por testes locais, sem ser fila de validação física.
   Cada subgrupo fica pronto com decoder, fixture de payload, rejeições de
   geometria e teste diferencial antes do próximo.

5. **Prova real e encerramento.** Executar um modelo MoE real para cada dtype
   presente no corpus, medir carregamento, cache, forward e tok/s, e comparar
   com a referência do `llama.cpp` ou uma referência numérica independente.
   Pronto quando todos os dtypes ativos da enumeração têm teste de formato e
   pelo menos uma prova de execução; apenas IDs removidos, desconhecidos ou
   GGUF inválidos podem terminar em recusa nominal. A suíte densa completa
   continua obrigatória em cada fase.

### Inventário GGUF atual

Os dtypes ativos são: `0 F32`, `1 F16`, `2 Q4_0`, `3 Q4_1`, `6 Q5_0`,
`7 Q5_1`, `8 Q8_0`, `9 Q8_1`, `10 Q2_K`, `11 Q3_K`, `12 Q4_K`, `13 Q5_K`,
`14 Q6_K`, `15 Q8_K`, `16 IQ2_XXS`, `17 IQ2_XS`, `18 IQ3_XXS`, `19 IQ1_S`,
`20 IQ4_NL`, `21 IQ3_S`, `22 IQ2_S`, `23 IQ4_XS`, `24 I8`, `25 I16`, `26 I32`,
`27 I64`, `28 F64`, `29 IQ1_M`, `30 BF16`, `34 TQ1_0`, `35 TQ2_0`,
`39 MXFP4`, `40 NVFP4`, `41 Q1_0` e `42 Q2_0`.

Os IDs `4 Q4_2`, `5 Q4_3`, `31 Q4_0_4_4`, `32 Q4_0_4_8`, `33 Q4_0_8_8`,
`36 IQ4_NL_4_4`, `37 IQ4_NL_4_8` e `38 IQ4_NL_8_8` permanecem na faixa
histórica da enumeração, mas o `llama.cpp` marca-os como removidos de arquivos
GGUF. O loader deve rejeitá-los como dtype removido, com o ID e o nome na
mensagem, em vez de tratá-los como um dos formatos ativos.

### Quadro final de cobertura dos dtypes ativos

Este quadro é o fechamento honesto desta rodada, conferido contra
`GGML_TYPE_COUNT` e a enumeração de `llama.cpp/ggml/include/ggml.h`. Cada linha
é individual para não transformar uma fixture em prova de modelo. A única prova
física registrada é o Gemma4 NVFP4; não há arquivo GGUF local para repetir uma
prova E2E dos K-quants ou IQ-quants da ksia neste worktree, e nenhum acesso
remoto foi feito nesta validação.

| ID | dtype | estado |
|---:|---|---|
| 0 | F32 | implementado e NÃO PROVADO |
| 1 | F16 | implementado e NÃO PROVADO |
| 2 | Q4_0 | implementado e NÃO PROVADO |
| 3 | Q4_1 | implementado e NÃO PROVADO |
| 6 | Q5_0 | implementado e NÃO PROVADO |
| 7 | Q5_1 | implementado e NÃO PROVADO |
| 8 | Q8_0 | implementado e NÃO PROVADO |
| 9 | Q8_1 | não coberto |
| 10 | Q2_K | implementado e NÃO PROVADO |
| 11 | Q3_K | implementado e NÃO PROVADO |
| 12 | Q4_K | implementado e NÃO PROVADO |
| 13 | Q5_K | implementado e NÃO PROVADO |
| 14 | Q6_K | implementado e NÃO PROVADO |
| 15 | Q8_K | implementado e NÃO PROVADO |
| 16 | IQ2_XXS | implementado e NÃO PROVADO |
| 17 | IQ2_XS | implementado e NÃO PROVADO |
| 18 | IQ3_XXS | implementado e NÃO PROVADO |
| 19 | IQ1_S | implementado e NÃO PROVADO |
| 20 | IQ4_NL | implementado e NÃO PROVADO |
| 21 | IQ3_S | implementado e NÃO PROVADO |
| 22 | IQ2_S | implementado e NÃO PROVADO |
| 23 | IQ4_XS | implementado e NÃO PROVADO |
| 24 | I8 | não coberto |
| 25 | I16 | não coberto |
| 26 | I32 | não coberto |
| 27 | I64 | não coberto |
| 28 | F64 | não coberto |
| 29 | IQ1_M | implementado e NÃO PROVADO |
| 30 | BF16 | implementado e NÃO PROVADO |
| 34 | TQ1_0 | implementado e NÃO PROVADO |
| 35 | TQ2_0 | implementado e NÃO PROVADO |
| 39 | MXFP4 | implementado e NÃO PROVADO |
| 40 | NVFP4 | implementado e PROVADO com arquivo real |
| 41 | Q1_0 | implementado e NÃO PROVADO |
| 42 | Q2_0 | implementado e NÃO PROVADO |

O dispatcher continua recusando tanto um dtype ativo sem decoder quanto um ID
fora da enumeração antes de ler o payload. `Q4LoadError::UnsupportedTensorType`
renderiza `dtype=<id> (<nome>)`; para um ID desconhecido, o nome é
`desconhecido`. Os testes `dtype_desconhecido_informa_id_e_nome` e
`dtype_ativo_sem_decoder_informa_id_e_nome` cobrem, respectivamente, os casos
999 e Q8_1. Portanto a lacuna é observável e nomeada, nunca uma interpretação
silenciosa como outro formato.

O quadro deixa seis dtypes ativos sem decoder próprio: Q8_1, I8, I16, I32, I64
e F64. Eles permanecem trabalho futuro explícito; o fechamento não os declara
implementados. Os 29 dtypes restantes têm decoder e testes locais, mas somente
o ID 40 tem prova E2E com arquivo GGUF real.

### Experts com dtypes heterogêneos

O inventário antigo exigia um dtype único por modelo e recusava os GGUF UD que
misturam IQ3_S, IQ2_S e IQ4_XS. Essa premissa foi removida para a fase F5-E1.
`detect_expert_format` agora devolve `Heterogeneous` quando os tensores de
expert têm mais de um dtype; `format_heterogeneous.rs` constrói um inventário
por camada e papel (`gate_up`, `gate`, `up`, `down`) e o `MoeRuntime` escolhe o
decoder de cada tensor individualmente. A rota, a ativação e a normalização
continuam vindo do descriptor do hook comum do llama.cpp. Não há ramo por
arquitetura, família de quantização ou nome de modelo.

O caminho heterogêneo aceita, por tensor, os dtypes com decoders já existentes:
F32, F16, Q4_0, Q4_1, Q5_0, Q5_1, Q8_0, Q2_K, Q3_K, Q4_K, Q5_K, Q6_K, Q8_K,
IQ2_XXS, IQ2_XS, IQ3_XXS, IQ1_S, IQ4_NL, IQ3_S, IQ2_S, IQ4_XS, IQ1_M, BF16,
TQ1_0, TQ2_0, MXFP4, Q1_0 e Q2_0. NVFP4 continua provado no caminho homogêneo;
se aparecer misturado com outro dtype, a combinação é recusada nominalmente
por tensor até que suas escalas sejam incluídas no inventário heterogêneo. Q8_1,
I8, I16, I32, I64 e F64 continuam sem decoder. Qualquer dtype não coberto
produz o nome do tensor, o ID e o nome GGML, sem interpretar os bytes por outro
formato.

O teste ignorado
`qwen_ud_iq3_s_real_inventaria_dtypes_por_tensor` exige
`POM_REAL_MOE_GGUF` e o arquivo real
`Qwen3.6-35B-A3B-UD-IQ3_S.gguf`; ele valida 40 camadas, 256 experts e os
dtypes `{21, 22, 23}`. A geometria observada é `[2048, 512, 256]` em
gate/up e `[512, 2048, 256]` em down. A abertura local sem esse arquivo não é considerada
prova. A prova E2E deve executar a mesma revisão do código no host que contém
o GGUF e mostrar resposta, `executed_engine=moe`, sem divergência e com
`input_tokens > 0`.

## Fase 0: baseline local

### Método reproduzível

Para uma conversa real, cada resposta deve guardar o objeto `trace` retornado
por `/api/generate` ou por `/v1/chat/completions` com `stream_options` de uso.
Para cada turno, registrar:

```text
prefill_fraction = prefill_ms / (prefill_ms + decode_wall_ms)
input_tokens, output_tokens, prefill_ms, total_ms
sum(hop.prefill.processing_ms), sum(hop.decode.processing_ms)
```

O tempo total é medido no cliente ao redor da requisição. O `RequestTrace` já
separa `prefill` e `decode` por salto e não deve ser reconstruído a partir de
um total agregado. A conversa deve ter pelo menos quatro turnos, mantendo o
contexto anterior em cada pedido.

Para throughput, usar o mesmo prompt e `max_new_tokens` em todos os ensaios,
executar 1, 2 e 4 requisições simultâneas, e repetir com `max_sessions` 1, 2 e
4. O A/B de `kv_unified` deve manter todos os outros parâmetros iguais. Para
cada célula registrar tokens de saída, duração de parede, tok/s por requisição,
tok/s agregado, erros e a configuração efetiva reportada por `/health`.

### Ambiente encontrado nesta execução

| Medição | Resultado local | Motivo da lacuna |
|---|---|---|
| Conversa real multi-turno | não executada, `0` requisições | não há modelo GGUF no worktree, `ollama list` não lista modelos e as portas locais 8181/8080 não têm servidor POM |
| Prefill/decode | `n/a` | depende da geração real e do `RequestTrace` produzido por um modelo carregado |
| Tok/s com 1, 2 e 4 requisições | `n/a`, `0` células | não há runtime local nem modelo para carregar |
| A/B `kv_unified=true` | não executado | o parâmetro ainda não está exposto no runtime e a única instância POM observada pertence a outro worktree e mantém conexões com o cluster remoto, que está fora do escopo |

Nenhum acesso ao cluster remoto, SSH ou deploy foi feito. Portanto, não há
número de desempenho para inventar. Por decisão do captain, a Fase 2 seguirá
mesmo sem esse baseline; a medição posterior deve usar o método acima.

As medições de geração continuam fora do alcance deste worktree sem um GGUF e
um servidor local. A infraestrutura nativa, porém, foi validada com o symlink
temporário documentado em `build-execucao.md`; os resultados estão abaixo.

### Validação executada

No worktree com `<worktree>/llama.cpp` apontando para
`/Users/jairpereira/Sites/rdma-p2p/llama.cpp`:

- `cargo build -p node`: código 0, 0 erros e nenhum warning.
- `cargo test -p node`: 761 testes passaram, 5 ignorados.
- `cargo fmt --check`: código 0.
- `cargo clippy -p node`: código 0, 0 erros, 45 warnings restantes, todos fora
  do motor MoE e do gateway refatorado neste goal; não foram silenciados.
- `apps/frontend`: `npm run build` passou, com o aviso do Vite sobre chunk
  maior que 500 kB; `npm test` passou com 44 arquivos e 278 testes.

Os warnings restantes foram preservados no registro de execução; não houve erro
do provider Hugging Face nesta validação e nenhum arquivo desse provider foi
alterado. O symlink foi removido ao fim e não é artefato versionado.

### Reexecução

Com um GGUF local e uma instância iniciada a partir deste worktree, repetir o
ensaio sem cluster remoto:

```bash
POM_HOME=/tmp/pom-ftk-local ./target/release/pom \
  --model /caminho/modelo.gguf --single-peer --gateway-port 8080 \
  --inference-port 8181 --n-ctx 8192 --max-new-tokens 128
```

Depois, enviar a conversa e as células de concorrência para o gateway local e
anexar as tabelas de números abaixo deste baseline. O executor deve registrar
commit, sistema, CPU/GPU, memória, feature de build, tipo de KV, `n_ctx`,
`max_sessions`, `n_batch`, `max_new_tokens`, modelo e temperatura.

## Invariantes de segurança

- Metadados ausentes não viram zero silenciosamente. Um orçamento de KV só pode
  ser calculado quando a geometria necessária estiver presente.
- Um rebuild acontece como nova geração: o servidor anterior para de aceitar
  conexões, drena requisições em voo e só então libera o contexto antigo.
  Reconfiguração compatível mantém os pesos no `ModelCache`; troca de fatia ou
  de carregamento limpa esse cache. Falha de rebuild não pode deixar a nova
  geração anunciada como pronta.
- Cancelamento é cooperativo e idempotente. O fechamento do stream interrompe
  a chamada do gateway em voo e impede o próximo chunk ou hop; um frame que já
  foi aceito pelo peer pode terminar. O cleanup libera a sessão em todos os
  peers alcançados.
- O motor novo nunca é selecionado por nome heurístico ou por falha do
  `llama.cpp`; a arquitetura GGUF precisa declarar MoE e a configuração pode
  forçar o caminho denso enquanto o caminho novo estiver em validação.
- O cache LRU de experts é local à máquina. Ele nunca atravessa o fatiamento e
  não transforma peso em tráfego de rede.

## Defeitos anotados

- As medições de throughput e de divisão prefill/decode ainda não são possíveis
  neste ambiente local sem obter um modelo e iniciar um runtime próprio.
