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

A última entrada de um plano por tamanho é sempre o resto do modelo. O limite
de armazenamento vale somente para a pasta de fatias; pastas de modelos
inteiros são somente leitura e nunca entram na retenção automática.
