# Plano: contabilidade de posições M-RoPE no prefill multimodal

> Objetivo: corrigir no POM a mistura entre células físicas de KV e posições lógicas M-RoPE, sem alterar o `llama.cpp` vendorizado.

## Diagnóstico e regressão

- Modelar um prompt com texto, imagem e texto em que a imagem tenha `n_tokens` maior que `n_pos`.
- Verificar que o estado de sessão conserva os dois totais, que a posição seguinte usa `n_pos` e que o avanço físico usa `n_tokens`.
- Verificar que um lote multimodal já totalmente avaliado não é reprocessado como pedaço adicional por comparar `n_past` lógico com comprimento físico.
- Manter um caso de texto puro para provar que o caminho existente continua usando a mesma contabilidade.

## Implementação

- Ajustar os metadados de prefill e o estado `Session` para nomear e transportar separadamente o fim lógico da sequência e a quantidade física de células.
- Fazer a reserva/continuação de lote no POM usar a grandeza correspondente: posição lógica para `n_past` e posição inicial, células físicas para métricas e quantidade de embeddings.
- Remover pressupostos de que `prompt_tokens.len()` é sempre igual a `Session::n_past`; preservar o mapeamento físico necessário para retenção sem habilitar reuso de mídia.
- Não modificar ciclo, offload, quantização, `FIT_MAX_UBATCH` ou qualquer arquivo sob `vendor/llama-cpp-sys-2/llama.cpp`.

## Validação e entrega

- Rodar primeiro o teste novo em vermelho antes da implementação e depois em verde.
- Reproduzir uma imagem pelo chat E2E e conferir descrição real, contagens físicas/lógicas e ausência de posições não consecutivas no caminho corrigido.
- Exercitar duas imagens na mesma conversa.
- Rodar a bateria completa uma vez no final, comparar com o baseline de 1150 passados e 8 falhas preexistentes, atualizar documentação afetada e publicar somente depois da resposta ao captain.
