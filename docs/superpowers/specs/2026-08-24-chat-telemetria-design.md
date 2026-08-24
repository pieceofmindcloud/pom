# Telemetria real do chat - design

## Contexto

O chat de teste (`apps/frontend/src/routes/Chat.tsx`) já recebe um resumo
`x_pom.trace` no último evento SSE, mas só mostra tempos internos, bytes e
telemetria TP. O cluster e os cards de implantação já carregam capacidade,
configuração e RTT direcional, e o servidor de inferência já expõe a colocação
efetiva no `/health`. A tarefa é levar essas informações verdadeiras para o
diagnóstico da execução que acabou, sem mudar a decisão do planejador nem o
formato do frame de inferência entre peers.

Antes da implementação, a tela foi observada em
`http://localhost:5299/chat`. O cluster local estava sem cobertura completa;
o envio terminou com "A rede respondeu sem nenhum token", então a seção de
telemetria não apareceu nessa execução. Esse estado é uma ausência real e não
será usado como dado de teste visual.

## Inventário do que já existe no fio

| Pedido | Estado atual | Fonte verificada | Decisão |
|---|---|---|---|
| RTT total da rede da execução | Parcial | `PhaseTrace::record` recebe `round_trip_ms`, mas só publica `network_ms` (RTT menos o processamento do peer) no `RequestTrace` que chega ao SSE | Propagar também `round_trip_ms` no resumo SSE e mostrar o total com rótulo que explica a soma |
| RTT entre cada par de peers | Parcial fora do chat | Cada nó mantém `RttTable` e `GET /api/ui/network/rtt`; o `deploy::survey` já consulta os relatórios autenticados de cada origem e monta `PairwiseRtt` por `(origem, destino)` | Criar uma leitura autenticada sob demanda para o chat, reutilizando os relatórios existentes; não medir nem inferir uma aresta nova no frontend |
| Capacidade das máquinas | Já chega, não é exibida | `useCluster` recebe `PeerView.node.system` com CPU, RAM, GPU e disco | Trabalho de exibição usando o snapshot capturado ao fim da execução |
| Camadas na GPU e CPU | Já chega, não é exibida | `GET /api/ui/peer/:node_id/runtime` repassa `/health`, cujo `placement` tem buffers reais por camada e `n_gpu_layers_effective` | Trabalho de consulta e exibição; contar pelos bytes dos buffers e declarar camadas mistas |
| Memória e configuração | Já chega, não é exibida | `PeerView.node.system`, `PeerView.node.config` e `PeerRuntimeHealth` já carregam RAM, GPUs, contexto, batch, fit e KV | Trabalho de exibição, com ausência explícita por peer |
| `POM_PROFILE_SYNC` | Parcial e indireto | `node_backend_completion_ms` é zero quando o perfil está desligado; `llm.rs` lê a variável por processo; o rodapé `chat.telemetry.footnote` é texto fixo | Publicar o estado explícito no `/health` e mostrar por peer. Nunca deduzir o estado de um zero de tempo |

O único novo dado necessário no backend é uma rota HTTP de observabilidade que
agrega os relatórios direcionais já existentes. O novo campo do `/health` e o
novo campo do resumo SSE são contratos HTTP da API, não alterações no frame
binário ou no `node_timings` que trafega entre peers.

## Desenho

### Fluxo de dados

1. O SSE continua sendo a autoridade da execução: tokens, fases, hops, bytes e
   tempos.
2. Ao terminar uma resposta com trace, o Chat guarda o snapshot de cluster
   daquele momento e consulta:
   - `POST /api/ui/chat/telemetry` com os `node_id` dos hops, para os RTTs
     direcionais das arestas consecutivas;
   - `GET /api/ui/peer/:node_id/runtime` para cada hop, para o `/health` real
     de cada máquina.
3. O componente apresenta duas medidas diferentes:
   - RTT agregado da execução: soma dos RTTs de chamadas observados no trace,
     que inclui o processamento do peer;
   - tempo de rede observado: `network_ms`, depois de descontar o processamento;
   - RTT da aresta: medição direcional `origem -> destino`, separada por par.
4. Para uma pipeline de uma máquina, a tela diz que não há arestas entre peers.
   Para dados ausentes, usa "não medido" ou "snapshot indisponível", nunca zero.

### Colocação e configuração

O resumo por máquina usa `system` e `config` do snapshot de cluster. A
colocação efetiva usa `placement.layers[].buffers`: bytes em buffers com nome de
CPU contam como CPU e bytes em outros buffers contam como GPU. Uma camada com
bytes dos dois lados aparece como mista, evitando transformar um override de
buffer type em um número falso. Se `placement` for nulo, o card diz que a
colocação efetiva não foi medida.

`profile_sync_enabled` será um booleano no `/health`, produzido pela mesma
função que controla `llama_synchronize`. O Chat mostra "ativo", "desligado" ou
"indisponível" por peer. O valor não é deduzido de `backend_completion_ms`.

### Tratamento de falhas

Falha na consulta de RTT ou runtime não apaga a telemetria SSE. Cada card
continua mostrando os campos disponíveis e marca apenas a parte ausente. Uma
resposta de peer sem a nova chave, como aconteceria em um nó ainda não
atualizado, vira estado desconhecido e não quebra o chat.

## Testes

- Testes do `ExecutionTelemetry` cobrem RTT agregado, diferença entre RTT de
  aresta e RTT do card, pipeline de uma máquina, pares sem medição, capacidade,
  colocação GPU/CPU/mista, configuração e `POM_PROFILE_SYNC` por peer.
- Testes backend cobrem a filtragem dos relatórios direcionais por origem
  autenticada e o contrato do `/health`.
- A suíte frontend e o build TypeScript são obrigatórios. A suíte Rust do
  crate `node` será executada quando disponível.

## Fora de escopo

- Não mudar o planejador, a escolha de rota ou os anúncios gossip.
- Não estimar RTT por `network_ms`, pela rota do gateway ou pela soma de outras
  arestas.
- Não adicionar compatibilidade para um formato antigo de inferência.
- Não alterar o frame binário de estado oculto entre peers.
