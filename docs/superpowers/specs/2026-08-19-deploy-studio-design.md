# Deploy Studio - design da tela de implantação

## Objetivo

Transformar a tela de implantação em um estúdio de planejamento de cluster em
tela inteira. O operador escolhe o modelo, define opcionalmente um nome de
implantação, vê todos os peers, arrasta máquinas para o plano e acompanha uma
sugestão que considera memória, GPU, KV cache, latência, cópias existentes e
concorrência.

O backend continua sendo a autoridade do plano. A interface visualiza o plano,
explica a sugestão e envia apenas intenções explícitas para validação final.

## Direção visual

- Usar a mesma linguagem dos cards compactos do Painel: fundo escuro, linhas
  finas, tipografia IBM Plex Sans para leitura e JetBrains Mono para valores.
- A rota ocupa 100% da largura e da altura disponíveis, como Documentação.
- Topo fixo com tipo de implantação, modelo, nome da implantação, estratégia e
  botão Implantar.
- Centro dedicado ao mapa de recursos: cada peer é um card arrastável, os
  links exibem RTT e a pipeline exibe a faixa de camadas entre os nós.
- Em Fatias, uma barra de cobertura fica acima do mapa. Cada segmento representa
  uma camada; verde indica cobertura completa, âmbar indica lacuna e rosa indica
  sobreposição. O resumo mostra `cobertas / total` e o estado que bloqueia ou
  permite a implantação.
- Inspetor estreito à direita, inspirado no inspetor do Delphi: duas colunas,
  propriedade e valor, agrupadas em seções densas.
- Barra inferior persistente com todos os peers conhecidos, mesmo os que não
  estão no plano. Cada item informa disponibilidade, capacidade e latência e
  pode ser arrastado para o mapa.
- Textos de apresentação são reduzidos. Estado, métricas e decisões ocupam o
  espaço principal.

## Modelo de interação

### Seleção de modelo e nome

O topo tem dois controles independentes:

- Modelo: seleciona o conjunto de fatias ou o GGUF inteiro, conforme o tipo.
- Nome da implantação: começa com o nome do modelo, mas pode ser editado.

O nome do modelo continua sendo a identidade técnica usada para roteamento,
catálogo e parâmetro `model` das requisições. O nome da implantação é uma
identidade operacional separada, propagada para runtime, status, jobs e
telemetria. Assim duas implantações do mesmo modelo podem ter nomes distintos
sem quebrar a descoberta do modelo.

### Tipo de implantação

O topo oferece três modos:

- **Fatias**: pipeline por faixas de camadas. O mapa mostra a ordem L0 em
  diante e cada card selecionado recebe uma faixa.
- **Modelo inteiro**: uma máquina ou um conjunto de destinos para cópia e
  serviço do GGUF inteiro. Não mostra tabela de fatias.
- **Tensor**: todos os peers selecionados recebem o modelo inteiro e a
  implantação extrai o shard tensor de cada rank. Não mostra seleção de fatias,
  primary e workers ficam visíveis no mapa e a ordem dos ranks é editável.

### Arrastar e soltar

Um peer pode ser arrastado da barra inferior para o mapa. O drop altera o
conjunto de máquinas desejadas e dispara uma nova prévia do plano. O backend
recalcula a disposição considerando o peer recém-adicionado; a interface não
faz uma cópia paralela do algoritmo.

Quando o peer ainda não possui a fatia ou o modelo, ele continua elegível. A
prévia informa a cópia necessária e o deploy transfere o arquivo na fase de
movimentação. A ausência local não exclui o peer automaticamente.

### Estratégias

O controle Simples/Completa é comum a Fatias e Modelo inteiro:

- **Simples**: usa o menor número de máquinas que cobre o modelo e prioriza
  arquivos já existentes, GPU livre e menor latência. É a opção padrão para
  colocar o modelo no ar com menor movimento.
- **Completa**: usa todos os peers elegíveis que o operador colocou no plano e
  redistribui as faixas disponíveis para ocupar mais GPU, respeitando contexto,
  KV cache, memória de segurança e latência. A sugestão pode escolher RAM como
  fallback, mas sinaliza isso como custo.

O planner não inventa limites de camada que não existem no catálogo durante
uma implantação. Ele pode escolher entre cortes já disponíveis e redistribuí-los
entre os peers. Re-fatiamento físico do GGUF para criar limites novos continua
sendo uma operação da tela Fatiar, não uma escrita escondida durante o deploy.
Essa regra evita que uma sugestão torne o cluster inconsistente ou inicie uma
operação de muitos minutos sem confirmação explícita.

### Inspetor de propriedades

O inspetor alterna entre `GLOBAL` e o peer selecionado.

Propriedades globais, iguais em toda a pipeline ou grupo TP:

- contexto;
- batch e chunk de prefill;
- máximo de tokens de saída;
- temperatura, top-p, top-k, repetição e pensamento;
- máximo de sessões e paralelismo de requisições;
- ajuste automático de memória como política da implantação;
- speculative decoding e MTP quando compatíveis com o modo.

Propriedades por máquina:

- número de camadas em GPU;
- localização do KV cache, GPU ou RAM;
- ajuste de memória local e reserva efetiva;
- limite de sessões local, quando diferente da política global;
- papel no TP e rank;
- faixa de camadas no pipeline.

O inspetor exibe a origem do valor: `global`, `sugerido` ou `override local`.
Editar um valor global dispara nova prévia. Editar um valor local altera apenas
o peer selecionado e as faixas que ele serve.

## Sugestão e telemetria do plano

O endpoint de prévia deve retornar, além do plano atual:

- estratégia escolhida;
- nome de implantação normalizado;
- capacidade de cada peer: GPU, VRAM total e livre, RAM disponível, disco,
  utilização e estado do runtime;
- RTT medido entre o coordenador e cada peer e RTT dos links adjacentes usados
  na sugestão;
- motivo da posição de cada peer: GPU livre, arquivo já presente, cópia
  necessária, memória insuficiente, latência ou fallback para RAM;
- estimativa de bytes a transferir, memória de pesos, KV cache e margem de
  segurança;
- rank TP e membros do grupo quando o modo for Tensor.

O score do planner deve ser determinístico e testável. Em ordem de prioridade:

1. cobertura completa e ausência de sobreposição;
2. capacidade de pesos mais KV cache dentro da memória segura;
3. uso de GPU discreta ou unified memory antes de RAM;
4. menor RTT nos links consecutivos da pipeline;
5. menor quantidade de transferências quando os demais critérios empatam;
6. distribuição de sessões para evitar concentrar toda a concorrência em um
   único peer.

Além da rota principal, a prévia retorna rotas alternativas compatíveis com a
cobertura atual. Cada alternativa informa a sequência de peers, RTT acumulado,
camadas cobertas, transferências adicionais e capacidade de sessões. A tela
mostra as alternativas como caminhos secundários no mapa e como uma lista curta
no inspetor. A rota principal continua selecionada por padrão.

O balanceamento de carga usa duas camadas complementares: o planner distribui
as sessões máximas sugeridas entre os peers de acordo com memória e latência, e
o roteador de inferência continua usando o `LoadTracker` para não escolher um
peer já saturado quando houver uma alternativa equivalente. O deploy não inicia
rotas sobrepostas que processem as mesmas camadas sem uma seleção explícita.

Uma máquina sem arquivo pode receber a tarefa se houver uma fonte no cluster.
O motivo aparece como `transferir depois da implantação`, nunca como erro.

## Contrato de backend

`POST /api/ui/deploy/plan` e `POST /api/ui/deploy` passam a aceitar campos
compatíveis e opcionais:

```json
{
  "group": "Qwen3.5-9B-Instruct",
  "kind": "shards",
  "deployment_name": "qwen-prod-salem",
  "strategy": "simple",
  "preferred_node_ids": ["salem-gpu", "ksia"],
  "selection": [],
  "params": {},
  "tp": null
}
```

`strategy` aceita `simple` e `complete`. `preferred_node_ids` expressa as
máquinas que o operador arrastou para o plano. Ausência mantém a compatibilidade
com clientes antigos e permite ao planner escolher o conjunto inteiro.

O retorno mantém `steps`, `available`, `machines`, `params` e os campos antigos,
adicionando campos opcionais para capacidade, latência, score, motivo e
estimativas. Para Fatias, adiciona `coverage`, `route_options` e
`load_distribution`. Clientes antigos continuam lendo o formato existente.

`LaunchRequest` e `ApplyRequest` recebem `deployment_name`. `NodeConfig` e o
status do runtime passam a guardar essa identidade separadamente de
`model_label`. O anúncio de rede continua usando `model_label` para que a
descoberta e o roteamento encontrem o mesmo modelo em todos os peers.

## Tensor Parallelism

No modo Tensor, o mapa substitui a pipeline por um grupo de ranks. O operador
seleciona os peers diretamente no mapa ou na barra inferior. O backend escolhe
rank 0 pelo peer marcado como primary e atribui os demais ranks por capacidade e
latência. Todos recebem o modelo inteiro; a extração dos shards tensor ocorre
no fluxo de implantação existente.

O card antigo de seleção de fatias não aparece no modo Tensor. A prévia mostra
apenas modelo, membros, rank, VRAM/RAM, latência para o primary e motivo de uma
eventual recusa.

## Estados e erros

- Drop de peer: mostra `recalculando sugestão` sem perder o plano anterior.
- Peer sem arquivo: fica selecionado com aviso de transferência posterior.
- Memória insuficiente: o peer pode aparecer como fallback RAM, mas o deploy
  fica bloqueado se pesos mais KV cache não couberem nem na margem segura.
- Latência sem medição: mostra `não medida`, usa a rota disponível e não
  inventa zero.
- Deploy em andamento: botão no topo fica ocupado, o mapa preserva o plano e
  os jobs continuam visíveis.
- Erro de validação: o backend retorna o motivo por peer ou faixa e o inspetor
  destaca a propriedade que precisa ser revista.

## Testes e documentação

- Testar o planner com estratégias simples e completa, peer preferido, peer sem
  arquivo, capacidade GPU/RAM e latências diferentes.
- Testar a separação entre `model_label` e `deployment_name` no launch, runtime,
  status e compatibilidade com requests antigos.
- Testar o frontend para topo, modos, drop de peer, barra inferior, inspetor
  global/local e renderização do TP sem tabela de fatias.
- Atualizar `docs/subsystems/implantacao.md`, `docs/subsystems/web-ui.md` e os
  `AGENTS.md` das pastas Rust e frontend afetadas.
- Não modificar o fork nativo `llama.cpp`.
