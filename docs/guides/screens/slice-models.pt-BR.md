---
id: screen-slice-models
label: Partes tensor-parallel
summary: Corte uma parte tensorial por rank de um modelo inteiro (modo de desenvolvimento)
title: Partes tensor-parallel
route: /models/slice
order: 70
---

:::lead
Uma implantação em pipeline não precisa mais de fatias: cada máquina serve a sua faixa direto da sua própria cópia do modelo inteiro. Esta tela ficou para uma tarefa só, cortar as partes de uma implantação em tensor parallel, e funciona apenas num nó iniciado em modo de desenvolvimento (`POM_DEV_MODE=1`). Ela é aberta pelo botão **Fatiar** de um modelo na tela Modelos, que só aparece nesse modo.
:::

:::steps
### Escolha o modelo de origem
Só modelos inteiros podem ser cortados. Um modelo dividido em várias partes físicas precisa de todas as partes presentes; uma parte ausente é nomeada e o início fica desabilitado.
### Escolha a quantidade de ranks
Uma parte por rank, de 2 a 64. Todo rank cobre todas as camadas e recebe uma parte diferente de cada tensor.
### Leia a prévia
Uma linha por rank, rotulada `1of2`, `2of2` e assim por diante, com qualquer aviso que o nó levantar.
### Fatiar agora
O trabalho mostra a fase e os bytes lidos enquanto roda, e continua na lista quando termina ou falha.
:::

:::note Partes feitas antes
Fatias de pipeline que já estão numa máquina continuam servindo até serem desimplantadas, e a tela de implantação as mostra como implantação legada. Não há mais uma tela que as liste.
:::

:::deep docs/subsystems/slicing.md
Tensores obrigatórios e partes tensor-parallel
:::
