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
- `subsystems/freetoken-moe.md` - método, medições e invariantes do goal que
  adiciona o caminho MoE próprio ao lado do caminho denso, incluindo o marco
  dos quatro modelos da ksia, a salvaguarda por dtype e o goal CUDA posterior.

## subsystems/

Os guias permanentes do runtime ficam em `docs/subsystems/`. `fatiamento.md`
documenta a seleção de tensores obrigatórios, o custo real das cabeças repetidas
e o rebase de metadata de camadas para a numeração local. `paralelismo.md`
documenta a diferença entre pipeline e tensor parallelism, os custos de WAN e
a leitura da telemetria correspondente. `inferencia.md` e `web-ui.md` também
documentam o RTT observado do Chat, o RTT direcional por aresta, a leitura
efetiva de colocação e configuração por peer, a reserva do segundo contexto MTP
e o recuo dele para a RAM. `implantacao.md` também documenta a
seleção de `tp_parts`, os ranks `1ofN`, a transferência do `.pom` antes do
arquivo de peso e a regra de várias faixas por máquina com cobertura global
completa. `web-ui.md` descreve como essa validação aparece na tela Implantar e
como o mapa apresenta hops e RTTs direcionais. `inferencia.md` registra também
as causas conhecidas de perda do estado de sessão, o estado de disponibilidade
do MTP no `/health` e a tarefa de observabilidade recomendada para distingui-las
em campo.

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
`2026-08-26-auditoria-de-requisicoes-design.md` e o plano homônimo registram a
auditoria persistente de prompts em SQLite, a identidade não secreta da chave,
o FTS5 e o ciclo em duas etapas do registro.

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

## Defeitos anotados

Nenhum em aberto. O `README.md` da raiz foi reconciliado com o codigo em
2026-08-15, mas continua sendo apresentacao: para arquitetura valem o
`AGENTS.md` da raiz e os `docs/subsystems/*`.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
