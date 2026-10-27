---
id: human-routing
label: Roteamento
summary: Como o gateway escolhe a cadeia de máquinas de cada requisição
title: Por dentro do roteamento
reference: crates/node/src/routing/AGENTS.md
order: 30
---

:::lead
Para cada requisição, o gateway precisa escolher uma cadeia ordenada de máquinas que juntas cubram todas as camadas do modelo, da 0 à última. Esta pasta é o planejador que faz essa escolha - e ele escolhe a cadeia que entrega a resposta mais cedo, não só a mais próxima.
:::

:::diagram topology
Três máquinas cobrindo um modelo de 32 camadas. Quem recebe a pergunta não precisa ser quem tem a camada 0.
:::

:::steps
### Juntar os candidatos
Cada máquina viva anuncia as faixas de camadas que serve. Uma máquina pode servir várias faixas, e várias máquinas podem servir a mesma faixa (réplicas).
### Estimar cada caminho
Para cada cadeia que cobre `0..total_layers`, o planejador soma o custo de prefill de cada etapa, o custo de decode dos tokens esperados, a fila que já espera em cada máquina e o tempo de rede de cada ligação.
### Escolher junto com o modo de volta
A mesma estimativa roda para a resposta direta e para o anel, e o par mais barato ganha.
### Ou recusar
Sem cadeia que cubra todas as camadas, a requisição falha com o motivo. Não há alternativa para um modelo menor nem para um motor local.
:::

:::note A ordem é a das camadas
Um pipeline sempre roda na ordem das camadas. Uma sobreposição ou um buraco global entre as faixas é recusa, nunca um palpite.
:::

:::deep crates/node/src/routing/AGENTS.md
O planejador, arquivo por arquivo
:::

:::deep docs/subsystems/parallelism.md
Escolhendo o pipeline pelo tempo de entrega
:::
