---
id: human-models
label: Modelos
summary: Como um GGUF é achado, lido e cortado em fatias
title: Por dentro dos modelos
reference: crates/node/src/inference/AGENTS.md
order: 40
---

:::lead
Esta pasta conhece os arquivos de modelo: acha os GGUF nas pastas configuradas, lê os cabeçalhos sem carregar, planeja como cortar um modelo por tamanho e escreve as fatias. Também lembra qual fatia estava em serviço, para voltar a ela depois de um reinício.
:::

:::diagram slicing
Um GGUF de 32 camadas cortado por alvo de tamanho, não em partes iguais.
:::

:::cards
### Catálogo em duas passadas
Primeiro uma listagem rápida que só lê o sistema de arquivos, para as telas mostrarem algo na hora; depois os cabeçalhos são lidos em segundo plano e o catálogo é marcado como completo. Um arquivo que não dá para ler é marcado, sem esconder os outros.
### Fatias carregam a origem
Cada fatia ganha metadados `pom.*`: qual faixa de qual modelo ela é. Uma fatia sem esses metadados não sabe o total de camadas do modelo - o total é "desconhecido", nunca zero.
### Planejamento por tamanho
Um alvo em GB por fatia vira um número inteiro de camadas. As fatias intermediárias repetem os tensores de que a arquitetura precisa em cada etapa, e o plano conta esse custo.
### Arquivos de visão e MTP
Projetores de visão e rascunhos MTP são reconhecidos pelo cabeçalho e ligados ao modelo em que se encaixam, estejam onde estiverem na rede.
:::

:::diagram lifecycle
O ciclo de vida de uma fatia: só o estado que o próprio nó confirmou autoriza retomá-la depois de um reinício.
:::

:::note Pendente até o nó confirmar
Depois de um apply, a fatia é anotada como Pending. Só o observador do `/health`, vendo o modelo carregado, a passa para Serving - assim um nó nunca reinicia numa fatia que nunca conseguiu servir.
:::

:::deep crates/node/src/inference/AGENTS.md
As notas da pasta models ficam aqui, porque models/ está no gitignore
:::

:::deep docs/subsystems/slicing.md
Leitura de GGUF, tensores obrigatórios e planejamento por tamanho
:::
