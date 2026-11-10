---
id: screen-payments
label: Pagamentos
summary: Cobre dos visitantes que usam os modelos da sua rede
title: Pagamentos
route: /config/payments
order: 285
---

:::lead
Pagamentos é onde o principal da rede decide se os visitantes pagam pela inferência, define o preço de cada modelo, dá crédito às redes de onde vêm os visitantes e vê o que entrou ao lado do que as máquinas recebem pelo trabalho.
:::

:::steps
### Defina os preços
Cada modelo ganha um preço por 1000 tokens de entrada e 1000 tokens de saída, em créditos. Um modelo sem preço continua gratuito. Um preço definido numa implantação (Implantar, configurações do POM) substitui o daqui quando ela sobe.
### Ligue a cobrança
Escolha **Cobrar visitantes** e salve. Os visitantes passam a chegar aos modelos só através de um membro da rede, que os cobra. Se um visitante roda uma versão antiga que não faz isso, a tela o lista antes e pergunta de novo: ele perde o acesso até atualizar.
### Dê crédito
O crédito é pré-pago. Depois que a rede de um visitante pagar você fora do POM, credite a conta dela aqui, aprove o pedido de crédito dela, ou digite um valor ao aprovar a admissão.
:::

:::note Uma conta por rede visitante
Todos os visitantes que vêm da mesma rede dividem uma conta, nomeada pela chave do principal dessa rede. A coluna **Visitantes** mostra quem usa cada uma.
:::

:::note Quando uma conta acaba
Um pedido é reservado contra o saldo antes de rodar e cobrado pelo que de fato usou quando termina. Quando o saldo não cobre um pedido, o visitante recebe uma recusa dizendo quanto falta, e o Chat dele aponta para a página de Crédito.
:::

:::tip Receita e pagamentos
**Receita e pagamentos por ciclo** alinha o que os visitantes pagaram em cada ciclo de medição com o que a medição de trabalho de GPU paga às máquinas pelo mesmo ciclo, para você ver a margem da rede. Precisa do plugin de medição nesta máquina.
:::

:::deep docs/subsystems/payments.md
Como a cobrança funciona, entre quais máquinas, e seus limites
:::
