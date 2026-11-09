---
id: slice-and-deploy
label: Do modelo ao cluster servindo
summary: Escolher as faixas, levar o modelo a cada máquina, pôr em serviço
title: Do modelo ao cluster servindo
order: 50
---

:::lead
São três atos distintos, e vale separá-los na cabeça: escolher quem serve quais camadas, levar o modelo às máquinas certas, e mandar as máquinas carregarem a faixa delas.
:::

:::note Antes de tudo, um modelo
Os modelos inteiros vêm das pastas de modelos da máquina. Para baixar um, use Procurar Modelos, a tela do plugin do Hugging Face: ela mostra a licença antes do download e o progresso na aba Downloads. Um modelo que cabe inteiro numa máquina é implantado só nela.
:::

## 1. Escolher as faixas (sem cortar)

Cada máquina serve a sua faixa de camadas direto da sua própria cópia do arquivo inteiro, então nada é cortado antes de uma implantação. A tela de implantação propõe os cortes nas fronteiras que o modelo permite, e mudar os cortes depois é só uma recarga. Fatiar o arquivo só sobrevive para as partes de tensor parallel, um recurso de desenvolvimento (Partes tensor-parallel).

## 2. Levar o modelo a cada máquina

Arquivos não são mais enviados entre máquinas. Cada máquina que serve uma faixa guarda a própria cópia do modelo inteiro: baixe nela (por Procurar Modelos, ou **Baixar aqui** no cartão dela na tela de implantação, que usa o mesmo repositório e arquivo que outra máquina baixou), ou copie o arquivo para uma das pastas de modelos dela e **Varrer**. A máquina serve a faixa dela a partir dessa cópia; nada passa pela rede antes de o modelo começar.

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
### Preciso fatiar o modelo?
Não. Cada máquina serve a sua faixa a partir do arquivo inteiro. Um modelo que cabe inteiro numa máquina é implantado como modelo inteiro, numa máquina só. Mais máquinas podem entrar no mapa, desde que as faixas delas cubram todas as camadas.
### Pipeline ou tensor parallelism?
Pipeline é a opção recomendada entre máquinas pela internet: uma travessia por fronteira de camadas por token. No tensor parallelism cada camada é repartida e as máquinas somam resultados várias vezes por token, então ele pede uma rede rápida.
:::

:::deep docs/subsystems/slicing.md
Fatiamento (partes tensor-parallel), tabela de tipos
:::

:::deep docs/subsystems/deployment.md
Implantação, fusão e ciclo de vida da fatia
:::

:::deep docs/subsystems/parallelism.md
Pipeline e tensor parallelism
:::

:::deep docs/subsystems/file-transfer.md
As transferências entre máquinas, aposentadas
:::
