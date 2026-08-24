# `crates/node/src/models/`

Modelos, leitura de GGUF, planejamento e arquivos de fatias. O módulo é usado
por `main.rs` e pelos handlers de catálogo, fatiamento, implantação e
transferência.

- `catalog.rs` - descobre modelos inteiros e fatias nas pastas configuradas.
- `plan.rs` - calcula faixas por orçamento ou por tamanho real de camada e
  chama o extrator de GGUF.
- `storage.rs` - calcula uso recursivo da pasta de fatias e remove diretórios
  gerados mais antigos quando o limite configurado seria excedido. A saída
  atual e o modelo ativo são protegidos.
- `provider.rs` - contrato `ModelProvider`, tipos comuns de descoberta,
  download, progresso, licença, armazenamento e manifesto.
- `hf_cli.rs` - execução segura e parser isolado da saída do executável `hf`.
- `huggingface.rs` - implementação `HuggingFaceProvider` sobre o runner do
  CLI, com validação de entradas, cache externo e manifesto local.

Mensagens internas de `HfError` continuam passando por `redact_sensitive`, que
aceita texto emprestado ou possuído sem alterar a redação de tokens, headers de
autorização e `HF_TOKEN`. O teste de owned text fica em `provider.rs`; a suíte
também cobre que erros públicos não devolvem detalhes do provider.

O provider usa `hf --version`, `hf auth whoami`, `hf models ls/info/card` e
`hf download` por argumentos separados. O resultado do download enumera apenas
arquivos locais regulares, calcula SHA-256 com o mesmo `pom::hash_file` usado
pelos manifestos POM e não executa conteúdo remoto. O backend expõe o status do
CLI e usa a licença do contrato no detalhe do modelo; a tela remota mostra o
estado de autenticação e a licença antes do download. Callers internos devem
consumir `ModelProvider`/`ProgressSender`, nunca montar `hf` diretamente.

`pom.rs` mantém o manifesto por modelo original: no único formato v1, `pom_id`
é o sha256 do GGUF fonte, `parts` reúne fatias de camadas alternativas e
`tp_parts` reúne shards de Tensor Parallelism. Novas fatias do mesmo modelo são
mescladas na mesma pasta e no mesmo manifesto nomeado como a pasta; o catálogo
só expõe as partes cujo arquivo existe no disco. `catalog::ShardSet` mantém as
duas listas separadas e preserva `tp_rank`/`tp_size` para a UI agrupar por
tamanho e exibir `1ofN`.

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.

`lastslice.rs` serializa o `NodeConfig` inteiro com `serde(flatten)`. Portanto,
as configurações do scheduler local (`batching_enabled`, limites da fila,
janela e limites do microbatch) também sobrevivem a um reinício e voltam com a
mesma fatia servida. Os tipos globais do cache KV (`cache_type_k` e
`cache_type_v`) também são serializados no estado efetivo, mas o `NodeRuntime`
os repõe a partir do `pom.json` antes de carregar o modelo.
