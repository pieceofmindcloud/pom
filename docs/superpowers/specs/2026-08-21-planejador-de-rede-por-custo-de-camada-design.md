# Planejador de rede por custo de camada

Data: 2026-08-21
Estado: desenho aprovado, aguardando plano de implementação

## 1. O problema

Hoje a divisão de um modelo entre máquinas é decidida sem saber o que uma camada
custa. São dois planejadores, e nenhum dos dois tem a informação:

- `models/plan.rs::plan_split` conhece o peso real de cada bloco
  (`gguf::read_layout`), mas trata KV cache, ativações e runtime como um único
  desconto chapado de 15% (`DEFAULT_RESERVE_PCT`, `plan.rs:24`).
- `backend/deploy.rs` não modela memória nenhuma além de "cabe em disco" e
  "cabe na memória livre". Ele escolhe entre **cortes que já existem** e nunca
  inventa uma fronteira.

O dimensionamento honesto acontece tarde demais: o `common_fit_params` do
llama.cpp (`inference/llm.rs:230-300`) roda dentro de cada máquina, no momento
da carga, **depois** de a fatia já ter sido escolhida, cortada e transferida.
Quando ele descobre que não cabe, o custo já foi pago, e o que sobra é reduzir
camadas em GPU ou falhar.

O efeito prático numa rede heterogênea é a divisão proporcional: `64 × VRAM/48`.
Ela erra em dois eixos ao mesmo tempo:

- **Camadas não são iguais.** Numa arquitetura híbrida, uma camada de atenção
  com 250k de contexto custa dezenas de vezes o que custa uma camada recorrente
  (DeltaNet/Mamba) ao lado dela. Ver §2.
- **Máquinas não são equidistantes.** Duas placas na mesma máquina trocam estado
  oculto por loopback; duas máquinas em ISPs diferentes pagam RTT por token, a
  cada token. A ordem da cadeia é uma decisão de latência, e hoje ninguém a toma.

## 2. Escopo

Este documento cobre **o plano de rede**: decidir e mostrar a melhor divisão, e
executá-la cortando o que ainda não existe.

Fica **fora**, para um desenho seguinte (`Spec 2 - a rede que se reorganiza`):

- replanejamento sobre topologia viva (máquina entra ou sai enquanto a rede serve);
- sugestão de replano no sino com diff e custo, e aplicação automática apenas do
  que não interrompe inferência;
- semeadura do GGUF original em segundo plano para as máquinas que não o têm, de
  modo que o replanejamento seguinte seja local em todo mundo. Essa fase esbarra
  no limite já documentado em `implantacao.md §1.1`: `transfer::safe_model_path`
  prende o "puxar" à pasta de fatias, e um modelo inteiro mora fora dela;
- interação do solver com tensor parallelism.

A Spec 2 depende inteiramente desta e não tem sentido antes dela.

## 3. Decisões tomadas

| Decisão | Escolha |
|---|---|
| Unidade de custo do solver | a **placa**. Um nó expõe de 1 a N placas. |
| Etapa de pipeline | a **máquina**. Placas da mesma máquina formam uma faixa contígua num runtime só, com `tensor_split` explícito. |
| Objetivo | score combinado e **ajustável** entre latência e contexto, sempre exibido decomposto nas duas grandezas reais. |
| Corte de fronteira nova | extração local onde a máquina tem o GGUF original; transferência da fatia pronta só onde falta. |

A escolha de "etapa = máquina" cobre os dois modos que o produto precisa suportar:
uma instalação do POM por placa (a topologia de teste atual: 2 máquinas, 4
instalações) é o caso degenerado de uma placa por nó, e resolve pelo mesmo solver.

## 4. Modelo de custo por camada (`crates/node/src/models/cost.rs`)

Módulo novo, função pura: entra `GgufLayout` + cabeçalho estendido +
`RuntimeParams`, sai um custo por camada. Sem I/O, sem rede, sem carregar peso.

```
cost(l) = weights(l) + kv(l) + recurrent(l)
```

mais um custo **por dispositivo**, não por camada, para ativações e runtime (§4.4).

Os números abaixo foram lidos do llama.cpp que está no worktree, não escritos de
memória. As referências de arquivo e linha ficam no código, ao lado de cada
constante.

### 4.1 `weights(l)`

Já existe e já é honesto: `GgufLayout::layer_bytes[l]`, somado da tensor table,
byte real do quant real (`gguf.rs:305`). Nada a fazer.

### 4.2 `kv(l)` - camadas de atenção

De `llama-kv-cache.cpp:207-232`:

```
kv(l) = celulas(l) × [ row_size(type_k, n_embd_k_gqa(l))
                     + row_size(type_v, n_embd_v_gqa(l)) ]

n_embd_k_gqa(l) = n_embd_head_k(l) × n_head_kv(l)     (llama-hparams.cpp:131)
n_embd_v_gqa(l) = n_embd_head_v(l) × n_head_kv(l)     (llama-hparams.cpp:137)
```

Dois detalhes que só aparecem lendo o fonte e que mudam o resultado em ordem de
grandeza:

- **Camada com janela deslizante não paga `n_ctx`.** `llama-kv-cache-iswa.cpp:73`:
  `size_swa = pad256(min(n_ctx, n_swa × n_seq_max + n_ubatch))`. Num modelo com
  padrão 5:1 de SWA, cinco de cada seis camadas de atenção custam uma fração da
  sexta. Tratá-las como iguais erra o orçamento inteiro.
- **`celulas` é `n_ctx × max_sessions`, não `n_ctx`.** O POM não define
  `kv_unified`, então vale o padrão: `n_stream = n_seq_max` e
  `n_ctx_seq = n_ctx / n_seq_max` (`llama-context.cpp:290-300`). O produto volta
  a `n_ctx`, que é a mesma capacidade total que o `fit_memory` já usa hoje,
  agora explícita em vez de implícita.

Casos que o v1 trata explicitamente ou recusa nomeando: MLA (`is_mla` dispensa o
tensor V), `v_trans` (que usa `n_embd_v_gqa_max()` em vez do valor por camada).
Arquitetura cujo cálculo o módulo não reconhece é **recusa nomeando a chave**,
nunca estimativa - a mesma regra que `gguf::ggml_type_info` já aplica a tipo ggml
desconhecido, e pelo mesmo motivo: errar o tamanho não dá erro, dá resposta
errada.

### 4.3 `recurrent(l)` - camadas recorrentes

De `llama-memory-recurrent.cpp:99-101`:

```
linhas       = max_sessions × (1 + n_rs_seq)
recurrent(l) = linhas × [ row_size(type_r, n_embd_r) + row_size(type_s, n_embd_s) ]

n_embd_r = (d_conv - 1) × (d_inner + 2 × n_group × d_state)   (llama-hparams.cpp:204)
n_embd_s = d_state × d_inner                                   (llama-hparams.cpp:228)
```

**Esta é a assimetria que motiva o desenho inteiro, e ela é mais forte do que
"a barra é menor": o estado recorrente não depende de `n_ctx`.** Ele escala com o
número de sessões e nada mais. Uma camada DeltaNet com 8 sessões custa o mesmo
com 4k ou 250k de contexto, enquanto a camada de atenção ao lado dela custa
sessenta vezes mais no segundo caso.

A consequência é uma política de colocação, não um detalhe de contabilidade: com
contexto grande, camadas recorrentes são quase de graça e devem ir para a placa
pequena. Nenhuma divisão proporcional a VRAM chega a essa conclusão.

`n_embd_r`/`n_embd_s` têm ramos por família (RWKV, LFM2, Kimi KDA, MiniMax linear
attention) em `llama-hparams.cpp:183-228`. O v1 implementa o ramo Mamba/SSM e o
ramo genérico; os demais são recusa nomeando a arquitetura.

### 4.4 Ativações e runtime, por dispositivo

O buffer de compute não é por camada: escala com `n_ubatch`, `n_embd` e o número
de camadas residentes no dispositivo. Modelar como parcela por camada seria
inventar precisão que não existe. Fica:

```
reserva(dev) = compute_buffer(n_ubatch, n_embd, camadas(dev)) + fit_memory_margin(dev)
```

`fit_memory_margin(dev)` é a reserva configurável que o `llm.rs` passa ao fit
por dispositivo. O backend mede o livre real antes de comparar essa reserva,
então a ocupação de outros processos não é descontada duas vezes. O padrão de
256 MiB substitui a folga fixa de 1 GiB e mantém o orçamento do planejador
alinhado com o do motor.

O dispositivo que carrega a **camada 0** recebe reserva extra dimensionada por
`n_batch` (o pedaço de prompt), não por `n_ubatch`: quem tem a camada 0 processa
o prompt inteiro de toda requisição (`fatiamento.md §3`), e o pico de prefill é
dele. Isso é o que faz o solver naturalmente evitar pôr a camada 0 na placa de
4 GB, sem precisar de uma regra especial dizendo isso.

### 4.5 Que camada é de que tipo

Não vem de fórmula: vem de `GgufLayout::layer_kinds`, que já lista os sufixos de
tensor reais de cada bloco. Camada com `ssm_conv1d.weight` é recorrente.

É a mesma disciplina que o `splitter.rs` adotou depois do defeito do
Qwen3.8-27B (`fatiamento.md §2`) e pelo mesmo motivo: reimplementar a fórmula do
llama.cpp sobre o índice da camada é exatamente como se erra isso.

### 4.6 O que entra no `gguf.rs`

`read_info` lê seis campos hoje e nenhum serve para custo. Acrescentar, com os
nomes conferidos em `llama-arch.cpp:245-272,314-318`:

`{arch}.attention.head_count_kv`, `.key_length`, `.value_length`,
`.key_length_swa`, `.value_length_swa`, `.sliding_window`,
`.sliding_window_pattern`, e `{arch}.ssm.conv_kernel`, `.inner_size`,
`.state_size`, `.group_count`.

`attention.recurrent_layers` o `splitter.rs` já sabe escrever e ler.

Chave que pode ser escalar ou array por camada (`head_count_kv` é o caso comum)
tem que ser lida nas duas formas. Ler só o escalar num modelo que declara array
dá um número plausível e errado.

### 4.7 As duas travas contra a deriva

Este módulo reimplementa aritmética que vive no C++ e que vai mudar lá.

1. **Transcrição verificada no build.** As constantes e fórmulas ficam ao lado da
   citação de arquivo e linha do llama.cpp, com um teste que compara contra a
   transcrição - o mesmo mecanismo de `ggml_type_table_matches_ggml`
   (`gguf.rs`), que já quebra o build quando a tabela de tipos diverge.
2. **Comparação com o que foi alocado de verdade.** O llama.cpp reporta o
   tamanho real do KV na carga (`llama-kv-cache.cpp:296`). O nó compara previsto
   contra alocado e registra a divergência em `status.detail`, visível no painel.

A segunda trava existe porque o sintoma de um modelo de custo errado é OOM numa
máquina três passos adiante do erro, e sem ela ninguém liga uma coisa à outra.

## 5. Inventário de dispositivos

A varredura (`deploy.rs::survey`) já lê `gpus[]` de cada peer com
`vram_total_mb`/`vram_used_mb`, mas **colapsa tudo num único `gpu_free_mb`**
somando as placas (`deploy.rs:890-912`). Para o solver isso é perda de
informação: uma máquina com 4 GB + 12 GB não é "uma de 16", porque uma camada
precisa caber inteira numa placa.

`Machine` passa a carregar `devices: Vec<DeviceBudget>`:

```rust
struct DeviceBudget {
    node_id: String,
    host_id: String,
    gpu_index: u32,
    label: String,        // nome da placa como o sysmon reporta
    vram_free_mb: u64,
    has_model: bool,      // o GGUF original está nesta máquina
}
```

`gpu_free_mb` e `mem_free_mb` continuam existindo e continuam sendo o que as
verificações atuais usam - nada do caminho de hoje muda de comportamento.

### 5.1 `host_id`: quatro peers, duas máquinas

Na topologia de teste atual há 4 instalações do POM em 2 máquinas. A varredura de
hoje conta isso como **4 orçamentos independentes de RAM e de disco**, quando na
verdade são dois pares compartilhando o mesmo host. Um plano pode "caber" quatro
vezes no mesmo disco livre.

`/api/ui/node` passa a expor `host_id`: um hash estável e não identificante do
host (machine-id no Linux, `IOPlatformUUID` no macOS), com fallback para o
hostname. O solver então:

- soma VRAM **por placa** (cada placa é sua);
- checa RAM e disco **por host**, não por peer;
- trata RTT entre peers do mesmo `host_id` como zero, sem esperar medição.

## 6. O solver (`crates/node/src/models/placement.rs`)

Função pura. Entra o custo por camada, os dispositivos, a matriz de RTT e o peso
α; sai um plano e as alternativas. Sem rede, sem disco, testável sozinha.

### 6.1 A forma do problema

O pipeline é uma **cadeia**: camadas em ordem, atribuídas a uma sequência
ordenada de dispositivos. Duas decisões acopladas:

- **em que ordem** os dispositivos entram na cadeia - é isso que o RTT decide;
- **onde cortar** - é isso que o custo decide.

Observação que torna o problema tratável: **fixada a ordem, o termo de rede já
está determinado**, porque ele só depende de quais pares são consecutivos. O que
a partição decide é apenas quanto contexto cabe. Então:

```
para cada ordem π dos dispositivos:
    ctx_max(π) = maior n_ctx para o qual existe partição viável
    latencia(π) = Σ rtt(π_i → π_{i+1})        # já fixo pela ordem
    ponto(π) = (latencia(π), ctx_max(π))

fronteira = Pareto dos pontos
plano = ponto escolhido por α na fronteira
```

Da ordem de **dispositivos** para as etapas de **pipeline**: dispositivos
consecutivos que pertencem ao mesmo `node_id` são fundidos numa etapa só, cuja
faixa é a união das faixas deles e cujo `tensor_split` é a proporção entre elas.
É esse passo que faz "duas placas numa máquina" render um hop e não dois.

O `n_ctx` que o operador digitou em `RuntimeParams` é o **alvo**, não um teto: a
fronteira é explorada acima e abaixo dele, e o plano diz explicitamente se o alvo
foi alcançado, superado ou não atingido. A escada de contextos candidatos é
`n_ctx` do operador mais as potências de dois entre `FIT_MIN_CONTEXT` e o
`n_ctx_train` do modelo, cada uma arredondada para múltiplo de 256 como o
llama.cpp faz (`llama-context.cpp:288`). Não é uma busca contínua: contextos que
o operador nunca escolheria não precisam ser calculados.

### 6.2 Por que a viabilidade é gulosa (e por que isso importa)

Para uma ordem e um `n_ctx` fixos, empacotar prefixos de forma gulosa - cada
dispositivo leva quantas camadas couberem, na ordem - **maximiza a cobertura**.

A prova é curta e vale registrar porque é ela que torna o deslizador interativo:
seja `b(a)` a última camada que um dispositivo cobre começando em `a`. Como os
custos são aditivos e o orçamento é fixo, mover o início da janela para a direita
só pode mover o fim para a direita ou deixá-lo onde está, logo `b` é monótona em
`a`. Portanto avançar o máximo em cada dispositivo nunca prejudica os seguintes.

Isso derruba a viabilidade de `O(L²)` para `O(L)` por ordem. Com 4 a 8
dispositivos, 64 camadas e uma escada de 12 contextos candidatos, o solver
inteiro roda em milissegundos - que é o requisito real, porque o deslizador
recalcula a cada arrasto.

Três correções entram no laço guloso:

- **`snap_end_layer`** (`gguf.rs`): em modelo híbrido o corte só acontece em
  fronteira de tipo. Fatia que corta no meio de um grupo renumera camadas para o
  tipo errado.
- **`is_last`**: quem fecha a cobertura carrega a cabeça de saída
  (`slice_bytes(.., is_last)`).
- **camada 0**: quem a leva paga a reserva de prefill de §4.4.

### 6.3 Ordens: exaustivo enquanto dá, heurística depois

`D ≤ 8` enumera todas as permutações (8! = 40320, cada uma `O(L)` por contexto
candidato, o que dá algo como 40320 × 12 × 64 ≈ 31 milhões de operações simples)
e o resultado é **exato**. Acima disso, uma cadeia inicial por
vizinho-mais-próximo de RTT, agrupando por `host_id`, refinada por 2-opt, com o
plano dizendo na cara que é heurístico.

Dispositivo pode receber **zero camadas**: uma placa de 4 GB atrás de um link
ruim pode não valer o salto que acrescenta. Permitir faixa vazia no guloso cobre
todos os subconjuntos sem enumerá-los.

Placas do mesmo `host_id` adjacentes na ordem custam RTT zero, o que faz a regra
"máquinas próximas ficam com fatias próximas" cair fora do modelo em vez de
precisar de uma regra dizendo isso.

### 6.4 O score, e por que ele nunca aparece sozinho

O deslizador vai de "mais rápido" a "mais contexto" e escolhe um ponto da
fronteira de Pareto. Mas a tela **nunca mostra o score**: mostra as duas
grandezas reais do plano escolhido, mais os pontos vizinhos da fronteira.

Isso é deliberado. Duas divisões com o mesmo score são incomparáveis para o
operador; duas divisões com "38 ms/token, 96k de contexto" e "12 ms/token, 32k
de contexto" são uma decisão que ele sabe tomar.

O que o score **não** contém: tempo de cômputo por dispositivo. O POM não sabe o
FLOPS nem a banda de memória de uma placa, e inventar uma tabela de modelos de
GPU seria mais uma tabela para derivar. O desequilíbrio de cômputo entra como
**critério de desempate** entre partições igualmente viáveis (minimiza o maior
peso por dispositivo) e como **aviso** quando uma etapa carrega mais que o dobro
de outra. Quando houver telemetria de decode medida por nó, ela vira o
refinamento natural - e é dado medido, não tabela.

### 6.5 Saída

```rust
struct PlacementPlan {
    stages: Vec<Stage>,          // uma etapa por máquina, em ordem de camada
    achieved_ctx: u32,
    est_network_ms_per_token: f64,
    exact: bool,                 // false quando caiu na heurística de §6.3
    alternatives: Vec<PlacementPoint>,   // a fronteira de Pareto
    warnings: Vec<String>,
}

struct Stage {
    node_id: String,
    start_layer: u32,
    end_layer: u32,
    devices: Vec<StageDevice>,   // placa, faixa dentro da etapa, custo detalhado
    tensor_split: Vec<f32>,      // proporção por placa local; vazio se uma placa só
}
```

`tensor_split` é a peça que faz "duas placas numa máquina" funcionar sem quebrar
a invariante "um nó serve uma faixa". Hoje quem escreve esse vetor é o
`common_fit_params`, cegamente, no momento da carga (`llm.rs:237-265`); passa a
ser o solver, que sabe o custo de cada camada.

## 7. Integração com o deploy

### 7.1 O plano

`POST /api/ui/deploy/plan` ganha `boundaries: "existing" | "proposed"`, padrão
`existing`. O caminho de hoje não muda de comportamento: `existing` continua
escolhendo entre cortes que já estão em disco.

Com `proposed`, `plan_with_options` chama o solver e monta `Step`s cujas faixas
podem não existir em lugar nenhum. Cada `Step` ganha uma origem:

```rust
enum SliceSource {
    Held,                              // a máquina já tem essa faixa exata
    LocalCut { model_path: String },   // a máquina tem o GGUF original e corta
    Transfer { from_node_id: String }, // outro peer corta e manda a fatia pronta
}
```

O `Plan` ganha `placement: Option<PlacementPlan>`, e `transfer_bytes` passa a
contar só o que é `Transfer` - é a diferença entre "mover 40 GB" e "mover 6 GB",
e é o número que decide se o operador clica.

### 7.2 A execução

`run` ganha uma **fase 0, cortar**, antes da fase de transferência que já existe:

1. `Held` - nada a fazer.
2. `LocalCut` - `POST /api/ui/slice` no próprio alvo. Rota existente, job
   existente, progresso existente. Nenhum byte na rede.
3. `Transfer` - o peer que tem o original corta a faixa, e a parte resultante
   viaja pelo caminho de `transfer.rs` endereçada por `sha256`, como qualquer
   outra parte.

Depois disso, fases 1 (transferência) e 2 (apply/launch) seguem inalteradas.

Isso reusa o fatiamento inteiro sem exceção. `pom::finalize_run` já mescla partes
novas no mesmo manifesto por `sha256`, **inclusive faixas alternativas** como
`L0-7` ao lado de `L0-3` (`fatiamento.md §4`). Uma fronteira nova proposta pelo
solver é exatamente esse caso, e o manifesto já foi desenhado para ele.

### 7.3 `fit_memory` deixa de ser quem decide e vira rede de proteção

Com um plano do solver, o alvo recebe `n_gpu_layers = 99` e o `tensor_split`
explícito. O `fit_memory` continua ligado, porque um plano errado que mata o
processo é pior que um plano ajustado. Mas qualquer ajuste que ele faça agora é
uma **divergência entre o previsto e o real**, e é reportada: vai para o log, para
`status.detail` e para a tela, alimentando a trava de §4.7.

Hoje um ajuste do fit é invisível. Passa a ser o sinal mais importante de que o
modelo de custo errou.

## 8. Recusas

Todas nomeiam o objeto concreto, nunca o conceito - a regra que `deploy.rs` já
segue ("a fatia L0-31 em ...", não "uma fatia").

| Situação | Resposta |
|---|---|
| Uma camada não cabe em placa nenhuma, em contexto nenhum | `blocked`, nomeando a camada, o custo dela e a maior placa. Aponta as saídas reais: tensor parallelism ou quantização menor (`paralelismo.md`). |
| Nenhuma divisão viável no `n_ctx` mínimo | `blocked` com o maior `n_ctx` que caberia, em vez de empurrar KV para a RAM em silêncio. |
| Arquitetura cujo custo o módulo não sabe calcular | `blocked` nomeando a chave ou a família. Nunca estimativa. |
| `total_layers` desconhecido | `blocked`. Regra que já existe e que vem do defeito de 2026-08-10. |
| Nenhum peer tem o GGUF original | `blocked`: sem original não há corte novo. Sugere a tela de Transferências. |

## 9. A tela

`apps/frontend/src/routes/DeployModel.tsx` tem 75 KB. Ele **não** cresce: a
proposta entra como componentes próprios, e essa extração é parte do trabalho,
não um bônus.

- `components/PlacementProposal.tsx` - o deslizador, os dois números de manchete
  (latência de rede estimada por token, contexto alcançado) e os pontos vizinhos
  da fronteira como paradas discretas.
- `components/DeviceBudgetBar.tsx` - **uma barra por placa**, com as parcelas
  empilhadas (peso, KV, estado recorrente, reserva) contra a VRAM daquela placa.
  É o desenho que o operador pediu, com números reais em vez de proporção.
- `components/DeployNetworkCanvas.tsx` (existente) - passa a desenhar a cadeia
  proposta com o RTT em cada aresta, e as placas dentro de cada máquina.

A barra por placa é o ponto da tela. Ver "12,2 GB de 16" ao lado de "2,9 GB de 4"
é o que torna a decisão do solver auditável; um plano que só diz "L0-20 na ksia"
pede fé.

## 10. Testes

**`cost.rs`** - função pura, sem fixture de rede:

- KV de camada com janela deslizante é uma fração do de camada full, com o mesmo
  `n_ctx` (o caso que a proporcionalidade erra).
- Custo de camada recorrente **não muda** quando `n_ctx` vai de 4k a 250k, e o de
  camada de atenção muda proporcionalmente. É a asserção que prova a tese do
  desenho.
- Transcrição das constantes contra a verdade do llama.cpp quebra o build ao
  divergir (§4.7).
- Arquitetura desconhecida recusa nomeando, não estima.

**`placement.rs`** - pura, com dispositivos sintéticos:

- O cenário 4/12/16/16: as camadas recorrentes caem na placa de 4 GB e a camada 0
  não cai nela.
- Duas placas no mesmo `host_id` saem adjacentes na cadeia quando o RTT externo
  é alto.
- Acrescentar um dispositivo muda a divisão e aumenta `achieved_ctx`.
- Propriedade do guloso: para ordem e contexto fixos, nenhuma partição alternativa
  cobre mais camadas (teste por força bruta em instância pequena).
- Acima do limite de `D`, o plano sai com `exact: false`.
- Disco e RAM de dois peers no mesmo `host_id` não são contados duas vezes.

**`deploy.rs`** - com `gateway::testkit` e `fake_peer`, nunca UDP de verdade:

- `boundaries: "existing"` produz exatamente o plano de hoje (não regressão).
- `boundaries: "proposed"` emite `Step`s com `LocalCut` onde a máquina tem o
  original e `Transfer` onde não tem, e `transfer_bytes` conta só os segundos.
- Sem original em peer nenhum, `blocked`.

**Frontend** - testes de componente para a barra por placa e para o deslizador
percorrendo a fronteira.

## 11. Documentação a atualizar no mesmo commit

`docs/subsystems/fatiamento.md` (o modelo de custo substitui a reserva chapada),
`implantacao.md` (o modo `proposed` e a fase de corte), `paralelismo.md` (a
escolha pipeline/TP passa a ter um número por trás), `web-ui.md` (a tela),
`crates/node/src/models/AGENTS.md`, `crates/node/src/backend/AGENTS.md`,
`apps/frontend/src/components/AGENTS.md` e `apps/frontend/src/routes/AGENTS.md`.

Candidata a invariante na raiz, se sobreviver à implementação: **o custo de uma
camada é lido, não estimado, e o previsto é conferido contra o alocado.**

## 12. Defeitos existentes que este trabalho encosta

Anotados, não consertados de passagem, conforme a regra do repositório:

- `plan.rs:9-12` - o doc do módulo ainda fala do `split_model.py`, e o código
  chama `splitter::split`. Já anotado em `fatiamento.md §10`.
- `model.rs:109` - `guess_from_name` inventa camadas e tamanhos. O modelo de
  custo não o usa e não o piora, mas é o vizinho de tudo isto.
