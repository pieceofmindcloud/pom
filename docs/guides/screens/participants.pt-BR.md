---
id: screen-participants
label: Participantes da rede
summary: As máquinas da rede e como cada uma é alcançada
title: Participantes da rede
route: /config/network/participants
order: 230
---

:::lead
Participantes da rede lista as máquinas que esta conhece na rede: o nome, se a identidade foi verificada, a versão do PoM que rodam, a rota em uso para alcançá-las, as camadas que servem e se estão conectadas.
:::

![Participantes da rede: uma linha por máquina com os selos, as camadas que serve, o estado e o botão Bloquear.](../images/pt-BR/participants.webp)

:::cards
### verificado
A máquina provou a identidade: o certificado dela deriva da chave, e o roster atesta o que ela é.
### Versão
A versão do PoM que ela anuncia. Âmbar quer dizer que não é a mesma desta máquina.
### Rota
Como esta máquina a alcança agora: **Direta**, **Direta (P2P)**, **Direta (hole punch)** ou por um relé, e qual tipo de relé. O endereço anunciado só aparece na dica; a rota mostrada é a que está em uso.
### Bloquear
Recusa aquela máquina no handshake daqui em diante. É uma ação de administrador; o botão só aparece para um.
:::

:::note Identidade, não endereço
Uma máquina é a chave dela. A mesma máquina pode mudar de IP e continua a mesma linha; apagar a chave cria uma nova.
:::

:::deep docs/subsystems/security-model.md
Associação, revogação e bloqueio
:::
