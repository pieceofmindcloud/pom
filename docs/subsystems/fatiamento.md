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

O nome da fatia é a convenção que sustenta tudo:

- `parse_shard_name` - `qwen_0_15.gguf` -> `("qwen", 0, 15)`. Os dois últimos
  campos separados por `_` são a faixa. Parte `-00002-of-00003` (`is_secondary_part`)
  é pulada.
- `parse_quant` - quantização ignorando o sufixo da fatia.
- `build_shard_sets` - agrupa fatias pelo base (`shard_group`), ordena por faixa,
  mede a cobertura contígua a partir da camada 0 e deduz o total da cobertura
  quando a fatia não declara (`total_layers_inferred`). A flag `complete` diz se
  a cobertura fecha o modelo.

`total_layers` é `Option` do catálogo ao plano: fatia antiga sem metadata
`pom.*` não declara nada, e "não sei" e "zero" são fatos diferentes
(colapsá-los foi o bug de 2026-08-10, quando um plano "completo" morria em
`NodeConfig::validate`). O catálogo serve o resultado anterior enquanto revalida
por trás: `local_catalog` usa `CATALOG_TTL = 20 s` com stale-while-revalidate, e
`warm_catalog` roda no arranque para um volume lento não segurar a porta do
gateway (medido: primeira carga fria de `/api/ui/models` caía de 11,9 s para
0,18 s com o aquecimento).

## 2. Leitura do GGUF: cabeçalho e tensor table

`gguf.rs` lê o GGUF sem carregar pesos:

- `read_info` - só o KV header: arquitetura, `block_count`, `pom.*`
  (original/total), `shard_range`, `embedding_length`. Quem decide se um
  arquivo é fatia é `catalog::InstalledModel::is_shard`; a cópia que existia em
  `GgufInfo` não tinha chamador e saiu.
- `read_layout` - tensor table -> `GgufLayout` com bytes **reais** por camada
  (`layer_bytes`, `embedding_bytes`, `head_bytes`, `other_bytes`, `total_bytes`,
  `layer_kinds`). É o que torna o orçamento de memória honesto. Tipo ggml
  desconhecido sub-reporta em `other_bytes` em vez de inventar.
- `arch_boundaries`/`snap_end_layer` - modelos híbridos (Mamba + Attention) não
  são pilha de blocos iguais; o corte só acontece em fronteira de tipo, senão o
  bloco renumera para o tipo errado (`missing tensor` ou resposta errada
  silenciosa).
- `nextn_predict_layers` - MTP/Qwen3.5+: o bloco nextn vive só na última camada;
  fatia que não termina no fim do modelo precisa zerar o hiperparâmetro ou o
  llama.cpp aborta o load procurando `blk.<last>.nextn.*`.
- `attention.recurrent_layers` - modelo híbrido attention/SSM (Qwen3-Next-style,
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
- `plan_by_size` - alvos em MB (`SizedSlice { target_mb }`) -> faixas
  (`SizedRange { index, start_layer, end_layer, layers, bytes, target_mb, is_rest,
  is_last, too_small, empty }`). `target_mb: None` = "o resto do modelo". Uma
  camada mínima indivisível aparece como `too_small`/`empty`; cobertura
  incompleta **não** é recusa - `SizePlan { covered_layers, complete,
  missing_range, missing_bytes, blocked, warnings }` reporta quanto falta.
- `plan_equal`/`even_split` - "dividir em N": igual em camadas (diferença <= 1),
  com o alvo de cada faixa preenchido pelo tamanho real. Só aceita 1..=64 fatias.
- O resultado de um `plan_by_size` é um `SizePlan`, serializado como prévia em
  `POST /api/ui/slice/plan` (a tela chama a cada tecla, com o layout em cache por
  arquivo).

### O que a camada 0 significa

Quem fica com a camada 0 processa o prompt inteiro de cada requisição, então essa
fatia deve ir para a máquina com mais memória. É uma escolha do operador, não do
plano: a tela nomeia os papéis ("processa o prompt" / "gera os tokens").

## 4. Execução do fatiamento

`extract_shard` (`plan.rs`) roda `splitter::split` num `spawn_blocking` e apaga o
arquivo parcial no erro - senão a próxima varredura o tomaria por fatia válida.
O arquivo de saída é `{base}_{start}_{end}.gguf` em `<shard_dir>/<base>/`
(`base_name_of` tira o sufixo `_<start>_<end>`; `shard_filename` o remonta).

`splitter::split` (`splitter.rs`) é o fatiador em Rust, substituto de
`split_model.py` (o Python é legado, ver `scripts/AGENTS.md`):

1. Abre o GGUF por mmap, valida mágica `GGUF` e versões 2/3.
2. Lê a tensor table; o tamanho de cada tensor vem da **diferença entre offsets**
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
6. Grava `pom.original_block_count/start_layer/end_layer` no KV - a metadata
   que torna o total de camadas confiável para as fatias novas.
7. Escreve com `ALIGN` de 32 bytes e copia dados em blocos de `CHUNK` (8 MB),
   chamando `on_progress(0..100)`; o progresso é do arquivo, não do trabalho
   (100% de uma fatia é 1/N do serviço).

### Reaproveitamento

Arquivo já existente é reaproveitado sem reescrever: o `SliceOutcome { output_path,
bytes, reused }` distingue "gerado" de "reusado", e a UI mostra isso.

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
