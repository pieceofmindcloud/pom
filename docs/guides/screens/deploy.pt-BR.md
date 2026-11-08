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
O que implantar: **Pipeline**, o padrão, parte de um modelo inteiro: uma máquina serve tudo, ou várias máquinas que o têm servem cada uma uma faixa. **Fatias (legado)** aparece só para conjuntos de fatias feitos antes, ou para um serviço de inferência que roda fatias em estágios. Ao lado, o motor (llama.cpp, ou um serviço de inferência que um plugin publica), o modelo e o nome da implantação.
### O mapa
As máquinas da implantação, cada uma com a faixa de camadas que vai servir, a memória que tem e a projeção depois de carregar. O **Gateway** fecha o ciclo com o custo de um token assim que der para medir.
### O inspetor
**Globais** guarda as configurações da implantação inteira (contexto, sessões, amostragem, as configurações do POM); a aba da máquina guarda o que é por máquina: camadas na GPU e se o cache KV vai para a GPU.
:::

## Da escolha ao serviço

:::steps
### Escolher o modelo
Em Pipeline, os arquivos GGUF inteiros que alguma máquina conhece. Fatias (legado) lista os conjuntos de fatias que alguma máquina já tem.
### Pôr as máquinas no mapa
Arraste máquinas da faixa de baixo para o mapa. O planejador distribui as faixas, e **Cobertura do modelo**, no canto de baixo, conta as camadas cobertas.

Em **Pipeline**, o cartão de uma máquina que não tem o modelo avisa. **Baixar aqui** faz ela baixar o mesmo repositório e arquivo do Hugging Face que outra máquina baixou, com todas as partes de um arquivo dividido; o cartão passa a mostrar o progresso. Para um arquivo copiado à mão, coloque-o numa das pastas de modelos dessa máquina e aperte **Varrer**. Um modelo que ninguém baixou pelo POM oferece só Varrer. O mesmo vale para um projetor de visão ou um rascunho MTP que a implantação precisa numa máquina que não o tem: o cartão dela nomeia o arquivo e oferece Baixar aqui (o repositório de onde outra máquina o baixou) ou Varrer. Cada cartão também mostra o peso das camadas dele e do cache KV nas configurações atuais, e se o KV fica na VRAM ou na RAM. Uma máquina sem o modelo nunca recebe uma cópia pela rede: o botão Implantar fica cinza e diz o nome dela até o modelo estar lá. Uma máquina com mais camadas do que as GPUs dela comportam roda o resto na CPU; Implantar então pede uma confirmação que nomeia cada máquina assim com as camadas na GPU e na CPU.
### Dividir um modelo inteiro entre máquinas que o têm
Em **Pipeline**, quando duas ou mais máquinas do mapa têm o arquivo inteiro, a aba Máquina oferece **Dividir as camadas entre N máquinas**. Ela divide as camadas por igual, na ordem do mapa, em cortes que o modelo permite. Cada máquina mostra então sua faixa com **-** e **+**: um clique move o corte que ela divide com a vizinha em um passo válido, e um botão que não pode mover diz o porquê (não há corte válido nessa direção, ou uma máquina ficaria sem camadas). Cada máquina serve sua faixa a partir da própria cópia, então nada é copiado pela rede. Uma máquina que não consegue apontar para sua faixa grava a faixa em disco como fatia antes; a aba dela avisa, com o tamanho.

O cadeado ao lado de uma faixa a mantém exatamente como está. **Equilibrar pela memória livre** divide então as camadas das máquinas destravadas na proporção da memória livre de cada uma (memória da GPU, ou RAM numa máquina sem GPU), em cortes que o modelo permite, em volta das faixas travadas. O - e o + de uma faixa travada ficam desligados até destravá-la, e também os botões de uma vizinha quando moveriam o corte que ela divide com a faixa travada. Ele conta os pesos e a memória do contexto (o cache KV, para o tamanho de contexto e as sessões que você definiu), mas não os buffers de computação.

Quando a VRAM livre de uma máquina não comporta a faixa inteira, a aba dela mostra como as camadas se dividem, por exemplo **18 na GPU, 6 na CPU**, com a RAM que a parte da CPU precisa. As camadas que não cabem rodam na CPU, o que deixa a geração bem mais lenta; a implantação segue mesmo assim, depois da confirmação de sempre, a não ser que a parte da CPU também não caiba na RAM livre da máquina: aí o plano é recusado e diz quanta RAM precisa. O equilíbrio automático nunca escolhe isso sozinho, então dê menos camadas a essa máquina ou acrescente uma máquina quando puder.
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
