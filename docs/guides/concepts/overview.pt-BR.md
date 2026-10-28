---
id: overview
label: O que é esta plataforma
summary: Máquinas comuns servindo um modelo grande, juntas
title: O que é esta plataforma
order: 10
---

:::lead
Um modelo de linguagem que não cabe numa máquina é cortado em fatias de camadas, e cada máquina serve a sua. As máquinas se acham sozinhas na rede, combinam quem fica com qual faixa, e passam a responder como se fossem uma só.
:::

Todo nó roda o mesmo binário, todo nó serve esta interface e a API, e qualquer um deles pode receber a requisição de quem está perguntando. O que muda entre eles é a faixa de camadas que carregaram na memória e o papel que cada um tem na rede.

:::diagram topology
Três máquinas cobrindo um modelo de 32 camadas. Quem recebe a pergunta não precisa ser quem tem a camada 0.
:::

## As quatro peças

:::cards
### Nó
Uma máquina no cluster. Anuncia quem é, quanta memória tem e qual faixa de camadas serve. Descobre os outros por UDP na rede local e pelo endpoint iroh pela internet.
### Fatia
Um arquivo GGUF com uma faixa de camadas do modelo original. Fatiar é local: cada máquina extrai a própria faixa da própria cópia, nenhum peso viaja pela rede por causa disso.
### Pipeline
A cadeia ordenada de nós que cobre da camada 0 até a última. Sem cobertura completa não há resposta - a requisição falha com a explicação em vez de responder pior.
### Gateway
A porta de entrada HTTP, servida por todos os nós: a interface, a API compatível com OpenAI e a rota de geração.
:::

## Por que fatiar por camadas

As camadas de um modelo transformer rodam em sequência: a saída de uma é a entrada da seguinte. Isso permite cortar o modelo em pontos limpos e distribuir os pedaços, desde que os pedaços rodem na ordem certa. O que trafega entre as máquinas não são pesos nem texto, e sim o estado oculto - um vetor por token.

:::note O tamanho da fatia é decisão da máquina, não do modelo
As fatias não têm o mesmo tamanho. Quem manda é a memória da máquina que vai recebê-la, e as máquinas de um cluster real não se parecem. A tela de fatiar trabalha com um alvo em GB por fatia e converte isso em faixa de camadas.
:::

Se é a primeira vez aqui, siga os guias na ordem: primeiros passos, os perfis, o caminho de uma pergunta e, por fim, do modelo ao cluster servindo.

:::deep docs/subsystems/p2p-network.md
Descoberta, endereços anunciados e identidade dos nós
:::

:::deep AGENTS.md
Mapa do repositório e invariantes do projeto
:::
