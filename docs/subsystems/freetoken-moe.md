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

O override `POM_INFERENCE_MODE` aceita `auto`, `dense` ou `moe`. `dense` força o
caminho existente mesmo para um GGUF MoE. `moe` recusa um modelo sem
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

### Fase 5: co-execução CPU/GPU ainda não ativa

A investigação dos warnings mostrou que a política de banda e o double buffer
existiam apenas como scaffolding de testes: não havia perfil medido carregado
no runtime e não havia executor GPU para os pesos Q4_0 do motor próprio. O
`llama.cpp` tem seu próprio backend GPU para o transformer, mas não expõe uma
lane que execute os experts Q4_0 mmapados por `MoeRuntime`.

Esse scaffolding foi removido do binário para não sugerir que a co-execução
acontece. O caminho MoE que existe hoje é CPU para router e experts, com cache
LRU global; a fase 5 só poderá ser retomada quando houver um executor GPU
concreto e uma fonte de medições simultâneas CPU/transferência. Nenhum ganho de
desempenho ou sobreposição CPU/GPU é afirmado.

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
