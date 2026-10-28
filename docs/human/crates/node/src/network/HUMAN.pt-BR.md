---
id: human-network
label: Rede
summary: Como as máquinas se acham, provam quem são e atravessam um NAT
title: Por dentro da rede
reference: crates/node/src/network/AGENTS.md
order: 20
---

:::lead
Esta pasta é como um nó acha os outros e é achado: um anúncio UDP assinado na rede local, e um endpoint iroh com relés e hole punching pela internet. A regra dela: uma máquina é a chave dela, nunca o IP.
:::

:::diagram network
Na rede local a descoberta é por broadcast UDP. Pela internet, o endpoint usa relé e hole punching para alcançar uma máquina atrás de NAT.
:::

:::cards
### Identidade é uma chave
`.pom-node.key` é a única chave de um nó. O id, o certificado TLS, o endpoint iroh e o registro pkarr derivam dela. Apagar o arquivo cria uma máquina nova.
### Anúncio a cada cinco segundos
Cada nó anuncia, assinado, quem é, quanta memória tem e quais camadas serve. Peers que param de responder saem da lista.
### Endereços são dicas
Um endereço anunciado só vai na frente dos candidatos. Quem disca decide qual endereço usar, e lembra quais falharam.
### A escada
Direto primeiro, depois uma conexão com hole punching, depois um relé: os públicos da n0, um dedicado, ou um membro da sua própria rede. Um arquivo nunca atravessa um relé público.
:::

:::note TLS mútuo, sempre
Entre máquinas, toda chamada é TLS mútuo com certificados derivados das chaves dos nós. A chave de conexão sozinha não abre nada: quem chama também precisa ser admin da cadeia de confiança ou membro com token válido.
:::

:::deep crates/node/src/network/AGENTS.md
Cada arquivo da camada de rede
:::

:::deep docs/subsystems/p2p-network.md
Descoberta, identidade, relés e travessia de NAT completos
:::
