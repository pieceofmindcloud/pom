---
id: slice-and-deploy
label: Do modelo ao cluster servindo
summary: Fatiar por tamanho, distribuir, pôr em serviço
title: Do modelo ao cluster servindo
order: 50
---

:::lead
São três atos distintos, e vale separá-los na cabeça: cortar o arquivo, levar os pedaços às máquinas certas, e mandar as máquinas carregarem o que receberam.
:::

:::note Antes de tudo, um modelo
Os modelos inteiros vêm das pastas de modelos da máquina. Para baixar um, use Encontrar Modelos HF: a tela mostra a licença antes do download e o progresso fica em Transferências. Um modelo que cabe inteiro numa máquina pode ser implantado sem fatiar.
:::

:::diagram slicing
Um GGUF de 32 camadas cortado por alvo de tamanho, não em partes iguais.
:::

## 1. Fatiar (Fatiar modelos)

Você monta uma lista de fatias e dá a cada uma um alvo em GB; a última pode simplesmente levar o resto. A tela converte GB em faixa de camadas somando o tamanho real de cada bloco do arquivo - não uma divisão do total pelo número de camadas, porque os blocos não são iguais entre si e algumas partes do modelo se repetem em toda fatia.

:::note O arredondamento fica à vista
Camada é unidade indivisível, então o que você pediu e o que saiu quase nunca batem exatamente. A tela mostra os números lado a lado: o alvo pedido, a faixa que saiu e o tamanho real do arquivo.
:::

## 2. Distribuir (Transferências)

Uma fatia chega à outra máquina de dois jeitos: ela puxa, ou o dono do arquivo oferece. Uma oferta aparece como notificação do outro lado e só começa depois de aceita - nunca se escreve no disco alheio sem convite.

:::cards
### Retomada
O arquivo baixa como .part e continua de onde parou. Interromper não significa recomeçar.
### Conferência
O arquivo é conferido antes de tomar o nome final. Não bateu, o parcial é apagado.
### Espaço em disco
Verificado antes de começar, e a implantação conta a união das fatias mais o que ainda falta.
:::

## 3. Implantar (Implantar na rede)

Aqui você escolhe qual máquina serve qual faixa e manda o cluster carregar. O nó sugere uma distribuição; a sua escolha viaja na requisição e o cluster é revarrido na hora de aplicar, em vez de confiar na prévia. Os cartões do mapa mostram quanto de memória cada máquina vai usar depois do deploy.

| Regra | Por quê |
|---|---|
| A ordem do pipeline é a ordem das camadas | quem fica com a camada 0 recebe o prompt inteiro de cada requisição; escolher a ordem e escolher as faixas são o mesmo ato |
| Várias faixas na mesma máquina são válidas | desde que não se sobreponham e a cobertura global seja completa; a fusão só junta faixas vizinhas |
| Sobreposição é recusa | a mesma camada rodando duas vezes corrompe a resposta |
| Buraco é recusa | todas as camadas exigidas precisam estar cobertas antes de formar o pipeline |

:::note Contexto e batch valem para o cluster; GPU vale por máquina
A janela de contexto e o tamanho do lote governam a cadeia inteira. Já quantas camadas vão para a GPU e onde fica o cache de atenção são propriedade do hardware - uma máquina com 16 GB de VRAM e outra sem GPU não podem estar as duas certas sobre isso, então cada uma tem o seu campo.
:::

:::checklist Antes de clicar em implantar
- Todas as camadas estão cobertas, sem buraco.
- Nenhuma faixa se sobrepõe a outra.
- A máquina com a camada 0 tem memória de sobra para o prompt e o cache.
- Os cartões do mapa não mostram aviso de memória.
:::

Para conferir, abra Implantações: a versão ativa mostra os parâmetros usados, e Reimplantar esta versão volta a qualquer versão anterior.

:::faq
### Preciso fatiar sempre?
Não. Um modelo que cabe inteiro numa máquina é implantado como modelo inteiro, numa máquina só. Mais máquinas podem entrar no mapa, desde que as faixas delas cubram todas as camadas.
### Pipeline ou tensor parallelism?
Pipeline é a opção recomendada entre máquinas pela internet: uma travessia por fronteira de camadas por token. No tensor parallelism cada camada é repartida e as máquinas somam resultados várias vezes por token, então ele pede uma rede rápida.
:::

:::deep docs/subsystems/slicing.md
Fatiamento, tabela de tipos e planejamento por tamanho
:::

:::deep docs/subsystems/deployment.md
Implantação, fusão e ciclo de vida da fatia
:::

:::deep docs/subsystems/parallelism.md
Pipeline e tensor parallelism
:::

:::deep docs/subsystems/file-transfer.md
Transferência entre máquinas
:::
