---
id: screen-dashboard
label: Painel
summary: O mapa ao vivo da rede e do modelo em serviço
title: O Painel
route: /
order: 10
---

:::lead
O Painel é a primeira tela do administrador: mostra se o modelo em serviço está completo, quais máquinas o carregam, quão rápido cada uma responde e como uma pergunta vai do gateway pelas máquinas e volta.
:::

![O Painel de uma rede com três máquinas servindo um modelo, na visão Fluxo, com as propriedades da implantação à direita.](../images/pt-BR/dashboard.webp)

## A barra do topo

:::cards
### Cobertura
Quantas camadas do modelo estão servidas agora. Abaixo de 100% não há resposta: a requisição falha com o motivo em vez de responder pior.
### Máquinas
Quantas máquinas a rede conhece e quantas delas estão servindo.
### Modelo
O modelo em serviço e se o pipeline dele está completo.
### Em voo
Requisições sendo geradas agora, na rede toda e neste nó.
:::

## Fluxo e Detalhes

A visão **Fluxo** desenha o caminho de uma pergunta: cada máquina é um cartão com VRAM, memória, carga, velocidades de decode e prefill e a temperatura da GPU, e as linhas entre elas trazem o tempo de ida e volta medido (RTT). O cartão **Gateway** fecha o ciclo com o custo de um token pela cadeia inteira. A legenda embaixo separa a rota em uso de uma rota paralela e do tráfego possível.

A visão **Detalhes** lista a rede inteira, não só o pipeline: primeiro as máquinas da implantação, na ordem das camadas, e depois as máquinas conectadas sem fatia, que são justamente as que somem quando se olha só o pipeline. Sem modelo em serviço, a tela abre em Detalhes.

:::note O painel da direita
É a leitura da versão ativa da implantação: o mesmo inspetor que a tela Implantar edita, com o contexto, as sessões, a amostragem e as configurações do POM com que ela subiu.
:::

## O que o administrador faz aqui

:::steps
### Escolher o modelo na tela
O chip do modelo no topo do mapa escolhe qual implantação é desenhada. Adicionar, Desimplantar e o painel de propriedades seguem esse modelo.
### Adicionar uma máquina
**Adicionar** coloca mais uma máquina na implantação; o diálogo lista o que a máquina escolhida já tem no disco.
### Desimplantar
**Desimplantar** tira o modelo de serviço em todas as máquinas da lista que ele mostra antes.
### Acompanhar o console
O **Console do cluster**, embaixo, abre para cima com os logs de processo das máquinas, ao vivo.
:::

:::warn Os outros perfis veem menos
Um worker ou um visitante vê só o Fluxo, com o modelo, a fatia e as requisições desta máquina. A visão Detalhes, Adicionar e Desimplantar são do administrador, e o nó recusa essas ações a qualquer outro, mostre a tela o que mostrar.
:::

:::deep docs/subsystems/web-ui.md
Cada painel da interface
:::

:::deep apps/frontend/src/routes/AGENTS.md
Como o Painel lê o cluster
:::
