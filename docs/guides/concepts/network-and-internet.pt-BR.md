---
id: network-and-internet
label: Rede local e internet
summary: Como as máquinas se acham e quais portas abrir
title: Rede local e internet
order: 70
---

:::lead
Na mesma rede local as máquinas se acham sozinhas. Pela internet elas precisam de um ponto de partida e de um endereço que o outro lado consiga discar.
:::

:::cards
### Rede local
Descoberta por UDP com broadcast e troca periódica de anúncios. Funciona sem infraestrutura nenhuma e anuncia mesmo sem modelo carregado.
### Internet
Um endpoint iroh sobre QUIC (UDP), com relés para atravessar NAT e hole punching. Os endereços também são publicados numa DHT.
:::

:::diagram network
Na rede local a descoberta é por broadcast UDP. Pela internet, o endpoint usa relé e hole punching para alcançar uma máquina atrás de NAT.
:::

## As portas

| Uso | Padrão | Protocolo |
|---|---|---|
| Descoberta P2P (--port) | 42777 | UDP |
| Endpoint p2p | P2P + 10000 (52777) | UDP |
| Relé do cluster | P2P + 10000 (52777) | TCP |
| Interface e API (--gateway-port) | 8080 | TCP |
| Servidor de inferência | 8181 | TCP |
| QUIC da inferência | P2P + 1000 (43777) | UDP |
| QUIC do gateway | P2P + 2000 (44777) | UDP |

:::steps
### Dê um ponto de partida
Inicie o nó com --bootstrap <máquina>:42777 apontando para uma máquina que já está na rede.
### Fixe o endereço anunciado, se precisar
Em Minha máquina, Endereço anunciado. Com um endereço fixado, ele é o único anunciado e discado pelos outros nós. Limpar o campo volta ao endereço detectado.
### Confira a travessia
A tela de Rede mostra o estado do NAT, os endereços diretos, os relés e os hole punches - os três degraus onde dois nós atrás de NAT costumam falhar.
:::

:::note Rede privada ou pública
Uma rede privada exige a chave de conexão e o trust.json para entrar. Uma rede pública aparece no diretório de Redes públicas com nome, país e cidade, e exige o trust.json.
:::

:::note Criptografia entre os nós
O administrador principal liga ou desliga TLS entre os nós em Preferências administrativas, sem reiniciar. Os outros nós adotam a política em alguns segundos. O certificado de cada nó é derivado da sua identidade.
:::

:::deep docs/subsystems/p2p-network.md
Descoberta, endereços, NAT e TLS
:::

:::deep docs/subsystems/operations.md
Portas e pegadinhas de campo
:::
