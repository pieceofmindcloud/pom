# Relatório de passagem: NaN no MoE heterogêneo do Ornith-1.5-35B

Data da passagem: 2026-08-27

## 1. Sintoma e reprodução

O modelo que falha é o Ornith-1.5-35B, identificado como arquitetura
`qwen35moe`, em GGUF heterogêneo Q4_K + Q6_K. A fatia observada serve as
camadas 0-19 e captura `l_out-19`. O inventário da execução tinha
`expert_count=256`, `expert_used_count=8` e `expert_feed_forward_length=512`.
O metadata do GGUF informa camadas recorrentes em
`qwen35moe.attention.recurrent_layers = [1,1,1,0,...]`, com intervalo de
atenção plena 4. A camada 1, onde a falha aparece, é recorrente.

Com o caminho do motor MoE selecionado na implantação e o processo iniciado
com `POM_MOE_ROUTE_DIAGNOSTICS=1`, a inferência falha com:

```text
hook MoE do llama.cpp falhou: tensor ffn_moe_weights da arquitetura qwen35moe,
camada 1, token 0, slot 0, expert 0 tem valor não finito (NaN)
```

O erro foi observado repetidamente na primeira posição: camada 1, token 0,
slot 0, expert 0. A mensagem só pode ser gerada por
`normalize_moe_route`, em `crates/node/src/inference/llm.rs:628-670`,
especificamente no teste de finitude de `weights` em 658-663. Essa função só
é chamada pelo callback MoE.

Há uma ressalva importante sobre a suposta determinismo: a evidência posterior
em `/tmp/moe-evidencia.txt` mostrou uma execução que respondeu corretamente
depois de uma recarga, mas essa instância não registrou aquecimento do cache
MoE residente e estava sem o executor externo. Portanto esse sucesso não prova
o MoE; ele prova apenas que o caminho nativo funcionou naquela instância.

Na evidência da ksia, o binário em execução era derivado de `686f839`, estava
no mesmo inode implantado às 08:33, o processo tinha
`POM_MOE_ROUTE_DIAGNOSTICS=1`, `RUST_LOG=info`, não tinha processo filho e o
callback realmente rodava, pois a mensagem de `normalize_moe_route` apareceu.
Mesmo assim, os arquivos `/tmp/pom-ksia-node-1.log` e
`/tmp/pom-ksia-node-2.log` tinham zero ocorrências de
`POM_MOE_ROUTE_DIAGNOSTICS`.

## 2. Correções da ponte já tentadas

As quatro tentativas de runtime foram feitas sem a prova diferencial CUDA
passar. O fato comum foi a persistência do NaN na primeira posição, não uma
prova de que cada hipótese abaixo seja falsa.

1. `899f503`, `fix: permitir K-quants heterogeneos no executor CUDA`.
   Ligou a decisão por tensor e permitiu que o executor externo recebesse
   K-quants heterogêneos. A hipótese era que selecionar o executor por um
   dtype global estava deixando a combinação Q4_K/Q6_K fora do caminho correto.
   O NaN continuou. Isso descarta essa mudança como solução suficiente, mas
   não descarta erro de payload, escala ou inventário depois do dispatch.

2. `f3cb1bb`, `fix: corrigir rota e reativar CUDA MoE heterogeneo`.
   Alterou a transposição da rota do llama.cpp para o layout esperado pelo POM
   e reativou a publicação CUDA heterogênea. A hipótese era que a ordem
   expert-major versus token-major estivesse corrompendo os ids e pesos.
   O NaN continuou byte a byte no mesmo primeiro elemento. Isso torna a
   transposição isolada insuficiente; ainda não prova que toda geometria e
   toda memória estejam corretas.

3. `74de697`, `fix: materializar rota externa do MoE antes do callback`.
   Materializou a rota externa com `ggml_cont` antes do reshape/callback. A
   hipótese era que apenas verificar `ggml_is_contiguous` não garantia que o
   produtor tivesse terminado de preencher a área para a cópia entre
   backends. O NaN continuou. A materialização, sozinha, não resolveu e a
   origem da área lida ainda não foi observada.

4. `8fdd064`, `fix: carregar sidecars no executor MoE heterogeneo`.
   Alterou os loaders K e heterogêneo para procurar `.scale` e `.input_scale`,
   transportou as escalas no payload/descriptor e aplicou-as nos estágios do
   expert. Também deixou o inventário aceitar `ffn_gate_up_exps.weight` junto
   da presença dos tensores separados, priorizando o fundido em vez de contar
   os duplicados como outra rota. A hipótese era exatamente a diferença entre
   o Gemma NVFP4 que funciona e o Ornith heterogêneo que falha: escalas ausentes
   e inventário incompatível. O NaN continuou na execução testada. Sem uma
   linha de diagnóstico do callback, isso não prova que o sidecar foi lido,
   aplicado ou associado ao tensor correto.

`f02a150` não foi uma quinta tentativa numérica. Foi a correção de compilação
do ABI depois que os sidecars aumentaram o descriptor. Em
`crates/node/src/inference/moe/cuda.rs:271-292`, Rust declara
`data`, `bytes`, `dtype`, `scale`, `input_scale`, `secondary_scale` e
`secondary_input_scale`, com offsets 0, 8, 16, 20, 24, 28 e 32 e tamanho 40.
`cuda_kernels.cu:49-81` repete a mesma ordem e mantém as asserções de offset e
tamanho. O build CUDA de `8fdd064` falhava em 32 bytes; `f02a150` corrigiu
isso, mas ainda não houve prova numérica do Ornith.

`686f839` trata aliases de cabeças/residual do DeepSeek4
(`l_last-N` versus `l_out-N`) e não é uma correção do payload MoE. Ele explica
o binário observado na ksia, mas não deve ser contado como validação do
executor externo.

## 3. Iterações de instrumentação

1. `b403859`, `feat: instrumentar origem da rota MoE`, criou a coleta detalhada
   de ids, pesos, contagens, bytes, dtypes e amostras do token zero. O cálculo
   acontecia depois da validação de ponteiros e da geometria. Nos quatro
   caminhos que falhavam primeiro, o callback retornava antes da coleta. Não
   havia evidência de entrada bruta.

2. `046a42b`, `fix: tornar diagnostico da rota MoE observavel`, adicionou
   `log_moe_callback_entry` e tirou o filtro que exigia uma camada específica
   para a primeira amostra. Também manteve a regra de amostrar a primeira
   chamada e rotas não finitas. Ainda assim a amostra detalhada dependia de
   chegar depois das validações; a entrada não identificava um retorno que
   abortasse antes dela.

3. `b182fa5`, `fix: registrar saídas do callback MoE`, colocou
   `callback_entry` no início do callback e adicionou `callback_exit` para cada
   `return false`, com motivos como `null_callback_argument`,
   `invalid_expert_count_used`, `hidden_geometry_overflow`,
   `route_geometry_overflow`, `route_normalization_failed`, falhas de metadata
   e `forward_failed`. A entrada passou a registrar apenas ponteiros e
   contagens crus, sem dereferenciar o descriptor. O teste unitário que fixa
   esse formato está em `llm.rs:735-761` e passou.

O problema restante não é mais uma ausência de caminho no callback. A evidência
da ksia mostrou zero linhas mesmo com a mensagem posterior de
`normalize_moe_route`, e confirmou que o callback entrou. Logo, a etapa que
precisa ser assumida agora é provar por que o evento `tracing::warn!` não chega
ao destino observado. Ainda não existe prova executável dessa cadeia; não se
deve iniciar outra correção numérica antes dela.

## 4. Estado atual no branch compartilhado

No momento da inspeção, `origin/feat/implantar` estava em `62b5f8e`, sobre
`686f839` e `b182fa5`. O código relevante permanece:

- `llm.rs:120-128`: `POM_MOE_ROUTE_DIAGNOSTICS` é lido por `OnceLock` e só
  valores `1`, `true`, `yes` e `on` habilitam o diagnóstico.
- `llm.rs:191-207`: `callback_entry` emite os argumentos crus. A chamada vem
  logo após a leitura da flag em `llm.rs:272-284`, antes da checagem de
  nulidade, sem ler o descriptor.
- `llm.rs:286-304`: a macro de saída registra os mesmos seis ponteiros e as
  contagens `n_tokens`/`hidden_width` antes de retornar `false`.
- `llm.rs:306-499`: todos os retornos de erro do callback estão associados a
  um motivo estável. O forward bem-sucedido continua sem evento de saída.
- `llm.rs:628-670`: `normalize_moe_route` valida o tamanho, transpõe de
  expert-major usando `source = slot * n_tokens + token` e recusa peso não
  finito. Essa função não foi alterada nesta passagem.
- `format_heterogeneous.rs:255-315`: se há tensor fundido, ele determina a
  geometria e os tensores separados não são tratados como uma segunda rota;
  o loader aceita a coexistência.
- `format_heterogeneous.rs:471-493`: o payload carrega as escalas primária,
  secundária e de entrada para gate/up/down.
- `cuda.rs:271-292` e `cuda_kernels.cu:49-81`: o descriptor Rust/CUDA tem os
  sete campos na mesma ordem e asserções campo a campo.
- `block.rs:1100-1149`: o bloco carrega o payload, aloca/resgata o expert
  residente e preenche o descriptor com as quatro escalas.

A única mudança feita por este trabalhador nesta etapa foi `b182fa5`, mais a
documentação correspondente e este relatório. Nenhum kernel CUDA, loader,
descriptor ou normalização foi alterado nesta passagem.

## 5. Evidências contrastantes sólidas

- O Gemma4-26b-NVFP4 funciona no motor MoE na ksia. O Ornith falha. O caso do
  Gemma usa NVFP4, enquanto o Ornith usa Q4_K + Q6_K heterogêneo. Isso é uma
  diferença factual de formato, mas não identifica sozinho a falha.
- O caminho NVFP4 lê explicitamente sidecars em `format_nvfp4.rs:530-535` e
  aplica a escala de entrada no decoder em `format_nvfp4.rs:569-587`.
  Antes de `8fdd064`, os caminhos K/heterogêneo não carregavam esses sidecars
  no payload e o descriptor não tinha `input_scale`.
- O GGUF do Ornith contém tensores fundidos
  `blk.N.ffn_gate_up_exps.weight` e também tensores separados
  `ffn_gate_exps`/`ffn_up_exps`, além de companheiros `.scale` e
  `.input_scale`. A coexistência foi confirmada no log de carga, não apenas
  inferida do código.
- O GGUF declara `[1,1,1,0,...]` para camadas recorrentes e intervalo pleno 4;
  a camada 1 do erro é recorrente. O log de carga confirmou essa filtragem no
  KV cache.
- O carregamento registrou resoluções `resolve_fused_ops` para DeepSeek V4 HC
  pre, comb e post. Isso confirma que fusões estão habilitadas no fork, mas não
  prova que a operação específica do callback recebeu layout diferente.
- A evidência de logging é sólida: `RUST_LOG=info` deveria aceitar `warn`, a
  mensagem de normalização prova que o callback entrou, não havia filho, e os
  arquivos examinados não continham as linhas. O destino examinado pode,
  contudo, não ser o destino efetivo do subscriber.

## 6. Hipóteses abertas e falsificação

### 1. O destino observado não recebe o subscriber

Esta é a hipótese mais forte para o Fato 1. `main.rs:341-346` instala um
`tracing_subscriber::fmt` com `EnvFilter` e `.with_target(false)`, enquanto
`runtime.rs:1169-1195` escreve o arquivo do servidor por `append_log`, que só
recebe mensagens explícitas do supervisor. Não há código ligando eventos
`tracing` a esse arquivo. O writer padrão do `fmt` deve ser conferido no
processo implantado. Além disso, `respawn.rs:40-44` cria o novo processo com
stdout e stderr em `Stdio::null()`, o que pode apagar a saída depois de um
restart.

Falsificação: executar uma prova mínima no mesmo binário que instala o
subscriber real, dispara um `warn!` com target `moe.route_diagnostic` e
verificar separadamente stderr, stdout, o arquivo `infer_server_<porta>.log` e
os descritores `/proc/<pid>/fd/1` e `/proc/<pid>/fd/2`. Se o evento aparecer no
stderr/stdout capturado, mas não no arquivo, a causa é o destino do log.

### 2. A flag está presente no ambiente, mas não habilita o gate

`llm.rs:120-128` usa `OnceLock` e compara exatamente os valores aceitos. A
evidência informou que a variável estava presente, mas não registrou seu valor.
Uma inicialização anterior com valor ausente ou um valor diferente de
`1/true/yes/on` faria o callback seguir sem emitir. Em um processo normal que
herda a variável desde o início, a primeira chamada deveria ler o valor
correto, portanto esta hipótese é mais fraca que a do destino.

Falsificação: registrar uma vez, fora do callback ou em uma prova dedicada,
o valor reconhecido (`enabled=true/false`) e a ordem de inicialização, e chamar
o callback com a variável definida antes do primeiro acesso. O teste deve
verificar que o evento aparece com `POM_MOE_ROUTE_DIAGNOSTICS=1`.

### 3. Filtro de target adicional

`EnvFilter` é construído com `RUST_LOG=info`; por nível, `warn` deveria passar.
`.with_target(false)` só omite o nome do target na formatação e não deveria
descartar o evento. Ainda assim, outro subscriber, uma diretiva de módulo ou
um writer customizado poderia filtrar `moe.route_diagnostic`.

Falsificação: imprimir um `warn!` de target padrão e um `warn!` com o target
MoE imediatamente no mesmo ponto, e capturar o `EnvFilter` efetivo. Se apenas
o target MoE sumir, testar uma diretiva explícita
`moe.route_diagnostic=warn`.

### 4. Redirecionamento de stdout/stderr durante restart ou carga do llama.cpp

O `llama.cpp` pode escrever em seus próprios callbacks de log, mas não há
prova no código Rust de que ele substitua o subscriber. A possibilidade de
descritor duplicado/nulo continua aberta, especialmente pelo
`respawn_self` que zera stdout/stderr.

Falsificação: comparar os links de `/proc/<pid>/fd/1` e `/proc/<pid>/fd/2`
antes e depois da carga, executar o teste de `warn!` após carregar o modelo e
capturar diretamente os dois descritores. Se o fd mudar ou apontar para
`/dev/null`, o problema é o redirecionamento, não o callback.

### 5. Expert compartilhado contado como roteado

O expert compartilhado `ffn_*_shexp` deve ficar fora do inventário roteado e
ser executado pelo `build_ffn` nativo. Se ele for contado entre os 256 experts
roteados, o índice 0 pode apontar para uma região sem preenchimento, coerente
com NaN na primeira posição.

Falsificação: com o logging funcionando, comparar por camada a lista de nomes
do descriptor, `expert_count`, os ponteiros e bytes de cada payload, exigindo
que nenhum `shexp` esteja entre `ffn_*_exps`. O GGUF de teste deve mostrar a
contagem efetiva 256 sem o compartilhado.

### 6. Layout fundido e sidecars associados ao componente errado

A coexistência de fundido e separado, mais `.scale`/`.input_scale` em cada
tensor, pode deixar gate/up usando offsets ou escalas de componentes errados.
O loader atual prioriza o fundido em `format_heterogeneous.rs:255-270` e monta
escalas em 471-493, mas ainda não há prova numérica do Ornith.

Falsificação: o callback deve registrar nomes, dtypes, dimensões, bytes e
escalas de cada papel; comparar cada componente contra a geometria GGUF e
contra uma leitura independente dos sidecars. O teste diferencial CUDA precisa
passar sem NaN e dentro da tolerância publicada.

### 7. Passo físico Q4_K/Q6_K ou ABI ainda divergente na máquina CUDA

Os códigos Rust/CUDA declaram 144 bytes por linha para dtype 12 e 210 para
dtype 14 em `cuda.rs:1066-1071`, mas a asserção de ABI só cobre o descriptor,
não a semântica de cada payload. Uma divergência de stride ainda poderia
atingir o primeiro bloco.

Falsificação: compilar com `nvcc`, imprimir o dtype, bytes, colunas e stride
por payload, e executar os testes diferenciais por estágio contra o decoder
CPU. A prova não pode ser substituída por `cargo test` no macOS.

## 7. Armadilhas e o que não repetir

- A regressão local verde nunca cobriu `cuda_kernels.cu`; o macOS compila o
  crate Rust, não o kernel CUDA. O erro de 32 versus 40 bytes só apareceu na
  ksia. Não tratar `cargo test` local como build CUDA.
- O sucesso após recarga não foi sucesso do MoE: a instância estava sem o
  aquecimento/admissão do executor externo. Não usar esse caso como prova.
- A presença de `POM_MOE_ROUTE_DIAGNOSTICS` em `/proc/<pid>/environ` não prova
  que o `OnceLock` reconheceu o valor nem que o subscriber escreveu no arquivo
  examinado.
- A linha `normalize_moe_route` prova entrada no callback, mas não prova que a
  primeira chamada de `tracing::warn!` alcançou o sink. O Fato 1 precisa de uma
  prova de transporte do evento.
- `with_target(false)` não significa que eventos com target customizado sejam
  filtrados; significa apenas que o nome do target não é impresso. Não alterar
  isso por suposição sem capturar o evento.
- `append_log` é um arquivo de ciclo de vida do supervisor, não um coletor
  automático de logs do `tracing`. Grepar esse arquivo sozinho pode produzir
  falso negativo.
- As mensagens `resolve_fused_ops: ... enabled` não demonstram, sozinhas, que
  um nó fundido foi executado para o FFN MoE. É preciso observar os nós ou a
  entrada efetiva do callback.
- Não refazer a ponte numérica, não reverter e não empurrar uma quarta tentativa
  sem executar e passar o teste diferencial CUDA do Ornith:

```bash
POM_MOE_NUMERIC_MODEL=<gguf> POM_MOE_NUMERIC_GPU_LAYERS=99 \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

O próximo trabalhador deve primeiro construir a prova executável do Fato 1,
fazendo o evento aparecer no sink correto. Só depois deve usar os valores crus
para decidir entre shared expert, sidecar/layout e stride.
