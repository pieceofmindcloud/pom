---
id: screen-deployments
label: Implantações (Histórico)
summary: Cada implantação da rede, as versões e o estado dela
title: Implantações
route: /deployments
order: 110
---

:::lead
Implantações é o histórico, aberto pelo botão Histórico ao lado de Implantar na tela Implantar: cada implantação que a rede registrou, da ativação mais recente para a mais antiga, com as máquinas que ela usa, se está no ar e qual responde quando uma requisição dá só o nome do modelo.
:::

![Implantações: uma linha por implantação, com o formato, as máquinas, a última ativação, o estado e as ações.](../images/pt-BR/deployments.webp)

:::cards
### No ar e Padrão
**No ar** quer dizer que máquinas vivas estão anunciando a implantação. **Padrão** marca a que responde a uma requisição que dá só o nome do modelo, sem o sufixo `@`. O mesmo modelo pode ser implantado mais de uma vez, com outras máquinas ou outras configurações.
### Versões
Uma implantação guarda uma versão a cada mudança de configuração ou de máquinas. Expandir a linha lista as versões, da mais nova para a mais antiga, com a ativa marcada e **Reimplantar esta versão** em cada uma.
### Reimplantar com novos parâmetros
Abre o mesmo inspetor da tela Implantar, preenchido com a versão ativa; **Recarregar com estes valores** manda só o que mudou, nas mesmas máquinas e nas mesmas camadas.
### Ver logs de processo
Abre os Logs da implantação filtrados para esta implantação.
:::

:::note O painel da direita
Clicar numa versão mostra as configurações dela só para leitura. O cache KV não aparece de propósito: ele pertence à máquina e ao momento, não à versão.
:::

## Modelos de imagem

Os modelos de imagem aparecem abaixo dos modelos de linguagem com o selo **Modelo de imagem**: os estágios e as máquinas de cada um, se todos os estágios estão servidos, e Tirar do ar. Abrir uma linha mostra as versões, da mais nova para a mais antiga; adicionar ou remover uma réplica no Dashboard grava uma versão nova, e **Reimplantar esta versão** traz uma antiga de volta. As versões ficam no nó que implantou o modelo; o nó de qualquer administrador o lista e pode tirá-lo do ar.

:::deep docs/subsystems/deployment.md
Versões, a implantação padrão e reimplantar
:::
