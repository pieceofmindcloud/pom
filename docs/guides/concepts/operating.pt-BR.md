---
id: operating
label: Operar e diagnosticar
summary: Subir, conferir e achar o que quebrou
title: Operar e diagnosticar
order: 90
---

:::lead
Um cluster distribuído falha de formas que uma máquina só não conhece: metade da rede enxerga a outra, uma fatia não carregou, um endereço mudou. Esta página é o roteiro de conferência.
:::

:::diagram lifecycle
O ciclo de vida de uma fatia: só o estado que o próprio nó confirmou autoriza retomá-la depois de um reinício.
:::

## A prova de que está funcionando

Toda resposta carrega a cadeia que a produziu em x_pom.source. Uma origem começando por pipeline: e listando dois ou mais saltos é a única prova de que a inferência foi conjunta. Um salto só, ou none, significa que o planejador não fechou a cobertura das camadas.

```
curl -s localhost:8080/peers | python3 -m json.tool
curl -sf localhost:8181/health
```

:::checklist Conferência rápida
- /peers lista todas as máquinas esperadas.
- /health do servidor de inferência responde em cada máquina com fatia.
- Uma chamada de teste volta com x_pom.source começando por pipeline: e com dois ou mais saltos.
:::

## Sintomas e onde olhar

| O que você vê | Causa provável | Onde confirmar |
|---|---|---|
| As máquinas não se enxergam | descoberta não saiu do nó, ou endereço anunciado errado | a tela de Rede e a lista de nós do painel |
| Origem “none” na resposta | a cobertura de camadas tem buraco | a tela Implantar na rede mostra a faixa que falta |
| Um nó aparece sem fatia | o modelo não carregou, ou a porta de inferência está ocupada | o card da máquina no painel diz o motivo |
| A resposta demora muito para começar | a máquina com a camada 0 está processando o prompt | mova a camada 0 para uma máquina com mais memória |
| Uma fatia não volta depois de reiniciar | ela nunca chegou a carregar antes de cair | o card mostra por que a retomada foi recusada |
| O nó aparece como idle | o modelo foi descarregado por ociosidade | nada a fazer: a próxima requisição recarrega |

:::warn Duas máquinas, dois lares
Rodar dois nós no mesmo diretório faz os dois carregarem a mesma identidade e o cluster os trata como uma máquina só. Portas diferentes não bastam: o segundo nó precisa de um diretório de trabalho próprio (POM_HOME).
:::

:::note Logs da implantação
Implantações > Logs reúne os logs de processo de todas as máquinas na ordem em que chegaram, e pode ser filtrada por implantação.
:::

:::deep docs/subsystems/operations.md
Subida, verificação e pegadinhas de campo
:::

:::deep docs/subsystems/build-and-run.md
Build, features de GPU e variáveis de ambiente
:::
