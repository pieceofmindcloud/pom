---
id: pom-settings
label: Configurações POM
summary: O que existe porque a inferência é dividida entre máquinas
title: Configurações POM
order: 120
---

:::lead
O llama.cpp roda um modelo numa máquina. Estas configurações existem porque o POM divide o modelo em fatias, em máquinas diferentes, e precisa decidir como cada fatia cabe na memória e como o estado passa de uma para a outra.
:::

| Propriedade | O que faz | Quando mudar |
|---|---|---|
| Desativar embeddings | Normalmente é automático: só o último peer dispensa a extração do estado oculto. Ligado à mão, impede o primeiro peer de extraí-lo, e o pipeline para de funcionar. | Quase nunca. |
| Ajustar à memória | Antes de carregar, o nó lê a memória livre de cada GPU, reserva uma margem de segurança (256 MiB por padrão) e roda o ajuste do llama.cpp para Contexto × Sessões. Se não couber, reduz as camadas na GPU ou manda parte para a RAM, em vez de falhar por falta de VRAM. | Deixe ligado. Um Camadas na GPU diferente de 99, ou Experts do MoE na RAM, desligam o ajuste de camadas. |
| Chunks em voo | Quantos pedaços de prefill andam pelo pipeline ao mesmo tempo (1 a 8). | Maior esconde a latência de cada salto num pipeline profundo; 1 desliga a sobreposição. |
| Pedaço de prefill automático | O gateway escolhe o tamanho do pedaço de cada prompt, entre 32 tokens e o Chunk prefill, pelo que mediu na rota; com o cache de experts do MoE ligado, no mínimo 64. Desligado, todo prompt vai em pedaços de exatamente o Chunk prefill. | Deixe ligado. Desligue para comparar medições com um pedaço fixo, ou numa rota em que a escolha automática não ajuda. |
| Juntar fatias vizinhas na mesma máquina | Duas faixas adjacentes na mesma máquina viram um arquivo só antes de subir. | Ligue quando uma máquina ficou com faixas vizinhas: economiza um salto. |
| Precisão do estado oculto | Bytes enviados entre os peers a cada token. f16 é o padrão; f32 não perde nada e dobra o volume; f8 e int4 por grupo têm perda e exigem suporte de todos os peers. | Só aparece quando o pipeline cruza máquinas. Experimente f8 num link lento. |
| Ceder a GPU para imagens | Quando um modelo de imagem (ComfyUI) na mesma GPU física precisa de espaço e este modelo não tem pedido em andamento (há 10 s), o POM o descarrega; o próximo pedido o recarrega, e uma imagem em andamento pausa até este modelo ficar quieto de novo. Desligado por padrão. | Ligue num modelo que recarrega rápido e divide a placa com um modelo de imagem. Deixe desligado num modelo grande (lento para recarregar) ou em uso constante. |

:::warn Sem o ajuste de memória
Com Ajustar à memória desligado, o llama.cpp tenta pôr a fatia inteira, o cache KV e os buffers de cálculo na VRAM. Se não couber, o nó falha ao subir e o supervisor tenta de novo, em laço. Desligado, o Microlote também deixa de valer e vale o Batch.
:::

## Receitas

:::steps
### Fazer caber uma fatia grande
Ligue Ajustar à memória, baixe o Microlote para 256 e confira a prévia de alocação. Se a fatia ainda não couber e o modelo for MoE, veja Modelos MoE maiores que a VRAM nas Configurações padrões.
### Pipeline em link lento
Suba Chunks em voo (até 8) para sobrepor o prefill entre os saltos. Se todos os peers anunciam suporte, troque a Precisão do estado oculto para f8.
### Duas faixas vizinhas numa máquina
Ligue Juntar fatias vizinhas: a máquina serve a união num arquivo só, e o token deixa de atravessar a rede entre as duas faixas.
:::

:::faq
### Ajustar à memória deixa a inferência mais lenta?
Só quando ele precisa mandar camadas ou experts para a RAM, porque é isso ou a fatia não sobe. Quando tudo cabe, ele não muda nada.
### Onde vejo o que o ajuste decidiu?
Na prévia de alocação, antes de implantar, e no log de carga do nó, nas linhas que começam com llama.cpp fit.
### Por que no mínimo 64 tokens com o cache de experts ligado?
O cache de experts serve lotes de até 32 tokens como geração. Um pedaço de prefill de 32 tokens passava por ele, atravessava o PCIe com os experts de cada pedaço e expulsava os que a geração mantém quentes.
### O que é o estado oculto?
O vetor de ativações que uma fatia entrega à próxima a cada token. É o que atravessa a rede no pipeline.
:::

:::deep docs/subsystems/deployment.md
Ajuste de memória, fusão e plano da implantação
:::

:::deep docs/subsystems/inference.md
Pipeline de inferência e precisão do estado oculto
:::
