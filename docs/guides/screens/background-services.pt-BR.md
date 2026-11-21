---
id: screen-background-services
label: Serviços em segundo plano
summary: Os plugins que rodam só enquanto abertos, a memória de cada um e como fechá-los
title: Serviços em segundo plano
route: /config/background-services
order: 315
---

:::lead
Alguns plugins rodam só enquanto você os usa. Serviços em segundo plano lista esses plugins, mostra quanta memória cada um em execução ocupa e fecha os que você não precisa mais. É o único lugar para fechar um plugin que não tem entrada própria no menu, como o agente Goose com que o Chat conversa.
:::

:::cards
### Em execução
Cada plugin sob demanda com o processo ativo, com a versão e, onde esta máquina consegue medir (Linux), a memória residente do plugin e de tudo o que ele iniciou.
### Fechar
Para o plugin na hora, depois de uma confirmação. Ele mantém a licença, a instalação, as configurações e os dados, e não usa memória, processador nem GPU até ser aberto de novo.
### Parados
Os plugins sob demanda instalados mas fechados. Abrir inicia um de novo; o Chat também abre sozinho um plugin de agente quando você escolhe o agente dele.
:::

:::deep docs/subsystems/plugins-ui.md
Plugins sob demanda e os agentes ACP que eles servem
:::
