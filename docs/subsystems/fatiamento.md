# Subsistema: fatiamento de modelos GGUF

O subsistema corta um modelo GGUF inteiro em fatias por faixa de camadas e junta
fatias adjacentes de volta num arquivo único. Vive em `crates/node/src/models/`
(`catalog.rs`, `gguf.rs`, `plan.rs`, `splitter.rs`, `model.rs`, `lastslice.rs`) e
é consumido por `crates/node/src/backend/` (`webui.rs`, `deploy.rs`, `gateway.rs`).
Fatiar é local e sob demanda: cada peer extrai a própria faixa da própria cópia
do GGUF - nenhum peso trafega pela rede.

## 1. Descoberta e catálogo

`catalog.rs` varre `shard_dir` (padrão `<project_root>/models`, a única pasta que
um peer pode puxar) e `model_dirs`, e monta o `Catalog` com `models` e
`shard_sets`. Cada diretório é lido pelo que ele é (`detect_source`):
`ModelSource::{LmStudio, Ollama, Local, Folder}`. A ordem da varredura importa:
`shard_dir` primeiro (a leitura que decide se um peer pode puxar a fatia é a de
fatia, `ModelSource::Local`), depois `model_dirs` deduplicados por caminho
resolvido - symlink não é varrido duas vezes.

Quem sustenta tudo é o **manifesto**, não o nome do arquivo. Uma parte se chama
pelo próprio sha256 (`<sha256>.gguf`), e o `.pom` ao lado dela diz de que modelo
ela é e que faixa cobre. Ver `models/pom.rs`.

- `pom::load_dir` - lê os `.pom` de uma pasta de modelo. É daí que sai o
  `shard_group` de cada parte listada.
- `describe` - a faixa vem do carimbo `pom.start_layer`/`pom.end_layer` que o
  fatiador grava no cabeçalho GGUF. Parte `-00002-of-00003` (`is_secondary_part`)
  é pulada.
- Os walkers do catálogo continuam a descoberta quando uma entrada filha é
  ilegível e registram o caminho e a causa em `debug`; uma raiz de catálogo
  ausente continua sendo representada como inexistente.
- `parse_quant` - quantização pelo nome de um modelo **inteiro**; uma parte
  herda a do `.pom`.
- `build_shard_sets` - agrupa fatias pelo **modelo original** (`<modelo>-<hash
  curto>`, o mesmo nome da pasta), ordena por faixa,
  mede a cobertura contígua a partir da camada 0 e deduz o total da cobertura
  quando a fatia não declara (`total_layers_inferred`). A flag `complete` diz se
  a cobertura fecha o modelo.

A convenção antiga (`<base>_<inicio>_<fim>.gguf`, lida por `parse_shard_name`)
foi removida: o nome de um arquivo deixou de ser prova de coisa nenhuma. Fatias
cortadas antes disso precisam ser refatiadas.

`total_layers` é `Option` do catálogo ao plano: fatia antiga sem metadata
`pom.*` não declara nada, e "não sei" e "zero" são fatos diferentes
(colapsá-los foi o bug de 2026-08-10, quando um plano "completo" morria em
`NodeConfig::validate`). O catálogo serve o resultado anterior enquanto revalida
por trás: `local_catalog` usa `CATALOG_TTL = 20 s` com stale-while-revalidate, e
`warm_catalog` roda no arranque para um volume lento não segurar a porta do
gateway (medido: primeira carga fria de `/api/ui/models` caía de 11,9 s para
0,18 s com o aquecimento). A primeira fase enumera e faz `stat`; a segunda lê
somente os cabeçalhos necessários. Um erro terminal fica em `metadata_error` no
modelo, sem impedir os demais, e a passagem publica `metadata_complete: true`.
O backend mantém uma única varredura e um único enriquecimento em voo, inclusive
quando a tela é recarregada ou a seleção muda. Um refresh pode trocar o
inventário, mas preserva metadata por identidade `path + size + mtime`; somente
modelo novo ou arquivo alterado volta a ler o cabeçalho. Hash integral continua
restrito à fronteira de fatiamento ou transferência.

## 2. Leitura do GGUF: cabeçalho e tensor table

`gguf.rs` lê o GGUF sem carregar pesos:

- `read_info` - só o KV header: arquitetura, `block_count`, `pom.*`
  (original/total), `shard_range`, `embedding_length` e a geometria do cache
  (`attention.head_count_kv`, `key_length` e `value_length`). Cada item pode
  ser escalar ou array por camada. Quem decide se um arquivo é fatia é
  `catalog::InstalledModel::is_shard`; a cópia que existia em `GgufInfo` não
  tinha chamador e saiu. O catálogo também lê os campos de multipart nesse
  header, sem mapear nem validar o payload dos tensores.
- `read_layout` - tensor table -> `GgufLayout` com bytes **reais** por camada
  (`layer_bytes`, `embedding_bytes`, `head_bytes`, `other_bytes`, `total_bytes`,
  `layer_kinds`). É o que torna o orçamento de memória honesto. Tipo ggml
  desconhecido recusa o layout em vez de inventar. Para multipart, a tabela
  vem de `multipart::GgufSource` e inclui todos os tensores do conjunto.
- `arch_boundaries`/`snap_end_layer` - modelos híbridos (Mamba + Attention) não
  são pilha de blocos iguais; o corte só acontece em fronteira de tipo, senão o
  bloco renumera para o tipo errado (`missing tensor` ou resposta errada
  silenciosa).
- `nextn_predict_layers` - MTP/Qwen3.5+: o bloco nextn vive só na última camada;
  fatia que não termina no fim do modelo precisa zerar o hiperparâmetro ou o
  llama.cpp aborta o load procurando `blk.<last>.nextn.*`.
- `attention.recurrent_layers` - modelo híbrido attention/SSM,
  como o Qwen3.8-27B): na ausência de um array explícito com essa chave, o
  llama.cpp decide sozinho quais camadas são recorrentes (levam
  `ssm_conv1d.weight`) por uma fórmula sobre o índice **absoluto** da camada
  (`(i+1) % full_attention_interval != 0`). `blk.(start+i)` -> `blk.i` muda esse
  índice; sem reescrever o array, uma fatia que não começa na camada 0 recalcula
  com o índice local errado e o llama.cpp exige `ssm_conv1d.weight` no bloco
  local errado - `missing tensor 'blk.0.ssm_conv1d.weight'` na fatia 35-64 do
  Qwen3.8-27B-Q3_K_S, em 2026-08-16, apesar do peso genuíno estar na 36 original.
  `splitter::split`/`merge` corrigem isso escrevendo o array explícito, derivado
  da presença real do tensor no arquivo fonte (não da fórmula) - ver §4.
- `slice_bytes(start, end, is_last)` - bytes de uma fatia: camadas + `token_embd`
  (toda fatia) + output head (só a última). É o que o plano soma.
- `ggml_type_info` + `ggml_type_table_matches_ggml` - a tabela de tipos é
  conferida contra a `GGML_TRUTH` no mesmo arquivo (teste de build quebra se
  divergirem; números escritos lendo o llama.cpp - `ggml.h` para os códigos,
  `ggml_type_size`/`ggml_blck_size` para os tamanhos -, nunca de memória).
  Errar um tamanho de tipo **não dá erro**: a fatia sai e só a resposta fica
  errada. Por isso um tipo desconhecido é recusa, não estimativa.

## 2.1 GGUF multipart

`multipart.rs` trata um GGUF simples como um conjunto de uma parte e um GGUF
com `split.count > 1` como uma fonte lógica. O catálogo valida `split.no`,
`split.count` e `split.tensors.count`, reconstrói os caminhos `N-of-M.gguf` e
lê somente o cabeçalho e o tamanho de cada parte. O caminho completo de
`GgufSource` é reservado para layout e fatiamento: nele os cabeçalhos e tabelas
de tensores são mapeados com `mmap`. A tabela unificada guarda, para cada tensor,
o índice da parte, o offset relativo à seção de dados e o tamanho calculado pela
diferença entre offsets.

Nenhum peso é remontado em arquivo temporário: o fatiador escolhe os tensores
pela faixa e copia cada um diretamente do mmap da parte que o contém. A mesma
fonte é consumida por `gguf::read_layout`, `splitter::validate_tensor_parallel`
e `splitter::shard_tensor_parallel`. Um GGUF sem `split.*` continua no caminho
de uma parte, com o mesmo comportamento.

O catálogo enumera somente a primeira parte, mas depois da leitura do cabeçalho
publica o tamanho físico somado e a lista ordenada de partes. Se uma parte
estiver ausente ou os campos declarados não fecharem, o modelo fica visível com
o motivo concreto, sem byte count falso e sem plano de fatiamento. O hash usado
para identidade de um modelo fonte é o SHA-256 da sequência ordenada de partes.
Esse hash é calculado somente na fronteira de fatiamento ou transferência, nunca
na resposta inicial do catálogo.
Transferências continuam sendo operações de arquivos POM individuais. O
endpoint de envio recusa uma fonte inteira que declara multipart, com a
orientação de manter o conjunto completo ou gerar fatias POM; a transferência
de um modelo inteiro multipart não foi generalizada nesta etapa.

## 3. Planejamento

`plan.rs` converte intenção (memória ou tamanho) em faixas de camadas. A regra de
arredondamento é para camada inteira e fica visível - a tela mostra lado a lado o
alvo pedido, a faixa que saiu e o tamanho real.

- `plan_split` - orçamentos por peer (`PeerBudget { node_id, label, budget_mb,
  has_model }`) -> faixas, greedy da camada 0 para cima. Cada peer reserva
  `DEFAULT_RESERVE_PCT` (15%) para KV cache, ativações e runtime; o último peer
  também tem que caber a cabeça de saída (`is_last_peer` muda o custo);
  `snap_end_layer` ajusta o corte para a fronteira de tipo. Avisa peer que fica
  sem camada e peer que não tem o modelo escolhido instalado.
- `plan_split_with_kv` - caminho de `POST /api/ui/plan` que cobra o custo em
  bytes para cada faixa: peso real da tensor table mais KV para
  `n_ctx * max_sessions`, com os tipos K e V configurados. A reserva é aplicada
  ao orçamento do peer antes da aritmética. O resultado traz
  `weight_bytes`, `kv_bytes` e `total_bytes`; geometria GGUF ausente, largura
  incompatível com bloco quantizado e overflow recusam o plano em vez de
  produzir uma estimativa silenciosa.
- `plan_by_size` - alvos em MB (`SizedSlice { target_mb }`) -> faixas
  (`SizedRange { index, start_layer, end_layer, layers, bytes, target_mb, is_rest,
  is_last, too_small, empty }`). `target_mb: None` = "o resto do modelo". Uma
  camada mínima indivisível aparece como `too_small`/`empty`; a última entrada
  da lista é sempre convertida para o resto, mesmo que um cliente antigo envie
  um alvo numérico; cobertura
  incompleta **não** é recusa - `SizePlan { covered_layers, complete,
  missing_range, missing_bytes, blocked, warnings }` reporta quanto falta.
- `plan_equal`/`even_split` - "dividir em N": igual em camadas (diferença <= 1),
  com o alvo das primeiras faixas preenchido pelo tamanho real e a última como
  resto. Só aceita 1..=64 fatias.
- O resultado de um `plan_by_size` é um `SizePlan`, serializado como prévia em
  `POST /api/ui/slice/plan` (a tela chama a cada tecla, com o layout em cache por
  arquivo).

  `SizePlan.model_bytes` é a soma dos arquivos GGUF que as faixas vão escrever,
  incluindo embeddings repetidos, tensores globais, cabeçalho, tabela de
  tensores, alinhamento e o norm sintético das fatias não finais.
  `source_model_bytes` conserva o tamanho do GGUF de origem para comparação.
  O custo recusa nominalmente qualquer dtype GGML desconhecido; nenhum tipo
  desconhecido entra como zero na prévia.

`POST /api/ui/slice/plan` também aceita `{ mode: "tensor_parallel", tp_size }`.
Nesse caso a resposta é um `TpSlicePlan` com um item por rank, todos cobrindo
`0..total_layers-1`. `POST /api/ui/slice` usa o mesmo modo para escrever todos
os ranks de uma vez e selar `tp_parts` no manifesto unificado. O rank guardado
no manifesto é zero-based; a interface mostra a identificação operacional
one-based `1ofN`, `2ofN` etc. O arquivo físico continua endereçado pelo hash do
conteúdo, conforme a regra de armazenamento por hash.

### O que a camada 0 significa

Quem fica com a camada 0 processa o prompt inteiro de cada requisição, então essa
fatia deve ir para a máquina com mais memória. É uma escolha do operador, não do
plano: a tela nomeia os papéis ("processa o prompt" / "gera os tokens").

## 4. Execução do fatiamento

### Cortes escolhidos durante a implantação

O deploy de um modelo inteiro consulta `GgufLayout::valid_prefix_end_layers`
antes de oferecer um slider. Em layout denso, todo fim de camada inteira é
aceito; em layout híbrido, só são aceitos os fins derivados das fronteiras
arquiteturais reais. `prefix_cut_granularity` é apenas uma descrição quando o
espaçamento é uniforme: cortes irregulares continuam sendo representados pela
lista explícita. `POST /api/ui/model-layer-options` expõe essa lista, e
`POST /api/ui/slice-ranges` usa as faixas selecionadas para executar o mesmo
fatiador local e selar o manifesto antes da transferência.

`extract_shard` (`plan.rs`) roda `splitter::split` num `spawn_blocking` e apaga o
arquivo parcial no erro - senão a próxima varredura o tomaria por fatia válida.
O corte sai numa pasta de trabalho (`pom::scratch_dir`), cada parte gravada
primeiro num `<uuid>.part` e renomeada para `<sha256>.gguf` quando termina - o
nome só se conhece no fim, porque o nome **é** o conteúdo. O sha256 sai do mesmo
passe que grava os bytes (`pom::HashingWriter`), sem custar uma segunda leitura.

No fim, `pom::finalize_run` guarda as partes em
`<shard_dir>/<modelo>-<hash curto do original>/` e sela
`<modelo>-<hash curto do original>.pom` lá. No único formato v1, `pom_id` é o
sha256 completo da fonte lógica: em GGUF simples é o arquivo; em multipart é a
sequência ordenada de partes, não um arquivo remontado. Assim, fatiar
novamente o mesmo modelo não cria outra pasta nem outro manifesto: as partes
novas são mescladas por `sha256`, inclusive quando cobrem faixas alternativas
como `L0-7` e `L0-3`. O mesmo manifesto contém `parts` para pipeline e
`tp_parts` para os shards de Tensor Parallelism.

O merge é protegido por lock por `pom_id` e grava o manifesto atomicamente.
Não há compatibilidade com outro formato de manifesto.

`splitter::split` (`splitter.rs`) é o fatiador em Rust, substituto de
`split_model.py` (o Python é legado, ver `scripts/AGENTS.md`):

1. Abre um GGUF simples ou todas as partes declaradas por `split.*` com
   mmap, valida mágica `GGUF` e versões 2/3, sem remontar os pesos.
2. Lê a tabela unificada; o tamanho de cada tensor vem da **diferença entre offsets**
   (mais confiável que a tabela de tipos: aguenta quant desconhecido e tensores
   weight-tied). Tensor de byte_size 0 é **recusa**, não estimativa - fatiar
   produziria um arquivo corrompido. Rank implausível também é recusa.
3. Seleciona tensores da faixa (`want_tensor`), remapeia `blk.(start+i).*` ->
   `blk.i.*`.
4. Fatia não-final injeta `output_norm.weight` identidade (o llama.cpp precisa
   dele para ler a fatia); zera `nextn_predict_layers` fora da última; atualiza
   `{arch}.block_count`.
5. Modelo híbrido (qualquer tensor `blk.N.ssm_conv1d.weight` na tabela): grava
   `{arch}.attention.recurrent_layers` explícito, sizado e indexado para a
   fatia, a partir de quais camadas **absolutas** realmente têm o tensor -
   nunca reimplementando a fórmula do llama.cpp. Modelo comum não ganha a
   chave. `merge` faz o mesmo sobre a numeração 0-based do arquivo unido.
6. Primeiro se decide se o array é por camada: ele precisa estar sob o
   namespace da arquitetura e não ser um array estrutural de rope. Só então um
   array escalar cujo comprimento é pelo menos `block_count` é recortado pelos
   índices absolutos da faixa e gravado com a contagem local. Comprimento 1
   continua broadcast; comprimento menor que `block_count` é recusa nominal.
   Arrays globais, como `tokenizer.ggml.tokens`, inclusive arrays de strings,
   são copiados intactos antes da comparação de comprimento. A regra `>=`
   permanece para vetores da arquitetura com cauda extra, como
   `compress_ratios`; a seleção é por posição, sem repetir um passo periódico.
7. Grava `pom.original_block_count/start_layer/end_layer` no KV - a metadata
   que torna o total de camadas confiável para as fatias novas.
8. Escreve com `ALIGN` de 32 bytes e copia dados em blocos de `CHUNK` (8 MB),
   chamando `on_progress(0..100)`; o progresso é do arquivo, não do trabalho
   (100% de uma fatia é 1/N do serviço).

### Reaproveitamento

Arquivo já existente é reaproveitado sem reescrever: o `SliceOutcome { output_path,
bytes, reused }` distingue "gerado" de "reusado", e a UI mostra isso.

### Shard por tensor (tensor parallelism, caminho C)

`splitter::shard_tensor_parallel(src, dst, tp_size, tp_rank, on_progress)`
produz um GGUF com os tensores do rank `tp_rank` de um grupo de `tp_size`. É a
dimensão ortogonal ao fatiamento por faixa de camadas: todas as camadas
continuam presentes, só com tensores menores. Não há remapeamento de nome nem
mudança de `block_count`.

A classificação é por nome (`tp_role`), estilo Megatron:

- **Coluna** (split por `ne[1]`, a dimensão de saída; fatia contígua, sem
  redução): `attn_q/k/v.weight`, `ffn_gate/up.weight`.
- **Linha** (split por `ne[0]`, a dimensão de entrada contígua; soma parcial a
  ser all-reduced): `attn_output.weight`, `ffn_down.weight`. Só vale para
  tensor cuja **entrada já chega particionada** - estes dois consomem a saída
  dos "coluna" do mesmo bloco.
- **Replicado** (igual em todos os ranks): `*_norm.weight`, `token_embd.weight`,
  rope/pos_embd e a cabeça de saída `output.weight`.

A cabeça esteve em "linha" e era um defeito: a entrada dela é a saída de
`output_norm.weight`, que é replicado, então o oculto que chega nela tem largura
cheia (`n_embd`) e nunca é particionado. Cortá-la por `ne[0]` produzia uma
cabeça esperando `n_embd/size` de entrada - largura que não existe em ponto
nenhum do grafo - e o llama.cpp recusava o load do shard:
`check_tensor_dims: tensor 'output.weight' has wrong shape; expected 4096,
128256, got 2048, 128256` (Llama-3.1-8B-Q4_K_M, 17/08). `validate_tensor_parallel`
não podia pegar isso: ela só confere se a dimensão **divide**, e 4096/2 divide.
Cortar a cabeça por vocabulário (`ne[1]`) seria a alternativa clássica, mas
exigiria um all-gather de logits - a coletiva do fork é um reduce - e reescrever
o vocabulário por rank, o que quebraria o tokenizer. Replicar custa memória e é
correto; `token_embd.weight` tem o mesmo formato e já era replicada.
- **Recusa** (não shardável com segurança, v1): tensores 3D de MoE
  (`ffn_gate_exps` etc.), nomes não reconhecidos, e `output.weight` empatado
  com `token_embd` (weight tying).

### A coletiva tem que somar 2 vezes por camada

São dois pontos row-parallel por bloco - a projeção de saída da atenção e o
`ffn_down` -, então um decode num modelo de 32 camadas dispara **64** reduces.
O motor conta e avisa quando o número não bate (`llm.rs`, depois do
`llama_decode`).

Isso é rede de proteção contra o defeito de 18/08: o fork tinha o `tp_reduce`
numa **única** sobrecarga de `build_attn` (a `attn_no_cache`), e o llama.cpp tem
sete. Todo modelo com cache KV somava só o FFN - 32 reduces em vez de 64 - e
cada rank rodava a atenção com metade das cabeças sem nunca receber a outra
metade.

O sintoma é o disfarce mais perigoso deste subsistema: **a resposta sai
gramatical e fluente, e só o conteúdo é vazio**. Não parece erro de cálculo,
parece modelo ruim. Por isso a contagem virou aviso automático em vez de ficar
na cabeça de quem depura.

### Carimbo de regra: `pom.tp_format`

Todo shard de tensor grava `pom.tp_format` (`splitter::TP_SHARD_FORMAT`), a
versão da regra que o escreveu, ao lado de `pom.tp_size`/`pom.tp_rank`.
`extract_tp_shard` só reaproveita um arquivo existente quando os três conferem;
carimbo ausente ou diferente é regeneração, com o motivo no log.

**Quem mudar o que o shard contém sobe esse número** - eixo de corte,
hiperparâmetro reescrito, tensor que passa a ser replicado.

Existe por um dia inteiro perdido: o critério de reuso era "existe e tem mais de
1 KB", que não distingue um shard atual de um escrito por uma versão com
defeito. Em 17/08 a ksia carregou o mesmo `_tp0of2.gguf` de cabeçalho não
dividido durante três correções seguidas, e o log anunciava "reaproveitando" a
cada tentativa - o sintoma era idêntico ao do defeito já corrigido, o que fez
procurar o problema no código que já estava certo.

### O cabeçalho é dividido junto com os tensores

Cortar o tensor não basta: o KV do shard tem que descrever o tensor que o shard
contém. Três chaves nomeiam exatamente as larguras particionadas e são
reescritas como `valor / tp_size`:

| Chave | Dimensão que ela descreve |
|---|---|
| `{arch}.attention.head_count` | colunas de `attn_q` (`n_head * head_dim`) |
| `{arch}.attention.head_count_kv` | colunas de `attn_k/v` (`n_head_kv * head_dim`) |
| `{arch}.feed_forward_length` | colunas de `ffn_gate/up` e linhas de `ffn_down` |

`embedding_length` e `block_count` **não** mudam: o eixo do resíduo atravessa o
all-reduce inteiro, e toda camada segue presente em todo rank. Deixar as três
passarem intactas foi o defeito de 17/08 - o tensor saía pela metade e o
cabeçalho prometia largura cheia, então o llama.cpp recusava com
`'blk.0.attn_q.weight' has wrong shape; expected 4096, 4096, got 4096, 2048`.

Divide-se a **contagem de cabeças**, nunca `head_dim`: é isso que mantém cada
cabeça inteira num rank só. Cabeça partida ao meio daria atenção errada sem
nenhum erro de forma para acusar. Largura que não divide por `tp_size` é recusa
nomeando a chave, nos dois caminhos (`validate_tensor_parallel` na tela e
`shard_tensor_parallel` na escrita).

O split por linha exige que `ne[0]` divida por `tp_size` em limite de bloco
do tipo ggml (senão o corte parte um bloco de quantização e o tensor fica
ilegível); a divisão por coluna exige `ne[1]` divisível por `tp_size`. Ambos
recusam em vez de estimar, na mesma regra do resto do fatiador.

A metadata de saída é `pom.tp_shard=1`, `pom.tp_size`, `pom.tp_rank`
(via `brand::gguf_key`). Um shard NÃO é um modelo autossuficiente: o llama.cpp
de hoje não o carrega sozinho - ele é a metade que o fork do llama.cpp (caminho
C) carrega junto com a coletiva all-reduce. Testado com o GGUF sintético
(`test_gguf_bytes`) para rank 0 e rank 1, incluindo a recusa de grupo inválido.

A mesma decisão tem um dry-run: `splitter::validate_tensor_parallel(src,
tp_size)` lê a tensor table e devolve `Err` nomeando o tensor que não dá para
partir, **sem escrever nada**. `shard_tensor_parallel` o chama como porta de
entrada (a classificação é uma só), e a rota `POST /api/ui/tp/check`
(`webui::tp_check`) o expõe ao coordenador do deploy, que recusa o grupo
tensor-parallel **na tela de implantar** (antes do job) em vez de deixar o apply
falhar na metade. MoE/SSM/weight-tied e dimensões que não dividem caem aqui.

`plan::extract_tp_shard` embrulha isso para o caminho
de `apply` (webui): rodam num `spawn_blocking`, reaproveitam arquivo já completo e
escrevem a parte endereçada por conteúdo, como qualquer outra.

## 5. Junção (merge)

`splitter::merge` junta fatias adjacentes num arquivo único (`MergePart { path,
start_layer, end_layer }`). Usado pelo deploy com `merge` ligado: uma máquina que
já tem duas faixas adjacentes vira uma unidade fundida, servindo a união. `parse_merged_src`
valida mágica, versão e plausibilidade; tensores são selecionados por
`blk.*` (remapeados para o lugar original) + output head (só da última) +
`token_embd` (só da primeira); fatia com tensor de byte_size 0 é recusa
("juntar produziria um arquivo corrompido"). O arquivo de saída sai como temp com
sufixo único e é renomeado no fim - nunca fica um arquivo pela metade com nome
final.

## 6. A fatia servida sobrevive ao reinício

`lastslice.rs` anota em `<config>.last-slice.json` a fatia que este nó serve
(caminho derivado da config, senão dois nós da mesma máquina se sobrescrevem).
A anotação carrega um **estado**, e ele é a lei:

- `pending` - escrito por `runtime::apply`. Só registra o que o `apply`
  decidiu, não o que o servidor conseguiu carregar - não retoma nada.
- `serving` - escrito **só** por `spawn_health_watch`, quando o `/health` do
  servidor de inferência responde `model_loaded`. É o único ponto do código que
  prova que a fatia está no ar, e o único que autoriza retomá-la num reinício.
- apagada - por `stop` (decisão do operador) e pelo ramo que desiste em
  `MAX_RESTARTS`. Arquivo órfão (sem nó vivo) não ressuscita nada.

No arranque o arquivo é **consumido-e-apagado** (`take`): `Serving` reaplica a
fatia; `NeverLoaded`/recusa chama `note_unloadable_slice` (a razão vai para o
`status.detail`, que é o que o painel mostra - um nó vazio e calado é
indistinguível de uma máquina que nunca recebeu fatia). Sem este mecanismo um
segfault do kernel CUDA (2026-08-13) derrubava o nó, o container religava e o
nó voltava sem fatia - sumindo do pipeline em silêncio. O outro lado da moeda
é o laço infinito de 2026-08-15: anotar no `apply` o que só o `/health` sabe
fazia a fatia que não cabe na memória matar o processo antes de carregar, o
container religar, e a mesma fatia ressuscitar para sempre (`MAX_RESTARTS` é
contador de memória e morre com o processo). Reprodução E2E em
`data/pom-reinicio-infinito-carga/`.

## 7. Estimativa de memória e perfis

`model.rs` tem os perfis embutidos (`ModelProfile { name, total_layers,
memory_per_layer_mb, overhead_mb, description }`), `from_ollama_model` para
perfis por tamanho de arquivo real, e `guess_from_name` como último recurso
(ver defeitos abaixo). `memory.rs` carrega a faixa de camadas para a RAM com três
estratégias em ordem (GGUF-aware por offsets reais, proporcional por byte,
`vec![0]` como pior caso), tocando cada página 4K para trazer a faixa para a
RAM física.

## 8. Rotas

| Rota | Papel |
|---|---|
| `GET /api/ui/models` | catálogo local (modelos + shard_sets + diretórios varridos) |
| `POST /api/ui/slice/plan` | prévia de fatiamento (`plan_by_size`/`plan_equal`) |
| `POST /api/ui/slice` | executa o fatiamento local (`slice_locally` -> `extract_shard`) |
| `POST /api/ui/model-layer-options` | cortes de prefixo aceitos pelo GGUF inteiro selecionado |
| `POST /api/ui/slice-ranges` | materializa faixas do GGUF inteiro e devolve caminhos por hash |
| `POST /api/ui/plan` | plano por orçamento do cluster (delega ao peer dono do GGUF quando preciso) |
| `POST /api/ui/apply` | aplica uma fatia (`skip_split`, `extract_shard` ou `merge_parts`) |

## 9. Testes

`splitter.rs` expõe `test_gguf_bytes(n_blocks)` - um GGUF pequeno mas
estruturalmente válido, compartilhado com os testes de `webui` para exercitar o
caminho de merge com arquivos reais. Em `plan.rs`, os testes provam: alvo menor
que uma camada é recusado com o número na mensagem; fatias que não cobrem o
modelo reportam quanto falta (`missing_range`) sem bloquear; pedir mais fatias
que o modelo tem deixa as extras vazias; o corte em `nextn_predict_layers` só
vale na última camada. `ggml_type_table_matches_ggml` quebra o build se a
tabela de tipos Rust divergir da `GGML_TRUTH` no mesmo arquivo (`gguf.rs`) -
números escritos lendo o llama.cpp, não de memória.

## 10. Defeitos anotados

- **DEFEITO ANOTADO: o doc do módulo `plan.rs` fala do `split_model.py`**
  (`crates/node/src/models/plan.rs:9-12`) - o doc diz que o fatiamento "continua
  no `split_model.py`, que sempre aceitou `--start-layer`/`--end-layer`", mas o
  código chama `splitter::split` (o Rust). O próprio
  `crates/node/src/models/AGENTS.md` reconhece: "A migração aconteceu e o doc não
  seguiu". O Python (`old/python-inference/split_model.py`) é legado.
- **DEFEITO ANOTADO: `guess_from_name` inventa camadas e tamanhos**
  (`crates/node/src/models/model.rs:109`) - fatia antiga sem metadata `pom.*`
  não tem como corrigir o total, e o palpite vem da heurística do nome de arquivo
  (qualquer nome contendo `qwen` vira `qwen35` 32/380/700; "9b"/"8b"/"7b" ->
  7b-llm 32/380/700; "3b"/"4b" -> 3b-llm 32/200/400; "1b"/"2b" -> 1b-llm 28/130/300;
  "small-llm" -> 40/600/1200; sem casar nenhum -> "unknown-llm" 32/300/600). O
  catálogo deduz o total da cobertura quando dá, mas `guess_from_name` é o último
  recurso - já causou o bug de 2026-08-10 (plano "completo" que morria em
  `NodeConfig::validate`).
