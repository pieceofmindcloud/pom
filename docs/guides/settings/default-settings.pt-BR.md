---
id: default-settings
label: Configurações padrões
summary: Contexto, lote, experts do MoE, amostragem, cache KV e especulação
title: Configurações padrões da implantação
order: 110
---

:::lead
Estas são as configurações que o próprio llama.cpp já oferece. O POM só as leva para cada máquina da implantação. Elas decidem quanta memória cada fatia reserva, quão rápido o prompt entra e como o modelo escolhe cada token.
:::

## Memória e capacidade

| Propriedade | O que faz | Quando mudar |
|---|---|---|
| Contexto | Janela de atenção de cada sessão, em tokens. Depois dos pesos, é o que mais pesa na VRAM do cache KV. | Aumente para conversas e prompts longos. Diminua só se a implantação não couber de outro jeito. |
| Sessões | Gerações atendidas ao mesmo tempo. Cada uma reserva o contexto inteiro, então o cache KV custa Contexto × Sessões. | Aumente para atender mais gente ao mesmo tempo. Cada sessão a mais custa um contexto inteiro. |
| Máx. tokens | Teto de uma perna do laço de continuação do gateway, não da requisição. Quem limita a resposta é o max_tokens do cliente. | Raramente. |
| Batch | Tokens do prompt que o llama.cpp processa num passo do prefill. Maior enche melhor a GPU em prompts longos e aumenta o buffer de cálculo. Não muda a velocidade da geração. | Diminua se faltar VRAM. Aumente se sobrar e os prompts forem longos. |
| Microlote | Quantos tokens o contexto decodifica de uma vez com o ajuste de memória ligado. Os buffers de cálculo na VRAM são dimensionados por ele. Nunca maior que o Batch; com o ajuste desligado, vale o Batch. | Baixe de 512 para 256 para liberar VRAM para pesos, MTP ou cache de experts. O prefill fica um pouco mais lento. |
| Chunk prefill | Tokens do prompt enviados pelo pipeline a cada ida e volta no prefill. | Menor começa a transmitir antes num link lento. Maior reduz o número de idas e voltas. |
| Cache K / Cache V | Tipo do cache KV que o llama.cpp aloca (f16, q8_0, q4_0...). Quantizado ocupa menos VRAM. Vale para todo contexto que o nó carregar em seguida. | q8_0 ou q4_0 em contextos grandes. |

:::note A conta da memória
Cada máquina precisa de: os pesos da fatia, o cache KV (Contexto × Sessões, no tipo do Cache K/V) e os buffers de cálculo (que crescem com o Microlote). Duas sessões de 32k custam o mesmo que uma de 64k.
:::

## Modelos MoE maiores que a VRAM

Num modelo MoE (mixture of experts), cada camada tem muitos experts e cada token usa só alguns. Os experts são a maior parte dos pesos, então dá para deixá-los na RAM e manter na VRAM o resto do modelo e os experts usados há pouco. As duas configurações são do próprio llama.cpp.

| Propriedade | O que faz |
|---|---|
| Experts do MoE na RAM | Mantém todos os experts roteados na RAM, calculados pela CPU (o --cpu-moe do llama.cpp), e deixa a VRAM para o resto do modelo. Desliga o ajuste de camadas. |
| Cache de experts na GPU (MiB) | VRAM que guarda os experts da RAM usados há pouco (o --moe-cache-mib do llama.cpp): só as faltas atravessam o PCIe, em lotes de até 32 tokens (a geração; o prefill usa o descarregamento normal). 0 desliga. É o total do nó: com duas GPUs ele é dividido entre elas como as camadas, e o llama.cpp então desliga o pipeline parallelism entre elas. |

:::stats
- **14,95 tok/s** Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), uma GPU de 16 GB, todos os experts na RAM
- **19,26 tok/s** O mesmo, com 8000 MiB de cache de experts
:::

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
Sem experts na RAM (nem pelo ajuste, nem por Experts do MoE na RAM), o cache não tem o que guardar e não muda nada. Comece pelo padrão (ajuste ligado, experts na RAM desligado, cache 0), veja na prévia e no log se o ajuste mandou experts para a RAM, e só então defina o cache. Baixar o Microlote para 256 libera VRAM para ele.
:::

## Amostragem

Só o último peer do pipeline amostra. A ordem é a do llama.cpp: primeiro os cortes do conjunto de candidatos, depois as penalidades de repetição, depois o DRY. A semente vem no fim.

| Propriedade | O que faz | Ponto de partida |
|---|---|---|
| Temperatura | Suaviza ou acentua a distribuição antes da amostragem. 0 é sempre o token mais provável; acima de 1 espalha a probabilidade. | 0,6 a 1,0 conforme o modelo. |
| Top-k | Mantém só os K tokens mais prováveis. Aplicado antes do top-p. | 0 desliga. |
| Top-p | Mantém o menor conjunto cuja probabilidade soma este valor e renormaliza. | 1 desliga. |
| Min-p | Descarta o candidato abaixo desta fração do mais provável, depois do top-k e do top-p. | 0 desliga; 0,05 é comum. |
| Penalidade | Divide o logit dos tokens vistos na janela recente, contra laços. | 1 é sem penalidade; acima de ~1,3 pode distorcer o texto. |
| Janela repetição | Quantos tokens recentes a Penalidade considera. | 0 desliga a janela. |
| Presença | Subtrai uma vez do logit de todo token já visto na janela, empurrando para palavras novas. | 0 desliga; a Qwen recomenda até 1,5 contra repetição sem fim. |
| Frequência | Subtrai do logit uma vez por aparição: quanto mais a palavra se repete, menos provável fica. | 0 desliga. |
| DRY: multiplicador | Liga o DRY, que pune a continuação de sequências já vistas (não palavras isoladas). Os outros números do DRY só aparecem com ele ligado. | 0 desliga; 0,8 é o usual. |
| DRY: base | O quanto a penalidade cresce a cada token a mais da sequência repetida. | 1,75. |
| DRY: tamanho permitido | Sequências repetidas menores que isto não são punidas. | 2. |
| DRY: janela | Quantos tokens recentes o DRY percorre. | 0 percorre o histórico inteiro, até 8192 tokens. |
| Semente | De onde o gerador aleatório parte quando a implantação carrega. Mesma semente, mesmo modelo e mesmas requisições na mesma ordem dão as mesmas respostas. | O padrão do POM. |
| Limite de raciocínio | Quantos tokens um bloco de raciocínio pode ocupar; no limite, o fim do raciocínio é forçado. | Vazio é sem limite; 0 fecha o bloco de imediato. |
| Desativar thinking | O modelo responde direto, sem o preâmbulo entre tags. | Ligue quando só a resposta importa. |

## Especulação

A especulação rascunha os próximos tokens e confere todos num lote só. Quando o rascunho acerta, a rodada pelo pipeline rende vários tokens em vez de um. Só um modo roda de cada vez.

| Propriedade | O que faz |
|---|---|
| Especulação | Nenhuma; MTP (draft), que rascunha com a cabeça treinada do próprio modelo (nextn); ou Especulativa (lookup), que repete texto que já está no contexto e só compensa em saída repetitiva. |
| Tokens rascunhados pelo MTP | Quantos tokens a cabeça nextn adivinha por rodada. Mais alarga o lote em todos os hops; menos desperdiça menos quando o palpite erra. |
| Probabilidade mínima do MTP | Piso de confiança para continuar o rascunho. Menor aceita mais palpites, inclusive os fracos; 0 aceita todos. |
| Onde o MTP fica | Automático: VRAM só se o modelo inteiro ainda couber junto. VRAM: o MTP reserva a dele primeiro e o modelo usa o resto. RAM: o MTP roda na CPU e não gasta VRAM. |
| MTP (cauda) | Aparece quando o modelo não traz a cabeça nextn: escolha o rascunho externo (mtp-*.gguf, ou um drafter DSpark do DeepSeek-V4.1, dspark-*.gguf) que a última máquina carrega. |
| Largura do rascunho / Janela de busca | Só no lookup: quantos palpites por lote, e o maior n-grama procurado numa repetição anterior. |

:::warn Meça o MTP num modelo MoE com experts na RAM
A rodada de verificação processa vários tokens de uma vez. Com os experts na RAM, ela busca muito mais experts que um token sozinho, e a geração pode ficar mais lenta que sem especulação. Compare os tok/s na telemetria antes de manter.
:::

## Como mudar uma propriedade

:::steps
### Abra o inspetor
Em Implantar, escolha o modelo e as máquinas. O inspetor mostra os valores com que a implantação vai subir. O ícone [i] à esquerda de cada nome explica o campo.
### Mude o valor
Digite o número no campo ou marque a caixa. Um campo que depende de outro (os números do DRY, os do MTP) aparece quando o outro o liga.
### Confira a prévia de alocação
Ela refaz a conta de memória de cada máquina com os valores novos. Se uma máquina passar do limite, ajuste antes de implantar.
### Implante ou reimplante
Para uma implantação que já está no ar, use Reimplantar no Histórico: a versão nova sobe com os parâmetros mudados e o Histórico guarda a anterior.
:::

:::faq
### A implantação não sobe por falta de VRAM. O que mexo primeiro?
Ligue Ajustar à memória, nas Configurações POM. Depois baixe o Microlote para 256 e use Cache K/V quantizado. Reduza Contexto ou Sessões só em último caso.
### O max_tokens do cliente ou o Máx. tokens: quem vence?
O max_tokens do cliente limita a resposta. O Máx. tokens só divide a geração em pernas dentro do gateway.
### Mudar a amostragem exige recarregar o modelo?
Os valores vão com a implantação, então a mudança entra numa nova versão (Reimplantar).
:::

:::deep docs/subsystems/deployment.md
Implantação, ajuste de memória e ciclo de vida da fatia
:::

:::deep docs/subsystems/deployment.md
Experts do MoE na RAM e cache de experts na GPU
:::

:::deep docs/subsystems/inference.md
Pipeline de inferência, prefill e especulação
:::
