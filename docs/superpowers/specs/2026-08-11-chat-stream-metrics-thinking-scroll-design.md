# Chat streaming: métricas, pensamento e rolagem

## Objetivo

Melhorar o chat de teste sem criar um caminho de inferência paralelo: ao fim de
cada resposta, mostrar a velocidade medida pelo backend; exibir separadamente
o raciocínio produzido por modelos com thinking habilitado; e não reposicionar
quem estiver lendo o histórico durante o streaming.

## Contrato de streaming

`POST /v1/chat/completions` continua compatível com clientes OpenAI. Os chunks
de conteúdo continuam em `choices[0].delta.content`. O último chunk, junto com
`finish_reason`, passa a incluir a extensão opcional:

```json
{
  "x_rdma_p2p": {
    "tokens_per_sec": 18.4,
    "tokens_generated": 312,
    "duration_ms": 16956,
    "source": "pipeline:peer-a→peer-b",
    "partial": false
  }
}
```

Os valores vêm de `GenSummary`, a mesma fonte usada pelas respostas não
streaming e pelo log de requisições. O cliente só mostra a métrica quando o
chunk final a contém, para manter compatibilidade com nós de versões anteriores.

## Pensamento

O cliente separa incrementalmente `<think>...</think>` da resposta de texto.
O delimitador não aparece na mensagem. O conteúdo entre as tags vai para
`reasoning`; o restante vai para `content`.

O bloco "Pensamento" usa `details/summary`, fica aberto enquanto está sendo
gerado e recolhe automaticamente quando a resposta termina. O leitor pode
reabrir o bloco. Respostas sem tags não criam o bloco. Tags incompletas durante
o streaming ficam no buffer de raciocínio até que possam ser classificadas.

## Rolagem

O painel de mensagens passa a manter uma referência ao seu próprio elemento de
rolagem. Antes de aplicar um token, ele mede se a pessoa está a até 32 px do
fim. Só acompanha o final automaticamente nessa condição. Um evento de
rolagem manual para cima desativa o acompanhamento até que a pessoa chegue
novamente ao fim. O envio de uma nova pergunta inicia a resposta no fim e volta
a ativar o acompanhamento.

Assim, o chat não muda a posição de leitura durante streaming, mas continua
conveniente para quem acompanha a resposta ao vivo.

## Estado persistido e componentes

`ChatMessage` ganha campos opcionais `reasoning`, `tokensPerSecond`,
`tokensGenerated` e `durationMs`. Como as sessões são locais, métricas e
pensamento continuam disponíveis ao reabrir a conversa. `Chat.tsx` concentra a
decodificação de SSE, o controle de rolagem e a renderização do bloco; a lógica
de separar tags é extraída para uma função pura testável.

## Verificação

1. Teste de backend confirma que o chunk final streaming inclui `x_rdma_p2p`.
2. Teste de frontend confirma a separação de texto e thinking inclusive quando
   uma tag chega dividida entre chunks.
3. Teste de frontend confirma que o detector de proximidade do fim não segue a
   resposta quando a pessoa rola para cima.
4. Teste manual com streaming em uma resposta longa: o pensamento aparece em
   bloco separado, o rodapé mostra tok/s ao concluir e o histórico permanece
   parado após rolagem manual.
