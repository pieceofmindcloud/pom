---
id: profiles
label: Os perfis
summary: Admin, admin secundário, worker e visitante
title: Os perfis
order: 30
---

:::lead
Toda rede tem um admin principal, quantos admins secundários ele promover e membros, que são workers ou visitantes. O papel de um membro é decidido na admissão; o de um admin vem da cadeia de confiança.
:::

:::diagram profiles
O admin principal governa a rede; o admin secundário implanta e admite; o worker serve camadas; o visitante conversa com os modelos.
:::

:::cards
### Admin
O principal da rede, provado pela cadeia de confiança. Faz tudo: configurações, chaves, promoções e rebaixamentos de admins, admissões, remoções e implantações.
### Admin secundário (promovido)
Um admin que o principal promoveu, com a concessão assinada na cadeia de confiança. Implanta e desimplanta modelos, vê logs e telemetria, aprova ou recusa admissões e remove workers e visitantes. Configurações, chaves e mudanças de admins continuam com o principal.
### Worker
Uma máquina admitida para processar: recebe fatias e serve a faixa de camadas que lhe cabe, mostra o modelo e a carga locais e pode sair da rede quando quiser.
### Visitante
Uma vista reduzida para conversar com os modelos pelo próprio nó: Chat, documentação, a própria chave de API e o Chat da rede quando a rede o abre. Nunca guarda nem serve um modelo.
:::

| Ação | Admin | Admin secundário | Worker | Visitante |
|---|---|---|---|---|
| Conversar no Chat | sim | sim | sim | sim |
| Implantar e desimplantar modelos | sim | sim | não | não |
| Aprovar ou recusar admissões | sim | sim | não | não |
| Remover workers e visitantes | sim | sim | não | não |
| Mudar configurações e chaves da rede | sim | não | não | não |
| Promover e rebaixar admins | sim | não | não | não |
| Servir fatias de um modelo | sim | sim | sim | não |
| Ativar e instalar plugins na própria máquina | sim | sim | sim | sim |
| Criar a própria chave de API | sim | sim | sim | sim |
| Sair da rede | sim | sim | sim | sim |

:::note A tela esconde, o servidor decide
Esconder um botão é só apresentação. A autorização de verdade é feita pelos nós, que recusam uma rota fora do papel mesmo que alguém a chame à mão.
:::

O papel é escolhido no pedido de admissão e o admin decide ao aprovar. Uma máquina admitida como visitante continua visitante, e o principal se recusa a promovê-la; só um worker pode ser promovido a admin secundário, em Minha Rede > Administradores.

:::deep docs/subsystems/security-model.md
Papéis, cadeia de confiança e tokens de membro
:::

:::deep docs/subsystems/web-ui.md
O que cada perfil vê na interface
:::
