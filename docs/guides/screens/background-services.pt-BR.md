---
id: screen-background-services
label: Pieces
summary: Todos os aplicativos que esta máquina roda, abertos e parados, como um dock e uma pasta
title: Pieces
route: /pieces
order: 315
---

:::lead
Pieces mostra cada plugin que esta máquina roda como um aplicativo: o ícone com o nome embaixo. Ela parece uma área de trabalho: as peças do POM se encaixam num céu noturno ao fundo, os aplicativos que podem ser abertos ficam nela como ícones que você arrasta para onde quiser (a arrumação fica neste navegador), e os abertos ficam num dock embaixo, como o Dock do macOS: primeiro os que ficam sempre ligados e, depois de uma divisória, os que podem ser fechados. Clique num aplicativo fechado para abri-lo: ele entra no dock, quica enquanto sobe e depois você vai para a tela dele. Um aplicativo sem tela própria, como um motor de inferência, não fica no dock: enquanto roda, aparece no canto superior direito como uma notificação do sistema, com botões para configurar e fechar. Clique num aplicativo do dock para ir à tela dele. A engrenagem que aparece ao passar o mouse sobre um ícone abre os detalhes, como a memória que ele usa, e o botão direito sobre qualquer ícone abre um menu com o que dá para fazer: abrir, ir para a tela, configurar, manter sempre ligado ou rodar sob demanda, fechar. É onde você inicia e para os aplicativos que não têm entrada no menu, como os motores de inferência (vLLM, Avalon) e o agente Goose com que o Chat conversa.
:::

Pieces fica no menu principal, logo abaixo de Plugins. O dock dos aplicativos abertos também flutua sobre as outras telas do POM: lá ele se esconde e aparece quando você leva o mouse até a borda de baixo. Clique no ícone de um aplicativo para ver as configurações dele: abrir ou fechar, a memória que usa e como ele carrega nesta máquina. **Sob demanda** sobe quando você o abre ou quando algo precisa dele, e você o fecha para liberar a memória; **Sempre ligado** sobe com o POM e fica rodando. A escolha do próprio plugin aparece como padrão, e você pode mudá-la em qualquer plugin, para que um plugin pouco usado não fique ocupando memória.

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
