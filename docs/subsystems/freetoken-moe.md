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
5. co-execução CPU/GPU baseada em banda medida, inicialmente no executor NVFP4
   CUDA.

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

O primeiro formato é Q4_0 packed. `format_q4_0.rs` valida a geometria GGML,
mmapa o GGUF, copia somente os bancos do expert solicitado para o cache,
decodifica os blocos de 32 elementos, executa gate-up, SiLU gated e down. Com
`--features cuda`, `block.rs` mantém o payload admitido no cache residente e a
ponte heterogênea executa as três fases na GPU; sem CUDA, o mesmo decoder roda
explicitamente em `CPUReference`. O loader aceita tanto
`ffn_gate_up_exps.weight` fundido quanto
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
IQ3_XXS (18), IQ3_S (21), IQ2_S (22), IQ4_XS (23) e MXFP4 (39) agora também
passam por essa seleção. O
decoder K-quant valida a ordem GGUF
`[columns, rows, experts]`, os blocos de 256 elementos e a convenção fundida ou
separada dos bancos gate/up antes de copiar um expert. O frame continua em f16
e não leva pesos.
Na convenção separada, gate e up têm a geometria GGUF
`[hidden, intermediate, experts]`; o loader não pode trocar `dims[0]` e
`dims[1]` ao transformar essa metadata em `hidden` e `intermediate`. O callback
valida também o rank e as dimensões recebidas do llama.cpp e, em caso de
divergência, informa a forma observada e a esperada.
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

O bloco K-quant foi validado localmente com fixtures Q2_K, Q3_K, Q4_K e Q6_K, nas duas
convenções de nomes, e com `MoeRuntime::open` seguido de `forward_batch`. A
falha inicial era uma inversão no validator: GGUF declara a dimensão de
colunas em `dims[0]`, enquanto o contrato do mat-vec recebe `rows, columns`.
O validator agora checa as duas dimensões e o eixo de experts antes do forward.
O teste retangular usa `hidden=2048` e `intermediate=512`, portanto falha se os
eixos forem trocados. Não há ainda uma prova física de Qwen3.6 neste worktree.

Em 2026-09-02, a prova de um bloco Q6_K real fechou a causa do NaN do caminho
`k_quant`: o layout GGML é `ql[128]`, `qh[64]`, `scales[16]` e `d[2]` no fim
dos 210 bytes. O decoder Rust e `k_dot_q6_k` liam `d` no início, deslocando
também todos os demais campos. A correção usa os offsets do `block_q6_K` real
nos dois executores. As provas
`q6_k_gate_up_oito_experts_hidden_2048_e_finito` e
`q6_k_gate_up_heterogeneo_oito_experts_hidden_2048_e_finito` usam entrada
finita e oito experts, e a primeira também verifica os índices de expert
carregados pelo GGUF. A validação CUDA exige `nvcc` na máquina de GPU; não há
GGUF Qwen3.6 nem compilador CUDA neste worktree local.

Durante essa validação também foi corrigida uma inconsistência preexistente no
decoder IQ: `IQ4_NL` tem blocos GGML de 32 elementos, enquanto os demais IQ
priorizados usam blocos de 256. O cálculo de bytes por linha e o tamanho
passado ao dequantizador agora respeitam essa diferença; a prova local de
dequantização deixa de ler além de um bloco de 18 bytes.

Os IQ-quants priorizados usam o mesmo loader e a mesma fronteira de cache.
O executor próprio mantém parser, mmap, router, top-k e mat-vec; a expansão do
bloco usa as primitivas de dequantização GGML linkadas pelo crate nativo, sem
duplicar os codebooks oficiais em Rust. Fixtures locais cobrem IQ3_S, IQ2_S e
IQ4_XS, os dois nomes de gate/up e a execução ponta a ponta do runtime. A ponte
CUDA cobre IQ3_XXS, IQ3_S, IQ2_S e IQ4_XS; um dtype fora dos decoders externos
continua recusado pelo dispatch com ID e nome.

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

### Travessia UD heterogênea

`format_heterogeneous.rs` mantém o dtype e a geometria de cada papel do expert
separadamente. O callback e o inventário Rust falam a mesma forma GGML
`[columns, rows, experts]`, enquanto ids e pesos da rota continuam em ordem
token-major `[token, slot]`. No CPU, o lote agrupa temporariamente as entradas
por expert e percorre cada linha de gate, up e down uma vez para todos os tokens
que a selecionaram; isso evita a descodificação integral repetida que fazia
combinações grandes falharem como timeout no prefill. A saída é reconstruída
na posição original de cada token e mantém os pesos da rota, tanto para bancos
fundidos quanto separados.

Essa é a combinação observada no `deepseek4` real da ksia: IQ3_XXS (18) em
gate/up e MXFP4 (39) em down, com 256 experts e top-6. Esses dois formatos têm
decoders CPU externos e ponte CUDA heterogênea; o plano mantém os experts fora
do buffer de pesos do llama.cpp e a execução em lote não cai na execução token a
token. Para
uma combinação sem decoder CPU ou ponte CUDA, `MoeRuntime::external_executor_plan`
devolve a razão com os dtypes e o servidor recusa a carga antes de abrir o
modelo; não há retorno silencioso ao MoE nativo.

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
ou o forward MoE retornam. `/health` publica `selected_path` e o estado
efetivo do executor MoE (`moe_requested`, `moe_executor`, dtypes e motivo),
porque uma seleção sem executor precisa ser visível mesmo antes de uma
requisição. O gateway agrega os campos por fase de
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
e por fase. Um binário sem a feature `cuda` usa o executor CPU próprio para os
formatos suportados. NVFP4 continua recusando a carga com a mensagem nominal
do executor, salvo quando `POM_MOE_NVFP4_BACKEND=cpu` pede explicitamente o
executor de referência.

No Qwen3.6-35B-A3B-UD-IQ3_S real, a ponte agora está autorizada a publicar
`moe_executor=cuda`, `moe_executor_dtypes=[21,22,23]` e nenhum motivo de
CPUReference, depois da prova diferencial dos três dtypes na GPU real. Q4_0
usa o mesmo contrato CUDA com dtype GGML `2`; a validação preserva blocos de 32
para esse formato, em vez de aplicar a exigência de 256 usada pelos K/IQ-quants.
Q5_K entra pelo dtype GGML `13`, bloco de 176 bytes em 256 elementos
(`d[2] dmin[2] scales[12] qh[32] ql[128]`): é o Q4_K com um plano de bit alto,
e `k_value_q5_k`/`k_dot_q5_k` reproduzem `dequantize_q5_k_block` do decoder CPU.

### Um dtype sem ponte derruba a rota inteira

`Ornith-1.5-35B-A3B-Q5_K_S` mede 2 dtypes Q4_0 e 13 Q5_K nos experts: enquanto
Q5_K não tinha kernel, a rota caía inteira para `CPUReference` mesmo com a
ponte Q4_0 pronta. Quants dinâmicos (UD) misturam dtypes **por papel de
expert**, portanto a cobertura da ponte se confere pelo dtype concreto na
telemetria (`moe_executor_dtypes`), nunca pelo nome do ficheiro. Provar o ganho
num modelo cujo dtype já tinha ponte engana a medição.

### `offloaded 42/42 layers to GPU` não fala dos experts

Com `moe_external_executor` ligado, o llama.cpp carrega a metadata dos experts
mas **não aloca os pesos deles** (`EngineConfig::moe_external_executor`). Logo
o seu `offloaded N/N layers to GPU` é verdadeiro sobre o que ele próprio possui
- atenção, normas, saída - e omite a maior parte do modelo. Na ksia isso
apareceu como `offloaded 42/42` ao lado de apenas ~1,6 GB de buffers de modelo
em GPU (CUDA0 615,83 MiB + CUDA1 978,08 MiB) num modelo de 35 B, com
`CPU_Mapped` de 333,44 MiB: a leitura correta do operador, "o modelo está todo
na RAM", estava certa. Quem aloja os experts é o executor MoE do POM, e onde
eles ficam depende do executor escolhido: no cache residente de device quando
há ponte CUDA para **todos** os dtypes, mmapados na RAM quando é
`CPUReference`. O log de inicialização do `infer_server` diz isso
explicitamente para que as duas linhas não se contradigam aos olhos de quem lê.
A prova física desse GGUF ainda não foi executada neste worktree, portanto a
telemetria esperada é um contrato a verificar, não um resultado já afirmado.

Para separar uma falha de dados recebidos de uma falha na leitura Rust, execute
o processo com `POM_MOE_ROUTE_DIAGNOSTICS=1`. Cada entrada do callback ganha
duas linhas adjacentes, `callback_entry_default` no target padrão e
`callback_entry` no target `moe.route_diagnostic`, como primeira ação, antes
das validações. A dupla testa o sink e um eventual filtro por target no mesmo
ponto do callback. Ambas registram os ponteiros e contagens crus; cada retorno
`false` ganha ainda uma linha
`callback_exit` com o motivo do caminho que abortou, inclusive argumentos
nulos, geometria, normalização, metadata e forward. A primeira chamada de cada
requisição e qualquer rota não finita ganham a amostra detalhada, sem depender
do índice da camada. Ela registra valores crus de token 0, slot e deslocamento
na ordem token-major, ponteiros e bytes das regiões lidas, quantidade de
elementos declarada pelo
llama.cpp versus esperada pelo POM, dtypes da metadata de gate/up/down e os
dtypes assumidos nos argumentos do callback. Depois da normalização, registra
novamente a amostra e as contagens de NaN. A variável é opt-in, fica desligada
por padrão e não altera a execução. Na carga do primeiro `LoadedModel` do
processo, uma linha adicional no target padrão registra uma vez o valor bruto
da variável e o booleano efetivo do gate. Assim o log do próximo deploy mostra
se o `OnceLock` reconheceu o ambiente, antes da primeira rota.

Antes de normalizar a rota, o callback também valida que o hidden de entrada é
finito. Depois do forward, valida a saída do executor antes de copiá-la para o
llama.cpp. NaN ou infinito publica uma falha com camada, índice e estágio
(`hidden_input_non_finite` ou `forward_output_non_finite`), interrompendo a
requisição em vez de contaminar a camada seguinte silenciosamente.

O teste ignorado
`llm::moe_route_tests::callback_global_emite_diagnostico_em_thread_externa_e_registra_gate`
instala o dispatcher global com `.init` equivalente, cria uma thread
`std::thread` fora do runtime Tokio e exige no mesmo sink a configuração, o
`callback_entry_default`, o `callback_entry` com target MoE e o `callback_exit`.
Ele passou localmente com `RUST_LOG=info`; isso prova o transporte no processo de
teste, mas não substitui a confirmação no binário implantado.

O callback do `llama.cpp` recebe a rota na ordem token-major que resulta do
`ggml_get_rows` com dimensões `[1, experts_usados, tokens]`: slot é o índice
mais rápido dentro de cada token. A normalização Rust valida e preserva essa
ordem, sem transposição. O teste unitário
`llm::moe_route_tests::rota_do_get_rows_do_llama_cpp_ja_chega_em_ordem_token_major`
fixa o contrato contra a sequência física produzida pelo builder C++.

As mensagens `resolve_fused_ops: ... enabled` não provam que uma operação
fundida foi usada: a sonda também imprime `enabled` quando não encontra nenhum
nó daquele tipo. No `qwen35moe.cpp`, o FFN MoE continua passando pelo builder
genérico e o expert compartilhado passa separadamente por `build_ffn`; a
presença real de HC fundido precisa ser demonstrada pelos nós da reserva ou
pelos logs da entrada do callback.

### Fase 5.1: ponte CUDA por dtype

Esta foi a fase formal da ponte por dtype. O executor CUDA cobre NVFP4 (40),
escalares, Q2_K/Q3_K/Q4_K/Q6_K e IQ3_XXS (18), IQ3_S (21), IQ2_S (22), IQ4_XS (23) e
MXFP4 (39) na ponte heterogênea. A prova diferencial de Q2_K e Q3_K foi
executada na GPU real; o E2E do GGUF Qwen3.6 continua pendente. IQ-quants
restantes, K-quants restantes, legacy e low-bit continuam no `CPUReference`,
com a escolha visível na telemetria. Quando uma combinação heterogênea contém
dtype sem ponte CUDA, a camada inteira usa `CPUReference` se houver decoder
externo; sem decoder, a carga é recusada antes do modelo. Cada tensor conserva
seu dtype e o decoder correspondente é escolhido por tensor.

Critério de pronto por bloco: prova numérica contra o oráculo GGML para cada
dtype, telemetria por hop/fase mostrando `cuda` sem `moe_executor_reason` de
CPU, prova E2E com GGUF real heterogêneo quando disponível e medição na ksia
de prefill, decode, bytes H2D/D2H, alocações e banda. A comparação deve
separar custo de kernel e transferência; nenhum ganho de desempenho será
afirmado apenas por suite verde.

### Fase 5.2: Q2_K/Q3_K no Qwen3.6 e fallback CPU paralelo - 2026-09-04

A reprodução E2E fornecida pelo captain estabeleceu a linha de base no mesmo
`Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf` e na ksia: o caminho `moe/q4_0` selecionava
`cpu-reference` por causa dos dtypes Q2_K (10) e Q3_K (11), com `0,3 tok/s`.
O caminho `llama.cpp`, nas mesmas condições, entregava `160 tok/s`. O trace
atribuía `91936 ms` de `llama_decode` ao backend local, não ao transporte.

Esta fase adiciona a ponte CUDA dos dois formatos. Q2_K usa blocos de 256
elementos e 84 bytes, com escalas, quants, `d` e `min`; Q3_K usa 256 elementos
e 110 bytes, com high bits, quants, escalas compactadas e `d`. O kernel faz o
produto diretamente no layout GGML, sem reinterpretar Q2_K ou Q3_K como Q6_K.
O validador Rust, a geometria da ponte heterogênea e a escolha de executor
agora aceitam `10` e `11`; Q5_K e Q8_K continuam fora da ponte e não podem
ser enviados ao kernel como outro formato.

O fallback CPU deixou de executar a rota em série: experts distintos são
processados em workers escopados, cada um com um vetor de saída próprio, e a
agregação ocorre em ordem de expert estável. A rota de token único usa a mesma
primitiva paralela. Isso mantém a soma determinística e transforma panic de
worker em erro de inferência. O diagnóstico `POM_MOE_LAYER_INPUT` da camada 0
fica desligado por padrão; só é habilitado com
`POM_MOE_LAYER_INPUT_DIAGNOSTIC=1` (também `true`, `yes` ou `on`).

A validação CUDA própria na ksia foi executada com `CUDAARCHS=120`, `nvcc`
12.9 e target separado. Os testes diferenciais do executor passaram:

| prova | resultado |
|---|---:|
| Q2_K, Q3_K, Q4_K e Q6_K contra decoder CPU | 1 teste verde |
| rota heterogênea Q2_K gate + Q3_K up + Q4_K down contra decoder CPU | 1 teste verde |
| tolerância da comparação CUDA | `abs <= 1e-3` ou `rel <= 1e-5` |

A medição E2E pós-ponte do GGUF Qwen3.6 ainda depende de o captain colocar
esta build na implantação compartilhada. Até essa execução, os números de
throughput são honestamente:

| caminho | antes | depois | referência |
|---|---:|---:|---:|
| MoE | `0,3 tok/s` | pendente de E2E | - |
| llama.cpp | - | - | `160 tok/s` |

O contrato esperado após a implantação é `moe_executor=cuda`,
`moe_executor_dtypes=[10,11,21,22,23]` e `moe_executor_reason` ausente. A
quebra por fase deve registrar TTFT/prefill, decode, `host_batch_prepare_ms`,
`cuda_kernel_ms`, H2D/D2H e tok/s; a suíte CUDA não substitui essa medição.

### Fase 5.3: prefill Q6_K em lote e especulação - 2026-09-04

O segundo par de medições do captain isolou a causa do prefill. No mesmo
`Ornith-1.5-35B-Q6_K.gguf`, na ksia, com `n_batch=512`, o dtype Q6_K já tinha
ponte CUDA, mas o MoE entregava somente `7,7` e `17,0 tok/s` de prefill,
contra `183,0` e `183,5 tok/s` do `llama.cpp`. A geração ficou em `15,9` e
`15,4 tok/s` no MoE, contra `36,2` e `38,2 tok/s` no denso. O TTFT foi
`6,57 s` e `6,41 s` no MoE, contra `3,21 s` e `574 ms` no denso.

| caminho | turno 1 | turno 2 | observação |
|---|---:|---:|---|
| MoE, geração | `15,9 tok/s` | `15,4 tok/s` | `2,3x` a `2,5x` abaixo do denso |
| MoE, prefill | `7,7 tok/s` | `17,0 tok/s` | `3.615,538 ms` e `4.883,604 ms` |
| llama.cpp, geração | `36,2 tok/s` | `38,2 tok/s` | referência |
| llama.cpp, prefill | `183,0 tok/s` | `183,5 tok/s` | referência |

No decode, a latência por chamada era próxima (`56,157` a `62,733 ms` no
MoE, contra `54,744 ms` no denso), mas o denso produzia várias fichas por
chamada: `14` chamadas para `27` fichas, contra `60` para `61` e `196` para
`197` no MoE. Isso torna a elegibilidade de MTP/especulação uma segunda
prioridade. O sampling MoE também acumulou `33,495 ms` de top-k em 61 fichas
e `113,09 ms` em 197 fichas; os bytes medidos foram cerca de 28 vezes maiores.

A primeira fatia de correção daquela medição atuou no maior buraco medido: o pipeline K CUDA
mantém workspace de device por executor e reutiliza buffers entre chamadas,
alocando somente quando a capacidade do lote cresce. Gate/up e down Q6_K
usam redução cooperativa por linha com 128 threads; Q2_K e Q3_K usam a mesma
ponte de dtype e a rota heterogênea também usa redução cooperativa para os
formatos cobertos. O `FIT_MAX_UBATCH` permanece `512`.

O benchmark sintético na geometria Q6_K real (`hidden=2048`,
`intermediate=512`, 28 tokens e 224 rotas) mediu `12,233 ms` de parede, com
`11,653 ms` de kernel, `0,118 ms` de H2D, `0,194 ms` de D2H e `0,157 ms` de
alocação na primeira capacidade. É uma medida do executor, não uma promessa
de E2E. A suíte CUDA na ksia passou com `325` testes e zero falhas.

Na data desta fase o E2E pós-fatia ainda precisava ser coletado nos dois
modelos. A tabela histórica permaneceu pendente até o binário novo servir a
mesma requisição:

| caminho | antes | depois | referência |
|---|---:|---:|---:|
| Qwen3.6 MoE | `0,3 tok/s` | pendente de E2E | llama.cpp `160 tok/s` |
| Ornith Q6_K MoE | `15,9`/`15,4 tok/s` | pendente de E2E | llama.cpp `36,2`/`38,2 tok/s` |

Com a nova evidência, a ordem de trabalho é: corrigir o MTP no contexto
metadata-only do MoE; confirmar a especulação por chamada; atacar o custo base
de `5,8x` por chamada, incluindo a duplicação estrutural e a sincronização
antecipada; só então atacar o prefill, que continua a pior distância por
chamada. A telemetria continua obrigatória para confirmar `moe_executor`,
`moe_executor_dtypes`, `moe_executor_reason`, MTP, TTFT, fases e tok/s depois
do deploy.

### Fase 5.4: escopo da telemetria e primeira fatia de prefill - 2026-09-04

O par pós-implantação do captain usou `Ornith-1.5-35B-Q4_K_M.gguf` e confirmou
que a ponte CUDA estava ativa (`moe_executor=cuda`). O resultado ainda ficou
longe do denso: no turno longo, `11,3 tok/s` no MoE contra `140,7 tok/s` no
llama.cpp; o prefill ficou em `16,1 tok/s` contra `1.343,6 tok/s` e o p50 por
chamada foi `2.515,7 ms` contra `135,8 ms`. No decode, foram `197` chamadas
para `198` fichas no MoE, contra `62` para `197` no denso; o p50 foi `84,6 ms`
contra `20,8 ms`. O acerto de prefixo com 19 fichas não retirou o custo de
`3.106,96 ms` do prefill MoE. Esse par é a linha de base E2E da fatia seguinte,
não uma prova isolada de ganho da ponte.

O par Q6_K com CUDA confirma a prioridade, mas não prova o ganho da ponte:
Q6_K já estava entre os dtypes cobertos antes da mudança. No turno longo, o
MoE foi de `15,4` para `16,4 tok/s`, enquanto o prefill mediu `17,0` e
`23,4 tok/s` contra `619,0 tok/s` do llama.cpp. O p50 do prefill CUDA ficou em
`1.826 ms` contra `54,6 ms`, uma distância de 33x; o decode ficou em
`50,3 ms` contra `21,7 ms` e ainda houve `196` chamadas para `197` fichas,
contra `73` para `197`. A prova da ponte continua sendo o Qwen
`Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`, que antes caía para `CPUReference` por
Q2_K/Q3_K.

A aparente contradição entre `caminho selecionado: moe/q4_0` no ambiente e
`llama.cpp` nos hops foi investigada no código. O `/health` usado pelo cartão
de ambiente publica o estado atual do processo: `InferState.inference_path`,
executor MoE instalado e colocação efetiva. Já cada `NodeTimings` do trace é
criado durante a requisição e publica `selected_path`, `executed_engine`,
`executed_engines`, `engine_mismatch` e `mixed_execution`. São escopos
distintos. O código também recusa um executor externo MoE sem callback e recusa
um callback instalado que não tenha observado nenhum forward, portanto não há
fallback denso silencioso autorizado no binário atual.

Assim, as linhas resumidas como "motor llama.cpp" não permitem concluir um
desvio de caminho sem os campos `selected_path`, `engine_mismatch`,
`mixed_execution`, `node_id` e o trace bruto da mesma resposta. A conclusão
comprovada é que o rótulo do ambiente estava sendo usado como se fosse prova
da requisição, embora seja apenas um snapshot atual. O cartão de telemetria
agora mostra separadamente `estado atual` e `nesta execução`, mostra o
`selected_path` em cada fase e avisa quando um snapshot atual diverge do path
observado. Se um trace bruto novo trouxer `selected_path=moe/q4_0` e
`executed_engine=llama.cpp`, ele aparecerá como divergência explícita e deve
ser tratado como processo antigo ou desvio real, nunca como resultado MoE.

A primeira fatia de otimização do prefill preserva o lote inteiro no executor
CUDA e troca os kernels escalares de gate/up e down por uma redução cooperativa
de 128 threads por linha para Q2_K, Q3_K, Q4_K e Q6_K. O benchmark sintético,
com `hidden=2048`, `intermediate=512`, 28 tokens e 224 rotas, mediu:

| dtype | parede | kernel | H2D | D2H | alocação |
|---|---:|---:|---:|---:|---:|
| Q4_K | `12,471 ms` | `11,907 ms` | `0,118 ms` | `0,194 ms` | `0,153 ms` |
| Q6_K | `11,517 ms` | `11,080 ms` | `0,116 ms` | `0,098 ms` | `0,152 ms` |

Essa medida é do executor isolado na ksia, com `nvcc 12.9`, `CUDAARCHS=120`
e target separado; não é ainda o depois E2E. O `FIT_MAX_UBATCH` continua em
`512`. A suíte CUDA passou com `325` testes e o diferencial novo de Q4_K em
lote passou; o número E2E após essa fatia fica para a implantação integrada do
captain.

### Fase 5.5: MTP no modelo MoE e controle sem especulação - 2026-09-04

O controle do captain no mesmo `Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`, com MTP e
especulação desligados nos dois motores, isolou o custo base. O MoE fez `16,9`
tok/s contra `93,7` do llama.cpp no turno longo; ambos produziram uma ficha por
chamada, mas o p50 da chamada foi `57,4 ms` contra `9,9 ms`, uma distância de
`5,8x`. O prefill continuou separado e muito pior: `1.105,8-3.065,9 ms` por
chamada no MoE contra `37,8-42,3 ms`, ou `26x-73x`. Portanto MTP não explica o
custo base, embora possa multiplicar a diferença: no par anterior o denso fazia
`3,8` fichas por chamada.

O `/health` do deploy MoE mostrou `mtp_ready=false` com
`llama_decode no contexto MTP falhou rc=-1`. A causa é a combinação do modelo
metadata-only com o fork: `llama_init_from_model` herda
`moe_external_executor`, mas o contexto `nextn` não tinha callback. O fork
recusa esse decode para não tentar experts nativos sem buffers. A correção em
`MtpDrafter` registra se o contexto usa ownership externo e
`LlmEngine::set_moe_runtime` instala o callback do runtime tanto no alvo quanto
no contexto MTP. O acompanhante CPU usado quando o contexto MTP recua para RAM
continua nativo e não recebe esse hook.

O caminho de especulação já existia: `should_draft` exige `mtp_ready` para MTP,
`mtp_draft` constrói o lote e a cauda verifica cada posição pelo sampler normal.
Assim, depois que o contexto externo ficar pronto, o mesmo caminho volta a
produzir lotes maiores no MoE sem alterar a distribuição da resposta. A
telemetria esperada após a implantação é `mtp_ready=true`, sem
`mtp_unavailable_reason`, e `speculation_active=true` quando o draft atingir o
limiar de probabilidade.

O callback externo do MoE já é o ponto de substituição de `build_moe_ffn` no
fork usado pelo projeto: quando o ownership externo está ativo, o builder
retorna o nó do callback e não acrescenta as matmuls nativas dos experts. No
Rust, o caminho de hidden/residual agora também falha fechado se o executor
externo estiver ativo sem hook, em vez de rodar um segundo forward no sidecar.
Isso preserva a correção e evita que um binário incompatível transforme uma
falha de instalação em duplicação silenciosa. O fork vendorizado não foi
alterado nesta fatia.

A ordem de medição e otimização fica: (1) confirmar MTP funcional no MoE;
(2) confirmar a especulação por chamada; (3) reduzir o custo base de `5,8x`,
incluindo qualquer duplicação comprovada e a sincronização antecipada; (4)
atacar o prefill, que segue sendo a maior distância absoluta. A medição E2E
após a correção MTP fica para a implantação integrada do captain.

### Fase 5.6: diagnóstico da reserva de sessões - 2026-09-04

A revisão da janela entre o binário bom e o mau não encontrou uma regressão no
caminho denso. `99d8977` só instala o callback externo no MTP quando
`moe_external_executor` está ativo; `74ab819` deixa a captura de logs fora do
caminho quando ela está desativada; e `84e7141` consulta o mapa de rotas no
envio, não durante o `llama_decode` local. Portanto nenhum desses commits
explica o salto de `20,8 ms` para `87,5 ms` por chamada.

O contrafactual controlado foi executado na ksia com o binário `4868f79`, o
GGUF real `Ornith-1.5-35B-Q4_K_M.gguf`, GPU `99`, `n_batch=512`, MTP ligado e
os mesmos prompt e parâmetros. A única variável foi `max_sessions`:

| configuração | node_p50_ms por chamada | observação |
|---|---:|---|
| `max_sessions=1` | `20,944 ms` | linha alinhada ao binário bom do captain |
| `max_sessions=2` | `105,3-124,4 ms` | mesmo binário, sem alteração de código |

No segundo caso, `/health` confirmou `n_ctx_total=500224`, `n_ctx_per_session=250112`,
`kv_seq_max=4` e `max_sessions=2`; a ocupação medida das duas RTX 5060 Ti
ficou em aproximadamente `15,3 GiB` por placa. A diferença reproduz o efeito
de fit e reserva de KV que contamina a comparação, embora o harness próprio
tenha uma latência absoluta maior que a implantação do captain. O resultado
descarta a hipótese de regressão introduzida pelo callback MTP no caminho
denso e identifica `max_sessions=2` com janela de `250k` como a causa do caso
medido.

O cartão de detalhes agora mostra um aviso localizado quando o `/health`
efetivo combina múltiplas sessões com KV medido em dispositivo: total de
contexto, contexto por sessão, estado do fit e o risco de latência por pressão
de VRAM. Uma sessão ou KV em CPU não gera o aviso. A mudança é diagnóstica e
não altera a seleção densa, MTP, transporte ou executor MoE. Para comparar
throughput com a linha de base, fixe `max_sessions=1` ou registre explicitamente
o total de contexto e a ocupação de VRAM de cada execução.

#### Procedimento E2E para a implantação do captain

Quando a implantação integrada estiver pronta, o captain deve medir cada
modelo em duas seleções separadas, `dense` e `moe`, mantendo a mesma máquina,
GPUs, `n_ctx`, `n_batch=512`, `n_gpu_layers`, número de sessões, temperatura,
prompt e `max_tokens`. A seleção deve ser feita no deploy e o processo deve
ser reiniciado pelo ciclo normal da implantação; não se deve tentar trocar
`POM_INFERENCE_MODE` num processo já servindo.

Em cada seleção, confirmar primeiro a porta de inferência e guardar a
telemetria de executor:

```bash
MODEL=Ornith-1.5-35B-Q6_K.gguf
GATEWAY=http://ksia:8080
INFERENCE=http://ksia:8092

curl -fsS "$INFERENCE/health" |
  jq '{selected_path,moe_requested,moe_executor,moe_executor_dtypes,moe_executor_reason,n_ubatch_effective,mtp_ready,mtp_unavailable_reason}'
```

Depois executar a mesma requisição em cada modo. O `tee` guarda o JSON bruto;
o `jq` imprime geração, TTFT, prefill, decode, contagem de chamadas e a
telemetria por hop:

```bash
MODE=dense # repetir com MODE=moe após selecionar o deploy correspondente
PROMPT='Escreva hello world 10 vezes.'
curl -fsS "$GATEWAY/api/generate" \
  -H 'content-type: application/json' \
  --data "$(jq -nc --arg model "$MODEL" --arg prompt "$PROMPT" \
    '{model:$model,prompt:$prompt,max_tokens:64}')" \
  | tee "ornith-q6k-${MODE}.json" \
  | jq '{tokens_generated,tokens_per_sec,duration_ms,timings,trace:{input_tokens,output_tokens,hops:[.trace.hops[] | {node_id,start_layer,end_layer,prefill:(.prefill | {selected_path,executed_engine,executed_engines,engine_mismatch,mixed_execution,processing_ms,calls,node_p50_ms,node_p95_ms,moe_executor,moe_executor_dtypes,moe_executor_reason}),decode:(.decode | {selected_path,executed_engine,executed_engines,engine_mismatch,mixed_execution,processing_ms,calls,node_p50_ms,node_p95_ms,moe_executor,moe_executor_dtypes,moe_executor_reason})}]}}'
```

Repetir o mesmo bloco para
`Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`. No resultado MoE, exigir
`moe_executor=cuda`, dtypes `[14]` no Ornith e `[10,11,21,22,23]` no Qwen,
sem `moe_executor_reason`; no denso, exigir `moe_executor=not-requested`.
No Qwen MoE após esta correção, exigir também `mtp_ready=true` e
`mtp_unavailable_reason=null`.
Comparar `timings.generation_tokens_per_sec` e `tokens_per_sec` como geração,
`timings.prefill_tokens_per_sec` como prefill e `timings.ttft_ms` como TTFT.
Também registrar de `trace.hops[].prefill` e `.decode` os campos
`processing_ms`, `calls`, `node_p50_ms`, `node_p95_ms`, bytes e
`moe_executor_diagnostics`. Guardar o intervalo completo do log para conferir
que `POM_MOE_LAYER_INPUT` só aparece quando
`POM_MOE_LAYER_INPUT_DIAGNOSTIC=1` foi explicitamente solicitado.

#### Procedimento E2E do impacto dos logs na ksia

O teste de impacto deve comparar o mesmo modelo, máquina, plano, prompt e
parâmetros com a captura desligada e ligada. O interruptor é aplicado pelo
principal, sem reinício, e a chamada de geração continua na rota pública:

```bash
MODEL=Ornith-1.5-35B-Q6_K.gguf
GATEWAY=http://ksia:8080
TOKEN='connection-key-do-cluster'
PROMPT='Escreva hello world 10 vezes.'
BODY=$(jq -nc --arg model "$MODEL" --arg prompt "$PROMPT" \
  '{model:$model,prompt:$prompt,max_tokens:64}')

for enabled in false true; do
  curl -fsS -X POST "$GATEWAY/api/ui/network/logs" \
    -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    --data "{\"logs_enabled\":$enabled}" >/dev/null
  curl -fsS "$GATEWAY/api/generate" \
    --data "$BODY" >/dev/null
  for run in $(seq 1 10); do
    curl -fsS "$GATEWAY/api/generate" \
      --data "$BODY" > "cluster-logs-${enabled}-${run}.json"
  done
  jq -s '{runs:length,
    mean_generation_tok_s: ([.[].timings.generation_tokens_per_sec // 0] | add / length),
    mean_total_tok_s: ([.[].tokens_per_sec // 0] | add / length),
    mean_ttft_ms: ([.[].timings.ttft_ms // 0] | add / length)}' \
    cluster-logs-${enabled}-*.json
done
```

Guardar os vinte JSONs e registrar no relatório o sistema, commit, modelo,
GPU, `n_ctx`, `n_batch=512`, `n_gpu_layers`, sessões, temperatura e os dez
valores de cada lado. Comparar a mediana e a média de
`generation_tokens_per_sec` e `tokens_per_sec`; conferir também que o
`GET /api/ui/cluster-logs` contém linhas quando `enabled=true` e que o contador
de descartes permanece explícito. Repetir trocando `MODEL` pelo Qwen3.6 se a
medição MoE desse modelo também fizer parte da entrega. A captura local usa
fila `try_send` limitada, então qualquer aumento deve ser investigado sem
transformar o envio em espera síncrona.

A investigação histórica da medição Gemma4 encontrou, antes da costura do
callback externo atual, uma divergência anterior a qualquer otimização de
kernel. O contexto capturava `l_out-29`, mas em
`vendor/llama-cpp-sys-2/llama.cpp/src/models/gemma4.cpp` esse tensor é criado
depois de `build_moe_ffn`, `ffn_post_norm`, a soma residual e `build_cvec`. O
callback de captura copia o tensor e retorna `true`; portanto a chamada de
`llama_decode` continua pelo grafo. Em seguida, `infer_server::apply_moe_hidden`
chama o `MoeRuntime` depois de `take_residual`. Isso acrescenta uma passada dos
experts à passada completa do llama.cpp, em vez de substituir `build_moe_ffn`.

O contrafactual mínimo foi identificado no próprio callback daquela versão:
retornar `false`
ao encontrar `l_out-29` só abandonaria nós posteriores a esse tensor, mas não
impediria o `build_moe_ffn` que já o produziu. Logo, esse contrafactual não
elimina o trabalho duplicado; a correção estrutural precisava mover a fronteira
para antes do FFN MoE e injetar a saída no restante do bloco. A fonte atual do
fork já faz essa substituição no `build_moe_ffn`: com
`moe_external_executor`, o builder retorna o nó customizado antes das matmuls
nativas. O Rust também deixa de aplicar o sidecar ao hidden quando o hook está
instalado e recusa fechado se o executor externo estiver sem hook. Portanto o
perfil histórico não deve ser usado para afirmar duplicação no binário atual;
ele precisa ser repetido na implantação integrada e conferido pelos campos
`executed_engine`, `engine_mismatch` e `mixed_execution`. A medição também
mostrou uma segunda causa possível, ainda não quantificada isoladamente: o
executor CUDA atual aloca buffers e copia hidden, gate/up e down de host para a
GPU em cada chamada, sincroniza os kernels e copia o resultado de volta. Esse
perfil só deve ser otimizado depois de separar o custo da transferência do
custo do kernel.

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
| 2 | Kernels CUDA para cada família de quantização | O relatório enumera bancos próprios em `_BANK_SCHEMAS`: `bf16`, `fp8_block`, `q4_0`, `nvfp4`, `mxfp4` e `ds_fp4`, com kernels CUDA/Triton fora do llama.cpp. | O executor CPU cobre várias famílias GGUF; a ponte CUDA cobre NVFP4, escalares, Q2_K/Q3_K/Q4_K/Q6_K e IQ3_S/IQ2_S/IQ4_XS após provas diferenciais, enquanto os demais dtypes publicam `CPUReference` com recusa nominal. | A linha de base histórica do Qwen UD-IQ3_S em `CPUReference` não é uma previsão do ganho atual: a ponte IQ foi aceita somente após comparar os três estágios na GPU real. |
| 3 | Preparação do lote fora do caminho quente | O relatório mostra um escalonador GPU-cêntrico que entrega views dos bancos para os kernels e sobrepõe o prefetch da próxima camada. Ele não publica uma métrica separada chamada `host_batch_prepare`; portanto não há evidência para afirmar que o FreeToken tenha custo zero nessa etapa. | O callback Rust monta lotes e descritores no host a cada forward. A telemetria mede esse custo como `host_batch_prepare_ms`, antes da chamada CUDA. | No perfil histórico, foi o maior componente do prefill: `3622,166 ms`. Isso não foi confirmado no estado atual de `38,4 tok/s`; o ganho real de reutilizar descritores e buffers depende do novo perfil. |
| 4 | Política compute-versus-transfer baseada em banda medida e double buffer real | `moe/benchbw.py` mede RAM/PCIe; `moe/bench_profile.py` grava perfil por GPU e calcula a fração híbrida `pcie/(pcie+cpu)`. `layers/moe.py` inicia o prefetch da camada seguinte e o orçamento reserva dois conjuntos de buffers. | `BandwidthProfile` persiste as bandas, `coexecution.rs` divide misses únicos e executa lanes CPU/GPU em paralelo no NVFP4; `DoubleBuffer` valida a reserva de dois slots. | A costura ainda precisa da prova CUDA desta revisão. Os números de H2D e kernel registrados no documento são históricos e não são uma promessa de ganho. |
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
os dtypes GGUF existentes. A comparação correta é: nós temos uma camada de
decodificação CPU mais ampla e recusa nominal por tensor; a ponte CUDA cobre
somente os formatos que passaram a prova, atualmente NVFP4, escalares, Q4_K,
Q6_K, IQ3_S, IQ2_S e IQ4_XS. A ordem de ataque deve ser determinada por
modelos reais e por prova numérica, não pela lista do relatório como se ela
cobrisse automaticamente IQ-quants.

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
o Qwen UD mediu segundos por token em `CPUReference` antes da ponte IQ. Dentro do Gemma NVFP4, a
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

O inventário segue a mesma prioridade do llama.cpp quando um GGUF traz
`ffn_gate_up_exps.weight` fundido junto com `ffn_gate_exps.weight` e
`ffn_up_exps.weight`: o tensor fundido é a representação ativa e os split
duplicados são ignorados. Isso evita transformar cópias do mesmo expert em
papéis concorrentes no descriptor. Nos caminhos K e heterogêneo, cada tensor
ativo também procura os sidecars canônicos removendo `.weight` do nome base e
anexando `.scale` ou `.input_scale`. Os valores por expert são validados como
escalares finitos, carregados no payload/descriptor e aplicados nos produtos
gate, up e down. O tensor compartilhado `ffn_*_shexp` continua fora desse
inventário.

Os testes `expert_shared_nao_entra_no_inventario_de_rota` e
`inventario_de_rota_separa_exps_de_shexp_e_dos_sidecars` confirmam que somente
os quatro tensores `*_exps.weight` são classificados como rota: `*_shexp` e
`.scale`/`.input_scale` ficam fora. O teste
`sidecars_fundidos_usam_componente_e_fallback_separado_corretos` confirma a
associação do componente 0/1 nos sidecars do tensor fundido e o fallback para
os sidecars de gate/up separados. As hipóteses de expert compartilhado contado
e sidecar associado ao componente errado não explicam, portanto, o NaN
observado no Ornith.

O descriptor CUDA heterogêneo é uma estrutura C compatível com `#[repr(C)]`:
`data` fica no offset 0, `bytes` no 8, `dtype` no 16, `scale` no 20,
`input_scale` no 24, `secondary_scale` no 28 e `secondary_input_scale` no 32;
o tamanho final é 40 bytes. Asserções de offset e tamanho existem no Rust e no
`cuda_kernels.cu`. Essa parte não é coberta pela compilação local no macOS:
qualquer mudança no kernel precisa de build com `nvcc` na ksia.

O ponto comum normaliza os tensores de ids e pesos da rota antes de remodelá-los.
Como esses buffers cruzam do backend que calcula top-k/gather para o custom op
CPU, o stride aparentemente contíguo não prova que a alocação do produtor já
foi materializada para a cópia do scheduler. O grafo cria sempre um produtor
`ggml_cont` antes do reshape, inclusive para o primeiro token e slot. Isso
fecha a leitura de memória não inicializada que aparecia como `NaN` em
`ffn_moe_weights`; `llama.cpp/tests/test-moe-route-layout.cpp` cobre as vistas
contígua e estriada.

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
o hidden real de cada camada. O `ggml_cont` da correção de layout é
deliberadamente chamado mesmo quando `ggml_is_contiguous` retorna verdadeiro:
somente a execução do produtor prova que a área está pronta para a cópia entre
backends. O custo por camada/token ainda precisa ser medido separadamente, pois
a telemetria atual não conta bytes ou tempo dessa cópia.

Para provar a regressão completa antes de reimplantar, execute isoladamente o
teste ignorado abaixo com um GGUF `qwen35moe` e CUDA real:

```bash
POM_MOE_NUMERIC_MODEL=/caminho/qwen35moe.gguf \
POM_MOE_NUMERIC_GPU_LAYERS=99 \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

Ele captura a saída da camada 1 para quatro tokens no caminho nativo e no
callback externo, exige `moe_forward_us > 0` e somente executores `cuda`, e
falha se qualquer residual não for finito. A medição publicada
`MOE_NUMERIC_PROOF` deve mostrar `max_abs` e `max_relative`; cada elemento deve
satisfazer `abs <= 5e-2 + 2e-2 * max(abs(native), 1)`. Essa combinação prova
que a rota foi materializada e lida sem NaN, que o callback realmente usou
CUDA e que o resultado externo acompanha o bloco nativo. O comando exige a
feature CUDA e não deve ser substituído por `cargo test` sem `--ignored`.

Para a prova do Ornith-1.5-35B-Q4_K_M, usar o GGUF completo real, não uma
fatia, no valor de `POM_MOE_NUMERIC_MODEL`. O caminho exato não está neste
worktree e não foi consultado na ksia nesta etapa, então permanece um
placeholder explícito:

```bash
POM_MOE_NUMERIC_MODEL=/caminho/absoluto/Ornith-1.5-35B-Q4_K_M.gguf \
POM_MOE_NUMERIC_GPU_LAYERS=99 \
CUDAARCHS=120 \
CUDACXX=/usr/local/cuda-12/bin/nvcc \
CCACHE=ccache \
RUSTC_WRAPPER=ccache \
CARGO_TARGET_DIR=/data/ftk-cuda-target \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

Essa prova não foi executada: aguarda autorização explícita para gastar
CPU/GPU na ksia.

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

Esta foi a lacuna ativa desta fase. A prioridade é estrutural e está ancorada nos
números já medidos, não em uma expectativa de ganho pequeno: no decode, o
executor MoE honesto está em `5,4 tok/s`, contra `20,7 tok/s` do llama.cpp e
`53,3 tok/s` do FreeToken; no prefill, está em `3,2 tok/s`, contra `55 tok/s`
e `325 tok/s`, respectivamente. Os dtypes IQ do Qwen UD ficaram em
`CPUReference`, aproximadamente cinco segundos por token, antes da ponte CUDA
por dtype. A ponte agora está provada em fixture diferencial; a medição E2E do
Qwen UD continua sendo a próxima validação de produto.

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
outro formato. Nesta entrega a ponte é declarada para Q2_K, Q3_K, Q4_K, Q6_K, IQ3_XXS,
IQ3_S, IQ2_S, IQ4_XS e MXFP4 somente depois das respectivas provas; os demais
IQ continuam protegidos pela recusa nominal.

Quando uma rota heterogênea combina somente esses nove dtypes, a decisão CUDA é
feita por tensor, e não pela igualdade de um `KQuantFormat` global. Assim, uma
combinação Q2_K/Q3_K/Q4_K/Q6_K como a do Qwen3.6 pode usar o mesmo executor heterogêneo,
com `k_dot` escolhendo o decoder já validado de cada tensor. Um dtype fora desse
conjunto usa o decoder CPU externo quando ele existe; se não existe, a admissão
recusa a combinação antes da carga e lista os dtypes que bloquearam a ponte. A comparação diferencial específica
da rota mista está no teste
`executor_cuda_k_quants_heterogeneos_preserva_q2k_q3k_q4k_do_decoder_cpu`, com
tolerância absoluta de `1e-4` e relatório também da diferença relativa; ela foi
executada com CUDA real antes de declarar a combinação provada.

O callback comum do llama.cpp entrega `selected_experts` e `weights` na ordem
token-major produzida por `ggml_get_rows` para `[1, experts_usados, tokens]`,
enquanto o runtime Rust também agrupa lotes como `[token, expert_slot]`. A
fronteira em `llm.rs` valida e preserva essa ordem, sem transposição adicional,
antes de chamar o runtime. A validação de finitude permanece antes do upload e
identifica `ffn_moe_weights`, arquitetura, camada, token, slot e expert no erro.
Isso é importante para `qwen35moe`, que usa o mesmo `build_moe_ffn` genérico
para experts roteados e mantém o expert compartilhado em outro caminho; não há
um contrato de rota alternativo por arquitetura.

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

#### Medição inicial da F5.2 - 2026-08-25

A medição foi executada na ksia, em uma RTX 5060 Ti, com `POM_MOE_CUDA_PROFILE=1`,
`CARGO_TARGET_DIR=/data/ftk-cuda-target` e `CUDACXX=/usr/local/cuda-12/bin/nvcc`.
O serviço não foi tocado: o teste rodou em um checkout temporário e os pesos
dos experts ficaram residentes no cache CUDA antes das 100 iterações de cada
ponto. O comando reproduzível foi:

```text
PATH=/home/jairpereira/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH \
POM_MOE_CUDA_PROFILE=1 CARGO_TARGET_DIR=/data/ftk-cuda-target \
CUDACXX=/usr/local/cuda-12/bin/nvcc \
cargo test -p node --features vulkan,cuda --bin pom \
  mede_banda_h2d_d2h_do_lote_nvfp4 -- --ignored --nocapture
```

Saída observada:

```text
CUDA_BANDWIDTH case=tiny hidden=64 intermediate=64 tokens=2 active_experts=2 calls=400 h2d_bytes=60000 h2d_ms=1.998 effective_h2d_GB_s=0.030 d2h_bytes=51200 d2h_ms=0.760 effective_d2h_GB_s=0.067 kernel_ms=7.400 wall_ms=14.320
CUDA_BANDWIDTH case=gemma4_token hidden=2816 intermediate=704 tokens=1 active_experts=8 calls=800 h2d_bytes=1140800 h2d_ms=2.262 effective_h2d_GB_s=0.504 d2h_bytes=1126400 d2h_ms=0.818 effective_d2h_GB_s=1.377 kernel_ms=22.398 wall_ms=39.021
CUDA_BANDWIDTH case=gemma4_batch8 hidden=2816 intermediate=704 tokens=8 active_experts=8 calls=6400 h2d_bytes=9120800 h2d_ms=3.543 effective_h2d_GB_s=2.574 d2h_bytes=9011200 d2h_ms=2.209 effective_d2h_GB_s=4.079 kernel_ms=162.502 wall_ms=185.358
```

Esses valores são taxas efetivas das cópias pequenas feitas por uma chamada
FFI síncrona, não a banda máxima do PCIe. Os bytes dos pesos não aparecem no
H2D da chamada porque o teste mede o caminho residente; o upload inicial é
contabilizado separadamente pelo cache. A chamada atual ainda aloca e libera
buffers por forward, faz duas `cudaDeviceSynchronize` e só então retorna, por
isso não há sobreposição nesta medição. Nos dois pontos representativos,
`T_total / max(T_compute, T_transfer)` foi `39,021 / 22,398 = 1,742` e
`185,358 / 162,502 = 1,141`, respectivamente. Ambos falham o critério de
`1,10`; a F5.2 continua aberta e nenhum ganho de co-execução foi declarado.

#### Medição após os dois slots CUDA - 2026-08-25

O commit `00bf6f1` substituiu o caminho de rota por um pipeline com dois slots
persistentes. Cada slot tem stream, buffers de device e buffers host pinned;
quando há pelo menos dois grupos de tokens com rotas, o segundo grupo é
preparado e transferido enquanto o primeiro calcula. A telemetria passou a
separar `kernel_ms` (trabalho somado), `kernel_span_ms` (janela crítica entre
slots) e `transfer_span_ms` (H2D mais D2H da janela crítica), além de publicar
`buffer_mask` e `buffer_switches`.

Na mesma ksia, GPU e comando da medição anterior, a saída foi:

```text
CUDA_BANDWIDTH case=tiny hidden=64 intermediate=64 tokens=2 active_experts=2 calls=400 buffer_mask=3 buffer_switches=100 h2d_bytes=60800 h2d_ms=2.885 effective_h2d_GB_s=0.021 d2h_bytes=51200 d2h_ms=0.795 effective_d2h_GB_s=0.064 kernel_ms=8.576 kernel_span_ms=6.053 transfer_span_ms=1.959 wall_ms=16.260
CUDA_BANDWIDTH case=gemma4_token hidden=2816 intermediate=704 tokens=1 active_experts=8 calls=800 buffer_mask=3 buffer_switches=0 h2d_bytes=1140800 h2d_ms=1.840 effective_h2d_GB_s=0.620 d2h_bytes=1126400 d2h_ms=0.472 effective_d2h_GB_s=2.387 kernel_ms=22.553 kernel_span_ms=22.553 transfer_span_ms=2.312 wall_ms=28.768
CUDA_BANDWIDTH case=gemma4_batch8 hidden=2816 intermediate=704 tokens=8 active_experts=8 calls=6400 buffer_mask=3 buffer_switches=100 h2d_bytes=9121600 h2d_ms=4.120 effective_h2d_GB_s=2.214 d2h_bytes=9011200 d2h_ms=1.386 effective_d2h_GB_s=6.502 kernel_ms=333.662 kernel_span_ms=168.081 transfer_span_ms=3.036 wall_ms=180.647
```

O ponto `gemma4_batch8` satisfaz o critério medido: `180,647 / max(168,081,
3,036) = 1,075`, abaixo de `1,10`, e o `buffer_mask=3` prova o uso dos dois
slots. O ponto de um token ainda fica em `28,768 / 22,553 = 1,276`, e o ponto
pequeno em `16,260 / 6,053 = 2,686`; portanto o resultado não autoriza
declarar a F5.2 inteira pronta. Na revisão que produziu essa medição, o
pipeline estava integrado ao runtime e a telemetria registrava os slots, mas o
perfil ainda não era persistido/carregado e a seleção CPU/GPU ainda não
consumia tempos comparáveis medidos para os dois backends. A entrega F5.2b
abaixo fecha essa lacuna de seleção e deixa a medição CUDA como gate restante.

#### F5.2b: co-execução NVFP4 por banda medida

Esta entrega fecha a costura operacional seguinte, sem declarar ganho de
desempenho antes da medição. `BandwidthProfile` persiste no sidecar CUDA as
bandas CPU e transferência medidas em isolamento e sob disputa; quando as
bandas sobrepostas existem, elas têm precedência. A fração de misses enviada à
GPU é `transfer/(cpu+transfer)`, limitada ao intervalo `[0, 1]`, e a política
divide experts únicos em ordem determinística. Hits já residentes no cache de
device permanecem na GPU, portanto um expert repetido na mesma rota não é
admitido nas duas lanes.

O runtime carrega o perfil uma vez por modelo NVFP4 e o passa a cada bloco. No
forward de rota, a lane CPU usa o `ExpertCache` global de RAM e o decoder
NVFP4 Rust; a lane GPU usa o cache residente e o kernel CUDA. As duas funções
rodam em threads irmãs e a saída é somada por token, com validação da largura
antes da combinação. O caminho unitário converte a rota em um lote de um token,
para não abrir uma exceção sem co-execução. Ausência de perfil mantém todos os
misses na GPU, preservando o comportamento anterior; perfil inválido recusa a
carga nominalmente.

Os diagnósticos agora expõem `cpu_expert_calls`, `gpu_expert_calls` e
`coexecution_gpu_fraction`, além das medições CUDA já existentes. Os testes
unitários cobrem a política de banda, duplicatas, erro de uma lane, reserva do
double buffer, soma das lanes e persistência do sidecar. A compilação CUDA e
os testes focados desta revisão foram executados na ksia; nenhuma velocidade
ou sobreposição efetiva é afirmada neste texto além das medições sintéticas
registradas abaixo.

Na ksia, a build desta revisão `ffe9669` foi executada com `CUDAARCHS=120`,
`CUDACXX=/usr/local/cuda-12/bin/nvcc`, `CCACHE=ccache`,
`RUSTC_WRAPPER=ccache` e `CARGO_TARGET_DIR=/data/ftk-cuda-target`. O build
release terminou em `9m45s`, com `BUILD_EXIT=0`; o ccache registrou `524/532`
chamadas cacheáveis (`98,50%`) e zero hits porque o target estava frio. Os
testes CUDA focados passaram: `7` testes de co-execução, `1` de persistência
do sidecar e o benchmark sintético de H2D/D2H.

O ensaio de co-execução mediu CPU e uploads CUDA simultaneamente em oito
experts sintéticos da geometria Gemma4. A saída foi:

```text
COEXEC_PROFILE cpu_ms=925.491 gpu_ms=117.632 upload_ms=3.765 payload_bytes=3345440 gpu_fraction=0.9959
```

O sidecar produzido pelo ensaio continha `cpu_wall_ms=925.491047`,
`cuda_wall_ms=117.632127`, `cpu_overlap_bytes_per_sec=28918183.58` e
`transfer_overlap_bytes_per_sec=7107826903.09`, resultando em fração GPU
`0.9959`. Isso é uma medição sintética de uma geometria, não uma medição E2E
do modelo real. A ksia tinha apenas o manifesto `.pom` de Gemma4 no caminho
consultado, sem os GGUF das fatias, então as provas ignoradas
`executor_cuda_nvfp4_lote_preserva_geometria_do_gguf_real` e
`nvfp4_matches_llama_cpp_from_first_layer_and_stage` ficaram sem executar por
ausência do arquivo exigido. Não se atribui ganho de desempenho ao caminho
co-executado com base nesse ensaio.

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
`53decb4` e o ownership por camada chegam juntos. Uma camada cujos tensores de
experts tenham executor CUDA ou `CPUReference` disponível pode ser retirada do
llama.cpp. Dtypes sem cache device usam `CPUReference` e publicam os dtypes
concretos; uma combinação sem decoder externo é recusada antes da carga.
Somente uma limitação real do runtime mantém a camada nativa, com motivo
visível, nunca deixando a placa sem dono.
2. **Cobertura do marco.** NVFP4, Q2_K/Q3_K/Q4_K/Q6_K e, em seguida, IQ3_S/IQ2_S/IQ4_XS
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
parcial que pudesse remover um tensor sem executor. A prova na `ksia` executou
o caminho individual para IQ3_S, IQ2_S e IQ4_XS contra o decoder CPU e também
o caminho heterogêneo com gate IQ3_S, up IQ2_S e down IQ4_XS. Em todos os casos
`gate_up`, ativação e `down` ficaram sem primeira divergência na tolerância
`1e-4`. A prova E2E nos GGUF reais dos dois UD e a medição de produto ainda
estão pendentes.

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

### Diagnóstico diferencial IQ - 2026-08-25

O commit `d631442` tornou a comparação IQ diagnosticável por dtype e estágio:
o teste CPU informa o primeiro valor não finito, e a prova CUDA registra o
primeiro índice divergente em `gate`, `up`, ativação ou `down`, com valor atual,
esperado, erro absoluto e erro relativo. A fixture CPU isolada passou:

```text
CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom cada_iq_priorizado_tem_bloco_e_dequantizacao_determinados -- --nocapture
1 passed, 891 filtered out
```

A primeira tentativa remota carregou `--ignored`, mas esses testes não são
ignorados e a saída foi `0 passed, 907 filtered out`; esse resultado não foi
tratado como sucesso. A validação foi repetida na `ksia`, com
`CARGO_TARGET_DIR=/data/ftk-cuda-target` e
`CUDACXX=/usr/local/cuda-12/bin/nvcc`, e a build nativa CUDA concluiu.

No commit `e1a7ab5`, o teste correto chegou à salvaguarda e retornou
`BackendUnavailable("dtype GGML 22 (iq2_s) ainda não tem ponte CUDA IQ-quant")`.
Para investigar sem alterar o dispatch de produção, `86226c7` adicionou um
construtor somente de teste. Com ele, no mesmo target CUDA, a prova individual
retornou `1 passed` e o diagnóstico por estágio publicou:

```text
IQ iq3_s estágios: gate_up=None, activation=None, down=None
IQ iq2_s estágios: gate_up=None, activation=None, down=None
IQ iq4_xs estágios: gate_up=None, activation=None, down=None
```

A prova heterogênea `executor_cuda_iq_heterogeneo_preserva_gate_up_down_por_dtype`
também retornou `1 passed`. Não foi reproduzida a divergência histórica e não
houve mudança no kernel; a única mudança de produção desta fase é reabilitar
os três dtypes depois dessa evidência. O construtor de teste foi removido na
mesma entrega, para que a prova passe pelo dispatch normal.

No commit final `89156d9`, o dispatch normal repetiu a prova individual e
retornou `1 passed`; o diagnóstico por estágio e o teste heterogêneo também
retornaram `1 passed`. A suíte completa com
`cargo test -p node --features vulkan,cuda --bin pom` terminou com `891 passed`,
`6 failed` e `10 ignored`, de `907` testes. As seis falhas são:

- `gguf::tests::canonical_qwen35_nextn_metadata_and_tensor_names_prove_mtp`,
  `gguf::tests::mtp_is_proven_by_real_nextn_tensors_even_without_metadata` e
  `gguf::tests::read_layout_sees_nextn_predict_layers`: `GGUF truncado antes
  da seção de dados`;
- `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`:
  `N=2 entrada=0: 7,06 pernas por token, esperado 2`;
- `transfer::offer_latency_tests::an_offer_leaves_without_reading_the_whole_file`:
  `oferta demorou 27,110586968s; ela não pode depender do tamanho do arquivo`;
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`: metadata
  `llama.attention.head_count_kv_by_layer` com comprimento 16, esperado 1 ou 32.

Essas falhas não envolvem o dispatch IQ alterado. A build release foi executada
no mesmo checkout e terminou com código 0:

```text
CARGO_TARGET_DIR=/data/ftk-cuda-target CUDACXX=/usr/local/cuda-12/bin/nvcc cargo build --release --features vulkan,cuda
Finished `release` profile [optimized] target(s) in 25m 38s
```

Na mesma rodada, a suíte densa local foi executada com
`CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom`:
`879 passed`, `4 failed`, `9 ignored`, de `892` testes. As falhas observadas
foram:

- `gguf::tests::canonical_qwen35_nextn_metadata_and_tensor_names_prove_mtp`:
  `GGUF truncado antes da seção de dados` em `gguf.rs:1202:52`;
- `gguf::tests::mtp_is_proven_by_real_nextn_tensors_even_without_metadata`:
  `GGUF truncado antes da seção de dados` em `gguf.rs:1174:52`;
- `gguf::tests::read_layout_sees_nextn_predict_layers`:
  `GGUF truncado antes da seção de dados` em `gguf.rs:1325:38`;
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`:
  `array metadata 'llama.attention.head_count_kv_by_layer' tem comprimento 16; esperado 1 ou 32 (contagem original de blocos)` em `webui.rs:5114:61`.

Essas falhas ficaram fora dos arquivos de diagnóstico IQ. Elas não constituem
prova da ponte CUDA nem alteram a salvaguarda.

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
| 14 | Q6_K | implementado; prova local de formato e `gate_up` CPU; CUDA/E2E real pendentes |
| 15 | Q8_K | implementado e NÃO PROVADO |
| 16 | IQ2_XXS | implementado e NÃO PROVADO |
| 17 | IQ2_XS | implementado e NÃO PROVADO |
| 18 | IQ3_XXS | implementado e NÃO PROVADO |
| 19 | IQ1_S | implementado e NÃO PROVADO |
| 20 | IQ4_NL | implementado e NÃO PROVADO |
| 21 | IQ3_S | implementado e PROVADO por comparação CUDA diferencial |
| 22 | IQ2_S | implementado e PROVADO por comparação CUDA diferencial |
| 23 | IQ4_XS | implementado e PROVADO por comparação CUDA diferencial |
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

Na ponte CUDA, o subconjunto heterogêneo validado é Q4_K (12), Q6_K (14),
IQ3_XXS (18), IQ3_S (21), IQ2_S (22), IQ4_XS (23) e MXFP4 (39). Os kernels
recebem um descriptor por expert e papel, validam o tamanho da matriz segundo o
dtype e despacham para o decoder K, IQ ou MXFP4 correspondente. Isso preserva
o caminho IQ anterior e permite que gate, up e down usem dtypes diferentes; a
ausência de uma ponte com decoder externo em qualquer tensor recusa a carga e
informa o dtype bloqueador.

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

Quando existe uma ponte CUDA para a fonte selecionada, o `MoeRuntime` pode
iniciar uma thread de aquecimento assíncrona no carregamento do modelo. Ela
percorre os experts por camada e índice, pede ao próprio loader o payload
canônico de cada tensor e chama a mesma admissão do caminho quente. O dispatch
é por `MoeSource` e pelos metadados do tensor; não há ramo por arquitetura,
modelo ou versão de quantização. Em um GGUF heterogêneo, cada papel e dtype
recebe a mesma chave e descriptor que o forward usa.

O aquecimento é padrão para restaurar a VRAM pré-aquecida. O operador pode
desligá-lo explicitamente com `POM_MOE_WARMUP=0`, `false`, `off` ou `no` para
medir uma rota fria; `POM_MOE_WARMUP_MAX_MS` continua controlando o prazo, com
30.000 ms como padrão. A ordem dos experts no arquivo não é a ordem da rota
real, portanto o orçamento limita o preenchimento e a primeira rota cancela o
restante antes do forward.

O limite continua sendo o orçamento de VRAM medido, incluindo a reserva de
compute já reportada pelo `llama.cpp`. Quando a admissão devolve
`NoBudget`/`BusyAndNoEviction`, o aquecimento para com
`budget-exhausted`; os candidatos ainda não visitados ficam explicitamente
fora do aquecimento e continuam sujeitos ao LRU sob demanda. Ele não falha o
carregamento por o modelo inteiro não caber.

O prazo pode ser ajustado por `POM_MOE_WARMUP_MAX_MS`. A thread verifica o
prazo e um `AtomicBool` antes de cada candidato; `MoeRuntime::cancel_device_warmup`
sinaliza a parada e aguarda somente a admissão CUDA que já começou, sem remover
entradas admitidas. Logs de início e fim registram modelo, status, bytes
admitidos e motivo de parada. A telemetria
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

### Estado da travessia MoE - 2026-09-01

O layout separado dos experts K-quant segue `[hidden, intermediate, experts]`;
o loader agora preserva essa ordem, inclusive quando `hidden != intermediate`.
O callback valida nome, dtype, rank e dimensões contra a fonte e informa as
duas geometrias quando divergem. A fonte dos experts continua externa ao
buffer de pesos do llama.cpp: a medição da ksia encontrou 2.531,86 MiB nesse
buffer para um modelo com 256 experts, enquanto os payloads roteados ficam no
cache próprio.

O caminho heterogêneo agrupa as rotas em lote e cobre IQ3_XXS (18) mais MXFP4
(39), a combinação do `deepseek4`, na ponte CUDA e no decoder CPU de referência.
Os índices da rota são lidos como I32 em ordem token-major, inclusive valores
acima de 127. Uma combinação sem decoder externo é recusada antes da carga do
modelo, com os dtypes que a bloquearam; não há fallback nativo silencioso.
Ainda não há prova CUDA nesta máquina, porque não existe compilador CUDA local;
a prova diferencial e o E2E nos GGUF reais precisam ser executados na ksia.

### Fase 5.7: cache residente e execução CUDA multi-GPU - 2026-09-15

O diagnóstico do captain na ksia comparou `moe/q4_0` e
`dense/llama.cpp` no mesmo nó `5b035803`, com duas RTX 5060 Ti. O MoE mediu
`765 ms` p50 e `952 ms` p95 por chamada de decode, enquanto o denso mediu
`52 ms` e `93 ms`. A contagem `36 GPU / 0 CPU / 0 mixed` não provava que o
executor externo usava as duas placas: ela descrevia somente as camadas que o
llama.cpp alocou.

A causa raiz ficou confirmada no código próprio do POM, não no fork
`vendor/llama-cpp-sys-2/llama.cpp`: `cuda_kernels.cu` fixava
`kMoeCudaDevice = 0`, e todas as alocações, uploads, streams, eventos e
liberações do executor usavam esse device. Além disso, `DeviceExpertCache`
mantinha uma única conta de memória e um único allocator. O caminho denso, em
contraste, passa a lista de devices e o `tensor_split` do `FitPlan` para
`llama_model_params` antes de `llama_model_load_from_file`; o fork recebe essa
configuração pelo caminho nativo, mas o executor MoE não a recebia.

O conserto mantém a mesma topologia observada pelo contexto denso:

- `cuda_compute_bytes_by_device` agrupa os compute buffers por `CUDA<n>` e
  ignora `CUDA_Host`;
- `DeviceExpertCache::new_sharded` cria um orçamento, LRU e allocator por placa.
  O hash estável de modelo/camada/expert mantém todos os papéis do mesmo expert
  na mesma GPU e os leases carregam o ordinal para a ponte CUDA;
- as pipelines residentes e todas as entradas FFI recebem o ordinal explícito.
  O RAII CUDA seleciona a placa para cada chamada e restaura o device anterior;
- uma rota é agrupada por GPU, cada grupo roda em sua pipeline própria em
  paralelo e os vetores completos são somados no host. Assim os experts
  selecionados em placas diferentes não voltam a CUDA0 nem compartilham um
  único mutex de pipeline.

A segunda execução sem melhora também tinha uma causa de estado no caminho
  externo. O orçamento agregado era aplicado a uma única placa, de modo que o
  LRU expulsava experts residentes quando o working set não cabia na capacidade
  de CUDA0, mesmo havendo VRAM livre na CUDA1. Em paralelo, o aquecimento
  assíncrono continuava admitindo experts de camadas futuras e podia competir
  com a primeira rota, causando misses, eviction e chamadas que não terminavam
  no tempo esperado. O orçamento agora é por placa e a primeira rota real chama
  `cancel_device_warmup`, preservando as entradas já admitidas e deixando o
  working set da requisição controlar o cache. Hits posteriores retornam o
  mesmo handle e o mesmo ordinal sem novo upload.

Os testes de parser e cache cobrem a separação por `CUDA0`/`CUDA1`, a admissão
em ambos os shards e a reutilização do device de uma chave. Neste worktree,
`cargo test -p llama-engine` passou com `306` testes e `11` ignorados. Não há
GPU NVIDIA nem `nvcc` local, então a prova física de duas placas, a medição de
decode e a confirmação final da queda de 15x precisam ser repetidas pelo
captain na ksia com o perfil CUDA ligado. O fork nativo não foi alterado nesta
correção.

### Fase 5.8: hot path heterogêneo alinhado ao FreeToken - 2026-09-15

A telemetria mais recente da ksia mostrou que fila e setup do nó ficavam abaixo
de 1 ms, enquanto o backend heterogêneo gastava `978-1495 ms` no prefill e
`83-117 ms` por chamada de decode. Portanto os timeouts de scheduler não eram a
causa daquele trace. O caminho também carregava bytes do mmap para cada papel
antes de consultar o cache residente; um hit descartava essa cópia. Isso
contradizia a decisão do FreeToken de manter o working set de experts residente
no device e explica por que uma segunda requisição não ficava materialmente
mais barata.

O caminho de produção agora usa três fronteiras explícitas:

- `HeterogeneousTensorSpec` valida papel, dtype, geometria e escalas sem copiar
  payload. `HeterogeneousMoeBlock` consulta primeiro o `DeviceExpertCache` e só
  chama `load_expert_tensor` em um miss real;
- `HeterogeneousRoutePipeline` mantém descriptors, hidden, ids, pesos, offsets,
  ativações, resultados intermediários e saída alocados por device. O pipeline
  é compartilhado por todas as camadas do runtime e indexado por
  `(CUDA<n>, hidden, intermediate)`, evitando uma reserva grande por camada;
- os experts continuam particionados pelo hash estável de modelo, camada e
  expert. Cada grupo por ordinal é executado em sua própria pipeline dentro do
  `sum_parallel_device_outputs`, e grupos em `CUDA0` e `CUDA1` podem progredir
  simultaneamente. A contagem de camadas do llama.cpp não é usada como prova de
  utilização das duas placas: a confirmação em campo deve olhar os bytes
  residentes e os contadores por device.

O pipeline copia apenas os metadados de rota e o hidden a cada chamada. A
`cudaMemcpy` D2H da saída continua sendo a barreira necessária para devolver o
resultado ao callback, mas o `cudaDeviceSynchronize` adicional foi removido;
`sync_count=0` no perfil desse caminho. `alloc_ms` só aparece quando a
capacidade cresce, e não em cada token. A atualização é código CUDA próprio em
`crates/llama-engine/src/moe/cuda_kernels.cu`; o fork do llama.cpp continua
inalterado e fornece apenas o callback e os metadados.

O cancelamento do aquecimento também é observado depois que um candidato é
obtido. Assim, uma leitura que terminou depois da chegada de uma requisição
real não pode admitir bytes de camadas futuras e expulsar o working set ativo.
O caminho ainda permite que uma única admissão já em andamento termine, mas não
inicia outra depois do cancelamento. O teste
`warmup_cancelado_depois_de_obter_candidato_nao_admite_payload` fixa essa
fronteira.

Esta fase elimina cópias, alocações e sincronizações redundantes, mas não
transforma o particionamento de experts em tensor parallelism. Se todos os
experts escolhidos por um token caírem no mesmo shard, a outra placa ficará
ociosa nessa rota; o caminho denso do llama.cpp pode usar as duas para cada
matriz por meio de `tensor_split`. A medição na ksia deve separar esse efeito
de roteamento do custo de kernel antes de declarar throughput superior ao
llama.cpp ou ao FreeToken.

### Fase 5.9: caminho efetivo IQ3_XXS e kernel cooperativo - 2026-09-15

A telemetria do modelo `Mistral-Small-4-119B-2603-UD-IQ3_XXS-9c2b030b`
mostrou os dtypes GGML `18`, `22` e `23` juntos. Isso significa que o modelo
usa o `HeterogeneousCudaExecutor`, não o `IqQuantCudaExecutor` uniforme. A
otimização heterogênea da fase anterior ainda deixava o dot product escalar:
o `HeterogeneousRoutePipeline` atribuía uma linha a um bloco de 128 threads,
e cada thread repetia o loop completo sobre as colunas.
Além disso, cada thread do kernel IQ calculava sozinha o dot product completo
de uma linha, repetindo o mesmo trabalho dentro do bloco. Esse era o custo
local observado como `978-1495 ms` no prefill e `83-117 ms` por decode, não uma
espera de fila: a telemetria do nó registrou fila e setup abaixo de 1 ms.

O caminho IQ uniforme continua compartilhando o mesmo workspace persistente por
`(device, geometria)` usado pelos K-quants. O runtime cria um pipeline para
cada shard `CUDA<n>` do cache residente, agrupa as rotas pelo ordinal e executa
os grupos em paralelo; nenhum device é escolhido como `main_gpu` único pelo
executor.
Os IQs genéricos atribuem uma linha a um bloco de 128 threads e reduzem os
fragmentos do dot product em shared memory. Para o IQ3_XXS medido, cada linha
usa uma única warp de 32 threads: cada lane processa os oito valores de uma
fatia `(group, il)` e a soma usa shuffle. Gate e up são calculados na mesma
warp, reutilizando o hidden e reduzindo pela metade os blocos dessa etapa.
Isso reduz de quatro para uma warp o trabalho de coordenação da linha e elimina a árvore de shared memory, mantendo a sequência
gate/up, SiLU, down e agregação. O D2H síncrono continua sendo a barreira
necessária para devolver o resultado ao callback, mas o
`cudaDeviceSynchronize` adicional foi removido e o perfil publica
`sync_count=0`. IQ3_XXS agora é elegível para a ponte CUDA e valida a geometria
do payload antes do lançamento.

No IQ3_XXS, as 32 lanes cobrem os 256 elementos do superbloco em grupos de
oito. O `aux32`, os dois índices do codebook e os sinais são lidos uma vez por
fatia, em vez de reconstruir esses metadados para cada par de colunas. Os
outros IQs mantêm o decoder cooperativo genérico até terem a mesma prova
diferencial de layout.

O cache não copia a fonte fria em hits: a admissão continua sendo por hash de
modelo, camada e expert em shards independentes. Assim, uma segunda chamada
reutiliza o mesmo handle em CUDA0 ou CUDA1. O warmup é cancelado antes de cada
`llama_decode` que tem callback MoE, e novamente na primeira rota como defesa;
isso impede que uma admissão concorrente expulse o working set ou segure o
shard enquanto o grafo já está em execução;
se os contadores por device ainda mostrarem uma única placa em uso, o próximo
diagnóstico é a distribuição efetiva dos experts selecionados, não um timeout.

Com `POM_MOE_CUDA_PROFILE=1`, `moe_executor_diagnostics.cuda_device_calls` agora
registra por ordinal o número de chamadas, experts processados e tempo de
kernel/parede. Esse campo deve ser comparado com
`device_cache_resident_bytes_by_device`: bytes em uma placa sem chamadas
indicam somente capacidade residente, enquanto chamadas em `CUDA0` e `CUDA1`
confirmam participação efetiva da rota.

Esta correção é código CUDA próprio em `cuda_kernels.cu`; não altera o fork do
llama.cpp. O fork continua fornecendo o callback externo e os metadados de
memória. A comparação contra `dense/llama.cpp` e contra FreeToken precisa ser
repetida na ksia com o build CUDA novo: este worktree não possui `nvcc` nem
GPU, então não há uma alegação local de throughput.

### Fase 5.11: MMVQ q8 também no executor heterogêneo - 2026-09-15

O JSON da telemetria fechou a causa do intervalo de cinco segundos. No prefill
de 45 tokens, `node_moe_forward_ms=4706,821` dentro de
`node_backend_enqueue_ms=4994,464`; `node_queue_ms=0,038`,
`node_lock_wait_ms=0`, `node_scheduler_wait_ms=0,012`, rede `0 ms` e a cópia
D2H `0,394 ms`. Portanto não havia um timeout de cinco segundos no pipeline.
O campo `node_moe_forward_ms` é o callback externo síncrono que o llama.cpp
espera terminar antes de devolver o prefill.

O mesmo JSON mostra o custo que dominava esse callback: do fim do prefill ao
fim do decode entraram mais `2406` misses/admissions, `2399` evictions e
`10,5 GB` de uploads para os devices. O cache residente tinha aproximadamente
`8,25 GB` em CUDA0 e `8,17 GB` em CUDA1, e o warmup já tinha admitido `3766`
experts antes de atingir o orçamento. Isso é thrashing por working set, não
fila de scheduler. Como os leases são liberados ao fim de cada camada e o
decode visita novas rotas, uma nova requisição pode expulsar os experts usados
no prefill; por isso repetir o prompt não garante hit. O `warmup_status` foi
`budget-exhausted`, não `timed-out`, e o warmup é assíncrono, então seus
`4726 ms` não representam uma espera bloqueante no decode.

O pipeline heterogêneo agora quantiza hidden e ativação em `block_q8_1` usando
buffers persistentes, chama os `vec_dot_*_q8_1` canônicos do fork para Q4/K/IQ
e faz uma warp de 32 lanes por linha. Isso elimina os quatro warps redundantes
do bloco escalar e usa dp4a também quando gate/up/down têm dtypes diferentes.
O lançamento passa de quatro para seis kernels (duas quantizações, gate/up,
ativação, down e agregação), sem `cudaDeviceSynchronize`; a cópia D2H continua
sendo a barreira de conclusão.

Para confirmar o restante na ksia, basta iniciar o executor MoE. Os
diagnósticos são automáticos e devem mostrar `cuda_device_calls` para CUDA0 e CUDA1 e
separar `kernel_ms` de `cuda_call_wall_ms`; os contadores cumulativos de cache
devem ser comparados por delta entre prefill e decode. A otimização q8 reduz o
compute do callback, mas não pode fazer caber em 16 GB um working set de
experts maior que a VRAM: se os misses continuarem altos, o próximo ajuste é
afinidade/prefetch por camada, não um timeout maior.

### Fase 5.12: admissões heterogêneas paralelas por GPU - 2026-09-15

O caminho quente heterogêneo agora faz duas passagens sobre a rota. A primeira
consulta o cache e agrupa cada miss pelo mesmo hash de afinidade que escolhe o
shard. Uma thread por `CUDA<n>` lê os slices do mmap e faz os uploads em série
naquela GPU; threads de shards diferentes trabalham em paralelo. A segunda
passagem monta os descriptors somente depois de todas as admissões, mantendo
os leases vivos durante o forward para impedir eviction prematura.

Cada upload usa diretamente a fatia do mmap. O caminho CUDA não cria mais um
`Vec<u8>` intermediário para cada tensor, reduzindo uma cópia de dezenas de GB
na preparação de uma rota fria.

As especificações dos experts da mesma camada também são montadas em lote, em
uma travessia única do inventário. Isso evita repetir a busca de entradas e
geometria para cada expert selecionado durante o prefill.

Isso alinha a admissão ao agrupamento que já existia no kernel: o cache não
escolhe mais uma única fila global para dezenas de GB de H2D. A mudança é
somente no crate Rust e usa o allocator CUDA existente, que seleciona o ordinal
explicitamente em cada upload; o fork do llama.cpp não foi alterado. O ganho
de parede e a ocupação efetiva de cada placa ainda precisam ser medidos na
ksia. Com `POM_MOE_CUDA_PROFILE=1`, comparar `device_cache_upload_ms` por delta
com `cuda_device_calls` em `CUDA0` e `CUDA1`, sempre com `POM_MOE_WARMUP` ausente
ou desabilitado para medir a rota fria sem poluição especulativa.

Além disso, `cancel_device_warmup` agora aguarda o handle da thread depois de
sinalizar o `AtomicBool`. A espera cobre somente a admissão que já entrou em
`cudaMemcpy` ou no mutex do shard; a thread verifica o cancelamento antes de
cada próximo candidato. Assim o callback não pode começar o forward atrás de
uma varredura de warmup ainda ativa, e não existe espera fixa de cinco segundos
no caminho quente.

### Comparação isolada em uma máquina - 2026-09-15

Uma execução real no nó `5b035803`, com duas RTX 5060 Ti e sem salto de
pipeline, confirmou que o transporte não é o gargalo. O MoE usou 46 tokens de
prefill e gerou 188 tokens: `5667,7 ms` de prefill (`8,1 tok/s`) e
`35009,3 ms` de decode em 95 chamadas (`5,3 tok/s`), com p50/p95 de
`347,2/617,1 ms` por chamada. A rede foi `0-16 ms`, a fila e o setup ficaram
abaixo de `2 ms`, e `99%` do tempo ficou em `llama_decode` local.

O controle `dense/llama.cpp` no mesmo nó gerou 178 tokens a `18,5 tok/s`, com
`749,7 ms` de prefill (`29,2 tok/s`) para 22 tokens e p50/p95 de
`125,4/129,8 ms` por chamada de decode. A comparação não tem prompt idêntico
(`46` contra `22` tokens), portanto é evidência de ordem de grandeza, não uma
medição pareada final. Mesmo assim, o MoE ficou cerca de `3,6x` mais lento por
token no decode e `3,6x` mais lento no prefill normalizado, apesar de anunciar
`36 GPU / 0 CPU / 0 mixed`; esse contador descreve camadas do llama.cpp e não
prova utilização de CUDA0 e CUDA1 pelo executor externo.

A execução ainda não tinha `POM_MOE_CUDA_PROFILE=1`, então não permite dizer
qual ordinal recebeu as rotas nem separar kernel, H2D, D2H e misses do cache.
Para o próximo ensaio na ksia, o perfil deve ser ligado desde o arranque e a
telemetria deve incluir `cuda_device_calls` e
`device_cache_resident_bytes_by_device`. A ausência de transporte e de espera
de scheduler já elimina timeouts como hipótese principal; a hipótese restante
é o custo do callback por camada, das cópias host/device e do decoder IQ3_XXS
customizado, a comparar com as chamadas CUDA otimizadas do caminho denso.

### Fase 5.10: caminho IQ3_XXS alinhado ao MMVQ CUDA - 2026-09-15

O decoder cooperativo anterior ainda fazia a aritmética do codebook em float.
Isso divergia do caminho MMVQ do fork, que primeiro quantiza a ativação para
`block_q8_1` e usa `vec_dot_iq3_xxs_q8_1` com `dp4a`. O pipeline IQ3_XXS agora
mantém buffers q8 persistentes para hidden e ativação, quantiza cada linha em
uma warp e usa o decoder canônico do checkout ativo do fork. Gate e up usam a
mesma warp e down reutiliza a ativação q8. O `build.rs` inclui
`ggml/src/ggml-cuda/vecdotq.cuh` do mesmo `LLAMA_SOURCE` usado pelo binding;
nenhum arquivo do fork foi alterado.

O mapeamento de lanes segue a fórmula MMVQ para IQ3_XXS: oito chunks q8 por
superbloco, quatro superblocos cobertos em paralelo por warp, e cada lane
avança de quatro em quatro superblocos. Isso evita deixar 24 lanes ociosas no
dot product. As verificações de geometria e a capacidade persistente continuam
impedindo overflow ou realocação no caminho estável.

Duas medições posteriores na mesma máquina de duas RTX 5060 Ti confirmaram o
perfil do gargalo antes desta otimização. Para 16 tokens de prompt e 15 de
saída, o prefill levou `5323 ms` e o decode teve p50/p95 de `254,5/382,0 ms`.
Para 45 tokens de prompt e 91 de saída, o prefill levou `4998 ms` e o decode
teve p50/p95 de `277,1/470,7 ms`. Rede, fila e espera do mutex do motor ficaram
em torno de `0-4 ms`, e o setup caiu de `152 ms` na primeira carga para menos
de `1 ms` na seguinte. O prefill quase fixo perto de cinco segundos, mesmo com
prompts de tamanhos diferentes, é trabalho de compute dos 36 callbacks, não
timeout ou espera de transporte. O caminho q8/dp4a ataca justamente esse
trabalho, mas a confirmação de throughput e de correção numérica precisa ser
feita na ksia com `POM_MOE_CUDA_PROFILE=1`.

### Fase 5.13: lote CUDA para admissões e rotas repetidas - 2026-09-15

O JSON mais recente da ksia não mostra um timeout escondido de cinco segundos.
No mesmo nó, `node_lock_wait_ms` foi `0`, a fila e o scheduler somaram poucos
milissegundos e o transporte somou `54 ms`. O trabalho está em
`node_moe_forward_ms` (`6445,8 ms` no prefill) e nas chamadas de decode, não em
um `wait` do pipeline. O atraso observado no primeiro token acompanha o
prefill local de `6656,5 ms`.

O diagnóstico do cache identifica a causa operacional: em 320 chamadas de
decode houve `94522` misses/admissões, `90762` evictions e aproximadamente
`412 GB` de uploads cumulativos. Os shards estavam equilibrados em `CUDA0` e
`CUDA1` (`8242/8165 MB` residentes), portanto as duas placas eram usadas para
residência, mas o working set excedia o orçamento e era recarregado. O
`runtime_forward_ms` e `device_cache_upload_ms` são contadores cumulativos de
callbacks e devices; não são um único wait de cinco segundos.

O caminho quente agora agrupa os misses por shard e faz uma única consulta de
orçamento por GPU. O `DeviceAllocator` ganhou `upload_many`; o allocator CUDA
enfileira todas as alocações e cópias no stream default e sincroniza somente
uma vez por lote. Evictions usam `cudaFreeAsync`, mantendo a ordem do stream e
evitando a sincronização global de `cudaFree` em cada tensor. A sincronização da
cópia continua necessária porque os bytes vêm de uma fatia de `mmap` que pode
ser liberada assim que a admissão retorna. O fork do llama.cpp não foi
alterado.

Na mesma rota, o executor deduplica somente pares com o mesmo descriptor de
expert e o mesmo token. O kernel calcula o FFN uma vez e a agregação aplica
novamente cada peso através do mapa de rotas, preservando a semântica do
router. Isso reduz trabalho quando o top-k repete um expert para um token sem
alterar a soma de rotas distintas.

A telemetria `d5eeeaa` ainda foi gerada antes de `fef71ae`, que corrigiu a
escala compartilhada do `block_q8_1` no kernel de quantização. A saída sem
sentido daquela execução não é uma medição válida de qualidade do modelo; a
ksia precisa repetir o mesmo prompt com o SHA que contém essa correção e esta
otimização de lote. Este ambiente não possui `nvcc` nem GPU para medir a
redução final de prefill/decode localmente.

### Fase 5.14: warmup padrão e coexecução heterogênea medida - 2026-09-15

Os logs da ksia confirmaram uma regressão de configuração: o processo que
produziu a telemetria tinha `POM_MOE_WARMUP` ausente e, por isso, o warmup
estava `disabled`. O comportamento agora volta a ser o pré-aquecimento padrão.
`POM_MOE_WARMUP=0`, `false`, `off` ou `no` continua disponível para ensaios
explicitamente frios. O warmup permanece limitado pelo orçamento independente
de cada GPU; em uma 5060 Ti ele pode terminar com `budget-exhausted` antes de
carregar todos os experts, porque o modelo inteiro não cabe na VRAM.
Para aproveitar melhor esse orçamento, a enumeração do warmup é expert-major:
ela alterna as camadas para cada expert antes de avançar ao próximo expert.
Assim, quando a VRAM não comporta o modelo inteiro, todas as camadas recebem
um working set residente em vez de somente o prefixo do modelo.

O intervalo de aproximadamente cinco segundos não era timeout: no trace
pareado, `node_lock_wait_ms`, fila, scheduler e transporte ficaram próximos de
zero, enquanto `node_moe_forward_ms` consumiu praticamente todo o prefill. O
executor heterogêneo passa agora a aceitar o mesmo `BandwidthProfile` medido
que o NVFP4. Experts com todos os tensors residentes continuam na GPU; misses
são divididos por expert entre rotas CPU e GPU, executadas em paralelo e
somadas com a mesma largura de saída. A consulta de residência não cria lease
nem toca LRU, para que o planejador não falsifique hits ou impeça eviction.

Os diagnósticos são automáticos quando o executor CUDA MoE está ativo;
`POM_MOE_CUDA_PROFILE` não é necessário e não altera a execução. O sidecar
`<modelo>.cuda-profile.json` com o campo `bandwidth` continua sendo necessário
para habilitar a divisão medida entre as lanes CPU e GPU. Sem esse arquivo, o
runtime mantém a política exclusivamente CUDA e não fabrica uma taxa para a
CPU escalar Rust. Na ksia, a validação deve repetir o mesmo prompt com o
warmup padrão, persistir o sidecar medido para este modelo e comparar prefill,
decode, corretude textual e deltas de uploads.

### Fase 5.15: staging host pinned na saída heterogênea - 2026-09-16

A telemetria da ksia após o pipeline q8 separou o custo que ainda dominava o
callback. No prefill, o kernel CUDA acumulou aproximadamente `177 ms`, mas as
transferências D2H acumularam `6317 ms`; no decode, foram aproximadamente
`680 ms` de kernel contra `12801 ms` de D2H. O resultado correto precisava
continuar sendo escrito no tensor host do callback, então o contrato externo
não permitia simplesmente deixar a saída na VRAM. O custo observado vinha do
`cudaMemcpy` síncrono diretamente para a memória pageable que o llama.cpp
entrega ao callback.

Cada `HeterogeneousRoutePipeline` agora reserva um `cudaMallocHost` persistente
com a mesma capacidade do output device. A cópia D2H vai para esse staging
pinned e, depois da barreira de conclusão, um `memcpy` curto entrega o resultado
ao buffer host original. O staging é liberado junto com os buffers persistentes
da pipeline e cresce somente quando a geometria do lote muda. Isso remove o
staging implícito repetido do runtime CUDA sem alterar a semântica do callback,
a agregação de rotas ou a captura de grafo do llama.cpp.

O ganho precisa ser confirmado na ksia comparando `cuda_d2h_ms`,
`cuda_call_wall_ms`, `node_moe_forward_ms` e a corretude textual com o mesmo
prompt. A meta de superar o caminho denso em 1,5x ainda não está comprovada.

### Fase 5.16: tiles de quatro warps no kernel heterogêneo - 2026-09-16

O kernel q8 heterogêneo mantém uma warp por linha lógica, mas agora agrupa
quatro linhas consecutivas em cada bloco. O índice é calculado no espaço global
de `(expert, linha)`, e não agrupando todas as linhas de um expert em um único
bloco. Assim a grade continua tendo milhares de blocos independentes mesmo
quando a rota contém poucos experts, enquanto o custo de agendamento de blocos
é reduzido em quatro vezes. Cada warp conserva sua redução shuffle, seu
descriptor e seu resultado; gate/up e down usam a mesma geometria.

Esta mudança é uma hipótese de otimização do código CUDA próprio e não altera o
fork do llama.cpp nem o contrato do callback. Ela só pode ser mantida depois de
comparar na ksia o throughput frio e aquecido com `6d2d348c`/`41cf7cfa`; uma
regressão deve ser revertida imediatamente.

## Defeitos anotados

- As medições de throughput e de divisão prefill/decode ainda não são possíveis
  neste ambiente local sem obter um modelo e iniciar um runtime próprio.
- A bateria CUDA filtrada ainda tem uma falha numérica NVFP4 histórica, não
  introduzida pelo aquecimento. Em `ef36059` e em `c292403`, o teste isolado
  `executor_cuda_nvfp4_lote_de_rotas_agrega_tokens_no_device` reproduziu
  `13.464294 != 164.57135`. A prova IQ de `86226c7` não reproduziu as antigas
  diferenças IQ; os três dtypes passaram a prova diferencial e não devem ser
  protegidos por uma tolerância mais larga.
