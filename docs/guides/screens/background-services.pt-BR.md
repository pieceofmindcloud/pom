---
id: screen-background-services
label: Serviços
summary: Todos os aplicativos que esta máquina roda, abertos e parados, como um dock e uma pasta
title: Serviços
route: /plugins
order: 315
---

:::lead
Serviços mostra cada plugin que esta máquina roda como um aplicativo: o ícone com o nome embaixo. Os que podem ser abertos ficam em cima, como uma pasta de aplicativos; os abertos ficam num dock na parte de baixo da tela, como o Dock do macOS: primeiro os que ficam sempre ligados e, depois de uma divisória, os que podem ser fechados. Cada aplicativo aberto mostra quanta memória (RAM) usa. É onde você inicia e para os aplicativos que não têm entrada no menu, como os motores de inferência (vLLM, Avalon) e o agente Goose com que o Chat conversa.
:::

Serviços é a primeira aba de Plugins quando há algum plugin instalado. Clique no ícone de um aplicativo para ver as configurações dele: abrir ou fechar, a memória que usa e como ele carrega nesta máquina. **Sob demanda** sobe quando você o abre ou quando algo precisa dele, e você o fecha para liberar a memória; **Sempre ligado** sobe com o POM e fica rodando. A escolha do próprio plugin aparece como padrão, e você pode mudá-la em qualquer plugin, para que um plugin pouco usado não fique ocupando memória.

:::cards
### Abertos
A prateleira de cima: cada aplicativo com o processo no ar, com uma luzinha embaixo. Abaixo do nome: o que ele é (um motor de inferência e o modelo que serve), ou a memória que usa, onde esta máquina consegue medir (Linux). Um plugin que roda sempre diz "sempre ligado".
### Fechar
O X que aparece num aplicativo aberto ao passar o mouse o para, depois de uma confirmação. Ele mantém licença, instalação, configurações e dados, e não usa memória, processador nem GPU até ser aberto de novo. Fechar o motor que serve o modelo desta máquina libera esse modelo antes.
### Parados
A pasta de baixo: os aplicativos instalados mas fechados, esmaecidos. Clique num ícone para abri-lo. Um deploy sobe um motor de inferência sozinho, e o Chat abre um plugin de agente quando você escolhe o agente dele.
:::

:::deep docs/subsystems/plugins-ui.md
Plugins sob demanda, motores de inferência e os agentes ACP que eles servem
:::
