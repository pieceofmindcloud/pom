# `crates/node/src/models/`

Modelos, leitura de GGUF, planejamento e arquivos de fatias. O módulo é usado
por `main.rs` e pelos handlers de catálogo, fatiamento, implantação e
transferência.

- `catalog.rs` - descobre modelos inteiros e fatias nas pastas configuradas.
  `discover_fast` enumera arquivos e lê apenas `stat`/metadados do sistema,
  sem abrir o conteúdo GGUF; `enrich` lê os cabeçalhos depois e marca
  `Catalog.metadata_complete`. Falha de leitura mantém o item pendente para
  nova tentativa, em vez de publicar sucesso. As capacidades vêm do header e
  de uma varredura leve dos nomes de tensores `nextn`; o cálculo completo de
  layout continua em `gguf::read_capability_evidence` para callers que exigem
  essa prova.
  `discover` combina as duas fases para callers síncronos que ainda precisam da
  resposta completa. O catálogo não calcula hash de conteúdo: hashes vindos de
  manifestos ou do nome de uma fatia são apenas dados de identidade, e o
  SHA-256 integral continua sendo trabalho de manifesto/transferência.
- `plan.rs` - calcula faixas por orçamento ou por tamanho real de camada e
  chama o extrator de GGUF. O endpoint de orçamento usa `GgufInfo` e
  `kv_budget.rs` para somar peso e KV em bytes antes de aceitar uma faixa.
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
  duplicações.
 - As opções de corte para um modelo inteiro vêm de
  `GgufLayout::valid_prefix_end_layers`: em layout denso, qualquer fim de
  camada inteira é válido; em layout híbrido, só os fins derivados de
  `arch_boundaries` são válidos. `prefix_cut_granularity` só informa um passo
  quando todos os cortes têm a mesma distância; o consumidor usa a lista de
  cortes quando as fronteiras são irregulares.
- `kv_budget.rs` - política pura de bytes do cache KV. Replica o tamanho de
  linha dos tipos suportados e recusa largura que não fecha bloco ou qualquer
  overflow, sem consultar FFI ou dispositivo.
- `storage.rs` - calcula uso recursivo da pasta de fatias e remove diretórios
  gerados mais antigos quando o limite configurado seria excedido. A saída
  atual e o modelo ativo são protegidos.
- `provider.rs` - contrato `ModelProvider`, tipos comuns de descoberta,
  download, progresso, licença, armazenamento e manifesto.
- `huggingface.rs` - implementação `HuggingFaceProvider` por HTTP nativo, com
  validação de entradas, retomada por faixa, cache próprio e manifesto local.

O token do Hugging Face é resolvido em cada requisição nesta ordem: `HF_TOKEN`,
`HUGGING_FACE_HUB_TOKEN`, o campo `huggingface_token` do `pom.json` e, por fim,
o arquivo de cache (`HF_TOKEN_PATH`, `HF_HOME/token` ou
`~/.cache/huggingface/token`). A configuração da UI fica em memória no
`HfTokenStore`, então salvar na tela atualiza autenticação sem reiniciar o nó.
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

`deployment::DeploymentIdentity` persiste a prova do artefato que entrou em
serviço: `pom_id` para uma rodada de fatias ou SHA-256 do GGUF inteiro. Registros
antigos sem essa prova só podem ser associados por caminho canônico no mesmo
peer, nunca por nome.

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.

`lastslice.rs` serializa o `NodeConfig` inteiro com `serde(flatten)`. Portanto,
as configurações do scheduler local (`batching_enabled`, limites da fila,
janela e limites do microbatch) também sobrevivem a um reinício e voltam com a
mesma fatia servida. Os tipos globais do cache KV (`cache_type_k` e
`cache_type_v`) também são serializados no estado efetivo, mas o `NodeRuntime`
os repõe a partir do `pom.json` antes de carregar o modelo.

O fatiador trata arrays GGUF sob o namespace da arquitetura como metadata por
camada quando o comprimento é a contagem original de blocos e os reescreve para
a faixa local. Comprimento 1 é broadcast e permanece igual; arrays estruturais
de rope e namespaces globais não são arrays por camada. Qualquer outro
comprimento sob a arquitetura é recusa nominal com chave e tamanho, para que um
metadado novo não seja copiado cru para uma fatia que não carrega.

## Manutenção deste arquivo

Este arquivo é um mapa curto da pasta. Ao adicionar metadados de modelo ou
mudar a fronteira entre catálogo, GGUF e planejamento, atualize aqui e o
subsistema correspondente no mesmo commit. Detalhes de implementação ficam no
código ou em `docs/subsystems/`.
