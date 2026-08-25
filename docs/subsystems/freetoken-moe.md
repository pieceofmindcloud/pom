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
`moe_executors`, `moe_executor_dtypes`, `moe_executor_reason` e
`moe_forward_ms`. O backend só entra nesses campos depois de
`MoeRuntime::forward_batch` retornar com sucesso. `cuda` significa o executor
CUDA NVFP4; `cpu-reference` significa o decoder de referência; nesse caso a
telemetria lista os dtypes GGML concretos e informa que não há ponte CUDA;
`not-run` é uma seleção MoE sem forward registrado e `mixed` denuncia mais de
um backend no mesmo hop. O `PhaseTrace` do gateway preserva os campos por hop
e por fase. Um binário sem a feature `cuda` não transforma NVFP4 em CPU por
acidente: ele recusa a carga com a mensagem nominal do executor, salvo quando
`POM_MOE_NVFP4_BACKEND=cpu` pede explicitamente o executor de referência.

No Qwen3.6-35B-A3B-UD-IQ3_S real, a telemetria agora deve mostrar
`moe_executor=cpu-reference`, `moe_executor_dtypes=[21,22,23]` e o motivo
`IQ3_S`, `IQ2_S` e `IQ4_XS` sem ponte CUDA. Isso é uma escolha explícita do
decoder CPU heterogêneo, não uma falha de lançamento CUDA escondida.

### Fase 5.1: ponte CUDA por dtype

Esta é a próxima fase formal do goal. O executor CUDA atual cobre somente
NVFP4 (40); IQ-quants, K-quants, legacy, low-bit e escalares continuam no
`CPUReference`, com a escolha visível na telemetria. A fase deve adicionar a
ponte por dtype sem assumir homogeneidade em arquivos UD: cada tensor conserva
seu dtype, o decoder CUDA correspondente é escolhido por tensor e um dtype
sem kernel recusa nominalmente com o dtype e o motivo.

Critério de pronto por bloco: prova numérica contra o oráculo GGML para cada
dtype, telemetria por hop/fase mostrando `cuda` sem `moe_executor_reason` de
CPU, prova E2E com GGUF real heterogêneo quando disponível e medição na ksia
de prefill, decode, bytes H2D/D2H, alocações e banda. A comparação deve
separar custo de kernel e transferência; nenhum ganho de desempenho será
afirmado apenas por suite verde.

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

### Linha de base CUDA observada na ksia

Em 2026-08-25, com `rdma-p2p=b8e24aa`, `llama.cpp=03902a5`, a mesma
requisição `Escreva hello world 10 vezes` foi executada no Gemma4 NVFP4, primeiro
com MoE e depois com o caminho denso. O pedido MoE teve `input_tokens=21`,
`output_tokens=30`, `prefill_ms=35655`, `decode.processing_ms=49706` em 29
chamadas e `moe_executor=cuda`, dtype `[40]`, sem divergência. Isso equivale a
`1697,857 ms/token` de prefill e `1714,000 ms/token` por chamada de decode.
O denso teve `input_tokens=21`, `output_tokens=55`, `prefill_ms=337` e
`decode.processing_ms=2414` em 54 chamadas, ou `16,048 ms/token` e
`44,704 ms/token`, respectivamente.

No trace MoE, o tempo do backend e a chamada medida no nó coincidem:

| fase | backend total (`processing_ms`) | componentes backend | `node_decode_ms` |
|---|---:|---:|---:|
| prefill | 35649,000 ms | 35649,057 ms | 35649,066 ms |
| decode | 49706,000 ms | 49697,058 ms | 49697,246 ms |

Isso confirma que não há uma segunda execução escondida, mas não diz ainda qual
parcela interna do executor CUDA domina. A instrumentação diagnóstica separa
alocação, H2D, kernel, lançamento, sincronização, D2H e liberação.

### Perfil interno CUDA observado na ksia

O perfil foi executado em uma instância isolada, sem alterar o serviço nas
portas 8080/8092, com `rdma-p2p=8187aad`, `llama.cpp=03902a5`,
`POM_MOE_CUDA_PROFILE=1` e o GGUF real `Gemma4-26b-NVFP4.gguf`. A requisição
OpenAI tinha `input_tokens=20` e produziu `hello`; o trace confirmou
`executed_engine=moe`, `moe_executor=cuda`, `profiled=true` e
`engine_mismatch=false`.

Os contadores de CUDA abaixo são os tempos acumulados publicados pelo executor.
Na linha decode, subtraí os contadores do fim do prefill dos contadores do fim
do decode: o runtime mantém o diagnóstico por modelo, então a linha representa
os 240 experts selecionados desse único token de decode, não os 5040 calls
acumulados. Percentuais são somente da soma das categorias instrumentadas, não
do tempo total do nó.

| fase | calls | backend/enqueue | H2D | kernel | alloc | launch | sync | D2H | free |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| prefill, 20 tokens | 4800 | 33784,602 ms | 10189,819 ms | 19882,414 ms | 324,379 ms | 59,956 ms | 302,153 ms | 49,750 ms | 269,494 ms |
| decode, delta de 1 token | 240 | 1738,734 ms | 621,394 ms | 967,158 ms | 16,860 ms | 3,174 ms | 14,713 ms | 2,589 ms | 13,685 ms |

A soma das categorias instrumentadas foi `31077,965 ms` no prefill e
`1639,573 ms` no decode. O restante até o backend/enqueue foi,
respectivamente, `2706,637 ms` e `99,161 ms`, portanto também não é atribuído
ao kernel por inferência. Dentro do que foi medido, o kernel responde por
`63,98%` do prefill e `58,99%` do decode; H2D responde por `32,79%` e
`37,90%`. Portanto a transferência é grande, mas não é a maior categoria
medida nesta execução.

### Diagnóstico estrutural do caminho CUDA

O LRU não ficou órfão. `MoeRuntime::open` cria uma instância de
`ExpertCache::measured_system`, compartilha o mesmo `Arc<ExpertCache>` entre
todos os blocos e `Nvfp4Executor::run_with_loader` faz `lookup` antes de
`admit`, inclusive no caminho CUDA. O que o LRU mantém é o payload NVFP4 bruto
em RAM. Ele não mantém buffers CUDA nem um lease de device.

Para o Gemma4 real, a geometria observada no GGUF e no hook é `hidden=2816`,
`intermediate=704`, 30 camadas, 128 experts e top-k 8. O caminho atual faz,
por token de decode:

| operação | contagem |
|---|---:|
| chamadas de expert | 30 x 8 = 240 |
| cópias H2D de `gate_up` | 240 |
| cópias H2D de `down` | 240 |
| bytes H2D só de pesos | 802897920 (765,7 MiB) |
| bytes H2D incluindo hidden | 805601280 (768,3 MiB) |
| lançamentos de kernel | 480 |
| `cudaDeviceSynchronize` | 480 |
| `cudaMalloc` e `cudaFree` | 1200 de cada |
| cópias D2H de saída | 240 |

Cada expert selecionado atravessa PCIe uma vez por tensor de peso em cada
chamada, mesmo quando o `ExpertCache` registra hit. O perfil mediu
`805601280` bytes H2D por token de decode, dos quais `802897920` são pesos:
240 cópias de `gate_up` e 240 de `down`, ou `3.345.408 bytes` de pesos por uso de
expert. No prefill de 20 tokens, foram `4800` chamadas de expert e
`9600` cópias H2D de peso, totalizando `16112025600` bytes H2D. Além disso,
`Nvfp4CudaExecutor::run` repete a validação do cabeçalho do payload em CPU em
cada chamada.

O LRU estava efetivamente ativo: no fim do prefill havia `1279` misses,
`3521` hits e `1279` admissões; no decode houve mais `22` misses e `218` hits,
chegando a `1301` entradas e `4352417440` bytes residentes em RAM. Porém o
delta do decode ainda transferiu os pesos dos 240 usos, inclusive os 218 hits.
Assim, a resposta objetiva à pergunta de PCIe é: um tensor de peso cruza uma
vez por uso selecionado, e não uma vez por miss do LRU. Uma determinada chave
de expert cruza tantas vezes quanto for selecionada no roteamento; neste token,
o agregado foi 480 transferências de peso.

O caminho que produziu esse perfil tinha uma divergência concreta em
`cuda_kernels.cu`: o buffer copiado para device (`d_gate_up`) não era o ponteiro
passado a `gate_up_kernel`; o kernel recebia o ponteiro host `gate_up`. O teste
CUDA diferencial reproduziu a consequência antes da correção com tamanhos
impossíveis (`262144 != 262128`), consistente com corrupção causada pela
fronteira host/device. A correção passa `d_gate_up` ao kernel, sem mudar o
formato do payload ou o cálculo. O perfil de custo precisa ser repetido depois
desta correção antes de atribuir os `19882,414 ms` de kernel do prefill ou os
`967,158 ms` do decode ao cálculo NVFP4 em si.

O commit de validação `22f6c23` também alinhou o cálculo CUDA à convenção
numérica do ggml. Cada entrada de matmul é quantizada em blocos Q8_0 de 32
valores, com escala arredondada para FP16; o dot NVFP4 acumula os produtos
inteiros por subbloco e aplica as duas escalas depois da soma. A ativação GeGLU
usa o mesmo corte e o mesmo arredondamento FP16 do caminho ggml antes de ser
quantizada para o `down`. Isso é uma convenção do pipeline ggml, não uma regra
de Gemma4 ou de NVFP4: dimensões e payload continuam vindo da metadata do
tensor, e um formato CUDA que não possua esse contrato deve recusar o dtype.

A validação real no GGUF Gemma4 (`hidden=2816`, `intermediate=704`) passou
contra o oráculo ggml em todas as camadas e estágios. A ponte GPU passou ainda
nos testes diferenciais CUDA com entrada uniforme e com entrada na região
GELU, ambos comparando o mesmo payload e hidden entre Rust CPU e CUDA. A build
`cargo build --release --features vulkan,cuda` passou na ksia em 9m01s; o teste
CUDA filtrado passou em `2 passed, 0 failed` e a prova real passou em `1 passed,
0 failed`. Os dois avisos restantes são `Nvfp4Trace` não consumido fora do
diagnóstico e o helper `fp4_value`, já conhecidos. A prova real GPU com o GGUF
deve ser repetida antes de qualquer otimização agrupada, porque o teste CUDA
unitário não substitui a geometria real do modelo.

### Diagnóstico do kernel CUDA antes do agrupamento

O kernel atual usa paralelismo por saída: `gate_up_kernel` lança uma thread por
linha de gate/up e `down_kernel` uma thread por linha de saída, com blocos de
128 threads. Cada thread percorre todos os blocos e subblocos da sua linha. Há
paralelismo entre linhas, mas não existe cooperação entre threads para uma
matriz, tile em memória compartilhada, ou lote de experts. A leitura não foi
projetada como uma carga tiled coalescida: threads adjacentes percorrem linhas
distintas e repetem os mesmos bytes quantizados para cada saída. O grau real de
coalescência ainda precisa de Nsight Compute; o código atual não permite
afirmar coalescência eficiente.

A descompactação E2M1 e UE4M3 ocorre dentro de `dot_nvfp4` a cada uso do
expert. A quantização Q8_0 da ativação também é refeita por chamada, e o
GeGLU é recalculado antes do `down`. Portanto um hit do LRU não deixa os pesos
nem as ativações descompactados na GPU. O perfil anterior, feito antes do
alinhamento numérico, mediu `480` lançamentos por token de decode; esses
números não devem ser usados como ganho de uma implementação correta até o
perfil ser repetido. A próxima etapa aprovada é agrupar o trabalho preservando
primeiro a prova numérica; F5-E2 e a residência em VRAM vêm depois.

### Primeira validação do agrupamento CUDA

O commit `379ce6a` agrupou os experts ativos de uma rota em uma chamada CUDA:
o hidden é quantizado uma vez, os payloads são copiados em um bloco contíguo e
gate/up, GeGLU e down usam índices `(expert, linha)`. A chamada agrupada faz
cinco lançamentos de kernel e duas sincronizações para o lote inteiro, em vez
de repetir a sequência por expert. A soma ponderada continua no Rust, na ordem
original do roteamento. O agrupamento é genérico por contagem e geometria do
lote; não há ramo por arquitetura ou dtype.

O primeiro teste CUDA revelou um defeito específico da nova indexação: a
quantização Q8_0 comparava o índice global do lote com o comprimento de um
único vetor, deixando os vetores a partir do segundo sem valores. `379ce6a`
separa o deslocamento dentro do vetor do índice global. O caso de dois experts
falhava antes e passou depois da correção, sem mudar a fórmula de quantização.

Na revisão final `fb8da96`, validada na ksia, os resultados foram:

- `cargo build --release --features vulkan,cuda`: passou em `3m22s`, com os
  dois avisos conhecidos de `Nvfp4Trace` e `fp4_value` não consumidos;
- os três testes CUDA sintéticos, incluindo o lote, `3 passed, 0 failed`;
- lote CUDA com dois experts extraídos do GGUF real Gemma4, dimensões derivadas
  do arquivo: `1 passed, 0 failed`;
- comparação real camada a camada contra o oráculo ggml do mesmo Gemma4:
  `1 passed, 0 failed`.

O teste real do lote compara os mesmos bytes e hidden entre CUDA e o decoder
CPU, enquanto a prova do oráculo compara o decoder CPU com o llama.cpp. Essa
separação evita que o teste CUDA valide o próprio oráculo. A medição E2E
agrupada está registrada abaixo. O cache continua em RAM, sem residência de
experts em VRAM; F5-E2 permanece posterior ao agrupamento.

Conclusão desta medição: o LRU está ativo no caminho do hook, mas é somente
cache de RAM. O caminho CUDA repete transferência de pesos para cada expert
selecionado. A correção do ponteiro `gate_up` é uma etapa de correção numérica,
não uma otimização; depois dela, a telemetria deve ser coletada novamente antes
de comparar o custo do kernel. Os números acima continuam sendo a linha de
base pré-correção; nenhuma otimização foi feita nesta etapa.

### Medição E2E do agrupamento CUDA

Em 2026-08-25, a medição foi repetida na ksia com `rdma-p2p=340efc8`,
`llama.cpp=03902a5`, RTX 5060 Ti e o GGUF real `Gemma4-26b-NVFP4.gguf`.
Para obter a quebra interna sem reiniciar o nó servível, usei uma instância
isolada do mesmo binário, no mesmo GPU e em portas separadas, com
`POM_MOE_CUDA_PROFILE=1`. O serviço principal permaneceu ativo com o Gemma4 em
modo MoE. A requisição foi `Escreva hello world 10 vezes.`, com
`max_tokens=64`; ela produziu 30 tokens em 10 linhas coerentes e tinha
`input_tokens=22`.

No MoE agrupado, o trace mostrou `executed_engine=moe`,
`moe_executor=cuda`, `engine_mismatch=false`, `mixed_execution=false`,
`prefill_ms=6962`, `decode.processing_ms=5396` em 29 chamadas e duração total
de `12525 ms`. Portanto, prefill foi `316,455 ms/token` e decode foi
`186,069 ms/token`.

| fase | backend total | chamada de decode no nó | H2D | kernel CUDA | chamadas de experts |
|---|---:|---:|---:|---:|---:|
| prefill, 22 tokens | `6957 ms` | `6956,219 ms` | `1758,498 ms` (`79,932 ms/token`) | `157,755 ms` (`7,171 ms/token`) | `5280` |
| decode, 29 tokens | `5396 ms` | `5390,302 ms` | `1892,275 ms` (`65,251 ms/token`) | `234,272 ms` (`8,078 ms/token`) | `6960` |

Na linha decode, H2D e kernel são o delta dos contadores acumulados ao fim do
prefill. Os `6960` experts são `29 x 240`, e o H2D foi
`23294062080` bytes no total do delta, ou `803243520 bytes/token`
(`766,033 MiB/token`). O agrupamento reduziu os lançamentos para cinco por
lote, mas ainda copia os payloads selecionados para a GPU.

O par denso, na mesma instância e com a mesma requisição, produziu o mesmo
texto com `executed_engine=llama.cpp`, `engine_mismatch=false`,
`prefill_ms=398`, `decode.processing_ms=1402` em 29 chamadas e duração total
de `1970 ms`: `18,091 ms/token` de prefill e `48,345 ms/token` de decode.
H2D/kernel de experts não se aplica ao caminho denso, que não executa o
executor MoE.

Comparado à linha de base pré-agrupamento de `1689,3 ms/token` no prefill,
`1738,7 ms/token` no decode, `621,4 ms/token` de H2D e `967,2 ms/token` de
kernel no decode, o agrupamento medido foi uma melhoria isolada: cerca de
`5,34x` no prefill, `9,34x` no decode, `9,52x` no H2D e `119,73x` no kernel.
O nó principal terminou servível em `8080/8092`, com Gemma4 NVFP4 ativo em
modo MoE; a instância isolada foi encerrada após a medição.

### Decomposição fechada do tempo após o agrupamento

Para separar submedições CUDA que podem se sobrepor, repeti a mesma requisição
em uma instância isolada do ramo de validação `0d6d70f`, compilado na ksia com
`--features vulkan,cuda`. O serviço principal não foi reiniciado. A medição
usou `POM_MOE_CUDA_PROFILE=1` e acrescentou duas paredes não sobrepostas:
`runtime_forward_ms` para o forward MoE inteiro e `cuda_call_wall_ms` para a
chamada FFI CUDA inteira. O texto foi novamente dez linhas de `hello world`,
com 30 tokens de saída e 22 tokens de entrada.

| fase | backend total | forward MoE | restante llama.cpp | total por token |
|---|---:|---:|---:|---:|
| prefill, 22 tokens | `8039,369 ms` | `7802,851 ms` (`97,058%`) | `236,518 ms` (`2,942%`) | `365,426 ms/token` |
| decode, 29 chamadas | `5979,008 ms` | `5481,247 ms` (`91,675%`) | `497,761 ms` (`8,325%`) | `206,173 ms/token` |

O restante é a diferença medida entre a parede do backend e o forward MoE,
não uma estimativa de custo. Dentro do forward MoE, a partição também fecha
exatamente:

| componente não sobreposto | prefill | decode, delta de 29 chamadas |
|---|---:|---:|
| lookup do cache | `1,263 ms` | `3,784 ms` |
| leitura do expert no GGUF | `501,936 ms` | `92,603 ms` |
| admissão no cache RAM | `696,284 ms` | `57,419 ms` |
| preparação do lote no host | `3622,166 ms` | `2166,206 ms` |
| parede da chamada CUDA | `2836,915 ms` | `3086,384 ms` |
| cópia do resultado no host | `6,197 ms` | `7,706 ms` |
| agregação ponderada | `2,229 ms` | `2,258 ms` |
| restante de orquestração do runtime | `135,860 ms` | `64,887 ms` |
| **total do forward MoE** | **`7802,851 ms`** | **`5481,247 ms`** |

Assim, o maior componente isolado do prefill é a preparação do lote no host
(`45,055%` do backend), seguida da parede CUDA (`35,288%`). No decode, a
parede CUDA é o maior componente (`51,620%`), seguida da preparação do lote
(`36,230%`). A leitura do expert e a admissão deixam de dominar no decode,
porque o LRU está ativo: o prefill teve `1448` misses e `3832` hits; o delta
do decode teve `177` misses e `6783` hits. Mesmo assim, cada token de decode
mediu `803243520` bytes (`766,033 MiB`) de H2D, pois o cache é de RAM e não
residente em VRAM.

As submedições internas da parede CUDA, que não devem ser somadas à tabela
acima, foram no prefill: H2D `2246,346 ms` para `17671357440` bytes, kernel
`378,780 ms`, alocação `95,688 ms`, lançamentos `253,452 ms`, sincronização
`6,492 ms`, D2H `15,196 ms` e liberação `66,257 ms`. No delta de decode: H2D
`2618,950 ms`, kernel `224,304 ms`, alocação `100,680 ms`, lançamentos
`27,006 ms`, sincronização `16,399 ms`, D2H `21,769 ms` e liberação
`76,018 ms`. A parede CUDA é a medida para a soma de 100%; essas fases
internas explicam o conteúdo dela e podem ter sobreposição de relógios.

O campo legado `node_moe_forward_ms` continuou zerado no trace de decode,
apesar de `runtime_forward_ms` ter crescido `5481,247 ms`; a decomposição usa
o contador de parede do runtime e registra essa divergência de telemetria em
vez de ocultá-la. O executor observado foi `cuda`, dtype `[40]`,
`executed_engine=moe`, `engine_mismatch=false` e `mixed_execution=false`.
O maior próximo alvo medido é reduzir a transferência e a preparação do lote,
mas esta medição não inicia F5-E2 nem afirma ganho dela.

### Diagnóstico do prefill após a cobertura escalar - 2026-08-25

O benchmark mais recente do captain mediu três rodadas válidas do Gemma4 no
gateway: `38,4 tok/s` de prefill e `23,0 tok/s` de decode. Isso equivale a
`26,042 ms/token` e `43,478 ms/token`, respectivamente. A referência histórica
do `llama.cpp` é `49,0/20,4 tok/s` e a do FreeToken é `359,1/47,7 tok/s`.
Portanto o prefill atual está `1,276x` mais lento que o `llama.cpp` e `9,352x`
abaixo do FreeToken; o decode está `1,127x` acima do `llama.cpp` e ainda
`2,074x` abaixo do FreeToken. O benchmark novo não trouxe tamanho do prompt,
`n_ubatch` efetivo nem o trace interno, então esses números não autorizam
atribuir a diferença a uma única hipótese.

As hipóteses foram confrontadas com o caminho real:

1. **Prefill token a token.** Refutada no nível do servidor. `prefill::run`
   envia um pedaço inteiro e `prefill_blocking` chama `decode_tokens` uma vez
   com todos os tokens daquele pedaço. O padrão do pedaço é 512 e ele é
   limitado por `n_batch`; o `llama_batch` recebe o lote inteiro. Isso não
   significa um único cálculo físico: o `llama.cpp` processa o lote em
   microbatches e o hook externo é chamado dentro de cada um. O CUDA agrupado
   recebe `n_tokens` do microbatch e processa as rotas daquele lote em quatro
   kernels, não uma chamada por token.

2. **`n_ubatch=128`.** Confirmada como limitador potencial, não como causa
   suficiente. `effective_n_ubatch` reduz `n_batch=512` para 128 quando
   `fit_memory=true`; para um prompt maior que 128, o grafo é repetido em
   `ceil(tokens/128)` microbatches e o callback MoE repete a preparação de
   rotas, transferências de descritores e sincronização por microbatch. O
   perfil instrumentado que temos como quebra completa tinha 22 tokens, logo
   cabia em um único microbatch, e mesmo assim a preparação de lote dominou.
   O tamanho do prompt do benchmark novo não foi fornecido, então a fração
   atual atribuível a esse limitador permanece não medida.

3. **Sincronização por camada no prefill.** Existe sincronização no executor
   externo, mas não é o gargalo observado. Cada chamada FFI CUDA registra
   `sync_count=1` e executa `cudaDeviceSynchronize` depois dos quatro kernels;
   como o callback está dentro do grafo, isso ocorre por camada e microbatch.
   No perfil histórico, `sync_ms=6,492 ms` no prefill, contra
   `cuda_call_wall_ms=2836,915 ms`; no delta de decode foram `16,399 ms`
   contra `3086,384 ms`. Isso não sustenta a hipótese de que a espera de
   sincronização fosse o custo principal daquela carga, mas não é evidência
   do estado atual.

A quebra abaixo pertence exclusivamente ao perfil histórico de 22 tokens,
anterior ao estado que produziu `38,4 tok/s`. Ela fica registrada para
reprodução, mas está deliberadamente excluída de qualquer conclusão sobre o
prefill atual:

| componente | tempo | fração do backend total |
|---|---:|---:|
| restante do `llama.cpp` fora do forward MoE | `236,518 ms` | `2,942%` |
| lookup do cache | `1,263 ms` | `0,016%` |
| leitura do expert no GGUF | `501,936 ms` | `6,244%` |
| admissão no cache RAM | `696,284 ms` | `8,661%` |
| preparação do lote no host | `3622,166 ms` | `45,055%` |
| parede da chamada CUDA | `2836,915 ms` | `35,288%` |
| cópia do resultado no host | `6,197 ms` | `0,077%` |
| agregação ponderada | `2,229 ms` | `0,028%` |
| restante da orquestração do runtime | `135,860 ms` | `1,690%` |
| **backend total** | **`8039,369 ms`** | **`100%`** |

Essa tabela não prova onde o tempo está hoje. O perfil novo ainda está
pendente porque a GPU está sendo usada pelo benchmark de concorrência. A
coleta correta deve repetir exatamente as três rodadas atuais com
`POM_MOE_CUDA_PROFILE=1` e registrar o tamanho do prompt, `n_batch`,
`n_ubatch`, número de callbacks e a quebra equivalente. Só depois dessa
coleta, com a placa livre, será possível decidir entre aumentar o microbatch,
reutilizar a preparação de host ou alterar a sincronização.

### Análise formal de lacunas: FreeToken versus o caminho MoE atual

Esta análise foi feita em 2026-08-25, sem alterar código e sem acessar a ksia.
A fonte do lado FreeToken é o relatório de estudo em
`/Users/jairpereira/Sites/firstmate/data/rdma-freetoken-estudo/report.md`,
que leu o commit `bd372b630a028e3faa51f4ab0ef6a98c2f2de501`. A fonte do lado
local são os commits e perfis registrados neste documento. A ordem abaixo é
por impacto observado, não por facilidade de implementação.

| prioridade | lacuna | FreeToken tem | rdma-p2p tem | custo observado e ganho esperado |
|---|---|---|---|---|
| 1 | LRU de experts residente na VRAM | `moe/offload_cache.py` mantém um cache global de experts no device, com admissão pelo orçamento de memória livre. O payload quente não precisa cruzar PCIe a cada seleção. | `moe/cache.rs` mantém o payload canônico em RAM do host. O executor CUDA usa staging temporário e transfere o peso mesmo em hit do LRU. | Evidência direta: `803243520` bytes H2D por token de decode (`766,033 MiB`), com `240` usos de experts por token. No perfil fechado, H2D foi `2618,950 ms` em 29 tokens (`90,309 ms/token`) dentro de `cuda_call_wall_ms=3086,384 ms` (`106,427 ms/token`). Se a residência eliminar essas cópias, esse é o teto direto da redução da parede CUDA nessa carga, não uma promessa de reduzir o backend inteiro: a preparação no host continua. |
| 2 | Kernels CUDA para cada família de quantização | O relatório enumera bancos próprios em `_BANK_SCHEMAS`: `bf16`, `fp8_block`, `q4_0`, `nvfp4`, `mxfp4` e `ds_fp4`, com kernels CUDA/Triton fora do llama.cpp. | O executor CPU cobre várias famílias GGUF, mas a ponte CUDA real cobre somente NVFP4. IQ e outros dtypes publicam `CPUReference` com recusa explicada para a ausência da ponte, em vez de fingir CUDA. | No Qwen UD-IQ3_S medido, CPUReference custou aproximadamente `5284 ms/token` de prefill e `4990 ms/token` de decode. Isso prova um custo enorme para dtypes sem ponte, mas não autoriza projetar o ganho de uma ponte nova: cada dtype exige prova numérica independente, e a lista do FreeToken não prova cobertura de IQ3/IQ2/IQ4. |
| 3 | Preparação do lote fora do caminho quente | O relatório mostra um escalonador GPU-cêntrico que entrega views dos bancos para os kernels e sobrepõe o prefetch da próxima camada. Ele não publica uma métrica separada chamada `host_batch_prepare`; portanto não há evidência para afirmar que o FreeToken tenha custo zero nessa etapa. | O callback Rust monta lotes e descritores no host a cada forward. A telemetria mede esse custo como `host_batch_prepare_ms`, antes da chamada CUDA. | No perfil histórico, foi o maior componente do prefill: `3622,166 ms`. Isso não foi confirmado no estado atual de `38,4 tok/s`; o ganho real de reutilizar descritores e buffers depende do novo perfil. |
| 4 | Política compute-versus-transfer baseada em banda medida e double buffer real | `moe/benchbw.py` mede RAM/PCIe; `moe/bench_profile.py` grava perfil por GPU e calcula a fração híbrida `pcie/(pcie+cpu)`. `layers/moe.py` inicia o prefetch da camada seguinte e o orçamento reserva dois conjuntos de buffers. | A política de banda e o double buffer continuam andaime documentado, sem perfil persistido consumido pelo runtime. O caminho atual faz staging por chamada e não escolhe CPU/GPU pela banda medida. | Os números de H2D e kernel registrados no documento são históricos. Nenhum ganho ou gargalo atual deve ser inferido deles até o novo perfil do estado `38,4 tok/s`. |
| 5 | CUDA Graphs para o MoE quantizado | O FreeToken captura e recaptura CUDA graphs para as formas de lote e o cache, evitando custo repetido de montagem e lançamento. | Não há captura/replay de CUDA graph para o callback MoE Rust. O llama.cpp pode usar seus próprios graphs para o restante do transformer, mas isso não cobre o executor externo. | Depois do agrupamento, os lançamentos CUDA custaram `27,006 ms` em 29 tokens (`0,931 ms/token`) e sincronização `16,399 ms` (`0,565 ms/token`). Portanto graphs podem reduzir overhead de lançamento e alocação, mas o máximo explicitamente medido nessa categoria é pequeno diante dos `74,697 ms/token` de preparação host e `90,309 ms/token` de H2D. |

#### Item 1: cache em VRAM

O ponto do captain está confirmado e é estrutural. O hit do nosso LRU significa
que evitamos reler e descompactar o payload do GGUF, não que o device já possui
o peso. O caminho `lookup -> admit -> staging H2D` preserva o hit em RAM, mas
repete a travessia PCIe. O perfil anterior também mediu `805601280` bytes por
token antes do agrupamento; o agrupamento reduziu lançamentos, não o volume de
peso transferido. F5-E2, que remove a residência dos experts do carregador
llama.cpp, evita residência dupla no modelo, mas não transforma o LRU Rust em
cache de VRAM. São problemas distintos e ambos precisam aparecer na medição.

#### Item 2: banda medida e double buffer

O ponto também está confirmado, com uma correção de escopo. O FreeToken não
simplesmente escolhe sempre GPU: ele mede a banda de CPU e PCIe por formato e
GPU, ignora perfil de outra placa e usa a fração calculada para decidir o
trabalho híbrido. O nosso executor tem os campos de diagnóstico e a intenção
documentada, mas não um perfil de banda carregado pelo runtime nem dois buffers
CUDA alternados em produção. O double buffer do relatório é principalmente
prefetch entre camadas durante o prefill; ele não é equivalente a apenas
agrupar experts em uma chamada.

#### Item 3: CUDA Graphs

Também está confirmado, mas o impacto atual é menor que a cópia H2D e a
preparação host. O agrupamento já reduziu o número de lançamentos. Assim, um
graph deve ser medido depois de o layout dos buffers ser estável e depois da
residência em VRAM; capturar uma rota dinâmica antes disso arriscaria recapturas
frequentes e esconderia o custo que realmente domina.

#### Item 4: kernels por formato

O relatório confirma seis bancos próprios do FreeToken, não uma prova de todos
os dtypes GGUF existentes. A comparação correta é: nós já temos uma camada de
decodificação CPU mais ampla e recusa nominal por tensor, mas somente NVFP4
atravessa a ponte CUDA. Isso explica por que Gemma4 pode publicar
`moe_executor=cuda` e o Qwen UD-IQ3_S publica `CPUReference`. A ordem de ataque
deve ser determinada por modelos reais e por prova numérica, não pela lista
do relatório como se ela cobrisse automaticamente IQ-quants.

#### Item 5: montagem do lote no host

O FreeToken oferece evidência de uma organização GPU-cêntrica com views de
bancos, prefetch e streams, mas o relatório não mede uma caixa equivalente a
`host_batch_prepare_ms`. Portanto a afirmação segura é que o nosso custo foi
medido e o deles não foi decomposto dessa forma, não que o FreeToken "monta
tudo na GPU". A lacuna local é objetiva: no nosso perfil o host prepara o
lote por token antes da chamada CUDA e ocupa mais tempo que o kernel. Qualquer
ganho atribuído a mover essa montagem precisa preservar a rota dinâmica, a
ordem dos experts e a prova numérica por dtype.

#### Histórico do número de 9,3 tok/s e da execução duplicada

Não existe no repositório, no status persistido ou na documentação uma linha
que associe explicitamente o número `9,3 tok/s` a um SHA de implantação. O
estado duplicado que é possível provar no código e que corresponde ao período
anterior ao hook do fork é `5304228c0cf45dc729524bee21c217dd346be0af`,
commitado em 2026-08-24 às 18:15:18, com a ressalva de que o log de deploy que
ligaria esse SHA ao número 9,3 não foi preservado.

Nesse commit, `infer_server.rs` fazia primeiro `decode_hidden` ou
`decode_tokens` (`linhas 2870-2887`), isto é, deixava o llama.cpp executar o
grafo completo. No terminal, depois desse retorno, fazia
`take_residual` e chamava `apply_moe_hidden`, mas descartava o resultado com
`let _ = s.apply_moe_hidden(...)` (`linhas 2889-2892`). Logo depois, os logits
eram lidos de `s.engine.logits_ith(...)` e amostrados (`linhas 2936-2941`),
sem usar a saída do executor externo. Para uma requisição terminal de um hop,
a resposta vinha do llama.cpp; a passada externa era trabalho adicional
descartado. O commit ainda não tinha o callback opt-in
`moe_external_executor` que hoje permite impedir o bloco nativo.

A leitura do captain precisa de uma correção de precisão: naquele commit a
telemetria não reportava somente o caminho escolhido. Ela já serializava
`selected_path` e `executed_engine`, mas `record_terminal_engine` marcava
`executed_engine=moe` quando a passada externa terminava (`linhas 1188-1196`),
sem registrar que o llama.cpp também havia executado o grafo. Assim, o rótulo
`moe` era uma evidência de que a passada externa ocorreu, não de que ela era o
único executor nem de que sua saída alimentou a amostragem. Não havia detecção
de duplicação ou de mistura nesse caso.

Portanto, a afirmação forte "9,3 era a vazão do nosso motor" é incorreta para
esse estado duplicado. A afirmação mais precisa é: se o ensaio de 9,3 foi feito
no estado `5304228`, a resposta terminal não foi produzida pelo nosso executor
MoE e esse número não é baseline de velocidade do motor próprio. O repositório
não permite afirmar, sem o log de deploy, que `5304228` foi exatamente o SHA
do ensaio de 9,3; afirmar esse vínculo como fato seria inventar evidência.

O perfil honesto posterior é comparável por fase, não pelo tok/s agregado de
uma conversa: a medição agrupada registrou `316,455 ms/token` de prefill e
`186,069 ms/token` de decode, contra `1689,3` e `1738,7 ms/token` antes do
agrupamento. A repetição instrumentada registrou `365,426` e `206,173
ms/token`. Logo, nesses números por token o estado honesto posterior é melhor,
não pior. O `tok/s` agregado pode cair quando o prompt é longo porque inclui o
prefill; ele não invalida essa comparação por fase.

#### Decisão de prioridade

O próximo ganho mensurável deve começar pelo cache residente em VRAM, porque é
o único item que já tem custo direto de `766 MiB` de peso por token medido. Em
impacto de throughput entre modelos, a ponte CUDA por dtype vem logo depois:
o Qwen UD mediu segundos por token em `CPUReference`. Dentro do Gemma NVFP4, a
preparação host é o próximo alvo, seguida da política de banda e do double
buffer reais. Cada ponte de dtype exige prova numérica por formato. CUDA Graphs
fica depois de estabilizar residência, layout e política, pois os lançamentos
já agrupados são uma parcela pequena do tempo explicitamente medido.

### Objetivo formal: inversão da residência dos experts

O estado atual é consequência histórica, não uma decisão de produto. O cache
foi criado em `7a09d316befc51a2cbaf97775ece9caaab933f28` (2026-08-24,
`feat: adicionar cache global de experts q4_0`) quando o executor era CPU. O
`ExpertCache` guardava `Arc<[u8]>`, e a sonda padrão era `SystemFreeMemory`, que
lia `MemAvailable` no Linux, `vm_stat` no macOS e memória física disponível no
Windows. O próprio comentário dizia que uma sonda de VRAM seria uma integração
futura. Nesse contexto, manter o payload em RAM era coerente: era o cache do
decodificador de referência, funcionava sem CUDA e não assumia GPU discreta.

A ponte CUDA entrou depois, em `c782e557a8738d44afdc8505de7cba75261aa793`
(2026-08-24 às 20:14:30). Ela recebia `lease.bytes()` do cache e alocava os
buffers CUDA dentro de cada execução. Não houve nesse commit mudança de
residência em `cache.rs`, sonda de device ou política de admissão. O perfil
posterior confirmou a consequência: hits do LRU evitavam leitura e
descompactação no host, mas ainda transferiam o peso para a GPU. Portanto a
explicação do captain está correta: houve inércia de integração entre a Fase 4
CPU e a Fase 5 CUDA, não uma limitação técnica que obrigasse o cache a ficar em
RAM.

O segundo lado da inversão também tem causa histórica objetiva. No carregador
da fase CPU, `llama_model_default_params` recebia `n_gpu_layers=-1` durante o
ajuste de memória e não havia `moe_external_executor`; o carregamento normal do
llama.cpp podia colocar os tensores de experts em `CUDA0`. A opção de não
alocar experts foi adicionada depois como F5-E2. Assim, a residência do
llama.cpp na placa e a residência do nosso cache em RAM foram duas decisões
independentes que nunca foram fechadas como um único orçamento de device.

Este objetivo corrige a inversão como contrato do caminho MoE, sem alterar o
caminho denso:

1. O cache global de experts do executor próprio torna-se residente na VRAM do
   device que executará o lote. Um hit devolve ponteiros ou handles de device e
   não faz H2D. Um miss admite o expert uma vez, aguarda a conclusão do evento
   CUDA antes de publicar o lease e só então o reutiliza entre tokens e
   sessões.
2. F5-E2 deixa de ser apenas higiene de memória e vira pré-requisito. Quando o
   executor externo está selecionado, o loader do llama.cpp não aloca nem
   gerencia os pesos de experts em qualquer buffer de device. Ele conserva
   somente metadata e descriptors necessários ao hook; a fonte GGUF e o cache
   próprio continuam responsáveis pelos bytes. Sem o opt-in, o llama.cpp
   permanece byte a byte no comportamento denso atual.
3. A fronteira é genérica. A chave de residência inclui modelo, device,
   camada, expert, tensor/papel, dtype e geometria efetiva. Arquitetura,
   convenção gate/up e formato são dados do descriptor do hook. Um dtype,
   tensor, geometria ou arquitetura que não tenha decoder compatível recusa
   nominalmente no limite de admissão, nomeando o item; nunca reativa os pesos
   nativos nem assume CPU ou H2D silenciosamente.
4. A admissão usa orçamento real de VRAM, não constante. Para CUDA, cada miss
   consulta a memória livre do device e desconta pesos que continuam no
   llama.cpp, buffers fixos, KV, ativações, staging e uma reserva operacional.
   Após F5-E2, o termo de pesos de experts do llama.cpp deve ser zero. O limite
   é recalculado quando a memória externa muda e a eviction só pode remover
   entries sem lease e sem operação CUDA pendente.

#### Desenho de armazenamento e fluxo

O cache lógico atual será separado da representação física. A chave, LRU,
leases e estatísticas continuam comuns; a entrada passa a conter um backend de
armazenamento por device, um handle de alocação, bytes residentes, dtype,
geometria, evento de prontidão e último uso. O payload mmapado do GGUF pode
continuar como fonte fria ou fallback CPU explícito, mas não é confundido com
o cache quente CUDA.

```text
GGUF mmap / fonte fria
        |
        | miss + orçamento de VRAM medido
        v
cache de device: aloca -> H2D uma vez -> evento pronto -> lease
        |
        | hit: lease do mesmo device pointer, sem H2D
        v
kernel agrupado por dtype -> agregação -> release do lease
```

O descriptor não pode publicar um handle antes do H2D e dos sidecars do
expert estarem prontos no stream correto. A eviction deve esperar o evento de
uso, não apenas observar `active == 0`; isso evita reutilizar uma alocação
enquanto o kernel ainda está lendo. Quando um expert não cabe mesmo depois de
eviction ociosa, o erro deve nomear `device`, `dtype`, tensor, camada, expert,
bytes solicitados e orçamento disponível.

#### Fases e critério de pronto

- **R0, contrato e orçamento.** Definir a trait de device allocator/probe,
  separar bytes frios de handles residentes, e testar a aritmética com probes
  falsos. Pronto quando a decisão de admissão for reproduzível sem CUDA e
  nenhuma constante de VRAM existir no executor.
- **R1, F5-E2 do loader.** Ativar a opção externa no fork, manter metadata em
  contexto `no_alloc` e impedir buffers de experts do llama.cpp. Pronto quando
  o caminho MoE não mostrar bytes de experts em `CUDA0` ou `CUDA_Host`, enquanto
  o denso continuar carregando e executando sem a opção.
- **R2, cache CUDA residente.** Implementar a residência para o descriptor
  genérico e validar primeiro NVFP4 por ser o único dtype CUDA já provado. Uma
  sequência warm-up seguida de hits deve mostrar H2D de pesos igual a zero por
  uso, e um miss deve mostrar exatamente uma admissão por tensor residente.
  Pronto também exige a comparação numérica camada a camada já aprovada.
- **R3, formatos e arquiteturas.** Conectar cada ponte CUDA adicional por
  tensor e dtype, começando pelos modelos reais restantes. Cada bloco precisa
  de prova CPU Rust versus llama.cpp, CPU versus CUDA, recusa nominal para
  combinação não atendida e E2E coerente; não há promoção por fixture
  homogêneo.
- **R4, prova de inversão.** Na ksia, registrar antes/depois com a mesma
  build: `CUDA0`, `CUDA_Host`, CPU, RSS/PSS, bytes do cache de device, hits,
  misses, H2D de pesos e H2D de ativações. O critério é `llama.cpp expert
  bytes on device = 0`, cache MoE residente maior que zero, H2D de peso zero
  nos hits, e ausência de uma segunda residência. Medir prefill e decode por
  token no mesmo ensaio, sem atribuir ganho antes de fechar essa contabilidade.

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

O estado instrumentado atual ainda reutiliza o `ExpertCache` global com entrada
canônica em CPU/RAM e buffers CUDA de staging temporário. Isso é precisamente a
lacuna coberta pelo objetivo formal de inversão acima, não o desenho final. O
router e a política de admissão continuam no Rust. Cada forward deve publicar
`gpu_expert_calls`, `cpu_expert_calls`, bytes e banda H2D/D2H, `buffer_index`,
tempo de sobreposição, bytes residentes por device e o motivo de qualquer
fallback.

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

#### Prova numérica antes de desempenho

A resposta coerente do caminho MoE não prova que o callback calculou o FFN:
telemetria só prova seleção e execução. A regressão numérica começa na primeira
camada e no primeiro estágio, e só avança quando a comparação passa. O teste
ignorado `nvfp4_matches_llama_cpp_from_first_layer_and_stage` recebe um GGUF
real por `POM_MOE_NUMERIC_MODEL`, deriva a quantidade de experts dos tensores do
arquivo e percorre as camadas em ordem. Para cada expert e hidden determinísticos
iguais, ele compara `gate`, `up`, ativação e `down` do decoder Rust CPU com um
oráculo construído pelo próprio ggml/llama.cpp sobre os mesmos tensores do
modelo. O estágio que falhar primeiro é o diagnóstico publicado antes de
qualquer correção.

O critério inicial é `abs(a-b) <= 1e-4` ou erro relativo `<= 2e-3` para os
formatos quantizados; F32 usa `1e-5` absoluto e relativo. Valores não finitos
ou uma divergência acima do limite falham com camada, estágio, índice, erro
absoluto e relativo. A mesma matriz será repetida com o executor CUDA contra o
Rust CPU depois que a matriz CPU Rust versus llama.cpp estiver verde. Isso
separa erro de descompactação/escala, erro de geometria ou roteamento e erro
do kernel CUDA sem atribuir desempenho a um resultado incorreto.

##### Convenção NVFP4 usada pela prova

A resolução dos sidecars é genérica: para um tensor base `X.weight`, o nome
canônico de um sidecar é `X.scale` ou `X.input_scale`. O loader recusa o
tensor canônico ausente, nomeando-o; ele não transforma ausência em escala 1.
Os seis sidecars por expert são preservados no payload: `.scale` e
`.input_scale` para gate, up e down.

O cálculo externo segue a semântica observada no oráculo nativo do fork. O
`build_lora_mm_id` faz a multiplicação com o peso NVFP4 e aplica o `.scale`
correspondente depois do resultado por expert. O campo `.input_scale` é
carregado pelo modelo, mas não é consumido pelo `build_moe_ffn` nativo nem
aparece em outra operação desse FFN. Por isso o executor externo não o aplica
ao hidden, ao gate/up ou à ativação down. Isso não descarta o metadata: se uma
futura operação GGML declarar explicitamente esse campo como parte da
quantização de entrada, a regra será aplicá-lo antes da multiplicação daquela
operação, com uma prova independente; não se deve reutilizar esta decisão sem
essa evidência.

Para os valores E2M1, o fork define `kvalues_fp4` como os valores dobrados
`[0, 1, 2, 3, 4, 6, 8, 12, 0, -1, -2, -3, -4, -6, -8, -12]`. A escala UE4M3
é convertida para o valor bruto dividido por dois. A tabela dobrada e o fator
`0.5` são uma única convenção do ggml, compartilhada por NVFP4 e MXFP4; usar a
tabela E2M1 pela metade junto com UE4M3 dividido por dois aplica uma redução
dupla. CPU Rust e CUDA usam agora exatamente essa convenção, protegida por
teste unitário e pela comparação contra o ggml.

Há um limite conhecido na entrada sintética `POM_MOE_NUMERIC_INPUT=ones`: no
host x86 da ksia, o backend nativo usa oito acumuladores AVX2, FMA e redução
horizontal, enquanto uma implementação escalar acumula subbloco a subbloco.
Essa não-associatividade produz erro centrado apenas no estágio down de alguns
casos sintéticos, sem indicar diferença de escala, zero-point ou roteamento.
O executor seleciona a ordem AVX2 quando as capacidades correspondentes estão
disponíveis e conserva a redução escalar nas demais arquiteturas. A prova com
hidden determinístico do GGUF real Gemma4 passou em todas as camadas e estágios;
esse resultado real, e não o relaxamento da tolerância, é o critério de
correção.

Comando da prova real, fora da suíte padrão por exigir um arquivo grande:

```bash
POM_MOE_NUMERIC_MODEL=/caminho/modelo.gguf \
POM_MOE_NUMERIC_ACTIVATION=2 \
cargo test -p node nvfp4_matches_llama_cpp_from_first_layer_and_stage -- --ignored --nocapture
```

`POM_MOE_NUMERIC_ACTIVATION` é dado do GGUF, não uma seleção por arquitetura no
executor. A captura de rota não é usada como oráculo: o teste primeiro usa o
mesmo hidden, expert e pesos de um caso isolado; a prova E2E posterior reutiliza
o hidden real de cada camada. O `ggml_cont` da correção de layout só é chamado
quando `ggml_is_contiguous` é falso. Como o defeito original era exatamente um
reshape de view não contígua, aquela requisição necessariamente pagou uma cópia
de grafo para a rota afetada; o caso contíguo não paga cópia. O custo por
camada/token ainda precisa ser medido separadamente, pois a telemetria atual
não conta bytes ou tempo dessa cópia.

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

#### Implementação local da residência de device

O F5-E2 do loader já está no fork publicado em `03902a5ba` e o ramo Rust já
propaga `EngineConfig::moe_external_executor` até ele. A implementação nova
separa o armazenamento quente da fonte fria: `DeviceExpertCache` guarda apenas
um handle opaco do backend e seus leases, enquanto `ExpertCache` continua sendo
a LRU de RAM para os caminhos CPU. No caminho CUDA, um miss lê o payload da
fonte GGUF e o descarta depois do upload. A LRU de device é criada uma
vez por `MoeRuntime` NVFP4 e compartilhada por todas as camadas, portanto não
há uma LRU independente por camada.

O contrato é deliberadamente independente de CUDA e NVFP4. `DeviceAllocator`
mede memória livre, faz upload e devolve uma alocação com tamanho e handle;
qualquer backend futuro pode implementar a mesma fronteira. A implementação
CUDA atual usa `cudaMemGetInfo`, `cudaMalloc`, `cudaMemcpy` apenas na admissão e
`cudaFree` na eviction. O orçamento parte da memória livre observada depois da
carga do modelo e é recalculado a cada miss; não há capacidade fixa de placa.
Uma recusa nomeia o device, a chave do expert, os bytes pedidos e o limite
medido. Payload vazio, falha de sonda, upload incompleto ou handle nulo também
recusam explicitamente.

#### Correção de admissão por workspace medido

O orçamento agora também preserva o workspace transitório que o próprio
`llama.cpp` mediu depois de construir o contexto. `LlmEngine::cuda_compute_bytes`
extrai os buffers `CUDA*` de `llama_rs_context_memory_json`, soma apenas o
campo `compute` residente no device e ignora `CUDA_Host`; relatório ausente,
incompleto ou sem bytes CUDA positivos recusa o executor CUDA com mensagem
explícita. A reserva é `ceil(1,25 * compute_bytes)` usando aritmética inteira,
uma margem relativa de 25% para crescimento do grafo e fragmentação, e não uma
capacidade fixa de placa, contexto ou modelo.

O `DeviceExpertCache` usa essa reserva tanto no teto inicial quanto na nova
medição de memória livre de cada miss. Se o expert nomeado pela chave
`modelo/camada/expert/formato` não couber depois da reserva e da eviction de
entradas ociosas, a admissão recusa e inclui device, chave, bytes solicitados e
limite disponível. Não há tentativa de alocação para descobrir se cabe, nem
fallback silencioso para uma residência que consumiria o workspace do grafo.
O teste `cache_device_recusa_expert_quando_workspace_medido_consumiu_o_orcamento`
cobre o caso apertado com allocator falso. Esta correção está em implementação
local; a promoção exige build CUDA no ramo de checagem e validação no ambiente
real, sem alterar o serviço durante a medição do captain.

No NVFP4, o lote agora envia a lista pequena de ponteiros residentes para a
GPU. O H2D interno do lote contém hidden e ponteiros, não os pesos; `upload_ms`
e `upload_bytes` ficam separados nas estatísticas `device_cache_*`. O primeiro
acesso de uma chave é um miss e faz uma admissão/upload; hits posteriores não
fazem H2D de peso. A vida de um handle é protegida por lease, e a eviction só
remove entrada sem lease. A contiguidade, a geometria e a validação numérica do
payload continuam sendo verificadas antes da admissão, então a residência não
relaxa a prova de correção.

O teste unitário `cache_device_hit_reutiliza_handle_sem_novo_upload` usa um
allocator falso e prova um upload para dois acessos; os testes adjacentes
provam lease ativo e recusa pelo orçamento medido. Esses testes não substituem
a validação CUDA. Ainda falta compilar este commit na ksia com
`--features vulkan,cuda`; nenhum deploy, troca de modelo ou medição de
throughput foi feito nesta etapa porque a placa está sendo usada pelo captain.

O fork não mudou nesta etapa: a ABI genérica de substituição e o loader
metadata-only já estão em `03902a5ba`. Se a build CUDA revelar incompatibilidade
da ABI do kernel, ela será um commit separado no fork e será publicada antes da
promoção do ramo Rust.

#### F5-E3: lote de rotas no device

Depois da residência, o caminho NVFP4 ainda chamava o FFI uma vez por token e
devolvia um vetor por expert para a agregação Rust. Isso mantinha a montagem do
lote no host e repetia alocações, cópias de hidden e D2H intermediário. A revisão
seguinte mantém a rota resolvida como dados: os ids de expert, ids de token,
pesos e offsets por token entram em um único FFI por camada. O CUDA calcula
gate/up, GeGLU e down para todos os pares `(token, expert)` residentes e faz a
soma ponderada no device; somente o resultado final `[tokens, hidden]` volta ao
host. O caminho CPU continua token a token e não é alterado por esta fase.

Esta mudança não afirma ganho de tok/s sem medição. Ela precisa ser validada
com a prova numérica do mesmo hidden, pesos e rota antes de qualquer benchmark.
O teste CUDA `executor_cuda_nvfp4_lote_de_rotas_agrega_tokens_no_device` compara
dois tokens, quatro pares de rota e a agregação do device contra o decoder CPU.
O teste de build CUDA na ksia permanece obrigatório antes da promoção; nenhum
deploy, troca de modelo ou medição de throughput faz parte desta etapa enquanto
a placa estiver sendo usada pelo captain.

#### F5-E4: regressão e publicação

Cada fase produz commit separado no `rdma-p2p` e no fork. O teste de execução
deve cobrir callback opt-in, callback ausente e recusa nominal. O teste de
carga deve cobrir metadata preservada, ausência de tensor de dados e caminho
denso intacto. A suíte densa continua obrigatória; a prova de desempenho e de
residência exige o modelo real na ksia e não pode ser substituída por fixture.

#### F5-E5: pontes CUDA por dtype, antes dos CUDA Graphs

Esta é a próxima lacuna ativa. A prioridade é estrutural e está ancorada nos
números já medidos, não em uma expectativa de ganho pequeno: no decode, o
executor MoE honesto está em `5,4 tok/s`, contra `20,7 tok/s` do llama.cpp e
`53,3 tok/s` do FreeToken; no prefill, está em `3,2 tok/s`, contra `55 tok/s`
e `325 tok/s`, respectivamente. Os dtypes IQ do Qwen UD ficaram em
`CPUReference`, aproximadamente cinco segundos por token, portanto a ponte
CUDA por dtype vem antes de graphs.

Após a execução agrupada e a residência em device, a medição mais recente do
chat na mesma máquina registrou `23,3 tok/s` no MoE contra `26,7 tok/s` no
llama.cpp. No prefill foram `449,8 ms` para 23 tokens no MoE
(`19,56 ms/token`) contra `172,3 ms` (`7,49 ms/token`) no denso, e o decode
p50 foi `29,712 ms` contra `29,231 ms`. O p95 do MoE foi `38,363 ms`, contra
`29,521 ms` no denso; isso caracteriza cauda pior, mas não prova sozinho um
miss de cache. A quebra de colocação observada foi 30 camadas com peso em GPU
no MoE contra 23 em GPU e 7 mistas no denso. Esses números são linha de base,
não declaração de que a reserva de VRAM já foi validada.

A ordem concreta é Q4_K (`dtype=12`) e Q6_K (`dtype=14`), usados pelo Qwen3.6
Q4_K_M, seguidos por IQ3_S (`21`), IQ2_S (`22`) e IQ4_XS (`23`), usados pelos
dois GGUF UD reais do corpus. A implementação recebe dtype, geometria, papel e
payload como dados; não há ramo por arquitetura, nome de modelo ou família
global. Cada ponte precisa comparar a saída CUDA com o decoder Rust CPU para o
mesmo payload, hidden e rota, começando pela primeira camada e pelo primeiro
estágio. A prova com arquivo real move o formato de "implementado" para
"provado"; fixture sintética ou `cargo test` sem execução CUDA não faz essa
promoção.

Um dtype sem ponte permanece explicitamente visível como `CPUReference`, com
`dtype=<id> (<nome>)` e motivo, ou é recusado no limite de uma chamada CUDA
quando o contrato exigir CUDA. Nunca se interpreta um payload não coberto como
outro formato. Nesta entrega a ponte é declarada somente para Q4_K e Q6_K; IQ
3/2/4 ainda não está coberto até a sua prova numérica.

Critério de pronto por bloco: build `--features vulkan,cuda` verde no ramo de
checagem; teste diferencial executado para cada dtype; GGUF real correspondente
sobe e responde com `moe_executor=cuda`, sem `engine_mismatch` nem fallback
silencioso; e uma medição posterior em tok/s mostra o valor real contra a linha
de base `5,4/3,2` e o alvo FreeToken `53,3/325`. Enquanto a ksia estiver em
uso, a compilação é permitida somente com aviso prévio no status e nenhuma
medição ou alteração do serviço.

#### F5-E6: CUDA Graphs do caminho MoE

Graphs vêm depois das pontes por dtype. O graph será dirigido por dados e
indexado por uma chave de forma que inclua device, dtype, hidden, intermediate,
quantidade de tokens, pares de rota, strides e modo de ativação. A captura só
ocorre depois de os handles do cache residente, workspaces e descritores terem
estabilidade; mudanças de forma criam outro graph ou recusam nominalmente a
captura. Replay não pode trocar silenciosamente para CPUReference.

O critério de pronto é uma prova numérica antes/depois do replay para cada
chave, seguida de build CUDA e medição em tok/s na mesma carga. O ganho será
reportado como diferença de tok/s e de custo de lançamento/alocação, sem
atribuir aos graphs ganhos que vierem da ponte de dtype, do cache em VRAM ou da
montagem do lote. O alvo do goal continua sendo aproximar `53,3 tok/s` no
decode e `325 tok/s` no prefill do FreeToken; suite verde sem número não fecha
nenhuma dessas fases.

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

### Reorganização do marco de entrega - 2026-08-25

O critério operacional deste goal foi reorganizado para entregar valor nos
quatro MoE reais disponíveis na ksia antes de ampliar a cobertura abstrata de
todos os dtypes. O marco é: Gemma4 NVFP4, Qwen3.6 Q4_K_M com Q4_K/Q6_K, Qwen
UD-IQ3_S com IQ3_S/IQ2_S/IQ4_XS heterogêneos e Ornith UD-IQ3_XXS. Cada um
precisa subir e responder no caminho MoE, com `moe_executor` visível por hop,
sem `CPUReference` silencioso e sem regressão contra o llama.cpp na mesma
build. O número de aceitação é tok/s medido na ksia; suite verde ou carga de
metadata sozinha não fecha o marco.

A ordem vigente passa a ser:

1. **Salvaguarda por dtype e reserva de VRAM.** A reserva medida de
   `53decb4` e o ownership por camada chegam juntos. Somente uma camada cujos
   tensores de experts tenham executor device disponível pode ser retirada do
   llama.cpp. Dtypes sem cache device, inclusive combinações heterogêneas que
   incluam um dtype fora de IQ3_S/IQ2_S/IQ4_XS, permanecem com o llama.cpp. A
   recusa ou a preservação deve nomear os dtypes concretos, nunca deixar a
   placa sem dono.
2. **Cobertura do marco.** NVFP4, Q4_K/Q6_K e, em seguida, IQ3_S/IQ2_S/IQ4_XS
   por tensor, incluindo o cache device e a combinação heterogênea usada pelos
   dois UD. O decoder é escolhido por tensor e dtype; arquitetura, nomes,
   geometrias e convenções entram como metadata. Cada bloco exige prova
   numérica contra o oráculo, GGUF real quando disponível e prova E2E nos quatro
   modelos antes de ser chamado de pronto.
3. **Depois do marco.** Prefill, CUDA graphs e redução da cauda, priorizados
   por perfil medido. Os dtypes restantes seguem cobertos pela salvaguarda e
   só entram no executor device depois do marco, sem alterar o caminho nativo.

Esta reorganização substitui a fila anterior como ordem de entrega, mas não
remove o inventário completo nem a recusa nominal: os demais dtypes continuam
listados e explicitamente protegidos, e voltam à implementação depois que os
quatro modelos do marco tiverem números honestos.

### Estado da cobertura IQ heterogênea - 2026-08-25

A ponte device da fase seguinte está implementada localmente para os dtypes
IQ3_S (21), IQ2_S (22) e IQ4_XS (23), tanto em arquivos homogêneos quanto em
GGUF heterogêneo. No caso heterogêneo, o cache mantém uma entrada residente
por `(modelo, camada, expert, papel, dtype)`; o kernel recebe descritores
separados para `gate_up` fundido ou `gate`/`up` separados e `down`, e escolhe
o codebook pelo dtype de cada tensor. Isso evita transformar o UD em um
payload homogêneo no host e evita transferência de pesos em hits do cache.

O plano só externaliza uma combinação heterogênea quando todos os dtypes dos
experts pertencem ao conjunto suportado. Uma combinação como `[12, 21, 23]`
fica inteira com o llama.cpp e publica a recusa nominal; não há externalização
parcial que pudesse remover um tensor sem executor. A comparação unitária
CUDA usa gate IQ3_S, up IQ2_S e down IQ4_XS, com a mesma entrada e pesos da
referência CPU. A cobertura CUDA e a prova E2E nos GGUF reais dos dois UD ainda
precisam ser executadas no diretório de validação antes de promover esta fase;
os números do marco continuam em aberto.

### Estado da cobertura CUDA escalar - 2026-08-25

F32 (0), F16 (1) e BF16 (30) agora compartilham uma ponte CUDA parametrizada
por `PlainFormat`. O payload usado pelo `PlainSource` é admitido uma única vez
no `DeviceExpertCache`, incluindo o cabeçalho de comprimentos de `gate_up` e
`down`; o kernel valida esses comprimentos, decodifica o dtype informado e
executa gate/up, SiLU, down e agregação no lote de rotas. Não há ramo por
arquitetura ou nome de modelo, e um dtype fora de `{0, 1, 30}` é recusado antes
do lançamento.

O teste CUDA diferencial cobre os três formatos com os mesmos payloads e
hidden do `PlainExpert` CPU, com tolerância `1e-5` para F32 e `2e-3` para F16 e
BF16. A build e esse teste ainda precisam passar no worktree de validação, em
`/data/ftk-cuda-target`; também não há prova E2E com um GGUF MoE real desses
três dtypes. Sem CUDA, ou se a admissão por orçamento medido recusar o payload,
a posse permanece no llama.cpp e a telemetria informa o motivo.

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

### Aquecimento do cache residente - F5-E3a

O cache de device começa vazio por desenho: a carga do modelo cria o
`DeviceExpertCache`, mas a primeira admissão acontecia somente quando uma rota
selecionava um expert. Isso deixava a VRAM baixa após o carregamento e fazia as
primeiras requisições pagarem a leitura do GGUF e o upload dos pesos.

O `MoeRuntime` agora inicia, somente quando existe uma ponte CUDA para a fonte
selecionada, uma thread de aquecimento assíncrona no carregamento do modelo.
Ela percorre os experts por camada e índice, pede ao próprio loader o payload
canônico de cada tensor e chama a mesma admissão do caminho quente. O dispatch
é por `MoeSource` e pelos metadados do tensor; não há ramo por arquitetura,
modelo ou versão de quantização. Em um GGUF heterogêneo, cada papel e dtype
recebe a mesma chave e descriptor que o forward usa.

O limite continua sendo o orçamento de VRAM medido, incluindo a reserva de
compute já reportada pelo `llama.cpp`. Quando a admissão devolve
`NoBudget`/`BusyAndNoEviction`, o aquecimento para com
`budget-exhausted`; os candidatos ainda não visitados ficam explicitamente
fora do aquecimento e continuam sujeitos ao LRU sob demanda. Ele não falha o
carregamento por o modelo inteiro não caber.

O prazo padrão é 30.000 ms e pode ser ajustado por
`POM_MOE_WARMUP_MAX_MS`. A thread verifica o prazo e um `AtomicBool` antes de
cada candidato; `MoeRuntime::cancel_device_warmup` permite cancelamento
cooperativo sem remover entradas já admitidas. Logs de início e fim registram
modelo, status, bytes admitidos e motivo de parada. A telemetria
`moe_executor_diagnostics` publica `warmup_status`, candidatos visitados,
experts admitidos, bytes residentes, duração e erro. Assim uma subida não fica
bloqueada indefinidamente e uma parada por orçamento, prazo ou erro não vira
cache frio silencioso.

Os testes de cache cobrem aquecimento até o orçamento, cancelamento antes do
primeiro payload e prazo zero. A validação CUDA da integração e a comparação
E2E do Qwen UD após o aquecimento ainda precisam ser executadas no diretório
isolado `/data/ftk-cuda-target`; nenhum número de ganho é afirmado antes dessa
medição.

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

### Perfil novo do prefill após a correção de `max_tokens`

Em 2026-08-25, depois de a GPU ser liberada, executei uma instância isolada
com o binário compilado em `/data/ftk-cuda-target`, código da correção
`72cc5cc` (promovido ao histórico publicado `e3d1751`), fork
`llama.cpp=4bb64b070`, `--features vulkan,cuda`, `n_gpu_layers=99`,
`n_ctx=8192`, `n_batch=512`, `POM_MOE_CUDA_PROFILE=1` e `--no-thinking`. O
serviço nas portas 8080/8092 não foi reiniciado nem alterado. A instância usou
portas 18082/18094, `no-discovery` e o GGUF real Gemma4 NVFP4. A requisição
`Escreva hello world 10 vezes.` pediu `max_tokens=30` e retornou exatamente 30
tokens, com dez linhas coerentes e sem marcadores de template.

O trace real foi: `input_tokens=22`, `output_tokens=30`,
`executed_engine=moe`, `moe_executor=cuda`, `engine_mismatch=false`,
`mixed_execution=false`, prefill `processing_ms=1351`, decode
`processing_ms=923` em 29 chamadas. Isso dá `61,409 ms/token` (`16,284
tok/s`) no prefill e `31,828 ms/token` (`31,419 tok/s`) no decode. O p50 de
decode foi `28,886 ms` e o p95 `44,083 ms`. A composição instrumentada do
prefill foi:

| métrica | valor bruto |
|---|---:|
| backend / `node_decode_ms` | `1351 ms` / `1350,694 ms` |
| `node_moe_forward_ms` / `runtime_forward_ms` | `1132,604 ms` / `1131,824 ms` |
| `cuda_call_wall_ms` | `158,385 ms` |
| `host_expert_load_ms` | `405,762 ms` |
| `host_batch_prepare_ms` | `0 ms` |
| `cuda_h2d_ms` / bytes | `1,390 ms` / `7.524.240` |
| `cuda_kernel_ms` | `145,731 ms` |
| `cuda_d2h_ms` / bytes | `1,511 ms` / `7.434.240` |
| chamadas de experts | `5280` |

O prefill de 22 tokens fez uma chamada batelada de
`MoeRuntime::forward_batch_with_route`, não 22 passadas de gateway. As 5280
chamadas de experts são `22 tokens × 30 camadas × 8 experts ativos` dentro
dessa passada; portanto a métrica `calls` conta registros de experts, não
passadas do prefill. O perfil atual não mostra `host_batch_prepare_ms` como
gargalo, ao contrário do perfil histórico de antes do agrupamento; a quebra
nova é a que vale para a próxima investigação.

O mesmo ensaio confirmou o limite da rota: o pedido de 30 terminou em 30, e
não em 31. O fork não mudou nesta correção: `max_tokens` é contrato do
gateway Rust, então não há campo de API novo para sincronizar no
`llama.cpp`.

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
