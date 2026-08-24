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

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.

`lastslice.rs` serializa o `NodeConfig` inteiro com `serde(flatten)`. Portanto,
as configurações do scheduler local (`batching_enabled`, limites da fila,
janela e limites do microbatch) também sobrevivem a um reinício e voltam com a
mesma fatia servida. Os tipos globais do cache KV (`cache_type_k` e
`cache_type_v`) também são serializados no estado efetivo, mas o `NodeRuntime`
os repõe a partir do `pom.json` antes de carregar o modelo.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
