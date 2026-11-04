---
id: screen-deploy
label: Implantar na rede
summary: Coloque um modelo em serviço nas máquinas
title: Implantar um modelo
route: /deploy
order: 100
---

:::lead
Implantar na rede é o estúdio onde um modelo entra em serviço: você escolhe o que implantar, põe as máquinas no mapa, lê o veredito do planejador e ajusta as configurações antes de apertar Implantar.
:::

![O estúdio de implantação: a barra de modo no topo, o mapa no meio, o inspetor à direita e as máquinas da rede embaixo.](../images/pt-BR/deploy.webp)

## As três partes da tela

:::cards
### A barra do topo
O que implantar: **Fatias** (um conjunto de fatias, uma faixa por máquina) ou **Modelo inteiro** (o arquivo inteiro numa máquina). Ao lado, o motor (llama.cpp, ou um serviço de inferência que um plugin publica), o modelo e o nome da implantação.
### O mapa
As máquinas da implantação, cada uma com a faixa de camadas que vai servir, a memória que tem e a projeção depois de carregar. O **Gateway** fecha o ciclo com o custo de um token assim que der para medir.
### O inspetor
**Globais** guarda as configurações da implantação inteira (contexto, sessões, amostragem, as configurações do POM); a aba da máquina guarda o que é por máquina: camadas na GPU e se o cache KV vai para a GPU.
:::

## Da escolha ao serviço

:::steps
### Escolher o modelo
Em Fatias, a lista mostra os conjuntos de fatias que alguma máquina da rede já tem. Em Modelo inteiro, os arquivos GGUF inteiros.
### Pôr as máquinas no mapa
Arraste máquinas da faixa de baixo para o mapa. O planejador distribui as faixas, e **Cobertura do modelo**, no canto de baixo, conta as camadas cobertas.
### Ler o veredito
Quando a escolha não pode ser implantada, uma linha vermelha no topo diz o porquê - por exemplo, quais camadas não estão em nenhuma fatia escolhida. O botão Implantar fica desligado até o plano fechar.
### Implantar
Cada máquina extrai ou carrega a própria faixa. Uma janela acompanha a implantação passo a passo até ela ficar pronta ou falhar, com o motivo; **Ir para o painel** leva ao Painel, e **Fechar** deixa a implantação seguindo, com o resultado no sino. A implantação aparece no Painel assim que as máquinas confirmam que estão servindo. Uma implantação é feita uma vez por vez: apertar Implantar de novo enquanto ela sobe só reabre essa janela.
:::

:::diagram lifecycle
O ciclo de vida de uma fatia: só o estado que o próprio nó confirmou autoriza retomá-la depois de um reinício.
:::

:::warn Cópias precisam da sua permissão
Se uma máquina não tem o arquivo de que precisa, a implantação é recusada a menos que você permita transferências - e a tela lista as cópias antes de pedir. Pesos nunca se movem só por causa do fatiamento.
:::

:::note Ajuda das configurações
Cada categoria do inspetor tem um botão de ajuda que abre o guia dela: Configurações padrões e Configurações do POM.
:::

:::deep docs/subsystems/deployment.md
O plano, a fusão, a ordem e o ciclo de vida da fatia
:::

:::deep docs/subsystems/parallelism.md
Pipeline ou paralelismo de tensor
:::
