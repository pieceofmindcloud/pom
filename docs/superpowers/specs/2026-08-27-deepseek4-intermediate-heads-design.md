# Cabeças obrigatórias nas fatias intermediárias do DeepSeek-V4-Flash

## Contexto e evidência

O carregador do fork `llama.cpp` cria, sem a flag `TENSOR_NOT_REQUIRED`, os
tensores `token_embd.weight`, `output_norm.weight`, `output.weight`,
`output_hc_fn.weight`, `output_hc_base.weight` e `output_hc_scale.weight` para
`deepseek4`. A recusa atual impede qualquer faixa não final porque o GGUF não
contém a cabeça completa.

O POM captura o estado oculto no resultado da última camada da faixa. No
wrapper Rust, o callback copia o tensor-alvo antes da normalização e da cabeça.
No grafo DeepSeek4 do fork, a ordem observada é `l_last-N`, depois
`build_hc_head`, `output_norm` e `output`; a avaliação da cabeça ocorre depois
da captura. O callback não interrompe a avaliação ao encontrar seu alvo, então
a cabeça será executada nas fatias intermediárias e pagará seu custo de
computação, mas não altera o residual já copiado.

Há uma divergência de nome entre o alvo atual `l_out-N` do wrapper e
`l_last-N` do grafo DeepSeek4. O callback deve aceitar os dois nomes, mantendo
`l_out-N` como o caminho existente para as demais arquiteturas.

## Decisão

O contrato de saída passa a ser tratado como uma lista de tensores obrigatórios
por arquitetura. Ao gerar uma faixa não final, o fatiador copia da fonte todos
os tensores obrigatórios que ainda não são cobertos pela política normal de
embeddings e norma sintética. Nenhum dado é inventado. Se a fonte não contiver
um tensor obrigatório, se o tamanho for desconhecido ou se a leitura da fonte
falhar, a operação recusa antes de criar a saída e inclui a razão concreta.

`token_embd.weight` continua sendo copiado para toda fatia e
`output_norm.weight` continua sendo uma identidade F32 sintética nas fatias
intermediárias, pois essa transformação já é parte da fronteira de execução do
POM e não é usada para capturar o residual. Os tensores de cabeça restantes,
como `output.weight` e os três `output_hc_*` do DeepSeek4, são sempre copiados
dos bytes reais da fonte.

O cálculo de tamanho serializado usa exatamente a mesma seleção do fatiador.
Assim, a prévia e o job exibem o custo real, incluindo aproximadamente 0,44 GB
por fatia intermediária no arquivo medido do DeepSeek-V4-Flash.

Metadata que o llama.cpp compara com o índice local do bloco também precisa
acompanhar a renumeração `blk.(start+i)` para `blk.i`. O fatiador classifica
essas chaves por semântica: `leading_dense_block_count`, `hash_layer_count` e
`n_deepstack_layers` contam desde o início e são calculadas como
`min(max(original - start_layer, 0), blocos_da_fatia)`; `nextn_predict_layers`
conta desde o fim e continua zero nas faixas não finais. Um valor relativo ao
início que não seja inteiro escalar não negativo representável em `u32` causa
recusa nominal, sem copiar metadata ambígua.

## Invariantes

- A tabela e os payloads dos tensores de cabeça vêm da fonte, nunca de zeros ou
  identidades sintéticas.
- A reindexação de `blk.N.*` e o recorte de arrays por camada permanecem
  inalterados.
- A cabeça pode ser calculada pelo llama.cpp depois do ponto de captura, mas
  nenhum valor posterior à captura entra no estado oculto enviado ao próximo
  peer.
- A validação de faixas e a estimativa de tamanho compartilham a mesma lista
  de tensores obrigatórios.
- A UI mostra o tamanho total calculado pelo backend e o detalhe do custo de
  cabeça quando o plano o fornecer, em inglês e pt-BR.

## Fora de escopo

- Alterar kernels CUDA ou o grafo do llama.cpp.
- Fabricar uma cabeça para reparar uma fatia antiga.
- Remover a validação de contrato para arquiteturas que não podem ser copiadas.
