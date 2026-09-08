# Inferência distribuída pela internet — mudanças propostas

**Data:** 2026-08-12
**Estado:** proposta, nenhuma decisão tomada
**Contexto:** o captain quer o pipeline funcionando **pela internet**, entre máquinas
em redes diferentes. Não é cenário de LAN, e as mudanças abaixo partem disso.

## O que foi medido

Tudo aqui vem de medição nesta data, não de estimativa. As máquinas:

| nó | onde | endereço anunciado |
|---|---|---|
| `5a4dfd6a` | MacBook (gateway) | `127.0.0.1` (visto de si) |
| `02eb8c40`, `ff952586`, `0dba306a` | outra casa, mesmo host | `[2804:f30:3:1a6f:…]:8181/8182/8183` |

| medição | valor |
|---|---|
| RTT entre as duas casas (`ping6`, 10 pacotes, 0% perda) | **60,2 ms** (58,8–63,4) |
| RTT pela LAN, quando existe (`192.168.100.138`) | 4,0 ms |
| decode, por token | **70,7 ms** (14,1 tok/s) |
| └ transporte até o hop remoto | 55,2 ms |
| └ backend do hop 1 (com captura de residual) | 8,2 ms |
| └ backend do hop 2 (último, com LM head) | 0,14 ms |
| └ sampling (100% "acesso aos logits") | 4,9 ms |
| prefill de 3.414 tokens | 4,1 s, 13 MB pelo enlace |

**O teto já foi atingido.** Piso teórico = 1 RTT (60 ms) + compute (13 ms) = 73 ms.
Medido: 70,7 ms. Não há otimização de inferência que mova esse número — o sampler
que foi corrigido hoje custa 0,3 ms num orçamento de 60.

O que decide tudo é **quantas vezes um token atravessa a internet**.

## O problema estrutural: o gateway é um hub

`prefill.rs::step()` percorre os peers em série, e o gateway fala com cada um:
`gateway → hop1 → gateway → hop2 → gateway`. O estado oculto volta ao gateway
entre um hop e o seguinte.

Travessias de internet por token, conforme onde cada peça está:

| arranjo | travessias | ms/token |
|---|---|---|
| hoje (gateway local, hop1 remoto, hop2 local) | 2 | 60 |
| dois hops remotos, com hub | **4** | **120** |
| dois hops remotos, em anel | 2 | 60 |
| pipeline e gateway remotos, cliente local | 2 **por requisição** | ~0 por token |

A segunda linha é a mais importante: **hoje, cada nó a mais do outro lado
adiciona uma ida e volta inteira por token**. O paralelismo de pipeline piora
quanto mais nós remotos você usa. Isso inverte o propósito do produto.

---

## Mudança 1 — Rodar o pipeline onde ele não precisa da internet

**Tipo:** configuração. Nenhum código.

Os três nós remotos estão **no mesmo host**, em portas diferentes: entre eles a
rede é loopback. Implantar o modelo só neles, e usar **o gateway daquela
máquina**, faz a internet ser atravessada uma vez por requisição em vez de uma
vez por token — os tokens voltam pelo SSE já em transmissão.

Custa nada e é reversível. **Deve ser testado antes de qualquer código**, porque
mede quanto do problema é topologia.

O que isso não resolve: modelo que não cabe numa casa só.

## Mudança 2 — Anel em vez de estrela

**Tipo:** código. `prefill.rs`, `pipeline.rs`.
**Ganho:** metade das travessias quando hops consecutivos são co-localizados.

O hop N manda o estado oculto direto para o hop N+1, em vez de devolvê-lo ao
gateway. O gateway continua iniciando a requisição e recebendo o token final.

Pontos que precisam de decisão antes de implementar:

- **Quem carrega o `PipelinePlan`.** Hoje só o gateway o conhece. No anel, cada
  nó precisa saber quem é o seu sucessor.
- **Erro no meio do anel.** Hoje o gateway vê cada falha e decide a retentativa
  (`CHUNK_ATTEMPTS`, `TraceIssue`). Em anel, uma falha no hop 2 precisa voltar ao
  gateway de alguma forma, ou a requisição pendura.
- **A telemetria por fase** (`RequestTrace`) é montada pelo gateway a partir do
  que ele mede em cada chamada. Em anel ele deixa de medir os saltos
  intermediários, e os nós passam a ter de reportá-los.

O terceiro ponto é o que torna esta mudança maior do que parece: a telemetria que
acabou de ser construída pressupõe a estrela.

## Mudança 3 — Decodificação especulativa

**Tipo:** código. `llm.rs`, `infer_server.rs`, `prefill.rs`, `runtime.rs`.
**Ganho:** K tokens por travessia, em vez de 1. É o **único** caminho que quebra
o teto de 60 ms por token.

Com K=4 e aceitação de 60-70%, a projeção é **35-50 tok/s** contra os 14 de hoje.

### Por que o modelo rascunho, e não MTP

MTP não é requisito — é uma das variantes:

| variante | exige | treino |
|---|---|---|
| modelo rascunho | modelo pequeno com o **mesmo vocabulário** | não |
| MTP | cabeças treinadas junto com o modelo | já vem, se houver |
| Medusa / EAGLE | cabeças extras sobre o modelo congelado | sim |
| auto-especulativo | o próprio modelo pulando camadas | não |
| prompt lookup / n-grama | nada | não |

O modelo em uso é `qwen35moe` (Ornith-1.0-35B, 40 camadas, IQ3_XXS). O
vocabulário do Qwen tem 151.936 tokens, então **um Qwen3.5 denso de 0,5B ou 1,7B
serve de rascunho sem adaptação**.

Dois motivos para o rascunho ser o caminho preferido aqui:

- **Ele roda na máquina do gateway**, sem tocar na rede. O custo do rascunho é
  local e não entra no orçamento de 60 ms.
- **MoE ajuda a verificação.** Verificar K tokens custa quase o mesmo que
  verificar 1, porque o gargalo do MoE é carregar os pesos dos especialistas, não
  a aritmética.

**A verificar antes de decidir:** se este GGUF traz cabeças MTP, o rascunho sai de
graça. `gguf.rs` já lê o cabeçalho. Ressalva: o suporte a MTP no llama.cpp é
irregular por arquitetura — ter as cabeças não garante poder usá-las via
`llama_decode`.

### O que muda no código

1. **Logits de todas as posições no último nó.** Hoje `logits_all = !is_last`
   (`infer_server.rs`): o último nó calcula logits só da última posição. A
   verificação precisa dos K+1.
2. **Laço de aceitação no gateway.** Novo.
3. **Rebobinar o KV cache em todos os nós** quando um token é rejeitado.
   **A peça já existe**: `kv_remove` / `_rollback_seq_kv`, hoje usados para
   retentativa de pedaço no prefill. Foi construído para outra coisa e serve
   inteiro.
4. **Um segundo motor, local, para o rascunho.** `runtime.rs` já sabe subir e
   supervisionar um motor; passaria a supervisionar dois.

### A dependência com a Mudança 4

Especulação correta exige **amostragem por rejeição modificada** para preservar a
distribuição — senão acelera e muda o que sai. Isso é matemática delicada.

O cluster agora gera pela distribuição normalizada (ver Mudança 4). Portanto, a
verificação da especulação exige a amostragem por rejeição modificada para não
alterar essa distribuição.

**As duas decisões estão amarradas, e a ordem importa.**

## Mudança 4 — `top_p` corrigido

**Status:** entregue. A decisão foi resolvida em 2026-08-31.

`sample_token_profiled` (`llm.rs`) normaliza os pesos de softmax depois de
aplicar top-k e antes de avaliar top-p. O núcleo é o prefixo mínimo ordenado por
probabilidade cuja massa atinge `top_p`, é renormalizado e então alimenta o
sorteio multinomial. Empates não expandem top-k nem o núcleo: top-k desempata
por id e top-p preserva somente o prefixo necessário.

Temperatura positiva e top-k voltaram a afetar a distribuição como configurado.
A consequência já assumida pela Mudança 3 é que uma implementação especulativa
correta precisa de amostragem por rejeição modificada.

## Mudança 5 — Investigar o callback de captura do residual

**Tipo:** investigação, depois código.
**Ganho estimado:** até 8 ms/token, que é ~11% do orçamento atual.

Medido em **duas rodadas, com as máquinas trocadas de posição**:

| | backend |
|---|---|
| primeiro hop (tem `need_embeddings`, instala o callback) | 7,8 e 8,2 ms/token |
| último hop (não instala) | 0,67 e 0,14 ms/token |

Mesmo número de camadas, e o último hop ainda carrega a LM head, que é a maior
matmul do grafo. O custo segue a **posição**, não a máquina — não é Metal nem
CUDA, é o `ggml_backend_sched_eval_callback` que instalamos em `llm.rs`.

Um callback instalado muda como o scheduler do ggml agrupa os nós do grafo. O
comentário em `capture_callback` já registra uma armadilha antiga nessa mesma
função; esta é outra.

**Teste que isola:** subir a mesma fatia 0-19 como último nó (sem captura) e
comparar o backend.

## Mudança 6 — Medir o "acesso aos logits"

**Tipo:** medição. Nenhum código.

`node_logits_access_ms` é 4,9 ms/token no nó da LM head, e todas as outras etapas
de sampling estão em zero desde a correção do top-k. Com `backend_completion_ms`
em 0 — o sync está **desligado** — esse tempo é o candidato natural para a espera
real da GPU, que cai no primeiro toque de CPU nos logits.

**Basta subir aquele peer com `RDMA_P2P_PROFILE_SYNC=1`.** Se os ~5 ms migrarem
de "acesso" para "conclusão", está explicado e não é problema: é a GPU
trabalhando, cobrada na etapa errada.

## Mudança 7 — Prefill: vazão, não latência

**Tipo:** medição, depois decisão.

O prefill de 3.414 tokens mandou **13 MB** pelo enlace e levou 4,1 s, dos quais
2,0 s de transporte. Aqui o problema é vazão, e não as travessias.

O zstd nível 3 **já está ativo** e funcionando: 4,5 KB por chamada de decode
contra ~10 KB do estado oculto cru em f16, e `client_encode_ms` em 0 — comprimir
não custa tempo mensurável. Subir o nível é a alavanca barata.

Registrado por ser tentador e já ter sido decidido: **int8 foi recusado pelo
captain**, por degradar a resposta em silêncio.

---

## Ordem sugerida

1. **Mudança 1** — configuração, cinco minutos, e diz quanto do problema é topologia.
2. **Mudança 6** — uma variável de ambiente, e fecha um item aberto da telemetria.
3. **Mudança 4** — decisão que destrava o tamanho da Mudança 3.
4. **Mudança 5** — investigação barata, ganho de 11%.
5. **Mudança 2 ou 3** — as duas são grandes. Qual delas depende da resposta a uma
   pergunta que **ainda não foi respondida**: o caso de uso é *modelo grande
   demais para uma casa só* (então o anel é obrigatório), ou *máquinas ociosas em
   lugares diferentes, cada uma capaz de rodar um pipeline completo* (então a
   Mudança 1 já resolve e a 3 é o que sobra)?

## O que este documento não é

Não é plano de implementação. Cada mudança de código acima precisa do seu próprio
spec e plano antes de virar código — em particular a 2 e a 3, que mexem em
protocolo entre nós e em semântica de amostragem.

---

## O que foi medido e decidido em 2026-08-13

Fechando as lacunas do documento acima. Cada número aqui saiu de execução nesta
data, e o teste que o produz ficou no repositório.

### Mudança 6 — respondida: era a GPU, cobrada na etapa errada

`llm.rs::logits_access_versus_backend_completion` (ignorado por padrão; pede
`RDMA_P2P_TEST_GGUF`) decide isso numa máquina só. Fatia final de 16 camadas,
Metal, `n_gpu_layers=99`, 16 tokens por rodada, duas rodadas de cada modo:

| | total por token | enfileirar | conclusão | acesso aos logits |
|---|---|---|---|---|
| `PROFILE_SYNC=0` | 33,28 / 34,27 ms | 0,85 / 0,78 | 0 | **32,42 / 33,49** |
| `PROFILE_SYNC=1` | 33,06 / 32,27 ms | 1,50 / 1,65 | **31,55 / 30,62** | 0 |

O tempo migra inteiro de "acesso" para "conclusão" e **o total não muda** — a
hipótese da Mudança 6 estava certa e não há bug: é a GPU terminando o trabalho,
cobrada no primeiro toque de CPU nos logits.

Como a barreira não cria espera, só a nomeia, `RDMA_P2P_PROFILE_SYNC` passou a
ser **ligada por padrão**; `=0` desliga. A telemetria de todo mundo passa a
dizer a verdade sobre onde o tempo foi.

### Mudança 5 — o callback de captura agora é derivado, não copiado

A condição já existia (`no_embeddings`), mas era decidida **só** em
`webui::ApplyRequest`. O nó subido por linha de comando mandava sempre
`no_embeddings: false`, então uma fatia final subida por `run-p2.sh` instalava o
callback e pagava os ~8 ms/token para capturar um residual que ninguém lê.

Agora quem responde é `NodeConfig::needs_residual_export` (`runtime.rs`), usada
por `infer_server::run`: quem serve a última camada, ou é o último da divisão,
nunca exporta. Errar para menos não corrompe em silêncio — `take_residual`
recusa com "residual capture did not fire".

### Mudança 7 — a premissa só vale para o prefill

`wire::zstd_level_ratio_versus_encode_time` mede razão e tempo por nível num
tensor f16 de hidden_dim 4096:

| regime | nível 3 | nível 6 | nível 9 |
|---|---|---|---|
| prefill compressível (512x4096) | 1,56x / 14,9 ms | 1,73x / 32,5 ms | 1,75x / 39,6 ms |
| decode de alta entropia (1x4096) | 1,08x / 0,025 ms | 1,08x / 0,056 ms | 1,08x / 0,144 ms |

**No decode o nível não muda um byte**: 7.570 bytes em 1, 3, 6, 9 e 12. Subir o
nível ali não é alavanca nenhuma, ao contrário do que este documento supunha.

No prefill vale: do 3 para o 6 são 268 KB a menos, 21,4 ms de rede economizados
num enlace de 100 Mbps por 17,6 ms de CPU. Do 6 para o 9 são 2,5 ms de rede por
7 ms de CPU — negativo. Escolhido **nível 6** (`wire::ZSTD_LEVEL`).

### Mudança 3d — este GGUF não tem cabeças MTP

Lido o cabeçalho e a tensor table das fatias de
`lmstudio-community-Qwen3.6-35B-A3B-GGUF` (`general.architecture = qwen35moe`,
`rdmap2p.original_block_count = 40`, `embedding_length = 2048`):

- tensores fora de `blk.*`: `token_embd.weight`, `output_norm.weight`,
  `output.weight`. Nada de `nextn`, `mtp`, `eh_proj` ou `shared_head`;
- nenhum KV do tipo `*.nextn.*`.

**Não há MTP de graça.** O modelo rascunho separado também não foi feito: exige
um segundo GGUF denso com o mesmo vocabulário (não existe nas máquinas), um
segundo motor supervisionado por `runtime.rs` e memória sobrando na máquina do
gateway — a mesma que já processa o prompt inteiro de cada requisição por servir
a camada 0. Custo alto, e nada disso está medido.

O que foi feito no lugar: **melhorar o prompt-lookup**. `find_draft` passou a
tratar `speculative_lookup_window` como o **maior** n-grama tentado, não o
único: a busca desce `m, m-1, … 1` e para no primeiro que casa, e o quanto da
continuação é aproveitado escala com o comprimento casado (`len` tokens de
contexto autorizam no máximo `len + 1` palpites). Antes, uma janela que não
casasse não rascunhava nada; agora degrada para um palpite mais fraco em vez de
para palpite nenhum, o que é o regime certo num MoE, onde verificar K tokens
custa quase o mesmo que verificar 1.

Nota lateral que muda a conta de bytes deste documento: o hidden_dim deste
modelo é **2048**, não 4096 — são 4.096 bytes por token por salto em f16.

### Interface

`speculative_decoding_enabled`, `_k` e `_lookup_window` existiam no backend e
não tinham controle. Agora estão em Implantar na rede → ajustes avançados: um
interruptor, e os dois números só aparecem com ele ligado.
