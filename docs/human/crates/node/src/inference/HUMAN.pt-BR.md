---
id: human-inference
label: Inferência
summary: Como uma pergunta atravessa as máquinas e volta em tokens
title: Por dentro da inferência
reference: crates/node/src/inference/AGENTS.md
order: 10
---

:::lead
Cada nó roda um pequeno servidor de inferência na porta 8181. Ele carrega só a própria fatia do modelo, recebe o trabalho da máquina anterior, roda as camadas dele e entrega o resultado à próxima. Esta pasta é esse servidor, e o jeito como as peças conversam.
:::

:::diagram request
O prompt entra por qualquer nó; a última máquina da cadeia responde direto a quem perguntou.
:::

## Duas fases de uma resposta

:::steps
### Prefill
A máquina com a camada 0 lê o prompt inteiro de uma vez e enche o cache KV. O resultado das camadas dela - o estado oculto, não texto - vai para a próxima máquina, que faz o mesmo com as próprias camadas.
### Decode
Depois, um token de cada vez: cada token novo atravessa de novo todas as máquinas da cadeia, mas agora é uma única linha de números por salto, então é rápido.
### A resposta volta
A última máquina sorteia o próximo token. Na resposta direta, ela manda o token direto ao gateway que perguntou; no modo anel, mais antigo, o resultado volta pela cadeia.
:::

## O que viaja entre as máquinas

:::cards
### Estado oculto, nunca pesos
Cerca de `hidden_dim × 2` bytes por token por salto em f16, o padrão. f32 não perde nada e dobra o tamanho; f8 e int4 perdem precisão e são negociados por salto.
### Uma sessão, duas sequências
Cada conversa guarda o prompt numa sequência KV e gera numa cópia dela. O fim do turno apaga a cópia inteira, então o prompt fica pronto para o próximo turno - esse é o cache de prefixo.
### Ocioso e acordar
Com "descarregar se ocioso", uma máquina que ficou um tempo sem trabalhar descarrega o modelo mas continua na rede como `idle`. A próxima requisição recarrega, e acorda em paralelo as outras máquinas ociosas do plano.
:::

:::note Por que é feito assim
Máquinas pela internet ficam longe no tempo. Um pipeline atravessa cada fronteira de camadas uma vez por token e não precisa de barreira dentro de cada operação; por isso é o padrão entre casas - e por isso, quanto mais perto as máquinas, mais rápida a resposta.
:::

:::deep crates/node/src/inference/AGENTS.md
Cada arquivo do servidor de inferência, função por função
:::

:::deep docs/subsystems/inference.md
O caminho completo da inferência: prefill, step, wire, caches
:::
