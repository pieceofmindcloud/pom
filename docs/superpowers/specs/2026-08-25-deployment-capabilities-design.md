# Capacidades reais nas configurações de implantação

## Decisões aprovadas

As opções globais da implantação são condicionadas por evidência do modelo e
pela topologia efetivamente planejada. O backend é a fonte da decisão; o
frontend apenas renderiza o estado recebido.

| Opção | Evidência de origem | Estado quando não há prova |
| --- | --- | --- |
| Desativar Thinking | `tokenizer.chat_template` e o vocabulário GGUF lidos pelo backend. O backend só marca suportado quando reconhece a forma de controle que o runtime usa: ramo `enable_thinking` com token `<|think|>` para o renderer Gemma4, ou os marcadores `<think>` e `</think>` tokenizáveis no template genérico. | Visível, desabilitada, com fonte e motivo explícitos. |
| Decodificação Especulativa | Capacidades runtime anunciadas pelos peers selecionados, especialmente `speculate/v1-lookup`, e a capacidade de lookup do plano completo. | Visível, desabilitada, com o peer ou a razão que impediu a prova. |
| MTP | Metadado GGUF `<arquitetura>.nextn_predict_layers` e tensores `nextn_*` da parte final, combinado com a capacidade runtime `speculate/v1-mtp` ou `speculate/v1-mtp-sliced` do peer final. | Visível, desabilitada, com a evidência ausente ou indeterminada. |
| Desativar Embeddings | Topologia real do plano: `start_layer`, `end_layer`, `total_layers`, `peer_index`, `peer_count` e se o peer é o último. Um peer único ou o peer que não precisa exportar residual é seguro; um intermediário que precisa exportar residual não é. | Se a topologia não puder ser lida, visível, desabilitada e com motivo. Se o usuário ligar o controle e algum peer não for seguro, a implantação é recusada antes de alterar qualquer peer, nomeando cada peer e a razão. |
| Fit memory | Estratégia de execução do plano. A opção é suportada apenas onde o contrato de execução permite ajuste automático de memória; Tensor Parallelism permanece fora desse contrato. | Oculta quando comprovadamente incompatível; visível e desabilitada se a estratégia não puder ser determinada. |

Opções numéricas como contexto, batch, temperatura, top-p, top-k, penalidade,
sessões e limite de tokens não dependem da capacidade do modelo e permanecem
visíveis. O modo do motor continua sendo condicionado pela metadata MoE real já
existente.

## Regra de estado

Cada capacidade usa `supported`, `unsupported` ou `unknown`, além de `source` e
`reason`. `unsupported` oculta o controle. `unknown` mantém o controle visível
para que o operador entenda que a informação está incompleta, mas o deixa
desabilitado para impedir que uma falha de apuração vire uma decisão positiva
silenciosa. O alerta informa a origem consultada e o motivo concreto.

`no_embeddings` é a exceção de visibilidade: o switch permanece presente para
preservar o controle operacional aprovado. Quando ligado, a validação é
atômica: todos os peers precisam ser seguros. Não existe aplicação parcial.

## Contrato

O endpoint de planejamento retorna as capacidades calculadas para os alvos
selecionados. O endpoint de lançamento valida a mesma topologia novamente
antes de parar ou alterar qualquer processo. Cada alvo recebe a decisão efetiva
de `no_embeddings`; o cálculo automático existente continua desligando
embeddings somente no peer final quando o operador não pediu o override.

## Testes obrigatórios

- template e vocabulário que suportam thinking, um caso sem suporte e um caso
  indeterminado;
- lookup e MTP suportados, ausentes e indeterminados a partir das capacidades
  reais dos peers e dos metadados GGUF;
- `no_embeddings` em peer único, cadeia com intermediário que exporta residual,
  e topologia indeterminada;
- frontend para suportado, não suportado e indeterminado, incluindo o alerta;
- lançamento rejeitado atomicamente quando um peer não pode desligar
  embeddings com segurança.
