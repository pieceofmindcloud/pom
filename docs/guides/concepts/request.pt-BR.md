---
id: request
label: O caminho de uma pergunta
summary: Do prompt ao token, máquina por máquina
title: O caminho de uma pergunta
order: 40
---

:::lead
Entender esta página é entender de onde vem a latência do cluster, e por que a resposta chega no ritmo em que chega.
:::

:::diagram request
O prompt entra por qualquer nó; a última máquina da cadeia responde direto a quem perguntou.
:::

:::steps
### A pergunta chega
Qualquer nó aceita. Ele escolhe o caminho que entrega a resposta mais cedo entre as máquinas que cobrem o modelo. Se falta faixa, a requisição falha com a explicação - não existe um modo degradado que responda pior sem avisar.
### A cabeça lê o prompt inteiro
A máquina que tem a camada 0 tokeniza e processa o prompt em pedaços. É a etapa mais cara da requisição, e é por isso que a camada 0 deve ficar numa máquina com memória de sobra.
### O estado oculto atravessa a cadeia
Cada máquina roda as suas camadas e entrega o resultado à seguinte. Num modelo de dimensão 4096 são 8.192 bytes por token por salto, porque o vetor viaja em f16 e não em texto.
### A cauda escolhe o token
A última máquina da cadeia é a única que amostra: converte o estado oculto no token seguinte.
### A resposta volta direto
A cauda entrega o token diretamente a quem recebeu a pergunta, em vez de refazer a cadeia de volta. Quando isso não é possível, a resposta volta pelo anel.
:::

## O que isso custa

:::stats
- **8 KB** estado oculto por token, por salto (f16, dimensão 4096)
- **16 KB** o mesmo estado em f32, sem perda
- **~200 B** por token amostrado de volta
- **+1 RTT** por máquina extra na cadeia
:::

:::warn O prompt tem orçamento, e passar dele é recusa
Cada sessão tem a janela de contexto da implantação. Aumentar as sessões multiplica a memória reservada, não divide essa janela. O gateway mede o prompt antes de gerar e responde HTTP 413 quando não sobra espaço, em vez de derrubar o processo.
:::

:::note Decodificação especulativa
Quando o modelo permite, um rascunho de vários tokens é proposto de uma vez e a cauda confirma ou descarta o lote. Acertou, vários tokens saem pelo preço de uma travessia; errou, o cache volta atrás e nada de errado sai.
:::

:::faq
### Dá para trafegar menos bytes por token?
Sim. f16 é o padrão; f32 é sem perda e dobra o volume; f8 e i4 são formatos com perda opcionais, negociados por salto na implantação.
### E se duas máquinas servem a mesma faixa?
São réplicas do mesmo estágio, não dois saltos. O gateway escolhe uma réplica por sessão e mantém essa escolha nas rodadas seguintes.
### Quem decide o caminho?
O gateway estima, para cada caminho que cobre o modelo, o tempo de prefill, de decode e de rede, considerando a fila de cada máquina, e escolhe o que entrega mais cedo.
:::

:::deep docs/subsystems/inference.md
Prefill, decode, formato do estado oculto e especulativa
:::

:::deep docs/subsystems/parallelism.md
Pipeline e tensor parallelism: quando usar cada um
:::

:::deep docs/subsystems/http-api.md
Rotas, autenticação e a prova de execução distribuída
:::
