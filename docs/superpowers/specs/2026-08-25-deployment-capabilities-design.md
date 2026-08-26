# Capacidades reais nas configurações de implantação

## Decisões aprovadas

As opções globais da implantação são condicionadas por evidência do modelo e
pela topologia efetivamente planejada. O backend é a fonte da decisão; o
frontend apenas renderiza o estado recebido.

| Opção | Evidência do modelo selecionado | Evidência de runtime/topologia | Estado quando não há prova |
| --- | --- | --- | --- |
| Desativar Thinking | `tokenizer.chat_template` e o vocabulário GGUF lidos pelo backend. O backend só marca suportado quando reconhece a forma de controle que o runtime usa: ramo `enable_thinking` com token `<|think|>` para o renderer Gemma4, ou os marcadores `<think>` e `</think>` tokenizáveis no template genérico. | Não há gate de runtime separado: a implantação carrega esse arquivo no peer. | Ausência comprovada no GGUF oculta. Leitura incompleta mantém visível e acionável, com risco explícito. |
| Decodificação Especulativa | Não depende de uma capacidade específica do modelo selecionado. | Capacidades anunciadas pelos peers selecionados, especialmente `speculate/v1-lookup`, e a capacidade de lookup do plano completo. | Limitação comprovada do runtime não oculta: mantém visível e acionável com aviso de que pode não executar. Runtime não consultável fica `unknown`, visível e acionável com risco explícito. |
| MTP | O arquivo GGUF selecionado: `<arquitetura>.nextn_predict_layers` e/ou tensores `nextn` reais da parte final. A tabela de tensores também é prova quando o produtor omite a chave de metadata. | O protocolo `speculate/v1-lookup`, `speculate/v1-mtp-sliced` e `speculate/v1-mtp` do plano/peer final só pode limitar a execução; nunca transforma um modelo comprovadamente MTP em “não suportado”. | Ausência comprovada no GGUF oculta. GGUF indeterminado fica visível e acionável. Limitação ou ausência do runtime fica como aviso acionável. |
| Desativar Embeddings | Não depende do modelo. | Topologia real do plano: `start_layer`, `end_layer`, `total_layers`, `peer_index`, `peer_count` e se o peer exporta residual. Um peer único ou o peer que não precisa exportar residual é seguro; um intermediário que precisa exportar residual não é. | Topologia indeterminada mantém o controle visível e acionável, com peers e motivo. Peer comprovadamente inseguro causa recusa nominal e atômica antes de alterar qualquer peer. |
| Fit memory | Não depende do conteúdo do modelo. | Estratégia de execução e orçamento/memória observados pelo backend. Tensor Parallelism permanece fora do contrato de ajuste automático. | Incompatibilidade comprovada pela estratégia oculta; ausência de decisão mantém visível com aviso. |

Opções numéricas como contexto, batch, temperatura, top-p, top-k, penalidade,
sessões e limite de tokens não dependem da capacidade do modelo e permanecem
visíveis. O modo do motor continua sendo condicionado pela metadata MoE real já
existente.

## Regra de estado

Cada capacidade usa `supported`, `unsupported` ou `unknown`, além de `source`,
`reason` e, quando aplicável, `warning`. `unsupported` só oculta quando a
ausência foi provada na fonte correta do modelo ou da estratégia. `unknown`
mantém o controle visível e acionável: o alerta informa a origem, o motivo e
que ligar é por conta e risco do usuário. Uma limitação do runtime também
mantém o controle acionável e aparece em `warning`; runtime não substitui a
prova do modelo selecionado.

`no_embeddings` permanece presente para preservar o controle operacional
aprovado. Quando ligado, peers comprovadamente inseguros continuam recusando a
implantação de forma atômica. Se a topologia for indeterminada, o pedido pode
seguir somente porque o operador marcou explicitamente o controle avisado; não
há aplicação parcial silenciosa.

## Contrato

O endpoint de planejamento retorna as capacidades calculadas para os alvos
selecionados. O endpoint de lançamento valida a mesma topologia novamente
antes de parar ou alterar qualquer processo. Cada alvo recebe a decisão efetiva
de `no_embeddings`; o cálculo automático existente continua desligando
embeddings somente no peer final quando o operador não pediu o override.

## Testes obrigatórios

- template e vocabulário que suportam thinking, um caso sem suporte e um caso
  indeterminado;
- lookup com runtime suportado, limitado e indeterminado sem ocultar o campo;
- MTP com modelo final suportado, ausente e indeterminado, separando o aviso do
  runtime da prova do GGUF;
- `no_embeddings` em peer único, cadeia com intermediário que exporta residual,
  e topologia indeterminada;
- frontend para suportado, não suportado e indeterminado, incluindo o alerta;
- lançamento rejeitado atomicamente quando um peer comprovadamente não pode
  desligar embeddings com segurança, e aceito com aviso quando a topologia é
  indeterminada e o operador assume o risco.
