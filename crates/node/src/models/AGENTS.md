# `crates/node/src/models/`

> **Mudança de crate:** `gguf.rs`, `multipart.rs` e `kv_budget.rs` saíram para
> `crates/llama-engine` (o lado FFI precisa deles). `node` os re-exporta em
> `main.rs`; `crate::gguf::…` etc. seguem valendo. `splitter.rs` ficou aqui e
> usa `crate::gguf` normalmente (nos testes, `llama_engine::llama_cpp_sys_2`).

Modelos, leitura de GGUF, planejamento e arquivos de fatias. O módulo é usado
por `main.rs` e pelos handlers de catálogo, fatiamento, implantação e
transferência.

- `catalog.rs` - descobre modelos inteiros e fatias nas pastas configuradas.
  `discover_fast` enumera arquivos e lê apenas `stat`/metadados do sistema,
  sem abrir o conteúdo GGUF; `enrich` lê os cabeçalhos depois e marca
  `Catalog.metadata_complete`. Falha de leitura marca `metadata_error` no item,
  sem impedir os modelos válidos; depois de uma passagem terminal,
  `metadata_complete` vira `true`. Nova tentativa só ocorre em um novo refresh
  ou invalidação. As capacidades publicadas pelo catálogo vêm somente do
  cabeçalho; ele não percorre a tabela de tensores nem os pesos. A evidência
  estrutural de MTP continua em `gguf::read_capability_evidence`, hoje restrita
  às provas que exigem essa confirmação; o cálculo completo de layout continua
  separado.
  `discover` combina as duas fases para callers síncronos que ainda precisam da
  resposta completa. O catálogo não calcula hash de conteúdo: hashes vindos de
  manifestos ou do nome de uma fatia são apenas dados de identidade.
  Valores numéricos do cabeçalho GGUF são aceitos pela semântica de inteiro não
  negativo, independentemente de o produtor tê-los escrito como signed ou
  unsigned; valores negativos e overflow são recusados. Isso inclui
  `split.tensors.count`, que o `gguf-split` de referência escreve como INT32.
  Cada resultado enriquecido guarda apenas em memória a identidade
  `path + size + mtime` do arquivo. O backend usa essa marca para reaproveitar
  metadata quando um refresh cria outro inventário e para enriquecer somente
  modelos novos ou alterados.
  Para uma fonte GGUF multipart, `enrich` lê os cabeçalhos e faz `stat` das
  partes por `multipart::catalog_metadata`, sem abrir o mapeamento completo de
  pesos; soma os bytes e expõe as partes físicas no catálogo. Uma declaração
  incompleta fica visível com erro concreto e tamanho desconhecido. A identidade
  do modelo inteiro usa o hash da sequência completa de partes somente na
  fronteira de fatiamento ou transferência.
- `plan.rs` - calcula faixas por orçamento ou por tamanho real de camada e
  chama o extrator de GGUF. O endpoint de orçamento usa `GgufInfo` e
  `kv_budget.rs` para somar peso e KV em bytes antes de aceitar uma faixa. O
  plano valida o contrato de tensores de saída da arquitetura antes de oferecer
  ou gerar uma faixa; cabeças obrigatórias são cobradas e copiadas da origem
  quando o loader exige sua presença em faixas parciais.
- `gguf.rs` - lê a geometria de KV escalar ou por camada
  (`attention.head_count_kv`, `key_length` e `value_length`) e a evidência de
  MoE (`<arch>.expert_count`/`expert_used_count`) sem carregar os pesos. Também
  lê `tokenizer.chat_template`, tokens de controle reais e os campos/tensores
  `nextn` para produzir a evidência de thinking e MTP usada pelo planner. A
  chave canônica `<arquitetura>.nextn_predict_layers` e a forma sem prefixo são
  aceitas; a presença dos tensores `nextn` é prova estrutural quando o produtor
  omite a metadata. A ausência de leitura permanece indeterminada. `read_layout`
  valida os dtypes contra a tabela GGML suportada, descarta essa informação após
  a validação, usa offsets físicos para o custo dos tensores e calcula o tamanho
  serializado de cada fatia, incluindo cabeçalho, globais, alinhamento e
  duplicações e cabeças específicas da arquitetura. `validate_slice_ranges`
  compartilha a decisão com o fatiador. `deepseek4` exige seis tensores
  não-bloco, não aceita fallback para `token_embd`; faixas não finais copiam da
  origem os tensores reais da cabeça que o loader exige e mantêm o norm de
  identidade já usado pelo tronco. Se a origem não tiver um tensor copiável, a
  recusa nomeia os tensores ausentes. `LayerCountSemantics` classifica metadata
  que o llama.cpp compara com o índice local: contagens desde o início são
  rebaseadas por `start_layer` e limitadas ao tamanho da fatia, enquanto a
  contagem desde o fim do NextN é zerada fora da última faixa. Valor não escalar
  ou que não caiba em `u32` recusa o layout com a chave e a causa.
  `GgufLayout::required_head_bytes` mede o payload real repetido por fatia
  intermediária.
  `read_layout` usa a tabela unificada de multipart.rs, então o
  primeiro arquivo de um GGUF multipart, mesmo sem tensores, representa o
  conjunto inteiro. O custo e a seleção também consideram tensores presentes
  em todas as partes físicas, inclusive a terceira ou posteriores.
- multipart.rs - mapeia um GGUF simples ou todas as partes declaradas por
  split.count, split.no e split.tensors.count. Valida a convenção N-of-M.gguf,
  resolve cada tensor para parte e offset relativo e soma/hash somente a
  sequência física, sem remontar pesos.
 - As opções de corte para um modelo inteiro vêm de
  `GgufLayout::valid_prefix_end_layers`: em layout denso, qualquer fim de
  camada inteira é válido; em layout híbrido, só os fins derivados de
  `arch_boundaries` são válidos. `prefix_cut_granularity` só informa um passo
  quando todos os cortes têm a mesma distância; o consumidor usa a lista de
  cortes quando as fronteiras são irregulares.
- `splitter.rs` - mapeia a fonte para ler o cabeçalho e os pesos sem manter fd
  de GGUF aberto. A tabela lógica não é uma ordem de armazenamento: os payloads
  selecionados são ordenados por `(part_index, src_offset)` antes da cópia para
  evitar page faults regressivos em fonte maior que a RAM. Antes da saída, o
  fatiador confere a lista de tensores obrigatórios que o loader cria para a
  arquitetura. Para `deepseek4`, a faixa final carrega a cabeça completa e cada
  faixa intermediária copia os pesos obrigatórios reais, sem fabricar
  `output.weight` nem tensores HC. Metadata de contagem relativa ao início é
  reescrita para a numeração local da faixa; metadata relativa ao fim conserva
  o valor somente na faixa final. A estimativa de tamanho inclui a repetição
  e o plano expõe o custo adicional. A estimativa TP soma a
  replicação de tensores compartilhados por rank e reserva o cabeçalho gerado.
  Na fusão, arrays escalares sob o namespace da arquitetura são reconstruídos
  pela cobertura de cada fatia, em vez de copiar a primeira; qualquer cauda de
  camadas especiais é preservada uma vez. No `qwen4exp`, `ple.layers` é
  filtrado e rebaseado para a numeração local, enquanto
  `ple.layer_multipliers`, `ple.head_offsets` e `ple.head_vocab_sizes` são
  preservados inteiros. `per_layer_token_embd.weight` só entra na fatia que
  contém uma camada PLE, e os três tensores `output_hc_*` obrigatórios são
  copiados para cada fatia. O resultado é validado por `gguf::read_layout`
  antes de o backend o nomear por conteúdo.
- `kv_budget.rs` - política pura de bytes do cache KV. Replica o tamanho de
  linha dos tipos suportados e recusa largura que não fecha bloco ou qualquer
  overflow, sem consultar FFI ou dispositivo.
- `storage.rs` - calcula uso recursivo da pasta de fatias, registra e ignora
  filhos ilegíveis. A rotina de retenção continua disponível para uma ação
  explícita, mas o fatiamento não a chama: gerar uma faixa não apaga fatias
  anteriores em silêncio.
- `provider.rs` - contrato `ModelProvider`, tipos comuns de descoberta,
  download, progresso, licença, armazenamento e manifesto.
- `ollama.rs` - cliente opcional do Ollama; geração mantém o timeout longo do
  backend. `is_available` devolve o último valor em cache imediatamente e
  agenda uma sonda de 500 ms no máximo a cada 15 s, para o `/health` não abrir
  uma conexão ao serviço opcional em cada requisição.
- `huggingface.rs` - implementação `HuggingFaceProvider` por HTTP nativo, com
  validação de entradas, retomada por faixa, cache próprio e manifesto local.

O token do Hugging Face é resolvido em cada requisição nesta ordem: `HF_TOKEN`,
`HUGGING_FACE_HUB_TOKEN`, o segredo do `pom.db` e, por fim, o arquivo de cache
(`HF_TOKEN_PATH`, `HF_HOME/token` ou
`~/.cache/huggingface/token`). A configuração da UI fica em memória no
`HfTokenStore` e no `pom.db`, então salvar na tela atualiza autenticação sem
reiniciar o nó.
Nenhum endpoint devolve o token bruto; erros continuam passando por
`redact_sensitive`.

Mensagens internas de `HfError` continuam passando por `redact_sensitive`, que
aceita texto emprestado ou possuído sem alterar a redação de tokens, headers de
autorização e `HF_TOKEN`. O teste de owned text fica em `provider.rs`; a suíte
também cobre que erros públicos não devolvem detalhes do provider.

O provider usa os endpoints JSON do Hub e a URL `resolve` para busca, detalhes,
arquivos, licença, autenticação e downloads. O download faz streaming para
`.pom-hf/partials`, retoma com `Range` e `If-Range` somente após validar o
tamanho e a identidade remota, cancela removendo o parcial e verifica SHA-256
antes de finalizar. O backend mantém o JSON de status compatível com a tela,
mas o campo `cli.installed` é verdadeiro quando o provider Rust está disponível.
Callers internos devem consumir `ModelProvider`/`ProgressSender`, nunca montar
URLs do Hugging Face diretamente.

`pom.rs` mantém o manifesto por modelo original: no único formato v1, `pom_id`
é o sha256 do GGUF fonte, `parts` reúne fatias de camadas alternativas e
`tp_parts` reúne shards de Tensor Parallelism. Novas fatias do mesmo modelo são
mescladas na mesma pasta e no mesmo manifesto nomeado como a pasta; o catálogo
só expõe as partes cujo arquivo existe no disco. `catalog::ShardSet` mantém as
duas listas separadas e preserva `tp_rank`/`tp_size` para a UI agrupar por
tamanho e exibir `1ofN`.
Uma faixa avulsa encontra a pasta pelo `pom_id`, acrescenta sua parte ao
manifesto existente e só reaproveita uma parte quando o arquivo confere por
SHA-256 e tamanho.

`deployment::DeploymentIdentity` persiste a prova do artefato que entrou em
serviço: `pom_id` para uma rodada de fatias ou SHA-256 do GGUF inteiro. O
registro completo da implantação vive em `backend::db::PomDb`; os JSON em
`deployments/` são fonte legada preservada para a ferramenta descartável. Registros
antigos sem essa prova só podem ser associados por caminho canônico no mesmo
peer, nunca por nome.

`Deployment` é versionado (`DEPLOYMENT_VERSION = 2`): `versions:
Vec<DeploymentVersion>` guarda cada configuração que já rodou sob o mesmo
`deploy_id` e `active_version` aponta a que está no ar; `nodes` é a lista
achatada da versão ativa, mantida em sincronia. O que distingue uma versão é o
`RuntimeParams` do deployment inteiro (`params`); ajustes por máquina entram no
snapshot `nodes` mas não forkam versão. `upsert_version` reusa a versão de
`params` iguais ou anexa `max+1`; `set_active_version` só troca a ativa;
`ensure_versions` sintetiza a v1 de um registro v1 legado (sem `versions`), e é
chamado em toda leitura (`PomDb`, `find_record`, listagem). `matching_shape` é a
função pura que decide se um `POST /api/ui/deploy` reaproveita um `deploy_id`:
mesmo `kind`, mesma `DeploymentIdentity` provada e o mesmo conjunto de
`(node_id, faixa, tp_rank)`. Sem identidade provada, cunha id novo.

`DeploymentKind::Mixed` representa um GGUF inteiro e uma cabeça fatiada no mesmo
deployment. `Deployment::append_version` sempre cria `max+1` para alterações de
topologia, mesmo com os mesmos `RuntimeParams`, e mantém o conjunto de máquinas
da nova versão no histórico. A remoção parcial preserva a versão como ativa; a
última remoção grava uma versão ativa vazia, que é válida para o histórico mas
não é escolhida por `active()` como deployment vivo.

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.

`lastslice.rs` serializa o `NodeConfig` inteiro com `serde(flatten)`. Portanto,
as configurações do scheduler local (`batching_enabled`, limites da fila,
janela e limites do microbatch) também sobrevivem a um reinício e voltam com a
mesma fatia servida. Os tipos globais do cache KV (`cache_type_k` e
`cache_type_v`) e a margem de segurança do fit também são serializados no
estado efetivo. Os tipos do cache continuam sendo repostos a partir do
`pom.json` pelo `NodeRuntime` antes de carregar o modelo; a margem vem dos
parâmetros da implantação. Os parâmetros do detector de ciclos
(`cycle_detection_*`) também fazem parte do `NodeConfig` achatado, para que o
gateway preserve a mesma política depois de um restart e a fatia anotada não
volte a gerar com uma configuração diferente.

O fatiador primeiro classifica arrays GGUF pelo namespace: somente arrays sob o
namespace da arquitetura, exceto os arrays estruturais de rope, podem ser
metadata por camada. Nesses arrays, comprimento pelo menos igual à contagem
original de blocos é recortado para a faixa local, preservando a ampliação que
suporta vetores como `compress_ratios` com cauda extra. O llama.cpp lê os
primeiros `block_count` elementos desses vetores; por isso valores extras não
são preservados na fatia. Arrays globais, inclusive `tokenizer.ggml.tokens` e
arrays de strings, são copiados intactos antes de qualquer comparação de
comprimento. Comprimento 1 é broadcast e permanece igual; um comprimento
positivo menor que `block_count` em array da arquitetura é recusa nominal com
chave e tamanho. Os quatro arrays `qwen4exp.ple.*` são tratados por semântica
nominal: `layer_multipliers`, `head_offsets` e `head_vocab_sizes` representam
estrutura compartilhada da tabela e são copiados inteiros em cada fatia;
`ple.layers` é a lista de camadas PLE, filtrada para a faixa e rebaseada para
o índice local. O tensor `per_layer_token_embd.weight` acompanha somente as
fatias que contêm uma dessas camadas.

### Projetor multimodal

`catalog.rs::find_mmproj_sibling` segue a convenção do llama.cpp: GGUF `.gguf`
vizinho no mesmo diretório com `mmproj` no nome, escolhendo a quantização mais
próxima. `DiscoveredModel.mmproj_path` é a resposta persistida no catálogo;
`ShardSet.mmproj_path` herda o caminho de uma fatia ou do modelo inteiro de
origem com o mesmo rótulo. A interface não deve varrer o disco. O projetor não
é um modelo selecionável.

## Manutenção deste arquivo

Este arquivo é um mapa curto da pasta. Ao adicionar metadados de modelo ou
mudar a fronteira entre catálogo, GGUF e planejamento, atualize aqui e o
subsistema correspondente no mesmo commit. Detalhes de implementação ficam no
código ou em `docs/subsystems/`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
