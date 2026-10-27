---
id: screen-models
label: Modelos inteiros
summary: Os arquivos GGUF inteiros encontrados nesta máquina
title: Modelos inteiros
route: /models
order: 60
---

:::lead
Modelos inteiros lista cada arquivo GGUF inteiro que esta máquina achou nas pastas de modelos: um repositório do Ollama, uma biblioteca do LM Studio ou uma pasta comum de GGUF. Daqui um modelo é fatiado, enviado a outra máquina ou apagado.
:::

![Modelos inteiros: uma linha por modelo, com origem, camadas, arquitetura, projetor de visão, tamanho e ações.](../images/pt-BR/models.webp)

:::video https://www.youtube.com/watch?v=lCYW3TJ70M8
Vídeo: as telas de Modelos - modelos inteiros, fatias e transferências - explicadas.
:::

:::cards
### Camadas e arquitetura
Lidas do cabeçalho do GGUF, sem carregar o modelo. O número de camadas é o que o fatiamento e a implantação dividem.
### Visão (mmproj)
O projetor de visão que pertence ao modelo, quando existe. **Visão e MTP** liga a este modelo um projetor ou um rascunho MTP encontrado em qualquer máquina da rede.
### Fatiar
Abre Fatiar modelos com este arquivo escolhido.
### Enviar
Oferece o arquivo a outra máquina, em Transferências. O outro lado precisa aceitar.
:::

:::note De onde vêm estes arquivos
As pastas lidas são definidas em Configurações > Armazenamento. **Varrer de novo** lê de novo depois que você copia um arquivo à mão.
:::

:::note O fatiamento é local
Cada máquina extrai a própria faixa da própria cópia do GGUF. Nenhum peso viaja pela rede por causa do fatiamento.
:::

:::deep docs/subsystems/slicing.md
Como um GGUF é lido e cortado
:::
