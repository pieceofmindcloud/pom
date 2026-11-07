---
id: screen-chat
label: Chat
summary: Converse com o modelo que a rede serve
title: O Chat
route: /chat
order: 30
---

:::lead
O Chat conversa com o modelo que a sua rede serve, pela mesma rota compatível com a OpenAI que qualquer outro aplicativo usaria. Cada resposta pode mostrar quais máquinas trabalharam nela e quanto durou cada etapa.
:::

![O Chat numa rede que ainda não serve nenhum modelo: a peça que falta e os três passos para chegar lá.](../images/pt-BR/chat.webp)

## Embaixo da caixa

:::cards
### Modelo
Qual modelo responde. Só aparecem aqui os modelos que alguma máquina da rede está servindo.
### Cabeça de inferência
Quando mais de uma máquina tem a primeira camada, este seletor escolhe por qual delas a pergunta entra. **Auto** balanceia pela fila de cada uma.
### Think
Aparece só para um modelo que sabe pensar antes de responder. A escolha fica com a conversa até você mudar.
### Anexos
O clipe adiciona um ou mais arquivos à mensagem; imagens vão para modelos que têm visão. Um PDF vira texto antes do envio, e páginas escaneadas são lidas por OCR, que leva alguns segundos por página e pode trocar uma palavra, por isso esse anexo mostra uma marca OCR. Um PDF pode ter até 100 MB, 1000 páginas e 100 páginas escaneadas.
:::

## Gerando imagens

Quando a rede serve um modelo de imagem, um menu **Gerar imagem** aparece embaixo da caixa. No **Automático** (o padrão) basta pedir uma imagem: o modelo escreve um prompt detalhado a partir da conversa e faz a imagem, e **Prompt** embaixo das imagens mostra o que ele escreveu. **Sempre imagem** manda toda mensagem direto para o fluxo, e **Só texto** nunca faz imagens. O mesmo menu escolhe o fluxo - cada modelo de imagem oferece um pronto de texto para imagem, e um administrador pode publicar qualquer fluxo do ComfyUI com **POM > Publish to Chat**. Enquanto trabalha, a resposta mostra as peças se encaixando com o passo, e depois as imagens em miniatura. Um clique abre uma em tamanho cheio com **Baixar**. O mesmo menu escolhe o tamanho da imagem. Anexe uma imagem à mensagem para usá-la como base: a nova imagem parte dela (com o fluxo pronto, ou um publicado que aceite imagem). As imagens são copiadas para este nó e guardadas em `.comfyui/images` dentro do workspace, com o nome tirado do conteúdo, então continuam aqui quando a máquina que as fez sai da rede; a conversa guarda só esse nome. **Resposta em texto** volta para o modelo de linguagem.

Um administrador que publica um fluxo do ComfyUI escolhe quais entradas o Chat preenche (um texto, um número, uma chave, uma lista de opções, uma imagem) e escreve o que ele faz. Cada fluxo assim vira uma função: no **Automático** o modelo a chama quando a conversa pede, preenchendo os valores, e em **Funções**, no mesmo menu, **Executar...** abre um formulário com um campo por entrada, já com os valores do fluxo. Um fluxo pode fazer imagens, vídeo ou áudio; vídeo e áudio tocam na resposta, com **Baixar**. As mesmas funções funcionam pela API com qualquer chave deste nó, para programas que chamam ferramentas por conta própria.

## Depois de uma resposta

Cada resposta traz uma linha curta com o tempo até o primeiro token e a velocidade da geração. Abrindo, aparece a telemetria completa: as máquinas do caminho, o tempo de rede entre elas, quanto durou cada salto e o formato do estado oculto que atravessou a rede. O pensamento, quando o modelo pensa, fica fechado acima da resposta e abre com um clique.

:::note Quando o Chat mostra uma peça faltando
Nenhuma máquina da rede está servindo um modelo ainda. O administrador vê os três passos que faltam - baixar um modelo, implantar, conversar - e onde cada um é feito. Uma máquina que acabou de entrar numa rede vê "Conectando" até a rede deixá-la entrar.
:::

:::note Enter envia
Enter envia a mensagem, Shift+Enter quebra a linha. As conversas ficam à esquerda, neste navegador.
:::

:::deep docs/subsystems/inference.md
O que acontece com uma pergunta entre o gateway e a última máquina
:::
