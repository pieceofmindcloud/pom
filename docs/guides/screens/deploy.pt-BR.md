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

## Modelos de imagem

**Modelo de imagem** na barra de modos transforma o estúdio numa pipeline de imagem. Um modelo de imagem (Z-Image, Qwen Image, Flux...) é servido por estágio, na ordem em que a imagem é feita: o **text encoder** lê o prompt, o **modelo de difusão** desenha a imagem passo a passo e o **VAE** transforma o resultado em pixels. Cada estágio roda em máquinas que rodam o ComfyUI e já têm o arquivo daquele estágio, então uma GPU pequena guarda a difusão enquanto os outros ficam em outras máquinas.

:::steps
### Escolha os arquivos
Escolha o arquivo do modelo de difusão. O text encoder e o VAE são preenchidos quando a mesma pasta tem exatamente um de cada; troque se quiser outros.
### Arraste máquinas para os estágios
Arraste máquinas da barra de baixo para cada estágio. Mais de uma máquina num estágio são réplicas: cada chamada vai para a mais próxima que responder, e a imagem continua quando uma delas para. Uma máquina pode servir vários estágios; o VAE, pequeno, costuma ir junto com outro. Uma máquina que não pode receber um estágio diz o motivo no cartão.
### Leia o veredito e implante
A linha vermelha no topo diz o que ainda falta. Implantar segue a mesma janela de progresso de um modelo de linguagem, e a implantação aparece no Dashboard e em Implantações.
:::

O ComfyUI de qualquer máquina da rede lista os estágios como `POM ▸ <nome> ▸ <arquivo>` nos loaders normais, e o Chat pode gerar imagens com ele. Nada é copiado: uma máquina sem o arquivo não é oferecida.

:::deep docs/subsystems/deployment.md
O plano, a fusão, a ordem e o ciclo de vida da fatia
:::

:::deep docs/subsystems/parallelism.md
Pipeline ou paralelismo de tensor
:::
