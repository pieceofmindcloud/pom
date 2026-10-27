---
id: beta-settings
label: Configurações Beta
summary: Experts do MoE na RAM, cache de experts na GPU e prefill automático
title: Configurações Beta
order: 130
---

:::lead
Estas três configurações existem para rodar um modelo MoE que não cabe inteiro na VRAM. Elas funcionam, mas ainda estão sendo medidas: o melhor valor muda de modelo para modelo e de máquina para máquina.
:::

Num modelo MoE (mixture of experts), cada camada tem muitos experts e cada token usa só alguns. Os experts são a maior parte dos pesos. Por isso dá para deixá-los na RAM e manter na VRAM só o resto do modelo e os experts usados há pouco.

| Propriedade | O que faz |
|---|---|
| Experts do MoE na RAM | Mantém todos os experts roteados na RAM, calculados pela CPU (o --cpu-moe do llama.cpp), e deixa a VRAM para o resto do modelo. Desliga o ajuste de camadas. |
| Cache de experts na GPU (MiB) | VRAM que guarda os experts da RAM usados há pouco: só as faltas atravessam o PCIe, em lotes de até 32 tokens (a geração; o prefill usa o descarregamento normal). 0 desliga. O valor é por GPU: com duas placas, cada uma guarda os experts das próprias camadas. |
| Pedaço de prefill automático | O gateway escolhe o tamanho do pedaço de cada prompt, entre 32 tokens e o Chunk prefill, pelo que mediu na rota; com o cache de experts ligado, no mínimo 64. Desligado, todo prompt vai em pedaços de exatamente o Chunk prefill. |

:::stats
- **14,95 tok/s** Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), uma GPU de 16 GB, todos os experts na RAM
- **19,26 tok/s** O mesmo, com 8000 MiB de cache de experts
:::

## Como combinar

| Combinação | Experts na RAM | Cache (MiB) | Ajustar à memória | Quando usar |
|---|---|---|---|---|
| Tudo na VRAM | desligado | 0 | ligado | O modelo cabe na VRAM. É a mais rápida. |
| O ajuste decide, com cache | desligado | 4000 a 8000 | ligado | O modelo quase cabe. O ajuste manda para a RAM só o que não couber, e o cache serve os experts usados há pouco. Se tudo couber, o nó desliga o cache sozinho. |
| Todos os experts na RAM, com cache | ligado | o maior que couber | (desligado pelos experts na RAM) | O modelo é muito maior que a VRAM. A VRAM fica para atenção, KV, MTP e o cache. |
| Todos os experts na RAM, sem cache | ligado | 0 | (desligado pelos experts na RAM) | Pouca VRAM. A CPU calcula todos os experts; é a mais lenta. |

:::note Quanto de cache?
Dê ao cache a VRAM que sobra depois dos pesos, do cache KV, dos buffers de cálculo e do MTP. Se o contexto não couber, o nó corta o cache pela metade e tenta de novo, até desligá-lo abaixo de 512 MiB. No log de carga, a linha MoE cache size = X for Y of host experts diz quanto dos experts da RAM o cache cobre; ao descarregar, as linhas llama_moe_cache mostram a taxa de acerto.
:::

:::warn O cache só guarda experts que estão na RAM
Sem experts na RAM (nem pelo ajuste, nem por Experts do MoE na RAM), o cache não tem o que guardar e não muda nada. E com os experts na RAM, o MTP pode deixar a geração mais lenta: meça antes de manter.
:::

## Passo a passo

:::steps
### Comece pelo padrão
Ajustar à memória ligado, Experts do MoE na RAM desligado e cache 0. Implante e veja na prévia e no log se o ajuste mandou experts para a RAM.
### Ligue o cache
Se mandou, defina o Cache de experts na GPU (algo entre 4000 e 8000 MiB por GPU) e reimplante. Baixar o Microlote para 256 libera VRAM para ele.
### Modelo muito maior que a VRAM
Ligue Experts do MoE na RAM e dê ao cache o maior valor que couber.
### Compare
Use a telemetria dos logs para comparar tok/s de geração e de prefill entre as versões. O Histórico guarda cada versão, com os parâmetros com que subiu.
:::

:::faq
### Funciona com duas GPUs na mesma máquina?
Sim. O cache é por GPU: as duas placas mantêm a parte delas das camadas, e cada uma guarda os experts das próprias camadas.
### Por que o prefill continua lento com o cache?
O cache serve lotes de até 32 tokens, que é a geração. O prefill passa pelo descarregamento normal dos experts da RAM.
### Quando desligar o Pedaço de prefill automático?
Para comparar medições com um pedaço fixo, ou numa rota em que a escolha automática não ajuda. Desligado, vale sempre o Chunk prefill.
:::

:::deep docs/subsystems/deployment.md
Experts do MoE na RAM e cache de experts na GPU
:::
