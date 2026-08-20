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

`pom.rs` mantém o manifesto por modelo original: no formato v2, `pom_id` é o
sha256 do GGUF fonte, `parts` reúne fatias de camadas alternativas e `tp_parts`
reúne shards de Tensor Parallelism. Novas fatias do mesmo modelo são mescladas
por hash e o catálogo só expõe as partes cujo arquivo existe no disco.

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.

`lastslice.rs` serializa o `NodeConfig` inteiro com `serde(flatten)`. Portanto,
as configurações do scheduler local (`batching_enabled`, limites da fila,
janela e limites do microbatch) também sobrevivem a um reinício e voltam com a
mesma fatia servida.
