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

## subsystems/

Os guias permanentes do runtime ficam em `docs/subsystems/`. `paralelismo.md`
documenta a diferença entre pipeline e tensor parallelism, os custos de WAN e
a leitura da telemetria correspondente.

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
