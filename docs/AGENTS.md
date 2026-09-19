# AGENTS.md - docs/

Documentacao de design e plano. A regra de nomes: **spec e design, plan e
implementacao**. Ambas vivem em `docs/superpowers/`, com data no nome
(`YYYY-MM-DD-<slug>`). Alem disso ha uma pasta `architecture/` para notas
permanentes do repositorio. O manual do operador e o `AGENTS.md` da raiz
(manutencao separada - nao tocar).

## architecture/

- `routing-plugin-contract.md` - unico arquivo. Fronteira entre os crates
  publicos (`crates/routing-contract`, `crates/routing-base`) e um futuro
  provider enterprise privado: o provider so recebe `RoutingRequest` e devolve
  `RoutingPlan`, nunca sockets, arquivos de modelo, credenciais HTTP nem o
  motor de inferencia; o node segue no planejador interno
  (`crates/node/src/routing/pipeline.rs`) enquanto a cobertura de equivalencia
  cresce. O README.md da raiz aponta para ele.

- `rdma-hf-provider-reuso.md` - levantamento pré-implementação e fronteiras de
  reuso da integração `ModelProvider` com o CLI Hugging Face.

O relatório de entrega desta integração está em
`rdma-hf-provider-relatorio.md` e registra também o status autenticado do CLI,
a consulta de licença e o fallback para detalhes do modelo.
O relatório da troca para HTTP nativo está em
`rdma-hf-provider-http-relatorio.md`; ele registra a resposta factual sobre o
empacotamento Python, o cache próprio, retomada, cancelamento, integridade,
autenticação e a remoção do caminho por CLI.
O levantamento de reuso da fase de UI está em
`rdma-hf-provider-ui-reuso.md`.
O relatório de entrega da fase de UI está em
`rdma-hf-provider-ui-relatorio.md` e descreve o painel de status, os modelos
gated e a licença antes do download.
- `bench-llamacpp-qwen36-unified.md` - registro versionado da rodada de
  comparação do llama.cpp puro com cache unificado, MTP e a geometria do POM.
- `kv-cache-device-placement.md` - investigação da separação entre dispositivo
  dos pesos e dispositivo do cache KV, incluindo a decisão pendente que exige
  uma API nova no fork do llama.cpp.
- `subsystems/freetoken-moe.md` - método, medições e invariantes do goal que
  adiciona o caminho MoE próprio ao lado do caminho denso, incluindo o marco
  dos quatro modelos da ksia, a salvaguarda por dtype, o diagnóstico de reserva
  de KV multiplicada por `max_sessions` e o goal CUDA posterior.
- `subsystems/rede-p2p.md` - descoberta, identidade, TLS e os canais libp2p de
  logs de processo e auditoria replicados entre admins.

O fluxo incremental de topologia de deployments, incluindo o modo misto e as
versões criadas ao adicionar ou remover máquinas, é detalhado em
`subsystems/implantacao.md`, `subsystems/api-http.md` e `subsystems/web-ui.md`.

O plano da correção do reload após idle-unload está em
`superpowers/plans/2026-09-14-reload-idle-runtime-v2.md`.
O plano da correção do rollback recorrente Kimi-K3 está em
`superpowers/plans/2026-09-15-hybrid-memory-decode-segv.md`.
O plano do hot path CUDA heterogêneo alinhado ao FreeToken está em
`superpowers/plans/2026-09-15-moe-freetoken-performance.md`.
O plano da correção do contrato rico de heartbeat Enterprise está em
`superpowers/plans/2026-09-16-enterprise-heartbeat-schema.md`.

## subsystems/

Os guias permanentes do runtime ficam em `docs/subsystems/`. `rede-p2p.md`
documenta também que os testes usam o `Network::testkit` em loopback, com
destino de anúncio injetável e sem broadcast global.
As sondas autenticadas das políticas de cluster usam listeners reais no
testkit e percorrem os candidatos de gateway quando um endereço fantasma
responde 401/403. `fatiamento.md`
documenta a seleção de tensores obrigatórios, o custo real das cabeças repetidas,
o rebase de metadata de camadas para a numeração local e a leitura somente de
cabeçalho do catálogo, inclusive para o modelo selecionado. `paralelismo.md`
documenta a diferença entre pipeline e tensor parallelism, os custos de WAN e
a leitura da telemetria correspondente. `build-execucao.md` documenta também a
geração em tempo de compilação da lista MTP a partir do fork local do
`llama.cpp`, o helper condicional de sccache e a medição do target compartilhado.
`inferencia.md` e `web-ui.md` também
documentam o RTT observado do Chat, o RTT direcional por aresta, a leitura
efetiva de colocação e configuração por peer, a reserva do segundo contexto MTP
e o recuo dele para a RAM. Em `inferencia.md`, o fit do modelo principal é a
autoridade: a reserva do MTP só entra numa tentativa conjunta que não possa
reduzir suas camadas. `implantacao.md` também documenta a
seleção de `tp_parts`, os ranks `1ofN`, a transferência do `.pom` antes do
arquivo de peso e a regra de várias faixas por máquina com cobertura global
completa, além da prévia analítica de capacidade sem chamada ao fit nativo.
`build-execucao.md` e `freetoken-moe.md` também registram a investigação de
NCCL no link final CUDA e a separação entre o caminho llama.cpp e o executor
MoE próprio.
`superpowers/plans/2026-09-12-windows-crash-root-cause.md` registra o
rastreamento da confirmação `model_loaded`, as fronteiras nativas do `/health`
e as evidências que ainda precisam de reprodução real no Windows.
`web-ui.md` descreve também a escolha de exposição administrativa, o token
humano separado da `connection_key`, a autoridade do perfil `admin` sobre
adicionar máquina e desimplantar e a orientação de TLS em Preferências. A
fronteira é de rota no gateway: a pipeline continua dual-stack para o fallback
HTTP do túnel libp2p. O guia descreve como essa validação aparece na tela Implantar, como o mapa
apresenta hops e RTTs direcionais e como o perfil Inference reduz a vista sem
criar autorização no backend. `inferencia.md` registra também
as causas conhecidas de perda do estado de sessão, o estado de disponibilidade
do MTP no `/health`, o rollback por snapshots do Kimi-K3 e a tarefa de
observabilidade recomendada para distingui-las
em campo. A seção 2.1.1 registra a regressão de endereço anunciada de
2026-09-04, a reaplicação da rota morta por lote e a memória de falha que a
corrige.
`api-http.md` registra também que o endereço anunciado é uma mutação local
disponível a todo perfil, que o diretório público federado aceita somente
registros assinados e que o fallback da interface não intercepta métodos de
rotas administrativas concretas. `implantacao.md` e `inferencia.md` mantêm
o contrato de que deployments `idle` continuam selecionáveis pelo nome, mesmo
sem capacidade efetiva anunciada, para que a primeira requisição possa acordá-los.

`api-http.md`, `build-execucao.md` e `licenciamento-enterprise.md` documentam o
aviso diário de release `pom_core` e a aplicação somente após autorização
explícita na UI. Esse aviso não é um plugin: mora em
`crates/node/src/pom_core_update.rs`, com rotas e tela próprias
(`/api/ui/pom-update/*`, `/config/sobre`), separado do catálogo de plugins.

O armazenamento local unificado está especificado em
`superpowers/specs/2026-09-02-pom-db-unificado-design.md` e implementado no
backend. O runtime mantém uma conexão SQLite compartilhada por processo,
serializa mutações explicitamente e mantém o timeout apenas como segurança;
o contrato HTTP do histórico de conversas fica em `subsystems/api-http.md`.

## superpowers/specs/

Designs, o "por que" e o "o quê" antes de qualquer codigo. Estrutura tipica:
contexto e medições que balizam o desenho, decisoes de arquitetura, invariantes
explícitas e secao final "Fora de escopo". Ex.: a spec de rota dinamica
(`2026-08-13-rota-dinamica-e-implantacao-incremental-design.md`) carrega a
tabela de p50/p95 entre MacBook e ksia (9,6 contra 7,2 ms) que justifica a
margem de troca de 25% e 2 ms, e separa invariante por plano ("uma camada, um
no") de replica por cluster.

Par de specs notaveis: `2026-05-24-pkarr-discovery-design.md` e
`2026-05-25-dcutr-nat-traversal-design.md` (descoberta pela internet, base do
`pkarr_node.rs`/`discovery.rs`), `2026-08-07-principal-node-architecture-design.md`
(modo setup e chave de conexao), `2026-08-12-transferencias-simetricas-e-exclusao-design.md`,
`2026-08-12-request-telemetry-design.md` e
`2026-08-20-batching-continuo-inferencia-distribuida-design.md` (scheduler local,
frame multi-item e sincronizacao de lotes no Tensor Parallelism).
`2026-08-24-chat-telemetria-design.md` registra o inventário do fio e a decisão
de manter os dados no `x_pom`, no `/health` e numa rota autenticada de RTT, sem
mudar frame entre peers ou planner.
`2026-09-08-inferencia-trace-outside-node-design.md` registra o desenho do trace
por inferência, os relógios locais, a matemática de spans paralelos e a
decomposição de `outside-node`/TTFT; o plano homônimo cobre a execução
incremental e a publicação.
`2026-08-26-auditoria-de-requisicoes-design.md` e o plano homônimo registram a
auditoria persistente de prompts em SQLite, a identidade não secreta da chave,
o FTS5 e o ciclo em duas etapas do registro.

O wizard de primeiro acesso está descrito em
`superpowers/specs/2026-09-02-rdma-wizard-primeiro-acesso-design.md`, com o
fluxo navegável e os limites do protótipo. A execução correspondente está em
`superpowers/plans/2026-09-02-rdma-wizard-primeiro-acesso.md`.

## Proteção térmica da implantação

`superpowers/specs/2026-08-26-protecao-termica-gpu-design.md` registra a
política de temperatura e a máquina de histerese; o plano de implementação
correspondente está em `superpowers/plans/2026-08-26-protecao-termica-gpu.md`.

## superpowers/plans/

Planos de implementacao, task a task no estilo TDD: teste que falha →
implementacao minima → teste que passa → commit, com o codigo de teste e de
implementacao embutidos no proprio arquivo. Cada plano referencia a spec pelo
caminho no cabecalho. `- [x]` marca passo concluido. Arquivos e linhas citados
sao do momento em que o plano foi escrito - confira `file:linha` antes de usar,
o codigo anda.

Ex.: `2026-08-13-rota-dinamica-e-implantacao-incremental.md` implementa a spec
acima em duas fases (A: rota dinamica em `crates/node/src/network/route.rs` e
`addrs.rs`; B: endpoints incrementais em `backend/deploy.rs`), cada task com
commit proprio. `2026-05-24-pkarr-discovery.md` e o par do design homonimo.
O plano `2026-09-02-nome-comercial-gpu.md` cobre a resolucao de nomes de GPU
pela descricao do backend, pela base do sistema e pelo identificador PCI.
O plano `2026-09-14-reload-idle-runtime.md` registra a reprodução e a correção
da limpeza nativa durante a recarga de um peer ocioso.

## Convencao de nomes - e a excecao

- `specs/YYYY-MM-DD-<slug>-design.md` - design. Todos seguem o sufixo, menos um:
  `2026-08-12-inferencia-pela-internet-mudancas.md` (sem `-design`), que e uma
  "proposta" anterior a convencao, marcada "nenhuma decisao tomada". Na duvida,
  abra o arquivo e leia o cabecalho.
- `plans/YYYY-MM-DD-<slug>.md` - implementacao.
- Nomes em ingles ou portugues dependem do tema; a data desambigua versoes do
  mesmo assunto (ex.: `2026-08-12-request-telemetry-design.md` e plano).

## Onde NAO procurar

- O manual do operador (portas, cluster, pegadinhas) e o `AGENTS.md` da raiz.
- Os lancadores `run-p1.sh`/`run-p2.sh`/`request.sh`, citados como se fossem da
  raiz, vivem em `old/scripts/` (legado).
- Os scripts Python (`inference_server.py`, `split_model.py`), citados como se
  fossem da raiz, vivem em `old/python-inference/` e sao legado; o runtime
  suportado e Rust (`crates/node/src/inference/infer_server.rs`,
  `crates/node/src/models/splitter.rs`). Caracterizados em `scripts/AGENTS.md`.

`docs/subsystems/rede-p2p.md` também documenta o Chat da rede: protocolo
libp2p, validação e retransmissão pelo principal, catraca autenticada do
interruptor e prioridade do túnel de inferência sobre o tráfego de conversa.

`subsystems/licenciamento-enterprise.md` registra o contrato de licença por
`plugin_code` independente (catálogo `PLUGIN_CATALOG`, hoje só `enterprise`),
entitlement das telas de auditoria, a fronteira `pom`/`pom-plugin-host`, o
plugin `enterprise.db` e os limites restantes da integração P2P/release.

## Defeitos anotados

Nenhum em aberto. O `README.md` da raiz foi reconciliado com o codigo em
2026-08-15, mas continua sendo apresentacao: para arquitetura valem o
`AGENTS.md` da raiz e os `docs/subsystems/*`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
